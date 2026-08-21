//! `inf-bench gate-run m4.5` — the E4.5 performance-debt rows.
//!
//! Row 1 (M4.5-S29): tiered `FSYNC always` must scale with client
//! concurrency like the non-tiered `always` path does.
//! Row 2 (M4.5-S27, ADR-0083 D7): durable admission under staging
//! pressure paces instead of refusing — back-to-back sustained-write
//! repeats in a deliberately provoked regime (`--log-staging-mib 1`)
//! must show refusals ≤ 0.05 %, no monotonic decay, and a bounded max.
//! Row 3 (M4.5-S35, ADR-0087 D8): the `always` p50 measured against the
//! barrier the client actually waits on (write-through p50 under the
//! FUA class, linked-fdatasync p50 under FLUSH) at the 32-conn closed
//! loop S33's bound-2 arm lost, on N cells and on 1 cell in the same
//! session (S34's F2 ratio AC, owned by S35), plus a 256-conn `max`
//! leg and a pipelined read leg (the ±2 % non-regression row, compared
//! across arm reports). The S34/S35 arms (`--frames-in-flight`,
//! `--barrier-class`, `--staging-mib`) ride every spawn in this flow.
//!
//! The defect this row pins (2026-08-19 finding,
//! `reviews/tiered-always-group-commit-finding-20260819.md`): fabric
//! tiered applies were serialized one-per-fsync-window per origin FIFO
//! (the pump held its queue across the durable-ack wait), so a tiered
//! `always` namespace served a flat ~3.6k ops/s across an 8×
//! concurrency range while the non-tiered arm scaled near-linearly.
//! The row measures both arms on the same node, same binary, same
//! session, at two concurrency points, and gates on the **slope** and
//! the tiered:flat **parity ratio** — never on a single-point peak
//! (a fix that raises the peak without restoring the slope has not
//! fixed the defect).
//!
//! Shape per replicate: fresh server → create one tiered-`always` and
//! one flat-`always` namespace → deterministic fill → four closed-loop
//! 100%-write legs (tiered/flat × low/high conns). Medians across
//! replicates feed the gate keys; per-leg raw rows land in the report.

use std::time::{Duration, Instant};

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, env_gate, finish_report, load_gates, median, scrape_cells, spawn_infinityd,
    sum_field,
};
use crate::load::{LoadSpec, run as run_load};
use crate::resp::{connect, request};

/// Keys per namespace fill (× 1 KiB values ≈ 200 MiB per namespace —
/// enough that the tiered demoter runs against the budget below, small
/// enough that a 3-replicate row stays under ~5 minutes).
const FILL_KEYS: u64 = 200_000;

/// Tiered namespace RAM budget (per cell). With `FILL_KEYS` × 1 KiB
/// across 4 cells (~50 MiB/cell) this sits above the 25% mutable target,
/// so seal/flush/release run during the legs — the row exercises the
/// tiered write path, not a degenerate all-mutable table.
const MEM_BUDGET: &str = "128mb";

/// The two closed-loop concurrency points. The defect signature is the
/// missing slope between them, so both are load-bearing.
const CONNS_LOW: usize = 64;
const CONNS_HIGH: usize = 256;

/// One measured leg's medians-input sample.
struct LegSample {
    ops_per_sec: f64,
    p99_us: f64,
    p999_us: f64,
}

