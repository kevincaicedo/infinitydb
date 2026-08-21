//! `inf-bench gate-run m2` (M2-S09 today; S22 adds the durable rows): the
//! durability milestone's gate rows. The S09 row set is the memory-namespace
//! zero-cost A/B — an M1-baseline `infinityd` build vs the M2 build, driven
//! by this (single) load generator over the M0/M1 memory-only gate mixes
//! with interleaved replicates — plus the report-enforced
//! `log_records_appended == 0` tripwire on every memory-only row.
//!
//! Tier honesty (L10): identical to the m0/m1 flows — dev runs report
//! measured values with non-binding verdicts on reference-box gates; the
//! `log_records_appended` tripwire is box-independent and always binds.
//! Resolution honesty: p99.9 comes from `LogHistogram` (32 linear
//! sub-buckets per octave ⇒ ~3% quantization); a 0.0% delta means "same
//! bucket", and any non-zero delta is at least one bucket (~3%) — disclosed
//! as a note in every report this command writes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, max_field, median,
    rss_bytes_of, scrape_cells, spawn_infinityd, sum_field,
};
use crate::load::{LoadSpec, render, run as run_load};
use crate::resp::{connect, request};

/// Measurement keys for one A/B row (`Measurements` keys are `'static`).
struct RowKeys {
    ops_delta: &'static str,
    p999_delta: &'static str,
}

/// Relative spread of a replicate set: (max − min) / median, in percent.
pub(crate) fn rel_spread_pct(values: &mut [f64]) -> f64 {
    let med = median(values);
    let (min, max) = (values[0], values[values.len() - 1]); // median() sorted them
    if med == 0.0 { 0.0 } else { (max - min) / med * 100.0 }
}

/// Signed delta (b − a) / a in percent.
pub(crate) fn delta_pct(a: f64, b: f64) -> f64 {
    if a == 0.0 { 0.0 } else { (b - a) / a * 100.0 }
}

/// One crossover A/B row — the ADR-0064 D1 instrument fix, ported from
/// `m4rows::degenerate_row` (readiness F5: the fix landed on the M4 rows
/// only, and any m2 tail verdict cited from the pre-fix shape carries the
/// bias below).
///
/// The pre-fix shape spawned both servers **once** and then alternated only
/// the *load order* across replicates. That cancels drift and thermal bias,
/// but not slot bias: the first-spawned slot's unpipelined p99.9 reads one
/// LogHistogram bucket high with *identical* binaries (the week-4 A/A
/// control, `.artifacts/m4/s03/week-4-risk-gate/verdict.md`) because the
/// bias follows spawn order, port draw and process lifetime — and with
/// fixed slots that bias landed entirely on one build, every replicate.
///
/// Fix, identical to M4's: servers respawn fresh **per replicate** and the
/// binary↔slot assignment alternates; legs run in spawn order, so the slot
/// and load-order nuisances move together and alternate sign against the
/// binary — both cancel in the leg medians over an even replicate count.
/// Both legs stay equally fresh within a replicate.
///
/// With no baseline binary the M2 leg still runs (the counter tripwire
/// needs it) and the delta gates stay PENDING.
#[allow(clippy::too_many_arguments)] // orchestration row: linear, not branchy
fn ab_row(
    m: &mut Measurements,
    name: &str,
    keys: &RowKeys,
    replicates: usize,
    infinityd_bin: &str,
    baseline_bin: Option<&str>,
    cells: u16,
    server_extra: &[&str],
    spec_for: impl Fn(u16) -> LoadSpec,
) -> Result<(), String> {
    println!("\n== row: {name} (crossover A/B × {replicates}) ==");
    let mut base_ops: Vec<f64> = Vec::new();
    let mut base_p999: Vec<f64> = Vec::new();
    let mut m2_ops: Vec<f64> = Vec::new();
    let mut m2_p999: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        // Slot order this replicate: (binary, is_m2) in spawn order.
        let order: Vec<(&str, bool)> = match baseline_bin {
            Some(base_bin) if rep.is_multiple_of(2) => {
                vec![(infinityd_bin, true), (base_bin, false)]
            }
            Some(base_bin) => vec![(base_bin, false), (infinityd_bin, true)],
            None => vec![(infinityd_bin, true)],
        };
        // Spawn every slot before any leg runs: the pre-fix shape kept both
        // servers resident on the same cpu set while one served — the
        // crossover changes assignment, never the concurrency shape.
        let servers = order
            .iter()
            .map(|(bin, _)| spawn_infinityd(bin, cells, server_extra))
            .collect::<Result<Vec<_>, String>>()?;
        for ((_, is_m2), server) in order.iter().zip(&servers) {
            let report = run_load(&spec_for(server.port))?;
            let label = if *is_m2 { "m2" } else { "m1-baseline" };
            println!(
                "  rep {rep} {label}: {:.0} ops/s, p999 {} µs",
                report.ops_per_sec, report.p999_us
            );
            m.raw_section(&format!("{name} {label} rep {rep}"), &render(&report));
            let (ops, p999) =
                if *is_m2 { (&mut m2_ops, &mut m2_p999) } else { (&mut base_ops, &mut base_p999) };
            ops.push(report.ops_per_sec);
            p999.push(report.p999_us as f64);
            // Audit the M2 leg before its server drops: with per-replicate
            // respawn there is no long-lived server left to scrape after
            // the row, so each server lifetime owns its own zero (same
            // move as `m4rows::degenerate_row`).
            if *is_m2 {
                assert_zero_log_records(m, server.port, cells, name)?;
            }
        }
    }
    if baseline_bin.is_some() {
        let (a_ops, b_ops) = (median(&mut base_ops), median(&mut m2_ops));
        let (a_p999, b_p999) = (median(&mut base_p999), median(&mut m2_p999));
        // The gate is a REGRESSION bound (the plan's "pays zero"): a faster
        // M2 build clamps to 0 and the signed delta is disclosed below —
        // improvements are findings to explain, never gate failures.
        let ops_signed = delta_pct(a_ops, b_ops); // negative = m2 slower
        let p999_signed = delta_pct(a_p999, b_p999); // positive = m2 worse tail
        m.set(keys.ops_delta, (-ops_signed).max(0.0));
        m.set(keys.p999_delta, p999_signed.max(0.0));
        m.note(format!(
            "{name}: m1 {a_ops:.0} ops/s (spread {:.2}%) vs m2 {b_ops:.0} ops/s (spread {:.2}%) \
             — signed ops delta {ops_signed:+.2}% · p999 {a_p999:.0} → {b_p999:.0} µs \
             ({p999_signed:+.2}%)",
            rel_spread_pct(&mut base_ops),
            rel_spread_pct(&mut m2_ops),
        ));
    }
    Ok(())
}

