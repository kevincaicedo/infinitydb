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
//!   runs of two legs, `a` and `b`, interleaved (`a b`, then `b a`); both
//!   run the same binary unless `--binary-b` names another. The legs'
//!   means must agree within 5 % on `X` and 10 % on p99, else the point is
//!   `Evidence-pending`: a number outside its own control's spread is
//!   never a number.
//! - **Liveness, with its planted red.** Over a leg, the server's
//!   `tiering_blob_extents_created` must rise by the acknowledged `SET`s,
//!   within the connections (an extent whose reply the deadline cut off),
//!   else the leg is `VACUOUS`; every reply must be `+OK`, else the leg is
//!   red. `--plant blob-threshold-above` creates the namespace with a
//!   threshold above the value size, so no extent is created and the run
//!   must exit `VACUOUS`; `--plant reply-err` feeds the reply path a
//!   fixture stream holding one `-ERR`, and the run must exit red.
//!
//! The report labels its tier: without `--reference-box` every number is
//! dev-tier, disclosed and never published.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::cli::Flags;
use crate::envcheck;
use crate::finehist::FineHistogram;
use crate::gaterun::{ServerGuard, admit_device_root, env_gate, spawn_infinityd};
use crate::resp::{connect, encode_command, reply_len, request};

const FLAGS: (&[&str], &[&str]) = (
    &["allow-dirty", "unsafe-env", "reference-box"],
    &[
        "allow-dirty",
        "unsafe-env",
        "reference-box",
        "infinityd-bin",
        "binary-b",
        "data-root",
        "artifacts-root",
        "cells",
        "pin-start",
        "conns",
        "replicates",
        "warmup-s",
        "duration-s",
        "value-size",
        "blob-threshold",
        "plant",
    ],
);

/// The curve's default connection counts: both sides of a 16-entry window.
const CONNS_DEFAULT: &str = "1,8,16,17,32,64,128";
/// The namespace every leg creates on a fresh data directory.
const NAMESPACE: &[u8] = b"blobset";
/// The spread budget on `X` between the two legs' means, in percent.
const SPREAD_X_PERCENT: f64 = 5.0;
/// The spread budget on p99 between the two legs' means, in percent.
const SPREAD_P99_PERCENT: f64 = 10.0;
/// Server counters read beside the curve; an absent one is reported as
/// absent, never as zero.
const COUNTERS: &[&str] =
    &["extent_parks", "frame_waits_extent", "fsync_latency_p99_us", "tiering_blob_extents_created"];

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
/// extent counter does not confirm is vacuous; the rest is a measurement.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LegVerdict {
    Measured,
    Vacuous(String),
    Red(String),
}

/// One leg's measurement.
struct Leg {
    name: &'static str,
    conns: usize,
    replicate: usize,
    acked: u64,
    errors: u64,
    warmup: Duration,
    window: Duration,
    hist_us: FineHistogram,
    extents_before: Option<u64>,
    extents_after: Option<u64>,
    counters: BTreeMap<&'static str, Option<u64>>,
    verdict: LegVerdict,
}

impl Leg {
    fn throughput(&self) -> f64 {
        self.acked as f64 / self.window.as_secs_f64()
    }
}

/// One connection's share of a leg: its acknowledged `SET`s inside the
/// window, every non-`+OK` reply, and its latencies.
struct ConnResult {
    acked: u64,
    errors: u64,
    first_error: Option<String>,
    hist_us: FineHistogram,
}

/// Classifies one reply of the closed loop: `+OK` is an acknowledgement,
/// anything else an error carried whole. The plant's fixture stream goes
/// through this same function.
fn classify_reply(reply: &[u8]) -> Result<(), String> {
    if reply == b"+OK\r\n" {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(reply).trim_end().to_string())
    }
}

