//! `inf-bench blob-set` — the blob `SET` throughput instrument: a
//! load-response curve of acknowledged blob `SET`s per second against one
//! cell of an `infinityd` node, closed loop, by connection count, with
//! the client-side ack latency beside it and the counters the server
//! reports for the write path.
//!
//! The instrument is specified before it measures:
//!
//! - **Estimator and scope.** One tiered `always` namespace, `BLOB-THRESHOLD`
//!   below the value size so every `SET` creates an extent, 64 KiB values,
//!   `C` connections each with one command in flight. Every key carries one
//!   hash tag, so one cell owns the whole load. `X(C)` is the count of
//!   `+OK` replies received inside the measured window divided by its
//!   length, after a warm-up the count excludes; p50, p99 and p99.9 are
//!   the per-reply latencies at the client, in microseconds.
//! - **Resolution.** Counts are exact: one reply over the window, 1/30 of
//!   a `SET` per second at the default 30 s. A latency reads to the bucket
//!   of its histogram (32 per octave, about 3 %), disclosed in the report.
//! - **Spread, with a same-binary control leg.** A point is `--replicates`
//!   runs of two legs, `a` and `b`, interleaved (`a b`, then `b a`), both
//!   of the one binary: the instrument's control. The legs' means must
//!   agree within 5 % on `X` and 10 % on p99, else the point is
//!   `Evidence-pending`: a number outside its own control's spread is
//!   never a number. An A/B of two binaries is four legs (A, A′, B, B′),
//!   each binary with its own A/A spread, and a difference counts only
//!   above both spreads; that shape is not built here.
//! - **Liveness, with its planted red.** Over a leg, the server's
//!   `tiering_blob_extents_created` must rise by the acknowledged `SET`s,
//!   the warm-up's counted beside the window's, within the connections
//!   (an extent whose reply the deadline cut off), else the leg is
//!   `VACUOUS`; every reply must be `+OK`, in the warm-up too, else the
//!   leg is red. `--plant blob-threshold-above` creates the namespace
//!   with a threshold above the value size, so no extent is created and
//!   the run must exit `VACUOUS`; `--plant reply-err` feeds the reply
//!   path a fixture stream holding one `-ERR`, and the run must exit red.
//!
//! - **Both sides of the load.** The generator's own CPU over the window
//!   (`utime + stime` of this process from `/proc`, in cores) is printed
//!   beside `X` per leg; a leg whose generator used more than half of the
//!   CPUs it may run on is marked not server capacity and is no number.
//!
//! The data root (`--data-root`) is a directory on the filesystem under
//! test, never a memory filesystem. Each leg runs on its own directory
//! under it, `<leg>-c<connections>-r<replicate>`, created fresh and
//! removed when the leg ends — on success, on an error and on a panic
//! alike, since a guard value owns it. Only a killed process leaves one
//! behind, so a run removes every leg directory it finds under the root
//! before its first leg and prints each.
//!
//! The report labels its tier: without `--reference-box` every number is
//! dev-tier, disclosed and never published.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cli::Flags;
use crate::envcheck;
use crate::finehist::FineHistogram;
use crate::gaterun::{
    CLOCK_TICKS_PER_S, ServerGuard, admit_device_root, env_gate, read_proc, spawn_infinityd,
};
use crate::resp::{connect, encode_command, reply_len, request};

const FLAGS: (&[&str], &[&str]) = (
    &["allow-dirty", "unsafe-env", "reference-box"],
    &[
        "allow-dirty",
        "unsafe-env",
        "reference-box",
        "infinityd-bin",
        "data-root",
        "artifacts-root",
        "cells",
        "pin-start",
        "connections",
        "replicates",
        "warmup-s",
        "duration-s",
        "value-size",
        "blob-threshold",
        "plant",
    ],
);

/// The curve's default connection counts: both sides of a 16-entry window.
const CONNECTIONS_DEFAULT: &str = "1,8,16,17,32,64,128";
/// The namespace every leg creates on a fresh data directory.
const NAMESPACE: &[u8] = b"blobset";
/// The two legs of the one binary, interleaved per replicate.
const LEG_NAMES: [&str; 2] = ["a", "b"];
/// The spread budget on `X` between the two legs' means, in percent.
const SPREAD_X_PERCENT: f64 = 5.0;
/// The spread budget on p99 between the two legs' means, in percent.
const SPREAD_P99_PERCENT: f64 = 10.0;
/// The generator's CPU bar: a leg whose generator used more than this
/// share of the CPUs it may run on (in cores over the window) is not
/// server capacity.
const GENERATOR_CPU_BAR_PERCENT: f64 = 50.0;
/// How a server field read beside the curve folds across the node's
/// cells. The population is named by the fold: a sum is the node's, a
/// loaded-cell read is that cell's.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Fold {
    /// A per-cell total: the node's value is the cells' sum, which on
    /// this one-cell load is the loaded cell's.
    Sum,
    /// A percentile of one cell's lifetime histogram: read from the
    /// loaded cell alone and labeled as its lifetime value. A sum of
    /// percentiles across cells is not a number.
    LoadedCell,
}

/// The server fields read beside the curve, each with its fold; an
/// absent one is reported as absent, never as zero.
const COUNTERS: &[(&str, Fold)] = &[
    ("extent_parks", Fold::Sum),
    ("frame_waits_extent", Fold::Sum),
    ("fsync_latency_p99_us", Fold::LoadedCell),
    (EXTENTS_FIELD, Fold::Sum),
];
/// The per-cell total of blob extents created: the liveness counter, and
/// what names the loaded cell (the one whose count rose).
const EXTENTS_FIELD: &str = "tiering_blob_extents_created";

/// Folds one field's per-cell reads: `Sum` adds every cell that printed
/// it (`None` when none did); `LoadedCell` reads the loaded cell alone.
fn fold_counter(values: &[Option<u64>], fold: Fold, loaded: Option<usize>) -> Option<u64> {
    match fold {
        Fold::Sum => {
            let present: Vec<u64> = values.iter().flatten().copied().collect();
            if present.is_empty() { None } else { Some(present.iter().sum()) }
        }
        Fold::LoadedCell => values.get(loaded?).copied().flatten(),
    }
}