/// The report-enforced M2-S09 tripwire: a memory-only row must not append a
/// single log record. Scraped from `INFO` `log_records_appended` (cumulative
/// per cell) after the row's replicates; any non-zero count aborts the run —
/// same teeth as the m1 fan-out receiver assert.
fn assert_zero_log_records(
    m: &mut Measurements,
    port: u16,
    cells: u16,
    row: &str,
) -> Result<(), String> {
    let appended = sum_field(&scrape_cells(port, cells)?, "log_records_appended");
    m.set("tripwire:mem_only_log_records_appended", appended as f64);
    if appended != 0 {
        return Err(format!(
            "memory-only row `{row}` appended {appended} log records — the M2-S09 zero-cost \
             contract is broken (memory namespaces must never touch inf-log)"
        ));
    }
    Ok(())
}

/// One M2-S12 pressure leg: a fresh durable node (`--data-dir`, 32 MiB
/// segments), a `press` everysec namespace, deterministic fill, then the
/// saturating 1:1 mix — with VmRSS sampled at 100 ms throughout the mix.
/// `interval_bytes = 0` is the no-checkpoint control; the pressure leg
/// cycles checkpoints continuously via the bytes trigger.
/// Polls `INFO persistence` (loading-allowed) until the `-LOADING`
/// window closes — fresh durable nodes recover instantly but the DDL
/// below is gated until every cell reports ready (M2-S15).
fn wait_loaded(port: u16, cells: u16) -> Result<(), String> {
    // 30 s ceiling: a fresh node recovers instantly, but boot-time
    // dir-fsyncs stall for seconds when a prior leg left the device with
    // a dirty-writeback backlog (ext4 journal entanglement — observed on
    // the DRAM-less campaign NVMe at S22; -LOADING service during the
    // wait is correct behavior, not a failure).
    for _ in 0..3000 {
        if let Ok(infos) = scrape_cells(port, cells)
            && sum_field(&infos, "loading") == 0
        {
            return Ok(());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("node stayed in -LOADING for 30s".into())
}

/// Spawn a durable node and wait out its `-LOADING` window, retrying once
/// on a wedge. The S22 campaign observed an intermittent (~1-in-10-spawns
/// under prior-leg writeback) cell-boot wedge: one cell never publishes
/// ready on the RecoveryBoard (captured stderr shows N-1/N cells ready
/// forever on an empty data dir) — root-caused and fixed at M2.5-S01
/// (boot-metadata fsyncs off the ready path, driver barriers in the
/// commit ledger). The retry mechanism stays as a **tripwire that must
/// read zero** (`tripwire:spawn_retries`); a second wedge fails the run.
fn spawn_durable_loaded(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    extra: &[&str],
    label: &str,
) -> Result<ServerGuard, String> {
    for attempt in 0..2 {
        let server = spawn_infinityd(infinityd, cells, extra)?;
        match wait_loaded(server.port, cells) {
            Ok(()) => {
                m.set("tripwire:spawn_retries", f64::from(attempt));
                return Ok(server);
            }
            Err(e) if attempt == 0 => {
                m.note(format!(
                    "SPAWN RETRY ({label}): {e} — post-S01 this tripwire must read zero; \
                     a firing retry is a regression finding, not a disclosure (server \
                     stderr in the capture dir when INF_GATERUN_STDERR_DIR is set)"
                ));
            }
            Err(e) => return Err(format!("{label}: second spawn also wedged: {e}")),
        }
    }
    unreachable!("loop returns or errors")
}

struct PressureLeg {
    ops_per_sec: f64,
    p999_us: u64,
    rss_peak: u64,
    ckpts_completed: u64,
    manifests_published: u64,
    segments_truncated: u64,
    ckpt_buffer_peak: u64,
    /// §8.2: every durable row carries its fsync histogram (max across
    /// cells — the honest storage-bound tail).
    fsync_p50_us: u64,
    fsync_p99_us: u64,
    fsync_p999_us: u64,
}

const PRESSURE_KEYS: u64 = 200_000;
const PRESSURE_VALUE: usize = 512;
const PRESSURE_SEGMENT_BYTES: u64 = 32 << 20;
const PRESSURE_INTERVAL_BYTES: u64 = 32 << 20;

/// Owns a row's data directory and removes it on drop — success, error,
/// and panic paths all clean up. (M3-session tooling finding: every `?`
/// between create and the tail cleanup leaked the dir; two interrupted
/// everysec rows leaked ~10 GB each and took the box's tmpfs down.)
/// Declared before the row's `ServerGuard`, so the server dies first.
struct DataDirGuard(std::path::PathBuf);

impl DataDirGuard {
    fn create(dir: std::path::PathBuf) -> Result<DataDirGuard, String> {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(DataDirGuard(dir))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Sweeps row data dirs left by a killed prior run (SIGKILL beats any
/// Drop guard): a dir named `inf-m2-<tag>-<pid>[-label]` is stale when
/// its embedded pid is no longer alive. Best-effort; sweeps are printed
/// so an operator sees what a crashed campaign left behind.
fn sweep_stale_row_dirs(data_root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(data_root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("inf-m2-") else { continue };
        let Some((_tag, tail)) = rest.split_once('-') else { continue };
        let pid = tail.split('-').next().unwrap_or("");
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // `/proc/<pid>` absent ⇒ the owning run is gone (non-Linux has no
        // /proc, so everything stale-named sweeps — dev-tier platforms).
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            println!("gate-run m2: swept stale row data dir {}", entry.path().display());
        }
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // orchestration script
fn pressure_leg(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    server_extra: &[&str],
    duration: u64,
    interval_bytes: u64,
    data_root: &std::path::Path,
    label: &str,
) -> Result<PressureLeg, String> {
    let guard = DataDirGuard::create(
        data_root.join(format!("inf-m2-press-{}-{label}", std::process::id())),
    )?;
    let dir_s = guard.path().to_string_lossy().into_owned();
    let seg_s = PRESSURE_SEGMENT_BYTES.to_string();
    let int_s = interval_bytes.to_string();
    let mut extra: Vec<&str> = server_extra.to_vec();
    extra.extend_from_slice(&[
        "--data-dir",
        &dir_s,
        "--segment-bytes",
        &seg_s,
        "--ckpt-interval-bytes",
        &int_s,
    ]);
    let server = spawn_durable_loaded(m, infinityd, cells, &extra, "pressure leg")?;

    let mut control = connect("127.0.0.1", server.port)?;
    let reply = request(
        &mut control,
        &[b"INF.NS", b"CREATE", b"press", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    if reply.starts_with(b"-") {
        return Err(format!("CREATE press failed: {}", String::from_utf8_lossy(&reply)));
    }
    let use_press: Vec<Vec<Vec<u8>>> =
        vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"press".to_vec()]];

    // Deterministic fill, then the measured mix.
    run_load(&LoadSpec {
        port: server.port,
        conns: 32,
        fill: Some(PRESSURE_KEYS),
        keys: PRESSURE_KEYS,
        key_prefix: "p:".into(),
        value_size: PRESSURE_VALUE,
        setup: use_press.clone(),
        ..Default::default()
    })?;

    // VmRSS + live ckpt-buffer sampler (100 ms) across the whole mix.
    let stop = AtomicBool::new(false);
    let rss_peak = AtomicU64::new(0);
    let buf_peak = AtomicU64::new(0);
    let pid = server.pid();
    let port = server.port;
    let (report, ()) = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                rss_peak.fetch_max(rss_bytes_of(pid), Ordering::Relaxed);
                // One INFO per ~300 ms: the live ckpt_buffer_bytes gauge
                // (the L5 attribution observable) at whichever cell answers.
                for _ in 0..3 {
                    #[allow(clippy::disallowed_methods)] // bench sampler thread, not cell code
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    rss_peak.fetch_max(rss_bytes_of(pid), Ordering::Relaxed);
                }
                if let Ok(infos) = scrape_cells(port, 1) {
                    buf_peak.fetch_max(sum_field(&infos, "ckpt_buffer_bytes"), Ordering::Relaxed);
                }
            }
        });
        let report = run_load(&LoadSpec {
            port: server.port,
            conns: 64,
            pipeline: 16,
            duration: Duration::from_secs(duration),
            set_weight: 1,
            get_weight: 1,
            keys: PRESSURE_KEYS,
            key_prefix: "p:".into(),
            value_size: PRESSURE_VALUE,
            setup: use_press,
            ..Default::default()
        });
        stop.store(true, Ordering::Relaxed);
        sampler.join().expect("rss sampler");
        (report, ())
    });
    let report = report?;
    m.raw_section(&format!("ckpt-pressure {label}"), &render(&report));

    let infos = scrape_cells(server.port, cells)?;
    let leg = PressureLeg {
        ops_per_sec: report.ops_per_sec,
        p999_us: report.p999_us,
        rss_peak: rss_peak.load(Ordering::Relaxed),
        ckpts_completed: sum_field(&infos, "ckpts_completed"),
        manifests_published: sum_field(&infos, "manifests_published"),
        segments_truncated: sum_field(&infos, "segments_truncated"),
        ckpt_buffer_peak: buf_peak.load(Ordering::Relaxed),
        fsync_p50_us: max_field(&infos, "fsync_latency_p50_us"),
        fsync_p99_us: max_field(&infos, "fsync_latency_p99_us"),
        fsync_p999_us: max_field(&infos, "fsync_latency_p999_us"),
    };
    drop(server);
    drop(guard);
    Ok(leg)
}

/// The M2-S21 `always` grouped-write row: saturating pipelined SET-only
/// writers into an `always` namespace — supplies
/// `loadgen:always_grouped_wps` (the §6 300k gate's source; binding on
/// the reference box) and the **grouping-ratio tripwire**
/// `tripwire:acks_per_fsync` (report-enforced: a ratio near 1 is a
/// broken group commit — Vortex's batch=1.0 disease). Every durable
/// number ships with its fsync histogram (§8.2): the p50/p99/p999 fields
/// ride the same scrape. `canary` inverts the shape (1 conn, pipeline 1
/// — grouping degenerates by construction) to prove the wire fires.
#[allow(clippy::too_many_arguments)] // orchestration script
fn always_row(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    server_extra: &[&str],
    duration: u64,
    data_root: &std::path::Path,
    ratio_floor: f64,
    canary: bool,
) -> Result<(), String> {
    let label = if canary { "grouping-canary" } else { "always-grouped" };
    println!("\n== row: always grouped writes (S21{}) ==", if canary { ", canary" } else { "" });
    let guard =
        DataDirGuard::create(data_root.join(format!("inf-m2-alw-{}-{label}", std::process::id())))?;
    let dir_s = guard.path().to_string_lossy().into_owned();
    let seg_s = PRESSURE_SEGMENT_BYTES.to_string();
    let mut extra: Vec<&str> = server_extra.to_vec();
    extra.extend_from_slice(&[
        "--data-dir",
        &dir_s,
        "--segment-bytes",
        &seg_s,
        "--ckpt-interval-bytes",
        "0",
    ]);
    let server = spawn_durable_loaded(m, infinityd, cells, &extra, "always row")?;

    let mut control = connect("127.0.0.1", server.port)?;
    let reply = request(
        &mut control,
        &[b"INF.NS", b"CREATE", b"alw", b"MODE", b"durable", b"FSYNC", b"always"],
    )?;
    if reply.starts_with(b"-") {
        return Err(format!("CREATE alw failed: {}", String::from_utf8_lossy(&reply)));
    }
    let use_alw: Vec<Vec<Vec<u8>>> =
        vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"alw".to_vec()]];

    let (conns, pipeline) = if canary { (1, 1) } else { (64, 16) };
    let before = scrape_cells(server.port, cells)?;
    let (frames_0, iters_0) =
        (sum_field(&before, "log_frames_queued"), sum_field(&before, "raw_iterations"));
    let report = run_load(&LoadSpec {
        port: server.port,
        conns,
        pipeline,
        duration: Duration::from_secs(duration),
        set_weight: 1,
        get_weight: 0,
        keys: 100_000,
        key_prefix: "a:".into(),
        value_size: 64,
        setup: use_alw,
        ..Default::default()
    })?;
    m.raw_section(&format!("always grouped writes ({label})"), &render(&report));

    let infos = scrape_cells(server.port, cells)?;
    // L3 tripwire: at most one log writev per loop iteration, measured as
    // counter deltas around the saturated durable row (frames/iterations
    // ≤ 1; iterations that log nothing keep the ratio below 1).
    let frames = sum_field(&infos, "log_frames_queued").saturating_sub(frames_0);
    let iters = sum_field(&infos, "raw_iterations").saturating_sub(iters_0).max(1);
    let writes_per_iter = frames as f64 / iters as f64;
    let acks = sum_field(&infos, "acks_gated");
    let fsyncs = sum_field(&infos, "fsyncs_completed").max(1);
    let ratio = acks as f64 / fsyncs as f64;
    let p50 = max_field(&infos, "fsync_latency_p50_us");
    let p99 = max_field(&infos, "fsync_latency_p99_us");
    let p999 = max_field(&infos, "fsync_latency_p999_us");
    // M2.5-S07 group formation: records covered per durability fsync vs
    // the writes the workload keeps in flight per cell (conns × pipeline
    // spread across cells by REUSEPORT — an average, disclosed as such).
    let group_p50 = max_field(&infos, "fsync_group_p50");
    let group_p99 = max_field(&infos, "fsync_group_p99");
    let available_per_cell = conns as f64 * pipeline as f64 / f64::from(cells);
    let formation = group_p50 as f64 / available_per_cell;
    drop(server);
    drop(guard);

    if canary {
        // The deliberate regression: grouping degenerated by construction;
        // the tripwire MUST fire (its floor check must fail) — proving the
        // wire has teeth before it guards the gate. Nothing is recorded:
        // canary numbers are not evidence.
        return if ratio < ratio_floor {
            println!(
                "grouping canary: ratio {ratio:.2} < floor {ratio_floor} — the tripwire fired \
                 as it must"
            );
            Ok(())
        } else {
            Err(format!(
                "grouping canary FAILED to trip: ratio {ratio:.2} >= floor {ratio_floor} — the \
                 tripwire has no teeth"
            ))
        };
    }

    m.set("loadgen:always_grouped_wps", report.ops_per_sec);
    m.set("tripwire:acks_per_fsync", ratio);
    m.set("tripwire:log_writes_per_iter", writes_per_iter);
    // §8.2: durable numbers carry their fsync histogram.
    m.set("info:fsync_latency_p50_us", p50 as f64);
    m.set("info:fsync_latency_p99_us", p99 as f64);
    m.set("info:fsync_latency_p999_us", p999 as f64);
    m.set("info:fsync_group_p50", group_p50 as f64);
    m.set("info:fsync_group_p99", group_p99 as f64);
    m.set("tripwire:group_formation_x", formation);
    m.note(format!(
        "always row: {acks} gated acks / {fsyncs} fsyncs = ratio {ratio:.1}; fsync latency \
         p50/p99/p999 = {p50}/{p99}/{p999} us (HDR ~3% quantization); log_writes_per_iter \
         {writes_per_iter:.3} ({frames} frames / {iters} iterations); group formation \
         p50/p99 = {group_p50}/{group_p99} records vs ~{available_per_cell:.0} available \
         in-flight writes/cell = {formation:.2}x (M2.5-S07 gate: >= 0.8x)"
    ));
    if ratio < ratio_floor {
        return Err(format!(
            "grouping-ratio tripwire: acks/fsync = {ratio:.2} < floor {ratio_floor} — group \
             commit is broken (the batch=1.0 disease; §16)"
        ));
    }
    Ok(())
}