/// Consumes every whole reply at the front of `rx`, classifying each.
/// Returns `(acked, errors, first error)`.
fn drain_replies(rx: &mut Vec<u8>) -> (u64, u64, Option<String>) {
    let (mut acked, mut errors, mut first) = (0u64, 0u64, None);
    while let Some(n) = reply_len(rx) {
        let reply: Vec<u8> = rx.drain(..n).collect();
        match classify_reply(&reply) {
            Ok(()) => acked += 1,
            Err(text) => {
                errors += 1;
                first.get_or_insert(text);
            }
        }
    }
    (acked, errors, first)
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
    classify_reply(&reply).map_err(|e| format!("USE on connection {index}: {e}"))?;
    let value = vec![0xABu8; value_size];
    let mut result =
        ConnResult { acked: 0, errors: 0, first_error: None, hist_us: FineHistogram::new() };
    let mut rx: Vec<u8> = Vec::with_capacity(256);
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
        let reply = loop {
            if let Some(n) = reply_len(&rx) {
                break rx.drain(..n).collect::<Vec<u8>>();
            }
            let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err(format!("connection {index}: closed mid-reply"));
            }
            rx.extend_from_slice(&chunk[..n]);
        };
        let done = Instant::now();
        let outcome = classify_reply(&reply);
        if sent < warmup_end {
            continue;
        }
        match outcome {
            Ok(()) => {
                result.acked += 1;
                let us = u64::try_from(done.duration_since(sent).as_micros()).unwrap_or(u64::MAX);
                result.hist_us.record(us);
            }
            Err(text) => {
                result.errors += 1;
                result.first_error.get_or_insert(text);
            }
        }
    }
    Ok(result)
}

/// Reads one numeric field across the node's cells, summed; `None` when
/// no cell prints it (an absent counter is reported as absent, never 0).
fn scrape_sum(port: u16, cells: u16, field: &str) -> Result<Option<u64>, String> {
    let infos = crate::gaterun::scrape_cells(port, cells)?;
    let values: Vec<u64> =
        infos.iter().filter_map(|i| i.get(field)).filter_map(|v| v.parse::<u64>().ok()).collect();
    Ok(if values.is_empty() { None } else { Some(values.iter().sum()) })
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

/// Boots a fresh node from `bin` on `data_dir` (removed and recreated).
fn boot(
    bin: &str,
    cells: u16,
    data_dir: &Path,
    pin_start: Option<&str>,
) -> Result<ServerGuard, String> {
    let _ = std::fs::remove_dir_all(data_dir);
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let dir = data_dir.display().to_string();
    let mut extra: Vec<&str> = vec!["--data-dir", &dir];
    if let Some(pin) = pin_start {
        extra.push("--pin-start");
        extra.push(pin);
    }
    spawn_infinityd(bin, cells, &extra)
}

struct LegSpec<'a> {
    bin: &'a str,
    cells: u16,
    data_dir: &'a Path,
    pin_start: Option<&'a str>,
    threshold: &'a str,
    value_size: usize,
    warmup: Duration,
    window: Duration,
}