/// What one plant does to the run (the instrument's planted reds).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// The namespace's `BLOB-THRESHOLD` is above the value size: no extent
    /// is created, so the liveness clause must exit `VACUOUS`.
    BlobThresholdAbove,
    /// The reply path is fed a fixture stream holding one `-ERR`: the leg
    /// must exit red.
    ReplyErr,
}

impl Plant {
    fn parse(raw: Option<&str>) -> Result<Plant, String> {
        match raw {
            None => Ok(Plant::None),
            Some("blob-threshold-above") => Ok(Plant::BlobThresholdAbove),
            Some("reply-err") => Ok(Plant::ReplyErr),
            Some(other) => {
                Err(format!("--plant {other}: the plants are blob-threshold-above and reply-err"))
            }
        }
    }
}

/// The verdict of one leg, in the order a report reads them: a reply that
/// was not `+OK` is red before any count is believed; a count the server's
/// extent counter does not confirm is vacuous; a leg whose generator was
/// over its CPU bar is not server capacity; the rest is a measurement.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LegVerdict {
    Measured,
    NotServerCapacity(String),
    Vacuous(String),
    Red(String),
}

/// One leg's measurement.
struct Leg {
    name: &'static str,
    connections: usize,
    replicate: usize,
    acked: u64,
    /// `+OK` replies to commands sent before the window: no part of `X`,
    /// but extents the liveness clause expects.
    warmup_acked: u64,
    errors: u64,
    window: Duration,
    latency_us: FineHistogram,
    /// Every cell's reads, in cell order.
    cells: Vec<CellScrape>,
    /// The generator's `utime + stime` over the window, in cores.
    generator_cpu_cores: f64,
    /// The CPUs the generator may run on (its affinity mask).
    generator_cpus: usize,
    verdict: LegVerdict,
}

/// One cell's reads of the fields beside the curve, `None` for a field
/// the cell did not print.
type CellFields = BTreeMap<&'static str, Option<u64>>;

/// One cell's reads of the fields beside the curve: the extent counter
/// before the leg, every field after it.
struct CellScrape {
    cell: u16,
    extents_before: Option<u64>,
    after: CellFields,
}

impl CellScrape {
    /// The rise of the extent counter over the leg; `None` when the cell
    /// did not print it at either read.
    fn extents_created(&self) -> Option<u64> {
        let after = self.after.get(EXTENTS_FIELD).copied().flatten()?;
        Some(after.saturating_sub(self.extents_before?))
    }
}

impl Leg {
    fn throughput(&self) -> f64 {
        self.acked as f64 / self.window.as_secs_f64()
    }

    /// The generator's CPU bar in cores: half the CPUs it may run on.
    fn generator_cpu_bar_cores(&self) -> f64 {
        self.generator_cpus as f64 * GENERATOR_CPU_BAR_PERCENT / 100.0
    }

    /// The extents created on every cell, or `None` when a cell did not
    /// print the counter.
    fn extents_created(&self) -> Option<Vec<u64>> {
        self.cells.iter().map(CellScrape::extents_created).collect()
    }

    /// The index of the one cell whose extent counter rose: the cell
    /// owning the hash tag. `None` before the verdict names it.
    fn loaded_cell(&self) -> Option<usize> {
        let created = self.extents_created()?;
        let mut loaded = created.iter().enumerate().filter(|&(_, &n)| n > 0).map(|(i, _)| i);
        let first = loaded.next()?;
        if loaded.next().is_some() { None } else { Some(first) }
    }

    /// Each field beside the curve folded over the cells, labeled with
    /// its population: `field` for a node sum, `field(cell N lifetime)`
    /// for the loaded cell's percentile.
    fn counters(&self) -> Vec<(String, Option<u64>)> {
        let loaded = self.loaded_cell();
        COUNTERS
            .iter()
            .map(|&(field, fold)| {
                let values: Vec<Option<u64>> =
                    self.cells.iter().map(|c| c.after.get(field).copied().flatten()).collect();
                let label = match (fold, loaded) {
                    (Fold::Sum, _) => field.to_string(),
                    (Fold::LoadedCell, Some(i)) => {
                        format!("{field}(cell {} lifetime)", self.cells[i].cell)
                    }
                    (Fold::LoadedCell, None) => format!("{field}(loaded cell unknown)"),
                };
                (label, fold_counter(&values, fold, loaded))
            })
            .collect()
    }
}

/// Which part of the leg a command was sent in, by its send instant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    WarmUp,
    Window,
}

/// One connection's share of a leg: its acknowledged `SET`s inside the
/// window and before it, every non-`+OK` reply, and its latencies.
struct ConnResult {
    acked: u64,
    warmup_acked: u64,
    errors: u64,
    first_error: Option<String>,
    latency_us: FineHistogram,
}

impl ConnResult {
    fn new() -> ConnResult {
        ConnResult {
            acked: 0,
            warmup_acked: 0,
            errors: 0,
            first_error: None,
            latency_us: FineHistogram::new(),
        }
    }

    /// Accounts one reply of the closed loop: `+OK` is an acknowledgement,
    /// counted in its phase with its latency recorded only in the window;
    /// anything else is an error carried whole, in either phase.
    fn account(&mut self, reply: &[u8], phase: Phase, latency_us: u64) {
        if reply != b"+OK\r\n" {
            self.errors += 1;
            self.first_error
                .get_or_insert_with(|| String::from_utf8_lossy(reply).trim_end().to_string());
            return;
        }
        match phase {
            Phase::WarmUp => self.warmup_acked += 1,
            Phase::Window => {
                self.acked += 1;
                self.latency_us.record(latency_us);
            }
        }
    }
}