/// The §6 `everysec` penalty row (S22): one durable node carrying a
/// `memory`-mode named namespace and an `everysec` durable namespace —
/// the identical 1:1 mix runs against each in interleaved ABBA order,
/// and the penalty is the throughput cost of the durable path. Both
/// namespaces are *named* (both ride the pump — ADR-0015), so the row
/// isolates the durability cost, not the named-ns dispatch cost.
/// §8.2: the everysec legs attach their fsync histogram.
fn everysec_row(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    server_extra: &[&str],
    duration: u64,
    replicates: usize,
    data_root: &std::path::Path,
) -> Result<(), String> {
    println!("\n== row: everysec penalty vs memory-mode ns (interleaved ABBA × {replicates}) ==");
    let guard =
        DataDirGuard::create(data_root.join(format!("inf-m2-esec-{}", std::process::id())))?;
    let dir_s = guard.path().to_string_lossy().into_owned();
    let seg_s = PRESSURE_SEGMENT_BYTES.to_string();
    let mut extra: Vec<&str> = server_extra.to_vec();
    extra.extend_from_slice(&[
        "--data-dir",
        &dir_s,
        "--segment-bytes",
        &seg_s,
        "--ckpt-interval-bytes",
        "0",
    ]);
    let server = spawn_durable_loaded(m, infinityd, cells, &extra, "everysec row")?;

    let mut control = connect("127.0.0.1", server.port)?;
    for create in [
        &[b"INF.NS" as &[u8], b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"][..],
        &[b"INF.NS" as &[u8], b"CREATE", b"memns", b"MODE", b"memory"][..],
    ] {
        let reply = request(&mut control, create)?;
        if reply.starts_with(b"-") {
            return Err(format!("everysec row DDL failed: {}", String::from_utf8_lossy(&reply)));
        }
    }
    let use_ns = |ns: &[u8]| -> Vec<Vec<Vec<u8>>> {
        vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), ns.to_vec()]]
    };

    // Deterministic fill of both keyspaces (GET hits identical per leg).
    for ns in [&b"esec"[..], &b"memns"[..]] {
        run_load(&LoadSpec {
            port: server.port,
            conns: 32,
            fill: Some(PRESSURE_KEYS),
            keys: PRESSURE_KEYS,
            key_prefix: "e:".into(),
            value_size: PRESSURE_VALUE,
            setup: use_ns(ns),
            ..Default::default()
        })?;
    }

    let spec_for = |ns: &[u8]| LoadSpec {
        port: server.port,
        conns: 64,
        pipeline: 16,
        duration: Duration::from_secs(duration),
        set_weight: 1,
        get_weight: 1,
        keys: PRESSURE_KEYS,
        key_prefix: "e:".into(),
        value_size: PRESSURE_VALUE,
        setup: use_ns(ns),
        ..Default::default()
    };
    let mut mem_ops: Vec<f64> = Vec::new();
    let mut mem_p999: Vec<f64> = Vec::new();
    let mut esec_ops: Vec<f64> = Vec::new();
    let mut esec_p999: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        let mem_first = rep % 2 == 0;
        for leg in 0..2 {
            if (leg == 0) == mem_first {
                let report = run_load(&spec_for(b"memns"))?;
                println!(
                    "  rep {rep} memory-ns: {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("everysec row memory-ns rep {rep}"), &render(&report));
                mem_ops.push(report.ops_per_sec);
                mem_p999.push(report.p999_us as f64);
            } else {
                let report = run_load(&spec_for(b"esec"))?;
                println!(
                    "  rep {rep} everysec:  {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("everysec row everysec rep {rep}"), &render(&report));
                esec_ops.push(report.ops_per_sec);
                esec_p999.push(report.p999_us as f64);
            }
        }
    }
    let infos = scrape_cells(server.port, cells)?;
    let p50 = max_field(&infos, "fsync_latency_p50_us");
    let p99 = max_field(&infos, "fsync_latency_p99_us");
    let p999f = max_field(&infos, "fsync_latency_p999_us");
    drop(server);
    drop(guard);

    let (mem, esec) = (median(&mut mem_ops), median(&mut esec_ops));
    let (mem_tail, esec_tail) = (median(&mut mem_p999), median(&mut esec_p999));
    let signed = if mem == 0.0 { 0.0 } else { (mem - esec) / mem * 100.0 };
    m.set("loadgen:everysec_penalty_pct", signed.max(0.0));
    // §8.2: durable numbers carry their fsync histogram.
    m.set("info:everysec_fsync_latency_p50_us", p50 as f64);
    m.set("info:everysec_fsync_latency_p99_us", p99 as f64);
    m.set("info:everysec_fsync_latency_p999_us", p999f as f64);
    m.note(format!(
        "everysec row: memory-ns {mem:.0} ops/s (spread {:.2}%) vs everysec {esec:.0} ops/s \
         (spread {:.2}%) — signed penalty {signed:+.2}%; p999 {mem_tail:.0} → {esec_tail:.0} µs \
         (§18 flat-tails supporting); fsync latency p50/p99/p999 = {p50}/{p99}/{p999f} us; both \
         namespaces named (both ride the pump — the row isolates durability cost)",
        rel_spread_pct(&mut mem_ops),
        rel_spread_pct(&mut esec_ops),
    ));
    Ok(())
}