pub fn cmd_gate_run_m45(flags: &Flags) -> Result<(), String> {
    let gates_list = load_gates(flags, "m4.5")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m4.5/s29");
    let replicates: usize = flags.usize_or("replicates", 3)?;
    let duration: u64 = flags.u64_or("duration", 10)?;
    let cells: u16 = flags.u16_or("cells", 4)?;
    let infinityd = flags.str_or("infinityd-bin", "target/release/infinityd");
    let reference_box = flags.bool("reference-box");
    // Server data lives on a real filesystem: an fsync against tmpfs
    // (p50 ~77 µs) measures nothing this row cares about.
    let data_root = flags.str_or("data-root", ".artifacts/m4.5/s29-gate-data");

    // `--only-s27` runs just the S27 backpressure row (the `gate-run m2
    // --only-always` precedent) — the A/B arms don't need the S29 legs.
    // `--only-s29` is the mirror (M4.5-S31: the tier-flush driver-op
    // A/B re-runs the scaling/parity row without the S27 legs).
    let only_s27 = flags.bool("only-s27");
    let only_s29 = flags.bool("only-s29");
    // `--only-s35` (M4.5-S35): the frame-pipeline A/B arms run just
    // their own row (K = 1 / 3 / 4 × barrier class, one report each).
    let only_s35 = flags.bool("only-s35");
    if usize::from(only_s27) + usize::from(only_s29) + usize::from(only_s35) > 1 {
        return Err("--only-s27, --only-s29 and --only-s35 exclude each other".into());
    }

    let env_ok = env_gate(flags)?;
    // Admission before any row (the S35 2026-08-21 lesson: three
    // "binding" FUA arms wrote to tmpfs behind a note): a binding run or
    // a FUA arm on a memory filesystem refuses here.
    let root_fstype =
        crate::gaterun::admit_device_root(flags, std::path::Path::new(&data_root), reference_box)?;
    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    if !reference_box {
        m.note("dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box");
    }
    m.note(format!("data root: {data_root} ({root_fstype})"));
    if crate::gaterun::is_memory_fs(&root_fstype) {
        m.note(format!(
            "data root is {root_fstype} (memory-backed): every durable leg measures the page \
             cache, not a device — harness smoke only, never an input to a row"
        ));
    }
    // The S34/S35 arms are disclosed on every report (a K = 1 / flush
    // row must never be mistaken for an unknown configuration).
    m.note(format!("durable arms: {}", crate::m2rows::pipeline_note(flags)));
    let arms_note = crate::m2rows::pipeline_note(flags);
    if only_s27 {
        m.note("--only-s27: the S29 scaling row was skipped; its gate keys are absent");
        s27_write_repeat_row(flags, &infinityd, cells, duration, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!("binary {infinityd} · cells {cells} · S27 row only · {arms_note}"),
        );
    }
    if only_s35 {
        m.note("--only-s35: the S29 and S27 rows were skipped; their gate keys are absent");
        s35_frame_pipeline_row(flags, &infinityd, cells, duration, replicates, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "binary {infinityd} · cells {cells} · {replicates} replicates · S35 row only · \
                 {arms_note}"
            ),
        );
    }
    m.note(format!(
        "row shape: {FILL_KEYS} keys × 1 KiB per namespace, tiered MEM-BUDGET {MEM_BUDGET}/cell \
         (demoter active), 100% SET closed-loop (pipeline 1), conns {CONNS_LOW} vs {CONNS_HIGH}, \
         {duration}s legs, median of {replicates} replicates, fresh server + data-dir per replicate"
    ));
    m.note(
        "data-root must not be tmpfs — the row's fsyncs must hit a real device or the \
         concurrency slope measures the page cache",
    );

    let mut t_low: Vec<f64> = Vec::new();
    let mut t_high: Vec<f64> = Vec::new();
    let mut f_low: Vec<f64> = Vec::new();
    let mut f_high: Vec<f64> = Vec::new();
    let mut t_high_p99: Vec<f64> = Vec::new();
    let mut f_high_p99: Vec<f64> = Vec::new();
    let mut t_high_p999: Vec<f64> = Vec::new();
    let mut f_high_p999: Vec<f64> = Vec::new();
    let mut raw = String::new();

    for rep in 0..replicates {
        let dir = format!("{data_root}/rep{rep}");
        // A fresh directory per replicate: recovery state from a prior
        // rep would change what the legs measure.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
        let mut extra: Vec<String> = vec!["--data-dir".into(), dir.clone()];
        if let Some(pin) = flags.get("pin-start") {
            extra.push("--pin-start".into());
            extra.push(pin.to_string());
        }
        // M4.5-S35: the durable arms ride this row too (before 2026-08-21
        // only the S27 spawn forwarded them — a K-arm campaign through
        // this row would have compared the default against itself).
        extra.extend(crate::m2rows::pipeline_args(flags));
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let server = spawn_infinityd(&infinityd, cells, &extra_refs)?;
        let port = server.port;

        create_ns(
            port,
            &[
                b"INF.NS",
                b"CREATE",
                b"s29tiered",
                b"MODE",
                b"durable",
                b"FSYNC",
                b"always",
                b"MEM-BUDGET",
                MEM_BUDGET.as_bytes(),
                b"DISK-BUDGET",
                b"10gb",
                b"TIER-IO-MODE",
                b"direct",
            ],
        )?;
        create_ns(
            port,
            &[b"INF.NS", b"CREATE", b"s29flat", b"MODE", b"durable", b"FSYNC", b"always"],
        )?;
        // The namespace fans to peer cells over the fabric; a leg started
        // before every cell applied it dies on `USE` (REUSEPORT spreads
        // the leg's connections across cells).
        await_fan(port, "s29tiered", cells)?;
        await_fan(port, "s29flat", cells)?;

        for ns in ["s29tiered", "s29flat"] {
            let report = run_load(&fill_spec(port, ns))?;
            if report.errors > 0 {
                return Err(format!(
                    "rep{rep} {ns} fill: {} errors (first: {:?})",
                    report.errors,
                    report.error_samples.first()
                ));
            }
        }

        for (ns, conns) in [
            ("s29tiered", CONNS_LOW),
            ("s29tiered", CONNS_HIGH),
            ("s29flat", CONNS_LOW),
            ("s29flat", CONNS_HIGH),
        ] {
            let sample = write_leg(port, ns, conns, duration, cells, &mut raw, rep)?;
            match (ns, conns) {
                ("s29tiered", CONNS_LOW) => {
                    t_low.push(sample.ops_per_sec);
                }
                ("s29tiered", _) => {
                    t_high.push(sample.ops_per_sec);
                    t_high_p99.push(sample.p99_us);
                    t_high_p999.push(sample.p999_us);
                }
                (_, CONNS_LOW) => {
                    f_low.push(sample.ops_per_sec);
                }
                _ => {
                    f_high.push(sample.ops_per_sec);
                    f_high_p99.push(sample.p99_us);
                    f_high_p999.push(sample.p999_us);
                }
            }
        }
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    let (t64, t256) = (median(&mut t_low), median(&mut t_high));
    let (f64_, f256) = (median(&mut f_low), median(&mut f_high));
    m.set("s29:tiered_scaling_slope_x", t256 / t64);
    m.set("s29:tiered_flat_ratio_c64", t64 / f64_);
    m.set("s29:tiered_flat_ratio_c256", t256 / f256);
    m.set("s29:tiered_flat_p99_ratio_c256", median(&mut t_high_p99) / median(&mut f_high_p99));
    // M4.5-S31 informational keys (no gate row): the foreground tail
    // during active sealing, per arm — the A/B's acceptance signal.
    m.set("s31:tiered_p999_c256_us", median(&mut t_high_p999));
    m.set("s31:flat_p999_c256_us", median(&mut f_high_p999));
    m.note(format!(
        "medians (ops/s): tiered {t64:.0} @{CONNS_LOW} → {t256:.0} @{CONNS_HIGH}; \
         flat {f64_:.0} @{CONNS_LOW} → {f256:.0} @{CONNS_HIGH}"
    ));
    m.row_open("tiered-always-scaling");
    m.row_write_amp(
        "not measured by this row — the S29 row gates the concurrency slope; write \
         amplification for tiered namespaces is owned by the M4 S16 rows",
    );
    m.raw_section("per-leg samples", &raw);

    if only_s29 {
        m.note(
            "--only-s29: the S27 backpressure and S35 frame-pipeline rows were skipped; their \
             gate keys are absent",
        );
    } else {
        s27_write_repeat_row(flags, &infinityd, cells, duration, &data_root, &mut m)?;
        s35_frame_pipeline_row(flags, &infinityd, cells, duration, replicates, &data_root, &mut m)?;
    }

    finish_report(
        "m4.5",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!("binary {infinityd} · cells {cells} · {replicates} replicates · {arms_note}"),
    )
}