/// Consumes every whole reply at the front of `received` into `result`, each
/// in `phase` with `latency_us`. The one reply loop: the connections
/// and the plant's fixture stream both pass through it.
fn drain_replies(received: &mut Vec<u8>, result: &mut ConnResult, phase: Phase, latency_us: u64) {
    while let Some(n) = reply_len(received) {
        let reply: Vec<u8> = received.drain(..n).collect();
        result.account(&reply, phase, latency_us);
    }
}

/// One closed-loop connection: `USE`, then `SET`s with one in flight until
/// the deadline; replies inside the window count.
fn run_conn(
    port: u16,
    index: usize,
    value_size: usize,
    warmup_end: Instant,
    deadline: Instant,
) -> Result<ConnResult, String> {
    let mut stream = connect("127.0.0.1", port)?;
    let reply = request(&mut stream, &[b"INF.NS", b"USE", NAMESPACE])?;
    if reply != b"+OK\r\n" {
        return Err(format!("USE on connection {index}: {}", String::from_utf8_lossy(&reply)));
    }
    let value = vec![0xABu8; value_size];
    let mut result = ConnResult::new();
    let mut received: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    let mut ordinal = 0u64;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // One hash tag on every key: one cell owns the whole load.
        let key = format!("{{blobset}}:c{index}:{ordinal}");
        ordinal += 1;
        stream
            .write_all(&encode_command(&[b"SET", key.as_bytes(), &value]))
            .map_err(|e| format!("write: {e}"))?;
        let sent = Instant::now();
        let phase = if sent < warmup_end { Phase::WarmUp } else { Phase::Window };
        // One command in flight: the next whole reply is its reply.
        while reply_len(&received).is_none() {
            let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err(format!("connection {index}: closed mid-reply"));
            }
            received.extend_from_slice(&chunk[..n]);
        }
        let latency_us = u64::try_from(sent.elapsed().as_micros()).unwrap_or(u64::MAX);
        drain_replies(&mut received, &mut result, phase, latency_us);
    }
    Ok(result)
}

/// Reads every cell's INFO once: `(cell, field → value)` in cell order,
/// a field absent from a cell's INFO read as `None`.
fn scrape(port: u16, cells: u16) -> Result<Vec<(u16, CellFields)>, String> {
    let infos = crate::gaterun::scrape_cells(port, cells)?;
    Ok(infos
        .iter()
        .map(|info| {
            let cell = info.get("cell").and_then(|v| v.parse().ok()).unwrap_or(0);
            let fields = COUNTERS
                .iter()
                .map(|&(field, _)| (field, info.get(field).and_then(|v| v.parse::<u64>().ok())))
                .collect();
            (cell, fields)
        })
        .collect())
}

/// Creates the namespace on a fresh node, riding out the boot's
/// `-LOADING` window, and waits for its fan to every cell.
fn create_namespace(port: u16, cells: u16, threshold: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(
            &mut stream,
            &[
                b"INF.NS",
                b"CREATE",
                NAMESPACE,
                b"MODE",
                b"durable",
                b"FSYNC",
                b"always",
                b"MEM-BUDGET",
                b"64mb",
                b"BLOB-THRESHOLD",
                threshold.as_bytes(),
            ],
        )?;
        if reply.starts_with(b"+OK") {
            break;
        }
        if !(reply.starts_with(b"-LOADING") && Instant::now() < deadline) {
            return Err(format!("INF.NS CREATE failed: {}", String::from_utf8_lossy(&reply)));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut streak: u16 = 0;
    while streak < 4 * cells {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(&mut stream, &[b"INF.NS", b"USE", NAMESPACE])?;
        if reply.starts_with(b"+OK") {
            streak += 1;
        } else {
            streak = 0;
            if Instant::now() >= deadline {
                return Err(format!(
                    "namespace never fanned to all cells: {}",
                    String::from_utf8_lossy(&reply)
                ));
            }
        }
    }
    Ok(())
}

/// This process's `utime + stime`, in clock ticks: the generator's side
/// of the load.
fn generator_cpu_ticks() -> Result<u64, String> {
    read_proc(std::process::id(), None)
        .map(|sample| sample.cpu_ticks())
        .map_err(|e| format!("the generator's /proc/self/stat: {e}"))
}

/// One leg's data directory under the data root, named
/// `<leg>-c<connections>-r<replicate>`. `create` makes it fresh and the
/// value removes it when dropped, so a leg that returns through `?` or
/// unwinds leaves nothing behind; only a killed process can, and
/// `clear_stale` removes those at the next run's start.
struct LegDir {
    path: PathBuf,
}

impl LegDir {
    fn name(leg: &str, connections: usize, replicate: usize) -> String {
        format!("{leg}-c{connections}-r{replicate}")
    }

    /// The inverse of `name`: `(leg, connections, replicate)` for a name
    /// this instrument made, `None` for any other directory.
    fn parse_name(name: &str) -> Option<(&str, usize, usize)> {
        let (leg, rest) = name.split_once("-c")?;
        if !LEG_NAMES.contains(&leg) {
            return None;
        }
        let (connections, replicate) = rest.split_once("-r")?;
        Some((leg, parse_count(connections)?, parse_count(replicate)?))
    }

    /// Removes whatever `root/<name>` holds and creates it empty.
    fn create(
        root: &Path,
        leg: &str,
        connections: usize,
        replicate: usize,
    ) -> Result<LegDir, String> {
        let path = root.join(LegDir::name(leg, connections, replicate));
        remove_tree(&path)?;
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(LegDir { path })
    }

    /// Removes every leg directory under `root` that an earlier run left
    /// behind (one a killed process could not remove), creating the root
    /// when it is absent; returns the names removed, sorted. One pass
    /// over the root's entries; a symbolic link is not followed.
    fn clear_stale(root: &Path) -> Result<Vec<String>, String> {
        let at = |e: std::io::Error| format!("{}: {e}", root.display());
        std::fs::create_dir_all(root).map_err(at)?;
        let mut removed = Vec::new();
        for entry in std::fs::read_dir(root).map_err(at)? {
            let entry = entry.map_err(at)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if LegDir::parse_name(name).is_none() || !entry.file_type().map_err(at)?.is_dir() {
                continue;
            }
            remove_tree(&entry.path())?;
            removed.push(name.to_string());
        }
        removed.sort();
        Ok(removed)
    }
}

impl Drop for LegDir {
    fn drop(&mut self) {
        if let Err(why) = remove_tree(&self.path) {
            eprintln!("blob-set: {why}");
        }
    }
}

/// A decimal count, digits only: `+1` is not a name of ours.
fn parse_count(digits: &str) -> Option<usize> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Removes `path` with everything under it; an absent path is nothing to
/// remove.
fn remove_tree(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: not removed: {e}", path.display())),
    }
}