/// The §6 attribution tripwire on a durable node (S22): after a
/// deterministic durable fill, `sum(domains)` — records, index, wheel,
/// eviction, wire buffers, conn state, pub/sub, log staging, checkpoint
/// buffer — must sit within 10% of VmRSS (L5: no unattributed memory).
fn attribution_row(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    server_extra: &[&str],
    keys: u64,
    data_root: &std::path::Path,
) -> Result<(), String> {
    println!("\n== row: memory attribution on a durable node ({keys} keys × 512 B) ==");
    let guard =
        DataDirGuard::create(data_root.join(format!("inf-m2-attr-{}", std::process::id())))?;
    let dir_s = guard.path().to_string_lossy().into_owned();
    let mut extra: Vec<&str> = server_extra.to_vec();
    extra.extend_from_slice(&["--data-dir", &dir_s, "--ckpt-interval-bytes", "0"]);
    let server = spawn_durable_loaded(m, infinityd, cells, &extra, "attribution row")?;

    let mut control = connect("127.0.0.1", server.port)?;
    let reply = request(
        &mut control,
        &[b"INF.NS", b"CREATE", b"attr", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    if reply.starts_with(b"-") {
        return Err(format!("CREATE attr failed: {}", String::from_utf8_lossy(&reply)));
    }
    run_load(&LoadSpec {
        port: server.port,
        conns: 32,
        fill: Some(keys),
        keys,
        key_prefix: "t:".into(),
        value_size: PRESSURE_VALUE,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"attr".to_vec()]],
        ..Default::default()
    })?;
    let infos = scrape_cells(server.port, cells)?;
    let domains = sum_field(&infos, "records_resident_bytes")
        + sum_field(&infos, "index_bytes")
        + sum_field(&infos, "wheel_bytes")
        + sum_field(&infos, "evict_bytes")
        + sum_field(&infos, "doc_resident_bytes")
        + sum_field(&infos, "doc_scratch_bytes")
        + sum_field(&infos, "doc_path_cache_bytes")
        + sum_field(&infos, "wire_buffers_bytes")
        + sum_field(&infos, "conn_state_bytes")
        + sum_field(&infos, "pubsub_state_bytes")
        + sum_field(&infos, "log_staging_bytes")
        + sum_field(&infos, "ckpt_buffer_bytes");
    let doc_domains = sum_field(&infos, "doc_resident_bytes")
        + sum_field(&infos, "doc_scratch_bytes")
        + sum_field(&infos, "doc_path_cache_bytes");
    let rss = server.rss_bytes();
    drop(server);
    drop(guard);
    let divergence = if rss == 0 {
        return Err("attribution row: VmRSS read failed".into());
    } else {
        ((rss as f64 - domains as f64) / rss as f64 * 100.0).abs()
    };
    m.set("attribution_divergence_pct", divergence);
    m.note(format!(
        "attribution (durable fill leg, log domains included): sum(domains) {domains} B \
         (document {doc_domains} B) vs VmRSS {rss} B — {divergence:.1}% divergence"
    ));
    Ok(())
}