/// The M4.5-S27 sustained-write-repeat row (ADR-0083 D7): a fresh
/// server in the deliberately provoked pressure regime
/// (`--log-staging-mib 1` — headroom below one burst at closed-loop
/// arrival, so admission pressure engages on a healthy device), one
/// flat `everysec` namespace, `S27_REPEATS` back-to-back 100 %-write
/// legs on one node. Gate keys: client-visible refusal rate, the
/// last:first throughput ratio (the finding's monotonic-decay
/// signature), and the worst per-leg max. An informational `always`
/// leg rides along (the ADR-0081 "measure `always` for the same
/// shape" obligation); its disposition lives in the ledger, not a
/// gate. Pre-fix, this row storms `-BUSY` (the fabric-owner refusal);
/// post-fix it parks — `log_admission_parked_total` in the raw
/// section proves the regime engaged.
fn s27_write_repeat_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    // Leg-set 1 — the **provoked regime** (--log-staging-mib 1, pipeline
    // 4): admission pressure engages deterministically because arrival
    // exceeds the drain during frame-write stalls. This set gates the
    // *refusal shape only* — max/decay here are device-writeback physics
    // (SLC state at saturation), not the admission mechanism.
    let mut raw = String::new();
    let mut ops_total: u64 = 0;
    let mut busy_total: u64 = 0;
    {
        let (server, port) = s27_spawn(flags, infinityd, cells, data_root, Some(1))?;
        let before = scrape_cells(port, cells)?;
        for rep in 0..S27_REPEATS {
            let report = s27_leg(port, "s27press", 4, duration)?;
            check_non_busy(&report, &format!("s27 provoked rep{rep}"))?;
            ops_total += report.ops;
            busy_total += report.busy_retryable;
            raw.push_str(&format!(
                "provoked rep{rep} everysec ops/s={:<8.0} p99_us={:<7} max_us={:<8} busy={}\n",
                report.ops_per_sec, report.p99_us, report.max_us, report.busy_retryable
            ));
        }
        let after = scrape_cells(port, cells)?;
        let parked = sum_field(&after, "log_admission_parked_total")
            .saturating_sub(sum_field(&before, "log_admission_parked_total"));
        let stall_p99 = after
            .iter()
            .filter_map(|c| c.get("log_write_stall_p99_us").and_then(|v| v.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        raw.push_str(&format!(
            "provoked regime: parked_total(delta)={parked} \
             write_stall_p99_us(worst cell)={stall_p99}\n"
        ));
        if parked == 0 {
            m.note(
                "s27: WARNING — parked_total delta is 0 in the provoked set: either pressure \
                 never engaged (regime vacuous) or the server predates the counter (pre-fix \
                 A/B arm)",
            );
        }
        // Informational `always` leg (the ADR-0081 "measure `always` for
        // the same shape" obligation — dispositioned in the ledger).
        let always = s27_leg(port, "s27always", 4, duration)?;
        raw.push_str(&format!(
            "provoked always informational ops/s={:<8.0} p99_us={:<7} max_us={:<8} busy={}\n",
            always.ops_per_sec, always.p99_us, always.max_us, always.busy_retryable
        ));
        busy_total += always.busy_retryable;
        ops_total += always.ops;
        drop(server);
    }

    // Leg-set 2 — the **ADR-0081 D5 shape**: default staging, the
    // finding's closed loop (32 conns, pipeline 1), back-to-back repeats
    // on one node. This set carries the D5 bar: no monotonic decay and a
    // bounded max at `everysec`.
    let mut max_us_worst: u64 = 0;
    let mut first_rep_ops_per_sec = 0.0f64;
    let mut last_rep_ops_per_sec = 0.0f64;
    {
        let (server, port) = s27_spawn(flags, infinityd, cells, data_root, None)?;
        for rep in 0..S27_REPEATS {
            let report = s27_leg(port, "s27press", 1, duration)?;
            check_non_busy(&report, &format!("s27 d5 rep{rep}"))?;
            ops_total += report.ops;
            busy_total += report.busy_retryable;
            max_us_worst = max_us_worst.max(report.max_us);
            if rep == 0 {
                first_rep_ops_per_sec = report.ops_per_sec;
            }
            last_rep_ops_per_sec = report.ops_per_sec;
            raw.push_str(&format!(
                "d5-shape rep{rep} everysec ops/s={:<8.0} p99_us={:<7} max_us={:<8} busy={}\n",
                report.ops_per_sec, report.p99_us, report.max_us, report.busy_retryable
            ));
        }
        drop(server);
    }

    m.set("s27:busy_refusals_pct", busy_total as f64 * 100.0 / ops_total.max(1) as f64);
    m.set("s27:write_repeat_decay_x", last_rep_ops_per_sec / first_rep_ops_per_sec.max(1.0));
    m.set("s27:max_ms", max_us_worst as f64 / 1000.0);
    m.note(format!(
        "s27 row: refusal gate spans both leg-sets ({S27_REPEATS} provoked \
         --log-staging-mib 1 pipeline-4 repeats + always leg, then {S27_REPEATS} \
         default-staging pipeline-1 repeats — the ADR-0081 D5 shape); decay and max gate the \
         D5 leg-set only ({duration}s legs, 32 conns, 1 KiB values, flat everysec)"
    ));
    m.row_open("durable-write-backpressure");
    m.row_write_amp(
        "not measured by this row — the S27 row gates the backpressure shape (refusals, \
         decay, max); write amplification is unchanged by it",
    );
    m.raw_section("s27 per-repeat samples", &raw);
    Ok(())
}

/// Seconds the S35 row idles before every durable leg (the S34/S35
/// drive-state rule: the DRAM-less reference device needs ~40 s to
/// digest the previous leg's zero-fill + frames, or its write-through
/// tail goes bimodal — 7 of 18 preview runs at 20 s, 0 of 9 at 40 s).
/// `--leg-idle-s` overrides; 0 for harness smoke.
const S35_LEG_IDLE_S: u64 = 40;

/// The S35 AC shape: the closed loop S33's bound-2 arm lost.
const S35_CONNS_AC: usize = 32;

/// One S35 durable leg's scrape-derived facts.
struct S35Leg {
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    max_us: f64,
    /// The barrier the client waited on: write-through p50 under the
    /// FUA class, linked-fdatasync p50 under FLUSH (cell median).
    barrier_p50_us: f64,
    /// The device tail for the same class (worst cell) — the S34
    /// drive-state discriminator, disclosed per leg.
    barrier_p99_us: f64,
    frames_in_flight_max: u64,
}

/// The M4.5-S35 frame-pipeline row (ADR-0087 D8). Per replicate: a
/// fresh N-cell server with the campaign's durable arms → one flat
/// `always` namespace → deterministic fill → a 32-conn closed-loop
/// 100 %-write leg (the AC: client p50 ÷ barrier p50) → a 256-conn
/// leg (the `max` row) → a pipelined 100 %-GET leg (the ±2 % read
/// row, compared across arm reports). Then a fresh 1-cell server, same
/// fill, the 32-conn leg (the 4c ÷ 1c p50 ratio — S34's F2 AC, owned
/// here). Every durable leg is preceded by the drive-state idle.
///
/// The barrier p50 is a whole-session histogram (the fill's frames are
/// in it: ~780 frames/cell at 64 × 4 in flight vs ~10k from the leg —
/// ≤ 10 % of samples, disclosed). `frames_in_flight_max` must equal the
/// configured K on the N-cell leg or the pipeline never filled and the
/// row is a K = 1 row in disguise (a note, never silent).
#[allow(clippy::too_many_arguments)] // orchestration script
fn s35_frame_pipeline_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let configured_k: u64 = flags.u64_or("frames-in-flight", 1)?;
    let class_fua = flags.str_or("barrier-class", "flush").eq_ignore_ascii_case("fua");
    let (p50_key, p99_key) = if class_fua {
        ("fua_latency_p50_us", "fua_latency_p99_us")
    } else {
        ("fsync_latency_p50_us", "fsync_latency_p99_us")
    };
    let mut raw = String::new();
    let mut ratio: Vec<f64> = Vec::new();
    let mut p50_n: Vec<f64> = Vec::new();
    let mut ops_n: Vec<f64> = Vec::new();
    let mut p99_n: Vec<f64> = Vec::new();
    let mut max_n: Vec<f64> = Vec::new();
    let mut barrier_n: Vec<f64> = Vec::new();
    let mut ops_256: Vec<f64> = Vec::new();
    let mut p99_256: Vec<f64> = Vec::new();
    let mut max_256: Vec<f64> = Vec::new();
    let mut read_ops: Vec<f64> = Vec::new();
    let mut read_p999: Vec<f64> = Vec::new();
    let mut p50_one: Vec<f64> = Vec::new();
    let mut fill_max_seen: u64 = 0;
    let mut device_tail_legs: usize = 0;

    for rep in 0..replicates {
        let dir = format!("{data_root}/s35-{cells}c-rep{rep}");
        s35_idle(idle_s, &format!("{cells}c rep{rep} c{S35_CONNS_AC}"));
        let (server, port) = s35_spawn(flags, infinityd, cells, &dir)?;
        let fill = run_load(&fill_spec(port, "s35alw"))?;
        if fill.errors > 0 {
            return Err(format!(
                "s35 rep{rep} fill: {} errors (first: {:?})",
                fill.errors,
                fill.error_samples.first()
            ));
        }
        let ac = s35_write_leg(port, cells, S35_CONNS_AC, duration, p50_key, p99_key)?;
        fill_max_seen = fill_max_seen.max(ac.frames_in_flight_max);
        if ac.barrier_p99_us > 10_000.0 {
            device_tail_legs += 1;
        }
        raw.push_str(&format!(
            "rep{rep} {cells}c c{S35_CONNS_AC:<3} ops/s={:<8.0} p50_us={:<6.0} p99_us={:<7.0} \
             max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} p50/barrier={:.2} \
             frames_in_flight_max={}\n",
            ac.ops_per_sec,
            ac.p50_us,
            ac.p99_us,
            ac.max_us,
            ac.barrier_p50_us,
            ac.barrier_p99_us,
            ac.p50_us / ac.barrier_p50_us.max(1.0),
            ac.frames_in_flight_max
        ));
        ratio.push(ac.p50_us / ac.barrier_p50_us.max(1.0));
        p50_n.push(ac.p50_us);
        ops_n.push(ac.ops_per_sec);
        p99_n.push(ac.p99_us);
        max_n.push(ac.max_us);
        barrier_n.push(ac.barrier_p50_us);

        s35_idle(idle_s, &format!("{cells}c rep{rep} c{CONNS_HIGH}"));
        let hi = s35_write_leg(port, cells, CONNS_HIGH, duration, p50_key, p99_key)?;
        if hi.barrier_p99_us > 10_000.0 {
            device_tail_legs += 1;
        }
        raw.push_str(&format!(
            "rep{rep} {cells}c c{CONNS_HIGH:<3} ops/s={:<8.0} p50_us={:<6.0} p99_us={:<7.0} \
             max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} \
             frames_in_flight_max={}\n",
            hi.ops_per_sec,
            hi.p50_us,
            hi.p99_us,
            hi.max_us,
            hi.barrier_p50_us,
            hi.barrier_p99_us,
            hi.frames_in_flight_max
        ));
        ops_256.push(hi.ops_per_sec);
        p99_256.push(hi.p99_us);
        max_256.push(hi.max_us);

        // The read row: pipelined GETs over the filled keyspace (the
        // M0 zero-cost shape on a durable namespace). No idle — reads
        // do not move drive state.
        let rd = run_load(&LoadSpec {
            port,
            conns: 64,
            pipeline: 16,
            duration: Duration::from_secs(duration),
            warmup: Duration::from_secs(2),
            set_weight: 0,
            get_weight: 1,
            keys: FILL_KEYS,
            key_prefix: "s35alw:".into(),
            value_size: 1024,
            setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s35alw".to_vec()]],
            ..LoadSpec::default()
        })?;
        if rd.errors > 0 {
            return Err(format!(
                "s35 rep{rep} read leg: {} errors (first: {:?})",
                rd.errors,
                rd.error_samples.first()
            ));
        }
        raw.push_str(&format!(
            "rep{rep} {cells}c read c64 P16 ops/s={:<8.0} p50_us={:<6} p99_us={:<7} \
             p999_us={:<7} nils={}\n",
            rd.ops_per_sec, rd.p50_us, rd.p99_us, rd.p999_us, rd.nils
        ));
        read_ops.push(rd.ops_per_sec);
        read_p999.push(rd.p999_us as f64);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The 1-cell arm of the F2 ratio (same binary, same arms, same
    // shape; only `--cells` differs).
    if cells > 1 {
        for rep in 0..replicates {
            let dir = format!("{data_root}/s35-1c-rep{rep}");
            s35_idle(idle_s, &format!("1c rep{rep} c{S35_CONNS_AC}"));
            let (server, port) = s35_spawn(flags, infinityd, 1, &dir)?;
            let fill = run_load(&fill_spec(port, "s35alw"))?;
            if fill.errors > 0 {
                return Err(format!("s35 1c rep{rep} fill: {} errors", fill.errors));
            }
            let one = s35_write_leg(port, 1, S35_CONNS_AC, duration, p50_key, p99_key)?;
            if one.barrier_p99_us > 10_000.0 {
                device_tail_legs += 1;
            }
            raw.push_str(&format!(
                "rep{rep} 1c c{S35_CONNS_AC:<3} ops/s={:<8.0} p50_us={:<6.0} p99_us={:<7.0} \
                 max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} p50/barrier={:.2} \
                 frames_in_flight_max={}\n",
                one.ops_per_sec,
                one.p50_us,
                one.p99_us,
                one.max_us,
                one.barrier_p50_us,
                one.barrier_p99_us,
                one.p50_us / one.barrier_p50_us.max(1.0),
                one.frames_in_flight_max
            ));
            p50_one.push(one.p50_us);
            drop(server);
            let _ = std::fs::remove_dir_all(&dir);
        }
    } else {
        m.note("s35: --cells 1 — the multi-vs-one-cell ratio key is absent (nothing to compare)");
    }

    m.set("s35:p50_over_barrier_x", median(&mut ratio));
    m.set("s35:always_c32_ops_per_sec", median(&mut ops_n));
    m.set("s35:always_c32_p50_us", median(&mut p50_n));
    m.set("s35:always_c32_p99_us", median(&mut p99_n));
    m.set("s35:always_c32_max_us", median(&mut max_n));
    m.set("s35:barrier_p50_us", median(&mut barrier_n));
    m.set("s35:always_c256_ops_per_sec", median(&mut ops_256));
    m.set("s35:always_c256_p99_us", median(&mut p99_256));
    m.set("s35:always_c256_max_us", median(&mut max_256));
    m.set("s35:read_c64p16_ops_per_sec", median(&mut read_ops));
    m.set("s35:read_c64p16_p999_us", median(&mut read_p999));
    m.set("s35:frames_in_flight_max", fill_max_seen as f64);
    if !p50_one.is_empty() {
        let one = median(&mut p50_one);
        m.set("s35:one_cell_c32_p50_us", one);
        m.set("s35:multi_vs_one_cell_p50_x", median(&mut p50_n) / one.max(1.0));
    }
    if fill_max_seen < configured_k {
        m.note(format!(
            "s35: WARNING — frames_in_flight_max {fill_max_seen} < configured K \
             {configured_k}: the pipeline never filled on the {cells}-cell AC leg; this \
             arm measured a shallower pipeline than its label"
        ));
    }
    if device_tail_legs > 0 {
        m.note(format!(
            "s35: {device_tail_legs} durable leg(s) saw a device barrier p99 > 10 ms (the S34 \
             drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a \
             longer --leg-idle-s before citing"
        ));
    }
    m.note(format!(
        "s35 row: flat always, {FILL_KEYS} keys × 1 KiB fill, {duration}s legs, median of \
         {replicates}; AC leg {S35_CONNS_AC} conns pipeline 1 on {cells} cells then on 1 cell; \
         max leg {CONNS_HIGH} conns; read leg 64 conns × P16 100% GET; {idle_s}s idle before \
         every durable leg; barrier = {p50_key} (cell median, whole-session histogram incl. \
         the fill's frames); device tail = {p99_key} (worst cell)"
    ));
    m.row_open("frame-pipeline");
    m.row_write_amp(
        "not measured by this row — the S35 row gates the pipeline's latency shape; the \
         class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, \
         zero_fill_bytes) and S36 owns write_amp_log_ckpt",
    );
    m.raw_section("s35 per-leg samples", &raw);
    Ok(())
}

/// Drive-state idle before a durable leg (disclosed on stdout so a
/// reader of the terminal log can see the campaign rule being applied).
fn s35_idle(idle_s: u64, leg: &str) {
    if idle_s > 0 {
        println!("  s35: {idle_s}s drive-state idle before {leg}");
        #[allow(clippy::disallowed_methods)] // bench orchestration idle, not cell code
        std::thread::sleep(Duration::from_secs(idle_s));
    }
}

/// Spawns the S35 row's server on a fresh data dir with the campaign's
/// durable arms and creates + fans the flat `always` namespace.
fn s35_spawn(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
) -> Result<(crate::gaterun::ServerGuard, u16), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    create_ns(port, &[b"INF.NS", b"CREATE", b"s35alw", b"MODE", b"durable", b"FSYNC", b"always"])?;
    await_fan(port, "s35alw", cells)?;
    Ok((server, port))
}

/// One S35 100 %-write closed-loop leg with its barrier scrape.
fn s35_write_leg(
    port: u16,
    cells: u16,
    conns: usize,
    duration: u64,
    p50_key: &str,
    p99_key: &str,
) -> Result<S35Leg, String> {
    let report = run_load(&LoadSpec {
        port,
        conns,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: "s35alw:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s35alw".to_vec()]],
        ..LoadSpec::default()
    })?;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s35 {cells}c c{conns}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let infos = scrape_cells(port, cells)?;
    let mut p50s: Vec<f64> =
        infos.iter().filter_map(|c| c.get(p50_key).and_then(|v| v.parse::<f64>().ok())).collect();
    if p50s.is_empty() {
        return Err(format!("s35: INFO field {p50_key} absent — pre-S34 binary?"));
    }
    Ok(S35Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        max_us: report.max_us as f64,
        barrier_p50_us: median(&mut p50s),
        barrier_p99_us: crate::gaterun::max_field(&infos, p99_key) as f64,
        frames_in_flight_max: crate::gaterun::max_field(&infos, "frames_in_flight_max"),
    })
}