/// Boots a fresh node from `binary` on `data_dir`, a `LegDir`'s path.
fn boot(
    binary: &str,
    cells: u16,
    data_dir: &Path,
    pin_start: Option<&str>,
) -> Result<ServerGuard, String> {
    let dir = data_dir.display().to_string();
    let mut extra: Vec<&str> = vec!["--data-dir", &dir];
    if let Some(pin) = pin_start {
        extra.push("--pin-start");
        extra.push(pin);
    }
    spawn_infinityd(binary, cells, &extra)
}

struct LegSpec<'a> {
    binary: &'a str,
    cells: u16,
    data_root: &'a Path,
    pin_start: Option<&'a str>,
    threshold: &'a str,
    value_size: usize,
    warmup: Duration,
    window: Duration,
}

/// Runs one leg: boot, the namespace, the counters before, `connections`
/// closed-loop connections through the warm-up and the window, the
/// counters after, the verdict.
fn run_leg(
    spec: &LegSpec<'_>,
    name: &'static str,
    connections: usize,
    replicate: usize,
) -> Result<Leg, String> {
    // The directory is declared before the server so it drops after it:
    // on every exit the node is stopped, then its directory is removed.
    let data_dir = LegDir::create(spec.data_root, name, connections, replicate)?;
    let server = boot(spec.binary, spec.cells, &data_dir.path, spec.pin_start)?;
    let port = server.port;
    create_namespace(port, spec.cells, spec.threshold)?;
    let before = scrape(port, spec.cells)?;
    let start = Instant::now();
    let warmup_end = start + spec.warmup;
    let deadline = warmup_end + spec.window;
    let (results, cpu_ticks) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..connections)
            .map(|index| {
                scope.spawn(move || run_conn(port, index, spec.value_size, warmup_end, deadline))
            })
            .collect();
        // The generator's CPU is read at the window's two ends: the
        // connections run through the warm-up meanwhile.
        std::thread::sleep(warmup_end.saturating_duration_since(Instant::now()));
        let ticks_before = generator_cpu_ticks();
        let results: Vec<Result<ConnResult, String>> =
            handles.into_iter().map(|h| h.join().expect("connection thread")).collect();
        let ticks = ticks_before
            .and_then(|before| generator_cpu_ticks().map(|after| after.saturating_sub(before)));
        (results, ticks)
    });
    let generator_cpu_cores =
        cpu_ticks? as f64 / CLOCK_TICKS_PER_S as f64 / spec.window.as_secs_f64();
    let mut leg = Leg {
        name,
        connections,
        replicate,
        acked: 0,
        warmup_acked: 0,
        errors: 0,
        window: spec.window,
        latency_us: FineHistogram::new(),
        cells: Vec::new(),
        generator_cpu_cores,
        generator_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        verdict: LegVerdict::Measured,
    };
    let mut first_error: Option<String> = None;
    for result in results {
        let result = result?;
        leg.acked += result.acked;
        leg.warmup_acked += result.warmup_acked;
        leg.errors += result.errors;
        leg.latency_us.merge(&result.latency_us);
        if first_error.is_none() {
            first_error = result.first_error;
        }
    }
    let after = scrape(port, spec.cells)?;
    leg.cells = before
        .into_iter()
        .zip(after)
        .map(|((cell, before), (_, after))| CellScrape {
            cell,
            extents_before: before.get(EXTENTS_FIELD).copied().flatten(),
            after,
        })
        .collect();
    drop(server);
    leg.verdict = verdict_of(&leg, first_error.as_deref());
    Ok(leg)
}

/// The leg's verdict from its counts (a pure rule the plants test).
fn verdict_of(leg: &Leg, first_error: Option<&str>) -> LegVerdict {
    if leg.errors > 0 {
        return LegVerdict::Red(format!(
            "{} reply(ies) not +OK; the first: {}",
            leg.errors,
            first_error.unwrap_or("?")
        ));
    }
    let Some(per_cell) = leg.extents_created() else {
        return LegVerdict::Vacuous(format!("{EXTENTS_FIELD} is absent from a cell's INFO"));
    };
    // Every acknowledged SET, the warm-up's included, created an extent;
    // a command whose reply the deadline cut off (at most one per
    // connection) is the slack between the counter and the counts.
    let created: u64 = per_cell.iter().sum();
    let acknowledged = leg.acked + leg.warmup_acked;
    if created < acknowledged || created > acknowledged + leg.connections as u64 {
        return LegVerdict::Vacuous(format!(
            "extents created {created} does not confirm {acknowledged} acknowledged SETs ({} in \
             the window, {} in the warm-up) within the connections ({})",
            leg.acked, leg.warmup_acked, leg.connections
        ));
    }
    // Scope one cell: the hash tag puts every key on one cell, so the
    // counter rose on exactly that cell.
    if leg.loaded_cell().is_none() {
        let risen = per_cell.iter().filter(|&&n| n > 0).count();
        return LegVerdict::Vacuous(format!(
            "the extent counter rose on {risen} cells; the load is scoped to one"
        ));
    }
    // USE on the generator's side: a saturated generator's X is its own.
    if leg.generator_cpu_cores > leg.generator_cpu_bar_cores() {
        return LegVerdict::NotServerCapacity(format!(
            "the generator used {:.2} cores of its {} over the window, above the {}% bar",
            leg.generator_cpu_cores, leg.generator_cpus, GENERATOR_CPU_BAR_PERCENT
        ));
    }
    LegVerdict::Measured
}