/// The M2-S12 rows: continuous checkpoints under a saturating durable
/// write mix vs the identical no-checkpoint control. Gate rows supplied:
/// `ckpt_under_load_p999` (absolute, the anti-BGREWRITEAOF bar) and
/// `ckpt_rss_overhead` (anti-2×: peak-RSS delta vs the control).
fn pressure_rows(
    m: &mut Measurements,
    infinityd: &str,
    cells: u16,
    server_extra: &[&str],
    duration: u64,
    replicates: usize,
    data_root: &std::path::Path,
) -> Result<(), String> {
    println!("\n== row: checkpoint under full durable load (S12) × {replicates} ==");
    m.note(format!(
        "S12 pressure data root: {} (default is the system temp dir — often tmpfs; point \
         --pressure-data-root at a real filesystem for device-exercising rows)",
        data_root.display()
    ));
    let mut base_p999 = Vec::new();
    let mut base_rss = Vec::new();
    let mut press_p999 = Vec::new();
    let mut press_rss = Vec::new();
    let mut cycles = 0u64;
    let mut manifests = 0u64;
    let mut truncated = 0u64;
    let mut buf_peak = 0u64;
    let mut fsync_p50 = 0u64;
    let mut fsync_p99 = 0u64;
    let mut fsync_p999 = 0u64;
    for rep in 0..replicates {
        // Alternate leg order (the ABBA discipline against drift).
        for leg in 0..2 {
            if (leg == 0) == (rep % 2 == 0) {
                let base = pressure_leg(
                    m,
                    infinityd,
                    cells,
                    server_extra,
                    duration,
                    0,
                    data_root,
                    &format!("baseline rep {rep}"),
                )?;
                println!(
                    "  rep {rep} baseline: {:.0} ops/s, p999 {} µs, rss {} MiB",
                    base.ops_per_sec,
                    base.p999_us,
                    base.rss_peak >> 20
                );
                base_p999.push(base.p999_us as f64);
                base_rss.push(base.rss_peak as f64);
            } else {
                let press = pressure_leg(
                    m,
                    infinityd,
                    cells,
                    server_extra,
                    duration,
                    PRESSURE_INTERVAL_BYTES,
                    data_root,
                    &format!("pressure rep {rep}"),
                )?;
                println!(
                    "  rep {rep} pressure: {:.0} ops/s, p999 {} µs, rss {} MiB, \
                     {} ckpts / {} manifests / {} segs truncated",
                    press.ops_per_sec,
                    press.p999_us,
                    press.rss_peak >> 20,
                    press.ckpts_completed,
                    press.manifests_published,
                    press.segments_truncated
                );
                if press.ckpts_completed < u64::from(cells) {
                    return Err(format!(
                        "pressure rep {rep}: only {} checkpoints across {cells} cells — the row \
                         did not exercise continuous checkpointing (invalid for the gate)",
                        press.ckpts_completed
                    ));
                }
                cycles += press.ckpts_completed;
                manifests += press.manifests_published;
                truncated += press.segments_truncated;
                buf_peak = buf_peak.max(press.ckpt_buffer_peak);
                fsync_p50 = fsync_p50.max(press.fsync_p50_us);
                fsync_p99 = fsync_p99.max(press.fsync_p99_us);
                fsync_p999 = fsync_p999.max(press.fsync_p999_us);
                press_p999.push(press.p999_us as f64);
                press_rss.push(press.rss_peak as f64);
            }
        }
    }
    let p999 = median(&mut press_p999);
    let rss_delta_mib =
        (median(&mut press_rss) - median(&mut base_rss)).max(0.0) / (1024.0 * 1024.0);
    m.set("loadgen:ckpt_under_load_p999_us", p999);
    m.set("loadgen:ckpt_rss_overhead_mib", rss_delta_mib);
    // §8.2: the pressure rows are durable rows — their fsync histogram
    // rides the report (worst leg across the pressure replicates).
    m.set("info:ckpt_fsync_latency_p50_us", fsync_p50 as f64);
    m.set("info:ckpt_fsync_latency_p99_us", fsync_p99 as f64);
    m.set("info:ckpt_fsync_latency_p999_us", fsync_p999 as f64);
    m.note(format!(
        "S12 pressure fsync latency (worst leg): p50/p99/p999 = \
         {fsync_p50}/{fsync_p99}/{fsync_p999} us (HDR ~3% quantization)"
    ));
    m.note(format!(
        "S12 pressure: durable everysec 1:1 mix, {PRESSURE_KEYS} keys × {PRESSURE_VALUE} B, \
         {} ckpt cycles / {} manifests / {} segments truncated across {replicates} pressure \
         legs; p99.9 {} µs under continuous checkpoints vs {} µs baseline; peak RSS delta \
         {rss_delta_mib:.1} MiB (ckpt buffer gauge peaked at {} KiB — the L5 domain); \
         truncation ran in-row (reclamation live under load)",
        cycles,
        manifests,
        truncated,
        p999,
        median(&mut base_p999),
        buf_peak >> 10,
    ));
    m.note(
        "S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides \
         S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the \
         raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound",
    );
    Ok(())
}