/// Spawns the S27 row's server (fresh data dir per spawn; optional
/// shrunk staging = the provoked regime) and creates + fans both
/// namespaces.
fn s27_spawn(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    data_root: &str,
    staging_mib: Option<u32>,
) -> Result<(crate::gaterun::ServerGuard, u16), String> {
    let dir = format!("{data_root}/s27");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir];
    if let Some(mib) = staging_mib {
        extra.push("--log-staging-mib".into());
        extra.push(mib.to_string());
    }
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    // M4.5-S35 (ADR-0087 D5): the frame-pipeline / barrier-class arms
    // ride this row (the retired `--sync-pipeline` is a no-op — the
    // FLUSH-class overlap it measured dissolves under write-through).
    // An explicit `staging_mib` (the provoked regime) wins over the
    // campaign-wide `--staging-mib`.
    let mut arms = crate::m2rows::pipeline_args(flags);
    if staging_mib.is_some()
        && let Some(at) = arms.iter().position(|a| a == "--log-staging-mib")
    {
        arms.drain(at..at + 2);
    }
    extra.extend(arms);
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    create_ns(
        port,
        &[b"INF.NS", b"CREATE", b"s27press", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    create_ns(
        port,
        &[b"INF.NS", b"CREATE", b"s27always", b"MODE", b"durable", b"FSYNC", b"always"],
    )?;
    await_fan(port, "s27press", cells)?;
    await_fan(port, "s27always", cells)?;
    Ok((server, port))
}

/// One S27 100%-write leg (32 conns, 1 KiB values) against `ns`.
fn s27_leg(
    port: u16,
    ns: &str,
    pipeline: usize,
    duration: u64,
) -> Result<crate::load::LoadReport, String> {
    run_load(&LoadSpec {
        port,
        conns: 32,
        pipeline,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(1),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: format!("{ns}:"),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), ns.as_bytes().to_vec()]],
        ..LoadSpec::default()
    })
}