/// A point of the curve: both legs over every replicate, and the spread
/// verdict between the legs' means.
struct Point {
    connections: usize,
    legs: Vec<Leg>,
}

/// The two legs' means: `(X, p99)` for each, and the spreads in percent.
struct Spread {
    x: [f64; 2],
    p99: [f64; 2],
    x_percent: f64,
    p99_percent: f64,
}

fn spread_of(legs: &[Leg]) -> Option<Spread> {
    let mean = |name: &str, f: &dyn Fn(&Leg) -> f64| -> Option<f64> {
        let values: Vec<f64> = legs
            .iter()
            .filter(|l| l.name == name && l.verdict == LegVerdict::Measured)
            .map(f)
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let x = [mean("a", &Leg::throughput)?, mean("b", &Leg::throughput)?];
    let p99 = [
        mean("a", &|l| l.latency_us.percentile(99.0) as f64)?,
        mean("b", &|l| l.latency_us.percentile(99.0) as f64)?,
    ];
    let percent = |pair: [f64; 2]| {
        let hi = pair[0].max(pair[1]);
        if hi == 0.0 { 0.0 } else { (pair[0] - pair[1]).abs() / hi * 100.0 }
    };
    Some(Spread { x, p99, x_percent: percent(x), p99_percent: percent(p99) })
}

/// The run's exit class, in order of precedence.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Measured,
    EvidencePending,
    Vacuous,
    Red,
}

fn outcome_of(points: &[Point]) -> Outcome {
    let mut outcome = Outcome::Measured;
    for point in points {
        for leg in &point.legs {
            match &leg.verdict {
                LegVerdict::Red(_) => return Outcome::Red,
                LegVerdict::Vacuous(_) => outcome = Outcome::Vacuous,
                LegVerdict::NotServerCapacity(_) => {
                    if outcome == Outcome::Measured {
                        outcome = Outcome::EvidencePending;
                    }
                }
                LegVerdict::Measured => {}
            }
        }
        if outcome == Outcome::Measured {
            match spread_of(&point.legs) {
                Some(s)
                    if s.x_percent <= SPREAD_X_PERCENT && s.p99_percent <= SPREAD_P99_PERCENT => {}
                _ => outcome = Outcome::EvidencePending,
            }
        }
    }
    outcome
}

fn render_report(points: &[Point], header: &str, outcome: Outcome) -> String {
    format!(
        "# blob-set curve\n\n{header}\noutcome: {outcome:?}\n\n\
         Latencies read to the bucket of a 32-per-octave histogram (about 3 %). `X` is the \
         count of `+OK` replies inside the window over its length. The spread column is the \
         two legs' means against each other; a point over 5 % on `X` or 10 % on p99 is \
         Evidence-pending. Counters are node sums of per-cell totals, except a percentile, \
         read from the loaded cell alone as its lifetime value (boot included). `gen cpu` is \
         this process's utime+stime over the window, in cores; above half its CPUs the leg \
         is not server capacity.\n\n{}\n## points\n\n{}",
        leg_table(points),
        point_table(points)
    )
}

/// One row per leg: the measurement, the counters and the verdict.
fn leg_table(points: &[Point]) -> String {
    let mut out = String::from(
        "| connections | leg | replicate | X (SET/s) | p50 us | p99 us | p99.9 us | acked | \
         extents | gen cpu (cores) | counters | verdict |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for leg in points.iter().flat_map(|point| point.legs.iter()) {
        let counters: Vec<String> = leg
            .counters()
            .iter()
            .map(|(k, v)| format!("{k}={}", v.map_or("absent".to_string(), |v| v.to_string())))
            .collect();
        let extents = match leg.extents_created() {
            Some(per_cell) => per_cell.iter().sum::<u64>().to_string(),
            None => "absent".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {:.2} | {} | {} | {} | {} | {} | {:.2} of {} | {} | {:?} |\n",
            leg.connections,
            leg.name,
            leg.replicate,
            leg.throughput(),
            leg.latency_us.percentile(50.0),
            leg.latency_us.percentile(99.0),
            leg.latency_us.percentile(99.9),
            leg.acked,
            extents,
            leg.generator_cpu_cores,
            leg.generator_cpus,
            counters.join(" "),
            leg.verdict
        ));
    }
    out
}

/// One row per point: the two legs' means and their spread verdict.
fn point_table(points: &[Point]) -> String {
    let mut out = String::from(
        "| connections | X a | X b | spread X % | p99 a | p99 b | spread p99 % | \
         point |\n|---|---|---|---|---|---|---|---|\n",
    );
    for point in points {
        match spread_of(&point.legs) {
            Some(s) => {
                let verdict =
                    if s.x_percent <= SPREAD_X_PERCENT && s.p99_percent <= SPREAD_P99_PERCENT {
                        "measured"
                    } else {
                        "Evidence-pending"
                    };
                out.push_str(&format!(
                    "| {} | {:.2} | {:.2} | {:.2} | {:.0} | {:.0} | {:.2} | {verdict} |\n",
                    point.connections,
                    s.x[0],
                    s.x[1],
                    s.x_percent,
                    s.p99[0],
                    s.p99[1],
                    s.p99_percent
                ));
            }
            None => out.push_str(&format!(
                "| {} | — | — | — | — | — | — | no measured leg |\n",
                point.connections
            )),
        }
    }
    out
}