/// Runs one leg: boot, the namespace, the counters before, `conns`
/// closed-loop connections through the warm-up and the window, the
/// counters after, the verdict.
fn run_leg(
    spec: &LegSpec<'_>,
    name: &'static str,
    conns: usize,
    replicate: usize,
) -> Result<Leg, String> {
    let server = boot(spec.bin, spec.cells, spec.data_dir, spec.pin_start)?;
    let port = server.port;
    create_namespace(port, spec.cells, spec.threshold)?;
    let extents_before = scrape_sum(port, spec.cells, "tiering_blob_extents_created")?;
    let start = Instant::now();
    let warmup_end = start + spec.warmup;
    let deadline = warmup_end + spec.window;
    let results: Vec<Result<ConnResult, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..conns)
            .map(|index| {
                scope.spawn(move || run_conn(port, index, spec.value_size, warmup_end, deadline))
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("connection thread")).collect()
    });
    let mut leg = Leg {
        name,
        conns,
        replicate,
        acked: 0,
        errors: 0,
        warmup: spec.warmup,
        window: spec.window,
        hist_us: FineHistogram::new(),
        extents_before,
        extents_after: None,
        counters: BTreeMap::new(),
        verdict: LegVerdict::Measured,
    };
    let mut first_error: Option<String> = None;
    for result in results {
        let result = result?;
        leg.acked += result.acked;
        leg.errors += result.errors;
        leg.hist_us.merge(&result.hist_us);
        if first_error.is_none() {
            first_error = result.first_error;
        }
    }
    leg.extents_after = scrape_sum(port, spec.cells, "tiering_blob_extents_created")?;
    for &field in COUNTERS {
        leg.counters.insert(field, scrape_sum(port, spec.cells, field)?);
    }
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
    let (Some(before), Some(after)) = (leg.extents_before, leg.extents_after) else {
        return LegVerdict::Vacuous("tiering_blob_extents_created is absent from INFO".into());
    };
    let created = after.saturating_sub(before);
    // Every acknowledged SET created an extent; the warm-up's and the
    // deadline's in-flight commands (at most the connections) are the
    // slack between the counter and the window's count.
    if created < leg.acked || created > leg.acked + 2 * leg.conns as u64 + warmup_slack(leg) {
        return LegVerdict::Vacuous(format!(
            "extents created {created} does not confirm {} acknowledged SETs within the \
             connections ({})",
            leg.acked, leg.conns
        ));
    }
    LegVerdict::Measured
}

/// The warm-up's acknowledged `SET`s also created extents: bounded by what
/// the window's rate would have produced over the warm-up, doubled.
fn warmup_slack(leg: &Leg) -> u64 {
    (2.0 * leg.throughput() * leg.warmup.as_secs_f64()) as u64 + 1
}

/// A point of the curve: both legs over every replicate, and the spread
/// verdict between the legs' means.
struct Point {
    conns: usize,
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
        mean("a", &|l| l.hist_us.percentile(99.0) as f64)?,
        mean("b", &|l| l.hist_us.percentile(99.0) as f64)?,
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
    let mut out = format!(
        "# blob-set curve\n\n{header}\noutcome: {outcome:?}\n\n\
         Latencies read to the bucket of a 32-per-octave histogram (about 3 %). `X` is the \
         count of `+OK` replies inside the window over its length. The spread column is the \
         two legs' means against each other; a point over 5 % on `X` or 10 % on p99 is \
         Evidence-pending.\n\n\
         | conns | leg | rep | X (SET/s) | p50 us | p99 us | p99.9 us | acked | extents | \
         counters | verdict |\n|---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for point in points {
        for leg in &point.legs {
            let counters: Vec<String> = leg
                .counters
                .iter()
                .map(|(k, v)| format!("{k}={}", v.map_or("absent".to_string(), |v| v.to_string())))
                .collect();
            let extents = match (leg.extents_before, leg.extents_after) {
                (Some(b), Some(a)) => format!("{}", a.saturating_sub(b)),
                _ => "absent".to_string(),
            };
            out.push_str(&format!(
                "| {} | {} | {} | {:.2} | {} | {} | {} | {} | {} | {} | {:?} |\n",
                leg.conns,
                leg.name,
                leg.replicate,
                leg.throughput(),
                leg.hist_us.percentile(50.0),
                leg.hist_us.percentile(99.0),
                leg.hist_us.percentile(99.9),
                leg.acked,
                extents,
                counters.join(" "),
                leg.verdict
            ));
        }
    }
    out.push_str("\n## points\n\n| conns | X a | X b | spread X % | p99 a | p99 b | spread p99 % | point |\n|---|---|---|---|---|---|---|---|\n");
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
                    point.conns, s.x[0], s.x[1], s.x_percent, s.p99[0], s.p99[1], s.p99_percent
                ));
            }
            None => out.push_str(&format!(
                "| {} | — | — | — | — | — | — | no measured leg |\n",
                point.conns
            )),
        }
    }
    out
}