#[allow(clippy::too_many_lines)] // orchestration script: linear rows, not branchy logic
pub fn cmd_gate_run_m2(flags: &Flags) -> Result<(), String> {
    let gates_list = load_gates(flags, "m2")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m2");
    let replicates: usize = flags.usize_or("replicates", 5)?;
    let duration: u64 = flags.u64_or("duration", 10)?;
    let cells: u16 = flags.u16_or("cells", 4)?;
    let infinityd = flags.str_or("infinityd-bin", "target/release/infinityd");
    let baseline_bin = flags.get("baseline-bin").map(str::to_string);
    let reference_box = flags.bool("reference-box");
    // Both legs share one pinned cpu set: they never carry load
    // concurrently, and identical placement is what makes the A/B fair.
    let mut pin_args: Vec<String> = flags
        .get("pin-start")
        .map(|v| vec!["--pin-start".to_string(), v.to_string()])
        .unwrap_or_default();
    // `--sync-pipeline` was retired by ADR-0087 D5: accepted, announced,
    // never forwarded (the server refuses it).
    if flags.get("sync-pipeline").is_some() {
        println!("note: --sync-pipeline is retired (ADR-0087 D5) — no-op; use --frames-in-flight");
    }
    // M4.5-S35 A/B arms (ADR-0087 D5/D8) ride every durable spawn in this
    // campaign and are disclosed in the report notes.
    pin_args.extend(pipeline_args(flags));
    let server_extra: Vec<&str> = pin_args.iter().map(String::as_str).collect();

    let env_ok = env_gate(flags)?;
    // One resolution for every device-exercising row; sweep what a killed
    // prior run left behind before this one writes its own ~13 GB.
    // `--data-root` is the campaign-wide spelling (the m4.5 rows' flag);
    // `--pressure-data-root` is this flow's older name — either resolves
    // every device-exercising row here. The 2026-08-21 S35 arms passed
    // `--data-root` to this flow and silently wrote to the temp dir.
    let data_root = flags
        .get("pressure-data-root")
        .or_else(|| flags.get("data-root"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    // Admission before any row: a binding run or a FUA arm on a memory
    // filesystem refuses here (never a note the reader may miss).
    let root_fstype = crate::gaterun::admit_device_root(flags, &data_root, reference_box)?;
    sweep_stale_row_dirs(&data_root);
    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    m.note(format!("data root: {} ({root_fstype})", data_root.display()));
    if crate::gaterun::is_memory_fs(&root_fstype) {
        m.note(format!(
            "data root is {root_fstype} (memory-backed): the durable rows measure the page \
             cache, not a device — dev-tier smoke only; the everysec row writes ~13 GB with \
             truncation disabled and a 16 GB tmpfs exhausts mid-row (engine fail-stops per \
             §8.4). Pass --data-root on the filesystem under test."
        ));
    }
    if !reference_box {
        m.note("dev-tier run: reference-box gates report measured values, non-binding verdicts");
    }
    m.note(
        "p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): \
         0.0% = same bucket; any non-zero delta spans ≥ 1 bucket",
    );
    m.note(
        "M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — \
         the S09 posture: no durable plane constructed); the zero-record assert on a \
         durable-enabled node is the node_e2e mixed-class test",
    );
    if baseline_bin.is_none() {
        m.note(
            "--baseline-bin not given: zero-cost delta rows report PENDING \
             (build the pre-M2 commit's infinityd and pass its path)",
        );
    }

    let ratio_floor =
        gates_list.iter().find(|g| g.id == "grouping_ratio").map_or(2.0, |g| g.threshold);
    if flags.bool("grouping-canary") {
        // The S21 deliberate-regression canary: run ONLY the degenerate
        // leg and succeed iff the tripwire fired.
        return always_row(
            &mut m,
            &infinityd,
            cells,
            &server_extra,
            duration.min(5),
            &data_root,
            ratio_floor,
            true,
        );
    }

    // M2.5-S07 A/B mode: just the always row (fast ABBA legs).
    if flags.bool("only-always") {
        always_row(
            &mut m,
            &infinityd,
            cells,
            &server_extra,
            duration,
            &data_root,
            ratio_floor,
            false,
        )?;
        return finish_report(
            "m2",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "cells: {cells} · duration: {duration}s · ONLY-ALWAYS (A/B leg; {})",
                pipeline_note(flags)
            ),
        );
    }

    // S09 rows — fresh servers per row (each row owns its keyspace state).
    type SpecFn = fn(u16, u64) -> LoadSpec;
    let rows: [(&str, RowKeys, SpecFn); 3] = [
        (
            "pipelined 1:10 (M0 gate mix)",
            RowKeys {
                ops_delta: "ab:mem_pipelined_ops_delta_pct",
                p999_delta: "ab:mem_pipelined_p999_delta_pct",
            },
            |port, duration| LoadSpec {
                port,
                duration: Duration::from_secs(duration),
                ..Default::default()
            },
        ),
        (
            "unpipelined 512-conn (M0 gate mix)",
            RowKeys {
                ops_delta: "ab:mem_unpipelined_ops_delta_pct",
                p999_delta: "ab:mem_unpipelined_p999_delta_pct",
            },
            |port, duration| LoadSpec {
                port,
                conns: 512,
                pipeline: 1,
                duration: Duration::from_secs(duration.min(5)),
                ..Default::default()
            },
        ),
        (
            "ttl-heavy 1:1 writes (M1 gate mix)",
            RowKeys {
                ops_delta: "ab:mem_ttl_ops_delta_pct",
                p999_delta: "ab:mem_ttl_p999_delta_pct",
            },
            |port, duration| LoadSpec {
                port,
                duration: Duration::from_secs(duration),
                set_weight: 1,
                get_weight: 1,
                ttl_range_ms: Some((100, 5_000)),
                ..Default::default()
            },
        ),
    ];

    if !server_extra.is_empty() {
        m.note(format!("server cells pinned: {} (same cpu set both legs)", pin_args.join(" ")));
    }
    for (name, keys, spec) in &rows {
        // Servers are spawned inside the row now — the crossover owns
        // their lifetimes so the binary↔slot assignment can alternate per
        // replicate (ADR-0064 D1; readiness F5). The zero-log-records
        // tripwire moved in with them, asserted per M2 server lifetime.
        ab_row(
            &mut m,
            name,
            keys,
            replicates,
            &infinityd,
            baseline_bin.as_deref(),
            cells,
            &server_extra,
            |port| spec(port, duration),
        )?;
    }

    // S21: always grouped writes + the grouping-ratio tripwire.
    if flags.bool("skip-pressure") {
        m.note("S21 always row SKIPPED (--skip-pressure): always_grouped_wps stays PENDING");
    } else {
        always_row(
            &mut m,
            &infinityd,
            cells,
            &server_extra,
            duration,
            &data_root,
            ratio_floor,
            false,
        )?;
        // §6 everysec penalty + the durable attribution rows (S22).
        everysec_row(&mut m, &infinityd, cells, &server_extra, duration, replicates, &data_root)?;
        let attr_keys = flags.u64_or("attribution-keys", 2_000_000)?;
        attribution_row(&mut m, &infinityd, cells, &server_extra, attr_keys, &data_root)?;
    }

    // S12: checkpoint under full durable load (anti-BGREWRITEAOF + RSS).
    if flags.bool("skip-pressure") {
        m.note("S12 pressure rows SKIPPED (--skip-pressure): ckpt gates stay PENDING");
    } else {
        let pressure_reps = flags.usize_or("pressure-replicates", 3)?;
        pressure_rows(
            &mut m,
            &infinityd,
            cells,
            &server_extra,
            duration,
            pressure_reps,
            &data_root,
        )?;
    }

    // S22: external campaign artifacts bind their §6 gate rows here — the
    // measured value rides the flag; the artifact path must be quoted via
    // --campaign-note (the report carries provenance, not bare numbers).
    let externals: [(&'static str, &'static str, &'static str); 5] = [
        ("recovery-gbps-per-cell", "external:recovery_gbps_per_cell", "S13 recovery artifact"),
        ("recovery-boot-s", "external:recovery_10gb_boot_s", "S15 cold-boot artifact"),
        ("dst-violations", "external:dst_sweep_violations", "S19 sweep manifest"),
        ("crash-failures", "external:crash_matrix_failures", "S17 matrix run"),
        ("m0m1-pct", "external:m0m1_regression_pct", "M0/M1 regression gate-runs"),
    ];
    let mut any_external = false;
    for (flag, key, what) in externals {
        if let Some(raw) = flags.get(flag) {
            let value: f64 =
                raw.parse().map_err(|_| format!("--{flag}: `{raw}` is not a number"))?;
            m.set(key, value);
            m.note(format!("external gate row `{key}` = {value} supplied from {what}"));
            any_external = true;
        }
    }
    if any_external && flags.get("campaign-note").is_none() {
        return Err("external gate values supplied without --campaign-note: quote the artifact \
             paths so the report carries provenance (L10)"
            .into());
    }
    if let Some(note) = flags.get("campaign-note") {
        m.note(format!("campaign: {note}"));
    }

    // S22 AC: every durable-mode number in the report carries its fsync
    // histogram — the report generator refuses to write one without it.
    let durable_rows: [(&str, &str); 3] = [
        ("loadgen:always_grouped_wps", "info:fsync_latency_p50_us"),
        ("loadgen:everysec_penalty_pct", "info:everysec_fsync_latency_p50_us"),
        ("loadgen:ckpt_under_load_p999_us", "info:ckpt_fsync_latency_p50_us"),
    ];
    for (row, hist) in durable_rows {
        if m.values.contains_key(row) && !m.values.contains_key(hist) {
            return Err(format!(
                "durable row `{row}` has no attached fsync histogram (`{hist}` missing) — \
                 §8.2 forbids durable numbers without their fsync latency distribution"
            ));
        }
    }

    finish_report(
        "m2",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!("cells: {cells} · replicates: {replicates} · duration: {duration}s"),
    )
}

/// The M4.5-S35 server arms (ADR-0087 D5): `--frames-in-flight K`,
/// `--barrier-class flush|fua`, `--log-staging-mib N` from the campaign
/// flags, forwarded verbatim to every durable spawn.
pub(crate) fn pipeline_args(flags: &Flags) -> Vec<String> {
    let mut args = Vec::new();
    for (flag, server_flag) in [
        ("frames-in-flight", "--frames-in-flight"),
        ("barrier-class", "--barrier-class"),
        ("staging-mib", "--log-staging-mib"),
    ] {
        if let Some(value) = flags.get(flag) {
            args.push(server_flag.to_string());
            args.push(value.to_string());
        }
    }
    args
}

/// The same arms for the report notes (defaults spelled out, so a
/// K = 1 / flush row is never mistaken for an unknown configuration).
pub(crate) fn pipeline_note(flags: &Flags) -> String {
    format!(
        "frames-in-flight {} · barrier-class {} · staging-mib {}",
        flags.str_or("frames-in-flight", "1"),
        flags.str_or("barrier-class", "flush"),
        flags.str_or("staging-mib", "4")
    )
}