fn env_header(flags: &Flags, env_ok: bool, filesystem: &str, command: &str) -> String {
    let stamp =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let probes: Vec<String> = envcheck::run_system_probes()
        .iter()
        .map(|o| {
            let result = match &o.result {
                envcheck::ProbeResult::Pass => "pass".to_string(),
                envcheck::ProbeResult::Fail(why) => format!("FAIL ({why})"),
                envcheck::ProbeResult::Skipped(why) => format!("skipped ({why})"),
            };
            format!("{}: {result}", o.name)
        })
        .collect();
    let tier = if flags.bool("reference-box") {
        "reference-box"
    } else {
        "dev (not a claim; never published)"
    };
    format!(
        "date: {stamp} (unix)\ntier: {tier}\nenv-check: {}\ndata-root filesystem: {filesystem}\n\
         probes: {}\ncommand: {command}\n",
        if env_ok { "OK" } else { "FAILED (overridden — not citation-grade)" },
        probes.join("; ")
    )
}

fn write_report(root: &str, report: &str) -> Result<String, String> {
    let stamp =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dir = format!("{root}/{stamp}-blob-set");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    let path = format!("{dir}/report.md");
    std::fs::write(&path, report).map_err(|e| format!("{path}: {e}"))?;
    Ok(path)
}

/// One cell's scrape holding the extent counter alone (the plant's and
/// the tests' fixture).
fn one_cell(before: Option<u64>, after: Option<u64>) -> CellScrape {
    CellScrape { cell: 0, extents_before: before, after: BTreeMap::from([(EXTENTS_FIELD, after)]) }
}

/// The `reply-err` plant: the reply path over a fixture stream holding
/// one `-ERR` among `+OK`s, through the one reply loop the connections
/// use.
fn plant_reply_err() -> Result<(), String> {
    let mut received = Vec::new();
    for _ in 0..7 {
        received.extend_from_slice(b"+OK\r\n");
    }
    received.extend_from_slice(b"-ERR fixture: one refused SET\r\n");
    received.extend_from_slice(b"+OK\r\n");
    let mut result = ConnResult::new();
    drain_replies(&mut received, &mut result, Phase::Window, 0);
    let leg = Leg {
        name: "a",
        connections: 1,
        replicate: 0,
        acked: result.acked,
        warmup_acked: 0,
        errors: result.errors,
        window: Duration::from_secs(1),
        latency_us: result.latency_us,
        cells: vec![one_cell(Some(0), Some(result.acked))],
        generator_cpu_cores: 0.0,
        generator_cpus: 1,
        verdict: LegVerdict::Measured,
    };
    match verdict_of(&leg, result.first_error.as_deref()) {
        LegVerdict::Red(why) => {
            println!("blob-set: plant reply-err: red as planted ({why})");
            Err("plant reply-err: the run is red".into())
        }
        other @ (LegVerdict::Measured
        | LegVerdict::NotServerCapacity(_)
        | LegVerdict::Vacuous(_)) => Err(format!("plant reply-err did not go red: {other:?}")),
    }
}

/// The run's arguments, parsed and validated once at the boundary.
struct BlobSetArgs {
    env_ok: bool,
    binary: String,
    data_root: String,
    filesystem: String,
    artifacts_root: String,
    cells: u16,
    pin_start: Option<String>,
    connections: Vec<usize>,
    replicates: usize,
    warmup: Duration,
    window: Duration,
    value_size: usize,
    threshold: String,
}

impl BlobSetArgs {
    fn parse(flags: &Flags, plant: Plant) -> Result<BlobSetArgs, String> {
        let env_ok = env_gate(flags)?;
        let reference_box = flags.bool("reference-box");
        let binary = flags.str_or("infinityd-bin", "target/release/infinityd");
        let data_root = flags.require("data-root")?.to_string();
        let filesystem = admit_device_root(flags, Path::new(&data_root), reference_box)?;
        if crate::gaterun::is_memory_fs(&filesystem) {
            return Err(format!(
                "data root {data_root} is {filesystem}: a blob SET's extent and its fsync must \
                 hit a device; point --data-root at the filesystem under test"
            ));
        }
        let connections: Vec<usize> = flags
            .str_or("connections", CONNECTIONS_DEFAULT)
            .split(',')
            .map(|s| s.trim().parse::<usize>().map_err(|e| format!("--connections `{s}`: {e}")))
            .collect::<Result<_, _>>()?;
        if connections.contains(&0) {
            return Err("--connections: every count is at least 1".into());
        }
        let window = Duration::from_secs(flags.u64_or("duration-s", 30)?);
        if window.is_zero() {
            return Err("--duration-s: the window is at least 1 s".into());
        }
        let value_size = flags.usize_or("value-size", 64 << 10)?;
        let threshold = match plant {
            Plant::BlobThresholdAbove => format!("{}", (value_size * 2).max(8 << 10)),
            Plant::None | Plant::ReplyErr => flags.str_or("blob-threshold", "4kb"),
        };
        Ok(BlobSetArgs {
            env_ok,
            binary,
            data_root,
            filesystem,
            artifacts_root: flags.str_or("artifacts-root", ".artifacts/blob-set"),
            cells: flags.u16_or("cells", 4)?,
            pin_start: flags.get("pin-start").map(str::to_string),
            connections,
            replicates: flags.usize_or("replicates", 5)?,
            warmup: Duration::from_secs(flags.u64_or("warmup-s", 5)?),
            window,
            value_size,
            threshold,
        })
    }
}

/// Every point of the curve: both legs of the one binary over every
/// replicate, interleaved `a b` then `b a`, so neither leg always warms
/// the other; each leg on its own directory under the data root, removed
/// when the leg ends, after the stale ones a killed run left are removed
/// and named.
fn run_points(run: &BlobSetArgs) -> Result<Vec<Point>, String> {
    let data_root = Path::new(&run.data_root);
    for name in LegDir::clear_stale(data_root)? {
        println!("blob-set: removed the stale leg directory {}", data_root.join(name).display());
    }
    let spec = LegSpec {
        binary: &run.binary,
        cells: run.cells,
        data_root,
        pin_start: run.pin_start.as_deref(),
        threshold: &run.threshold,
        value_size: run.value_size,
        warmup: run.warmup,
        window: run.window,
    };
    let mut points = Vec::new();
    for &c in &run.connections {
        let mut legs = Vec::new();
        for replicate in 0..run.replicates {
            let order: [usize; 2] = if replicate % 2 == 0 { [0, 1] } else { [1, 0] };
            for which in order {
                let name = LEG_NAMES[which];
                let leg = run_leg(&spec, name, c, replicate)?;
                println!(
                    "blob-set: connections {c} leg {name} replicate {replicate}: X {:.2} SET/s, \
                     p50 {} us, p99 {} us, p99.9 {} us, acked {}, gen cpu {:.2} cores, {:?}",
                    leg.throughput(),
                    leg.latency_us.percentile(50.0),
                    leg.latency_us.percentile(99.0),
                    leg.latency_us.percentile(99.9),
                    leg.acked,
                    leg.generator_cpu_cores,
                    leg.verdict
                );
                legs.push(leg);
            }
        }
        points.push(Point { connections: c, legs });
    }
    Ok(points)
}