fn env_header(flags: &Flags, env_ok: bool, fstype: &str, command: &str) -> String {
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
        "date: {stamp} (unix)\ntier: {tier}\nenv-check: {}\ndata-root filesystem: {fstype}\n\
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

/// The `reply-err` plant: the reply path over a fixture stream holding
/// one `-ERR` among `+OK`s. The same classifier the connections use.
fn plant_reply_err() -> Result<(), String> {
    let mut rx = Vec::new();
    for _ in 0..7 {
        rx.extend_from_slice(b"+OK\r\n");
    }
    rx.extend_from_slice(b"-ERR fixture: one refused SET\r\n");
    rx.extend_from_slice(b"+OK\r\n");
    let (acked, errors, first) = drain_replies(&mut rx);
    let leg = Leg {
        name: "a",
        conns: 1,
        replicate: 0,
        acked,
        errors,
        warmup: Duration::ZERO,
        window: Duration::from_secs(1),
        hist_us: FineHistogram::new(),
        extents_before: Some(0),
        extents_after: Some(acked),
        counters: BTreeMap::new(),
        verdict: LegVerdict::Measured,
    };
    match verdict_of(&leg, first.as_deref()) {
        LegVerdict::Red(why) => {
            println!("blob-set: plant reply-err: red as planted ({why})");
            Err("plant reply-err: the run is red".into())
        }
        other @ (LegVerdict::Measured | LegVerdict::Vacuous(_)) => {
            Err(format!("plant reply-err did not go red: {other:?}"))
        }
    }
}

/// `inf-bench blob-set`.
pub fn cmd_blob_set(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, FLAGS.0, FLAGS.1)?;
    let plant = Plant::parse(flags.get("plant"))?;
    if plant == Plant::ReplyErr {
        return plant_reply_err();
    }
    let env_ok = env_gate(&flags)?;
    let reference_box = flags.bool("reference-box");
    let bin = flags.str_or("infinityd-bin", "target/release/infinityd");
    let binary_b = flags.get("binary-b").map(str::to_string).unwrap_or_else(|| bin.clone());
    let data_root = flags.require("data-root")?.to_string();
    let fstype = admit_device_root(&flags, Path::new(&data_root), reference_box)?;
    if crate::gaterun::is_memory_fs(&fstype) {
        return Err(format!(
            "data root {data_root} is {fstype}: a blob SET's extent and its fsync must hit a \
             device; point --data-root at the filesystem under test"
        ));
    }
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/blob-set");
    let cells = flags.u16_or("cells", 4)?;
    let pin_start = flags.get("pin-start").map(str::to_string);
    let conns: Vec<usize> = flags
        .str_or("conns", CONNS_DEFAULT)
        .split(',')
        .map(|s| s.trim().parse::<usize>().map_err(|e| format!("--conns `{s}`: {e}")))
        .collect::<Result<_, _>>()?;
    if conns.contains(&0) {
        return Err("--conns: every count is at least 1".into());
    }
    let replicates = flags.usize_or("replicates", 5)?;
    let warmup = Duration::from_secs(flags.u64_or("warmup-s", 5)?);
    let window = Duration::from_secs(flags.u64_or("duration-s", 30)?);
    if window.is_zero() {
        return Err("--duration-s: the window is at least 1 s".into());
    }
    let value_size = flags.usize_or("value-size", 64 << 10)?;
    let threshold = match plant {
        Plant::BlobThresholdAbove => format!("{}", (value_size * 2).max(8 << 10)),
        Plant::None | Plant::ReplyErr => flags.str_or("blob-threshold", "4kb"),
    };
    let command = format!("inf-bench blob-set {}", args.join(" "));
    let header = env_header(&flags, env_ok, &fstype, &command);
    let specs = [("a", bin.as_str()), ("b", binary_b.as_str())];
    let mut points = Vec::new();
    for &c in &conns {
        let mut legs = Vec::new();
        for replicate in 0..replicates {
            // Interleaved: a b, then b a, so neither leg always warms the other.
            let order: [usize; 2] = if replicate % 2 == 0 { [0, 1] } else { [1, 0] };
            for which in order {
                let (name, leg_bin) = specs[which];
                let data_dir = Path::new(&data_root).join(format!("{name}-c{c}-r{replicate}"));
                let spec = LegSpec {
                    bin: leg_bin,
                    cells,
                    data_dir: &data_dir,
                    pin_start: pin_start.as_deref(),
                    threshold: &threshold,
                    value_size,
                    warmup,
                    window,
                };
                let leg = run_leg(&spec, name, c, replicate)?;
                println!(
                    "blob-set: conns {c} leg {name} rep {replicate}: X {:.2} SET/s, p50 {} us, \
                     p99 {} us, p99.9 {} us, acked {}, {:?}",
                    leg.throughput(),
                    leg.hist_us.percentile(50.0),
                    leg.hist_us.percentile(99.0),
                    leg.hist_us.percentile(99.9),
                    leg.acked,
                    leg.verdict
                );
                let _ = std::fs::remove_dir_all(&data_dir);
                legs.push(leg);
            }
        }
        points.push(Point { conns: c, legs });
    }
    let outcome = outcome_of(&points);
    let report = render_report(&points, &header, outcome);
    let path = write_report(&artifacts_root, &report)?;
    println!("blob-set: report {path}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(acked: u64, errors: u64, before: Option<u64>, after: Option<u64>) -> Leg {
        Leg {
            name: "a",
            conns: 4,
            replicate: 0,
            acked,
            errors,
            warmup: Duration::ZERO,
            window: Duration::from_secs(10),
            hist_us: FineHistogram::new(),
            extents_before: before,
            extents_after: after,
            counters: BTreeMap::new(),
            verdict: LegVerdict::Measured,
        }
    }

    /// The liveness clause: the extent counter confirms the count within
    /// the connections and the warm-up's share; fewer is vacuous.
    #[test]
    fn extents_below_the_acknowledged_count_are_vacuous() {
        assert_eq!(verdict_of(&leg(100, 0, Some(0), Some(100)), None), LegVerdict::Measured);
        assert_eq!(verdict_of(&leg(100, 0, Some(0), Some(104)), None), LegVerdict::Measured);
        assert!(matches!(
            verdict_of(&leg(100, 0, Some(0), Some(99)), None),
            LegVerdict::Vacuous(_)
        ));
        assert!(matches!(verdict_of(&leg(100, 0, None, Some(100)), None), LegVerdict::Vacuous(_)));
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
        let mut rx = b"+OK\r\n-ERR nope\r\n+OK\r\n".to_vec();
        let (acked, errors, first) = drain_replies(&mut rx);
        assert_eq!((acked, errors), (2, 1));
        assert_eq!(first.as_deref(), Some("-ERR nope"));
        assert!(matches!(
            verdict_of(&leg(acked, errors, Some(0), Some(2)), first.as_deref()),
            LegVerdict::Red(_)
        ));
    }

    /// The spread verdict reads the two legs' means against each other.
    #[test]
    fn a_point_over_the_spread_budget_is_evidence_pending() {
        let mut fast = leg(1000, 0, Some(0), Some(1000));
        let mut slow = leg(900, 0, Some(0), Some(900));
        slow.name = "b";
        fast.hist_us.record(1000);
        slow.hist_us.record(1000);
        let point = Point { conns: 4, legs: vec![fast, slow] };
        assert_eq!(outcome_of(std::slice::from_ref(&point)), Outcome::EvidencePending);
        let mut even = leg(1000, 0, Some(0), Some(1000));
        even.name = "b";
        even.hist_us.record(1000);
        let mut a = leg(1000, 0, Some(0), Some(1000));
        a.hist_us.record(1000);
        let point = Point { conns: 4, legs: vec![a, even] };
        assert_eq!(outcome_of(std::slice::from_ref(&point)), Outcome::Measured);
    }
}