/// A leg aborts on any non-BUSY error; BUSY replies are the row's
/// subject and are counted, never fatal.
fn check_non_busy(report: &crate::load::LoadReport, leg: &str) -> Result<(), String> {
    if report.errors > report.busy_retryable {
        return Err(format!(
            "{leg}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    Ok(())
}

/// Back-to-back sustained-write repeats in the S27 row — the shape that
/// exposed the finding (decay only shows across repeats).
const S27_REPEATS: usize = 3;

/// Sends one `INF.NS CREATE` and demands `+OK`, riding out the boot
/// `-LOADING` window (the listener accepts before recovery completes;
/// a fresh data-dir clears it in milliseconds).
fn create_ns(port: u16, argv: &[&[u8]]) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(&mut stream, argv)?;
        if reply.starts_with(b"+OK") {
            return Ok(());
        }
        if !(reply.starts_with(b"-LOADING") && Instant::now() < deadline) {
            return Err(format!("INF.NS CREATE failed: {}", String::from_utf8_lossy(&reply)));
        }
    }
}

/// Waits until `INF.NS USE` succeeds on `4 × cells` consecutive fresh
/// connections — the REUSEPORT-spread proof that the DDL fan reached
/// every cell.
fn await_fan(port: u16, ns: &str, cells: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut streak: u16 = 0;
    while streak < 4 * cells {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(&mut stream, &[b"INF.NS", b"USE", ns.as_bytes()])?;
        if reply.starts_with(b"+OK") {
            streak += 1;
        } else {
            streak = 0;
            if Instant::now() >= deadline {
                return Err(format!(
                    "namespace {ns} never fanned to all cells: {}",
                    String::from_utf8_lossy(&reply)
                ));
            }
        }
    }
    Ok(())
}

/// The deterministic fill: every key SET exactly once, 1 KiB values.
fn fill_spec(port: u16, ns: &str) -> LoadSpec {
    LoadSpec {
        port,
        conns: 64,
        pipeline: 4,
        fill: Some(FILL_KEYS),
        keys: FILL_KEYS,
        key_prefix: format!("{ns}:"),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), ns.as_bytes().to_vec()]],
        ..LoadSpec::default()
    }
}