/// The run's exit from its outcome: a vacuous or red leg fails the run;
/// a point over the spread budget says so and exits 0.
fn exit_of(outcome: Outcome) -> Result<(), String> {
    match outcome {
        Outcome::Measured => Ok(()),
        Outcome::EvidencePending => {
            println!("blob-set: Evidence-pending: a point's legs disagree past the spread budget");
            Ok(())
        }
        Outcome::Vacuous => Err("VACUOUS: a leg's acknowledged SETs are not confirmed by the \
                                 server's extent counter"
            .into()),
        Outcome::Red => Err("red: a reply was not +OK".into()),
    }
}

/// `inf-bench blob-set`.
pub fn cmd_blob_set(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, FLAGS.0, FLAGS.1)?;
    let plant = Plant::parse(flags.get("plant"))?;
    if plant == Plant::ReplyErr {
        return plant_reply_err();
    }
    let run = BlobSetArgs::parse(&flags, plant)?;
    let command = format!("inf-bench blob-set {}", args.join(" "));
    let header = env_header(&flags, run.env_ok, &run.filesystem, &command);
    let points = run_points(&run)?;
    let outcome = outcome_of(&points);
    let report = render_report(&points, &header, outcome);
    let path = write_report(&run.artifacts_root, &report)?;
    println!("blob-set: report {path}");
    exit_of(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(acked: u64, errors: u64, before: Option<u64>, after: Option<u64>) -> Leg {
        Leg {
            name: "a",
            connections: 4,
            replicate: 0,
            acked,
            warmup_acked: 0,
            errors,
            window: Duration::from_secs(10),
            latency_us: FineHistogram::new(),
            cells: vec![one_cell(before, after)],
            generator_cpu_cores: 0.0,
            generator_cpus: 4,
            verdict: LegVerdict::Measured,
        }
    }

    /// A generator over its CPU bar is not server capacity; a red reply
    /// and an unconfirmed count are judged before it.
    #[test]
    fn a_saturated_generator_is_not_server_capacity() {
        let mut busy = leg(100, 0, Some(0), Some(100));
        busy.generator_cpu_cores = 2.5;
        assert!(matches!(verdict_of(&busy, None), LegVerdict::NotServerCapacity(_)));
        busy.generator_cpu_cores = 2.0;
        assert_eq!(verdict_of(&busy, None), LegVerdict::Measured);
        busy.generator_cpu_cores = 2.5;
        busy.cells[0] = one_cell(Some(0), Some(99));
        assert!(matches!(verdict_of(&busy, None), LegVerdict::Vacuous(_)));
        let mut saturated = Point { connections: 4, legs: vec![leg(100, 0, Some(0), Some(100))] };
        saturated.legs[0].generator_cpu_cores = 3.0;
        saturated.legs[0].verdict = verdict_of(&saturated.legs[0], None);
        assert_eq!(outcome_of(std::slice::from_ref(&saturated)), Outcome::EvidencePending);
    }

    /// A percentile field is never summed: the table names it a
    /// loaded-cell read, and that fold answers one cell's value.
    #[test]
    fn a_percentile_field_is_read_from_the_loaded_cell_never_summed() {
        for &(field, fold) in COUNTERS {
            let percentile = field.contains("_p50") || field.contains("_p99");
            assert_eq!(
                fold == Fold::LoadedCell,
                percentile,
                "{field}: a percentile folds by LoadedCell, a total by Sum"
            );
        }
        let reads = [Some(29_126), Some(10), None, Some(7)];
        assert_eq!(fold_counter(&reads, Fold::LoadedCell, Some(0)), Some(29_126));
        assert_eq!(fold_counter(&reads, Fold::LoadedCell, Some(2)), None);
        assert_eq!(fold_counter(&reads, Fold::LoadedCell, None), None);
        assert_eq!(fold_counter(&reads, Fold::Sum, Some(0)), Some(29_143));
        assert_eq!(fold_counter(&[None, None], Fold::Sum, None), None);
    }

    /// The loaded cell is the one whose extent counter rose; a rise on
    /// two cells, or on none, leaves the load's scope unnamed and the
    /// leg vacuous.
    #[test]
    fn the_loaded_cell_is_the_one_whose_extents_rose() {
        let mut leg = leg(100, 0, Some(0), Some(100));
        leg.cells.push(one_cell(Some(5), Some(5)));
        assert_eq!(leg.loaded_cell(), Some(0));
        assert_eq!(verdict_of(&leg, None), LegVerdict::Measured);
        leg.cells[1] = one_cell(Some(5), Some(6));
        leg.cells[0] = one_cell(Some(0), Some(99));
        assert_eq!(leg.loaded_cell(), None);
        assert!(matches!(verdict_of(&leg, None), LegVerdict::Vacuous(_)));
    }

    /// The liveness clause: the extent counter confirms the window's and
    /// the warm-up's acknowledged count within the connections; fewer, or
    /// more than that, is vacuous.
    #[test]
    fn extents_outside_the_acknowledged_count_are_vacuous() {
        assert_eq!(verdict_of(&leg(100, 0, Some(0), Some(100)), None), LegVerdict::Measured);
        assert_eq!(verdict_of(&leg(100, 0, Some(0), Some(104)), None), LegVerdict::Measured);
        assert!(matches!(
            verdict_of(&leg(100, 0, Some(0), Some(99)), None),
            LegVerdict::Vacuous(_)
        ));
        assert!(matches!(
            verdict_of(&leg(100, 0, Some(0), Some(105)), None),
            LegVerdict::Vacuous(_)
        ));
        assert!(matches!(verdict_of(&leg(100, 0, None, Some(100)), None), LegVerdict::Vacuous(_)));
        let mut warmed = leg(100, 0, Some(0), Some(130));
        warmed.warmup_acked = 30;
        assert_eq!(verdict_of(&warmed, None), LegVerdict::Measured);
        warmed.cells[0] = one_cell(Some(0), Some(100));
        assert!(matches!(verdict_of(&warmed, None), LegVerdict::Vacuous(_)));
    }

    /// Every reply passes through the one loop: an ack in the warm-up is
    /// counted there without a latency, an ack in the window with one,
    /// and an error is red in either phase.
    #[test]
    fn replies_are_accounted_by_phase_and_errors_in_both() {
        let mut result = ConnResult::new();
        let mut received = b"+OK\r\n+OK\r\n".to_vec();
        drain_replies(&mut received, &mut result, Phase::WarmUp, 500);
        assert_eq!((result.acked, result.warmup_acked, result.latency_us.count()), (0, 2, 0));
        let mut received = b"+OK\r\n".to_vec();
        drain_replies(&mut received, &mut result, Phase::Window, 700);
        assert_eq!((result.acked, result.warmup_acked, result.latency_us.count()), (1, 2, 1));
        let mut received = b"-ERR warm\r\n".to_vec();
        drain_replies(&mut received, &mut result, Phase::WarmUp, 0);
        assert_eq!(result.errors, 1);
        assert_eq!(result.first_error.as_deref(), Some("-ERR warm"));
        let mut received = b"+OK\r\n+O".to_vec();
        drain_replies(&mut received, &mut result, Phase::Window, 0);
        assert_eq!(received, b"+O", "a partial reply stays at the front");
        assert_eq!(result.acked, 2);
    }

    /// The planted threshold above the value size creates no extent: the
    /// counter stays, the leg is vacuous.
    #[test]
    fn a_threshold_above_the_value_size_reads_vacuous() {
        assert!(matches!(verdict_of(&leg(100, 0, Some(7), Some(7)), None), LegVerdict::Vacuous(_)));
    }

    /// One reply that is not `+OK` is red before any count is read.
    #[test]
    fn one_error_reply_is_red() {
        let mut received = b"+OK\r\n-ERR nope\r\n+OK\r\n".to_vec();
        let mut result = ConnResult::new();
        drain_replies(&mut received, &mut result, Phase::Window, 0);
        assert_eq!((result.acked, result.errors), (2, 1));
        assert_eq!(result.first_error.as_deref(), Some("-ERR nope"));
        assert!(matches!(
            verdict_of(
                &leg(result.acked, result.errors, Some(0), Some(2)),
                result.first_error.as_deref()
            ),
            LegVerdict::Red(_)
        ));
    }

    /// The spread verdict reads the two legs' means against each other.
    #[test]
    fn a_point_over_the_spread_budget_is_evidence_pending() {
        let mut fast = leg(1000, 0, Some(0), Some(1000));
        let mut slow = leg(900, 0, Some(0), Some(900));
        slow.name = "b";
        fast.latency_us.record(1000);
        slow.latency_us.record(1000);
        let point = Point { connections: 4, legs: vec![fast, slow] };
        assert_eq!(outcome_of(std::slice::from_ref(&point)), Outcome::EvidencePending);
        let mut even = leg(1000, 0, Some(0), Some(1000));
        even.name = "b";
        even.latency_us.record(1000);
        let mut a = leg(1000, 0, Some(0), Some(1000));
        a.latency_us.record(1000);
        let point = Point { connections: 4, legs: vec![a, even] };
        assert_eq!(outcome_of(std::slice::from_ref(&point)), Outcome::Measured);
    }

    /// A leg directory's name round-trips through its parser; a name the
    /// instrument did not make is not ours to remove.
    #[test]
    fn a_leg_directory_name_round_trips_and_other_names_are_not_ours() {
        assert_eq!(LegDir::name("a", 17, 3), "a-c17-r3");
        assert_eq!(LegDir::parse_name("a-c17-r3"), Some(("a", 17, 3)));
        assert_eq!(LegDir::parse_name("b-c128-r0"), Some(("b", 128, 0)));
        let others =
            ["blob-set", "a-c1", "c-c1-r1", "a-cx-r1", "a-c1-r+1", "a-c1-r1-extra", "a-c-r1"];
        for other in others {
            assert_eq!(LegDir::parse_name(other), None, "{other}");
        }
    }

    /// The leg directory goes on every exit of the leg — a drop, an error
    /// returned through `?` — and a stale one, which only a killed run
    /// leaves, goes before the first leg while a sibling that is not ours
    /// stays.
    #[test]
    fn a_leg_directory_is_removed_on_every_exit_and_stale_ones_before_the_first_leg() {
        let root = std::env::temp_dir().join(format!("inf-bench-blobset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stale = root.join("a-c1-r3");
        std::fs::create_dir_all(stale.join("shard-0")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        assert_eq!(LegDir::clear_stale(&root).unwrap(), vec!["a-c1-r3".to_string()]);
        assert!(!stale.exists());
        assert!(root.join("notes").is_dir());
        let dir = LegDir::create(&root, "a", 1, 0).unwrap();
        let path = dir.path.clone();
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists());
        let through_an_error = || -> Result<(), String> {
            let _dir = LegDir::create(&root, "b", 1, 0)?;
            Err("the leg failed".into())
        };
        assert!(through_an_error().is_err());
        assert!(!root.join("b-c1-r0").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