/// One measured 100%-write closed-loop leg. Pipeline 1 — the defect is
/// a per-request serialization; pipelining would refill the origin FIFO
/// and mask it.
fn write_leg(
    port: u16,
    ns: &str,
    conns: usize,
    duration: u64,
    cells: u16,
    raw: &mut String,
    rep: usize,
) -> Result<LegSample, String> {
    let before = scrape_cells(port, cells)?;
    let spec = LoadSpec {
        port,
        conns,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: format!("{ns}:"),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), ns.as_bytes().to_vec()]],
        ..LoadSpec::default()
    };
    let report = run_load(&spec)?;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "rep{rep} {ns} c{conns}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let after = scrape_cells(port, cells)?;
    let acks = sum_field(&after, "acks_gated") - sum_field(&before, "acks_gated");
    let fsyncs = sum_field(&after, "fsyncs_completed") - sum_field(&before, "fsyncs_completed");
    let group = if fsyncs > 0 { acks as f64 / fsyncs as f64 } else { 0.0 };
    // M4.5-S31: the leg's sealing activity, from the reactor-drive round
    // counter — a tail claim about "during sealing" needs this nonzero
    // on the tiered legs (ADR-0084 D6; absent on pre-S31 binaries).
    let rounds =
        sum_field(&after, "tiering_flush_rounds") - sum_field(&before, "tiering_flush_rounds");
    // M4.5-S30 (ADR-0085 D5): the parity residual's discriminators —
    // how many of the leg's writes paid a foreground cold resolve, and
    // whether read promotion engaged at all (it must read 0 on this
    // 100%-write shape; absent on pre-S30 binaries).
    let cold = sum_field(&after, "cold_reads_issued") - sum_field(&before, "cold_reads_issued");
    let promos = sum_field(&after, "tiering_promotions") - sum_field(&before, "tiering_promotions");
    raw.push_str(&format!(
        "rep{rep} {ns:<9} c{conns:<3} ops/s={:<8.0} p50_us={:<6} p99_us={:<7} p999_us={:<7} \
         busy={} acks/fsync={group:.2} flush_rounds={rounds} cold_reads={cold} \
         promotions={promos}\n",
        report.ops_per_sec, report.p50_us, report.p99_us, report.p999_us, report.busy_retryable
    ));
    Ok(LegSample {
        ops_per_sec: report.ops_per_sec,
        p99_us: report.p99_us as f64,
        p999_us: report.p999_us as f64,
    })
}
