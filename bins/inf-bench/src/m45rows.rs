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
//! Row 4 (M4.5-S36, ADR-0088 D7/D9): the device-budget row — the leg A
//! pure-write `everysec` shape with the **server's CPU** measured across
//! the leg (an engine idle behind its device is a device row, not an
//! engine row), a same-session **tmpfs control** (the 0.85× denominator),
//! the `write_amp_milli_log_checkpoint` figure scraped with checkpoints
//! active, and the S27 D5 `max` leg at a **comparator-matched offered
//! rate** (ADR-0081 D5 was written for closed-loop saturation; the bar
//! binds at the rate the comparators were measured at).
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
    // `--only-s36` (M4.5-S36): the device-budget arms run just their row.
    let only_s36 = flags.bool("only-s36");
    // `--only-s39b` (M4.5-S39b): the segment-recycling A/B runs its row.
    let only_s39b = flags.bool("only-s39b");
    // `--only-s39d` (M4.5-S39d): the fixed-work recovery attribution row.
    let only_s39d = flags.bool("only-s39d");
    // `--only-s40` (M4.5-S40): the stall-attribution row at the memtier shape.
    let only_s40 = flags.bool("only-s40");
    // `--only-s37` (M4.5-S37 step 1): the cold-overwrite ceiling A/B.
    let only_s37 = flags.bool("only-s37");
    if usize::from(only_s27)
        + usize::from(only_s29)
        + usize::from(only_s35)
        + usize::from(only_s36)
        + usize::from(only_s39b)
        + usize::from(only_s39d)
        + usize::from(only_s40)
        + usize::from(only_s37)
        > 1
    {
        return Err("--only-s27, --only-s29, --only-s35, --only-s36, --only-s37, --only-s39b, \
                    --only-s39d and --only-s40 exclude each other"
            .into());
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
    if only_s39b {
        m.note(
            "--only-s39b: the S29, S27, S35 and S36 rows were skipped; their gate keys are absent",
        );
        let duration =
            if flags.get("duration").is_some() { duration } else { S39B_DEFAULT_DURATION_S };
        s39b_recycle_row(flags, &infinityd, cells, duration, replicates, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "binary {infinityd} · cells {cells} · {replicates} replicates · S39b row only · \
                 {arms_note}"
            ),
        );
    }
    if only_s37 {
        m.note("--only-s37: every other row was skipped; their gate keys are absent");
        s37_ceiling_row(flags, &infinityd, cells, duration, replicates, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "binary {infinityd} (bench-diagnostics) · cells {cells} · {replicates} \
                 replicates · S37 ceiling row only · {arms_note}"
            ),
        );
    }
    if only_s40 {
        m.note("--only-s40: every other row was skipped; their gate keys are absent");
        let duration = if flags.get("duration").is_some() { duration } else { 60 };
        s40_stall_row(flags, &infinityd, cells, duration, replicates, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "binary {infinityd} · cells {cells} · {replicates} legs · S40 row only · \
                 {arms_note}"
            ),
        );
    }
    if only_s39d {
        m.note(
            "--only-s39d: the S29, S27, S35, S36 and S39b rows were skipped; their gate keys \
             are absent",
        );
        s39d_recovery_row(flags, &infinityd, cells, replicates, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!(
                "binary {infinityd} · cells {cells} · {replicates} replicates · S39d row only · \
                 {arms_note}"
            ),
        );
    }
    if only_s36 {
        m.note("--only-s36: the S29, S27 and S35 rows were skipped; their gate keys are absent");
        s36_device_budget_row(flags, &infinityd, cells, duration, &data_root, &mut m)?;
        return finish_report(
            "m4.5",
            &gates_list,
            &m,
            env_ok,
            reference_box,
            &artifacts_root,
            &format!("binary {infinityd} · cells {cells} · S36 row only · {arms_note}"),
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
        s36_device_budget_row(flags, &infinityd, cells, duration, &data_root, &mut m)?;
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
    /// Exact client mean — disclosed beside the p50 (ADR-0087 D8 as
    /// amended 2026-08-22: never the gate's statistic on its own).
    mean_us: f64,
    p99_us: f64,
    max_us: f64,
    /// The barrier the client waited on: write-through p50 under the
    /// FUA class, linked-fdatasync p50 under FLUSH (cell median).
    barrier_p50_us: f64,
    /// The device tail for the same class (worst cell) — the S34
    /// drive-state discriminator, disclosed per leg.
    barrier_p99_us: f64,
    frames_in_flight_max: u64,
    /// Group size (gated acks per barrier) and frames issued — the
    /// @256 discriminator.
    acks_per_fsync: f64,
    frames: u64,
    parked: u64,
    /// The staging drain's binding variable (ADR-0083 D5), worst cell —
    /// a client tail with a healthy device tail points here.
    write_stall_p99_us: u64,
    /// M4.5-S39a: the leg's v3 padding share of its frame bytes (the
    /// leg's own delta of `log_padding_bytes` / `log_frame_bytes`, summed
    /// over cells; 0 on packed segments by construction) and the fill
    /// policy's hold episodes during the leg.
    padding_pct: f64,
    waits_fill: u64,
    /// M4.5-S43 (ADR-0092 D4 H5): the group hold's episodes during the
    /// leg and the round target it last waited for (summed over cells —
    /// a disclosure, never a gate statistic on its own).
    waits_group: u64,
    round_target: u64,
}

/// The M4.5-S35 frame-pipeline row (ADR-0087 D8). Per replicate: a
/// fresh N-cell server with the campaign's durable arms → one flat
/// `always` namespace → a 32-conn closed-loop 100 %-write leg (the AC:
/// client p50 ÷ barrier p50) → a 256-conn leg (the `max` row) → a
/// pipelined 100 %-GET leg over the keyspace the two write legs
/// populated (the ±2 % read row, compared across arm reports; `nils`
/// disclosed). Then, **inside the same replicate**, a fresh 1-cell
/// server and the 32-conn leg (the 4c ÷ 1c p50 ratio — S34's F2 AC,
/// owned here): the arms of the ratio are interleaved so drive-state
/// drift across the campaign lands on both, never on whichever block
/// ran last (review of `2cb6074`). Every durable leg is preceded by the
/// drive-state idle.
///
/// **No fill leg**: the barrier p50 is a whole-session histogram, and
/// the first campaign (2026-08-21, `.artifacts/m4.5/s35-gate/`) showed
/// a 64 × 4 pipelined fill's larger frames lifting it above the AC
/// leg's own client p50 (1-cell `p50/barrier = 0.97`, physically
/// impossible for the leg alone). The AC leg now runs first on a fresh
/// server, so the histogram it is scraped against holds only its own
/// frames. Each leg also reports its group size (`acks/fsync`), frames
/// issued, and parked admissions — the discriminators for the @256
/// finding (a deeper pipeline sealing smaller frames). `frames_in_
/// flight_max` must equal the configured K on the N-cell leg or the
/// pipeline never filled and the row is a K = 1 row in disguise (a
/// note, never silent).
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
    let mut group_256: Vec<f64> = Vec::new();
    let mut group_n: Vec<f64> = Vec::new();
    let mut read_ops: Vec<f64> = Vec::new();
    let mut read_p999: Vec<f64> = Vec::new();
    let mut p50_one: Vec<f64> = Vec::new();
    let mut barrier_one: Vec<f64> = Vec::new();
    // ADR-0087 D8 as amended 2026-08-22: the N-cell ÷ 1-cell quantities are
    // **per-replicate** ratios (each 4-cell leg against the 1-cell leg
    // interleaved with it), summarized by their median — never a ratio of
    // two medians, which pairs legs that never ran together.
    let mut ratio_client_rep: Vec<f64> = Vec::new();
    let mut ratio_barrier_rep: Vec<f64> = Vec::new();
    let mut padding_n: Vec<f64> = Vec::new();
    let mut padding_256: Vec<f64> = Vec::new();
    let mut fill_max_seen: u64 = 0;
    let mut device_tail_legs: usize = 0;
    let mut engine_tail_legs: usize = 0;

    for rep in 0..replicates {
        let dir = format!("{data_root}/s35-{cells}c-rep{rep}");
        s35_idle(idle_s, &format!("{cells}c rep{rep} c{S35_CONNS_AC}"));
        let (server, port) = s35_spawn(flags, infinityd, cells, &dir)?;
        let ac = s35_write_leg(port, cells, S35_CONNS_AC, duration, p50_key, p99_key)?;
        fill_max_seen = fill_max_seen.max(ac.frames_in_flight_max);
        if ac.barrier_p99_us > 10_000.0 {
            device_tail_legs += 1;
        }
        if ac.p99_us > 10.0 * ac.barrier_p99_us.max(1.0) {
            engine_tail_legs += 1;
        }
        raw.push_str(&format!(
            "rep{rep} {cells}c c{S35_CONNS_AC:<3} ops/s={:<8.0} p50_us={:<6.0} mean_us={:<6.0} \
             p99_us={:<7.0} max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} \
             p50/barrier={:.2} frames_in_flight_max={} acks/fsync={:.1} frames={} parked={} \
             write_stall_p99_us={} padding_pct={:.1} waits_fill={} waits_group={} round_target={}\n",
            ac.ops_per_sec,
            ac.p50_us,
            ac.mean_us,
            ac.p99_us,
            ac.max_us,
            ac.barrier_p50_us,
            ac.barrier_p99_us,
            ac.p50_us / ac.barrier_p50_us.max(1.0),
            ac.frames_in_flight_max,
            ac.acks_per_fsync,
            ac.frames,
            ac.parked,
            ac.write_stall_p99_us,
            ac.padding_pct,
            ac.waits_fill,
            ac.waits_group,
            ac.round_target
        ));
        ratio.push(ac.p50_us / ac.barrier_p50_us.max(1.0));
        p50_n.push(ac.p50_us);
        ops_n.push(ac.ops_per_sec);
        p99_n.push(ac.p99_us);
        max_n.push(ac.max_us);
        barrier_n.push(ac.barrier_p50_us);
        group_n.push(ac.acks_per_fsync);
        padding_n.push(ac.padding_pct);

        s35_idle(idle_s, &format!("{cells}c rep{rep} c{CONNS_HIGH}"));
        let hi = s35_write_leg(port, cells, CONNS_HIGH, duration, p50_key, p99_key)?;
        if hi.barrier_p99_us > 10_000.0 {
            device_tail_legs += 1;
        }
        raw.push_str(&format!(
            "rep{rep} {cells}c c{CONNS_HIGH:<3} ops/s={:<8.0} p50_us={:<6.0} mean_us={:<6.0} \
             p99_us={:<7.0} max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} \
             frames_in_flight_max={} acks/fsync={:.1} frames={} parked={} write_stall_p99_us={} \
             padding_pct={:.1} waits_fill={} waits_group={} round_target={}\n",
            hi.ops_per_sec,
            hi.p50_us,
            hi.mean_us,
            hi.p99_us,
            hi.max_us,
            hi.barrier_p50_us,
            hi.barrier_p99_us,
            hi.frames_in_flight_max,
            hi.acks_per_fsync,
            hi.frames,
            hi.parked,
            hi.write_stall_p99_us,
            hi.padding_pct,
            hi.waits_fill,
            hi.waits_group,
            hi.round_target
        ));
        ops_256.push(hi.ops_per_sec);
        p99_256.push(hi.p99_us);
        max_256.push(hi.max_us);
        group_256.push(hi.acks_per_fsync);
        padding_256.push(hi.padding_pct);

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

        // The 1-cell arm of the F2 ratio (same binary, same arms, same
        // shape; only `--cells` differs) — **interleaved** with its
        // N-cell replicate (review of `2cb6074`): drive-state drift
        // across a campaign then lands on both arms of the ratio, not
        // on whichever ran last.
        if cells > 1 {
            let dir = format!("{data_root}/s35-1c-rep{rep}");
            s35_idle(idle_s, &format!("1c rep{rep} c{S35_CONNS_AC}"));
            let (server, port) = s35_spawn(flags, infinityd, 1, &dir)?;
            let one = s35_write_leg(port, 1, S35_CONNS_AC, duration, p50_key, p99_key)?;
            if one.barrier_p99_us > 10_000.0 {
                device_tail_legs += 1;
            }
            raw.push_str(&format!(
                "rep{rep} 1c c{S35_CONNS_AC:<3} ops/s={:<8.0} p50_us={:<6.0} mean_us={:<6.0} \
                 p99_us={:<7.0} max_us={:<8.0} barrier_p50_us={:<5.0} barrier_p99_us={:<6.0} \
                 p50/barrier={:.2} frames_in_flight_max={} acks/fsync={:.1} frames={} parked={} \
                 write_stall_p99_us={} padding_pct={:.1} waits_fill={} waits_group={} round_target={} \
                 4c/1c: p50={:.3} barrier={:.3}\n",
                one.ops_per_sec,
                one.p50_us,
                one.mean_us,
                one.p99_us,
                one.max_us,
                one.barrier_p50_us,
                one.barrier_p99_us,
                one.p50_us / one.barrier_p50_us.max(1.0),
                one.frames_in_flight_max,
                one.acks_per_fsync,
                one.frames,
                one.parked,
                one.write_stall_p99_us,
                one.padding_pct,
                one.waits_fill,
                one.waits_group,
                one.round_target,
                ac.p50_us / one.p50_us.max(1.0),
                ac.barrier_p50_us / one.barrier_p50_us.max(1.0)
            ));
            ratio_client_rep.push(ac.p50_us / one.p50_us.max(1.0));
            ratio_barrier_rep.push(ac.barrier_p50_us / one.barrier_p50_us.max(1.0));
            p50_one.push(one.p50_us);
            barrier_one.push(one.barrier_p50_us);
            drop(server);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    if cells == 1 {
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
    m.set("s35:always_c256_acks_per_fsync", median(&mut group_256));
    m.set("s35:always_c32_acks_per_fsync", median(&mut group_n));
    // M4.5-S39a: the padding share per leg shape (raw, beside the latency
    // it is traded against — the fill policy's bytes row).
    m.set("s35:always_c32_padding_pct", median(&mut padding_n));
    m.set("s35:always_c256_padding_pct", median(&mut padding_256));
    m.set("s35:read_c64p16_ops_per_sec", median(&mut read_ops));
    m.set("s35:read_c64p16_p999_us", median(&mut read_p999));
    m.set("s35:frames_in_flight_max", fill_max_seen as f64);
    if !p50_one.is_empty() {
        m.set("s35:one_cell_c32_p50_us", median(&mut p50_one));
        m.set("s35:one_cell_barrier_p50_us", median(&mut barrier_one));
        // The F2 contention term (S34's finding: fsync p50 1,535 µs at 1
        // cell → 2,751 at 4 under FLUSH) measured directly — the barrier
        // the N-cell legs waited on over the barrier the 1-cell leg waited
        // on, per replicate. Binding (`s35_multi_vs_one_cell_barrier`).
        m.set("s35:multi_vs_one_cell_barrier_x", median(&mut ratio_barrier_rep));
        // The client-visible ratio at the same shape, per replicate —
        // disclosed (informational since the 2026-08-22 amendment): at
        // K ≥ 2 the 4-cell leg runs 8 conns per cell at 2.2 acks per
        // frame against a 1-cell leg at 32 conns and ~20 acks per frame,
        // so the ratio carries the pipeline's own seal wait (~0.2 ×) on
        // top of the contention term it was written to catch.
        m.set("s35:multi_vs_one_cell_p50_x", median(&mut ratio_client_rep));
        let spread = |v: &[f64]| {
            let lo = v.iter().cloned().fold(f64::INFINITY, f64::min);
            let hi = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            format!("{lo:.3}–{hi:.3}")
        };
        m.note(format!(
            "s35 4c/1c per-replicate ratios (median of {}; spread): barrier {} · client p50 {} \
             — client histogram 256 sub-buckets/octave (≈ 0.4 %, 2 µs at 512–1024 µs); the \
             barrier p50 is the server's 32-sub-bucket histogram (≈ 3 %, 16 µs at that octave)",
            ratio_barrier_rep.len(),
            spread(&ratio_barrier_rep),
            spread(&ratio_client_rep)
        ));
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
    if engine_tail_legs > 0 {
        m.note(format!(
            "s35: {engine_tail_legs} AC leg(s) saw a client p99 above 10× the device barrier \
             p99 — a tail the device does not explain (write stall, parking, or a drive-state \
             mode the barrier histogram missed); the leg's write_stall_p99_us is in the raw \
             rows; do not cite without attributing it"
        ));
    }
    m.note(format!(
        "s35 row: flat always, no fill (the AC leg runs first on a fresh server so its \
         barrier histogram holds only its own frames), {FILL_KEYS}-key space × 1 KiB, \
         {duration}s legs, median of {replicates}; AC leg {S35_CONNS_AC} conns pipeline 1 on \
         {cells} cells then on 1 cell (interleaved per replicate); max leg {CONNS_HIGH} conns; \
         read leg 64 conns × P16 100% GET over the keys the write legs populated (nils \
         disclosed); {idle_s}s idle \
         before every durable leg; barrier = {p50_key} (cell median, whole-session \
         histogram); device tail = {p99_key} (worst cell); 4c/1c ratios are medians of \
         per-replicate ratios — the barrier ratio binds (F2's contention term), the client \
         ratio is informational (ADR-0087 D8, amended 2026-08-22)"
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

/// The S36 row's offered rate when `--offered-ops` is absent: the
/// same-drive comparator median of the 2026-08-20 leg A KV 100 %-write
/// row (disclosed in the report; the ledger names the run it matched).
const S36_DEFAULT_OFFERED_OPS: u64 = 100_000;

/// One S36 write leg's facts.
struct S36Leg {
    ops_per_sec: f64,
    p99_us: f64,
    max_us: f64,
    /// Server CPU across the leg, percent of one core (400 = four cores
    /// flat out).
    cpu_pct: f64,
    parked: u64,
    write_stall_p99_us: u64,
}

/// The M4.5-S36 device-budget row (ADR-0088 D7/D9). On the device root:
/// a fresh server with the campaign's arms → one flat `everysec`
/// namespace → the leg A closed-loop 100 %-write leg (32 conns, 1 KiB)
/// for `2 × duration` with the server's CPU sampled across it (the
/// pure-write tripwire: an engine below 300 % of 400 % is waiting on its
/// device) and the write-amplification figure scraped at its end
/// (checkpoints run during it: `2 × duration` of ~100 MB/s is several
/// derived intervals) → after the drive-state idle, the S27 D5 `max`
/// leg at the comparator-matched **offered** rate (`--offered-ops`,
/// pipeline 16, latency from the intended send instant). Then the same
/// closed-loop leg on a **tmpfs control** server (the one memory-fs
/// root the admission rule exempts, labelled): the 0.85× denominator.
fn s36_device_budget_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let offered = flags.u64_or("offered-ops", S36_DEFAULT_OFFERED_OPS)?;
    let control_root =
        flags.get("tmpfs-control-root").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let control_fstype =
        crate::gaterun::fs_type_of(&control_root).unwrap_or_else(|| "unknown".to_string());
    let mut raw = String::new();

    // ---- device arm: the CPU/throughput leg, the write-amp scrape, the
    // offered-rate max leg.
    let dir = format!("{data_root}/s36-device");
    s35_idle(idle_s, &format!("s36 device leg ({cells} cells)"));
    let (server, port) = s36_spawn(flags, infinityd, cells, &dir, None)?;
    let device = s36_write_leg(&server, port, cells, 2 * duration, None)?;
    raw.push_str(&format!(
        "device closed-loop c32 everysec ops/s={:<8.0} p99_us={:<7.0} max_us={:<8.0} server_cpu_pct={:<5.0} parked={} write_stall_p99_us={}
",
        device.ops_per_sec,
        device.p99_us,
        device.max_us,
        device.cpu_pct,
        device.parked,
        device.write_stall_p99_us
    ));
    let infos = scrape_cells(port, cells)?;
    let undefined = crate::gaterun::max_field(&infos, "write_amp_log_checkpoint_undefined");
    let write_amp = crate::gaterun::max_field(&infos, "write_amp_milli_log_checkpoint");
    let ckpts = sum_field(&infos, "ckpts_completed");
    let model_absent =
        infos.iter().any(|c| c.get("io_budget_model").is_some_and(|v| v == "absent"));
    // The decomposition (ADR-0088 D9 amended by the first campaign): the
    // checkpoint term S36 owns — checkpoint + MANIFEST bytes over log
    // frame bytes, design bound 1/α = 0.5 — and the v3 padding share of
    // the log, S34's disclosed cost, which at small groups dominates the
    // combined figure (2.2 records per 4 KiB frame at K = 3 × 32 conns).
    let log_frame_bytes = sum_field(&infos, "log_frame_bytes");
    let ckpt_and_manifest =
        sum_field(&infos, "ckpt_bytes_total") + sum_field(&infos, "manifest_bytes_total");
    let padding = sum_field(&infos, "log_padding_bytes");
    let ckpt_over_log_milli =
        (ckpt_and_manifest as f64 * 1000.0 / log_frame_bytes.max(1) as f64).ceil();
    let padding_pct = padding as f64 * 100.0 / log_frame_bytes.max(1) as f64;
    raw.push_str(&format!(
        "device arm: io_budget_model={} ckpts_completed={ckpts} \
         write_amp_milli_log_checkpoint={write_amp} (undefined={undefined}) \
         checkpoint_over_log_milli={ckpt_over_log_milli:.0} log_padding_pct={padding_pct:.1} \
         log_frame_bytes={log_frame_bytes} ckpt_bytes_total={} zero_fill_bytes={} \
         ckpt_interval_bytes(max)={} deferrals[zero_fill={} tier_flush={} checkpoint={}] \
         frame_waits_pace={}\n",
        if model_absent { "absent" } else { "probed" },
        sum_field(&infos, "ckpt_bytes_total"),
        sum_field(&infos, "zero_fill_bytes"),
        crate::gaterun::max_field(&infos, "ckpt_interval_bytes"),
        sum_field(&infos, "io_budget_deferrals_zero_fill"),
        sum_field(&infos, "io_budget_deferrals_tier_flush"),
        sum_field(&infos, "io_budget_deferrals_checkpoint"),
        sum_field(&infos, "frame_waits_pace"),
    ));
    if undefined == 0 && ckpts > 0 {
        m.set("s36:write_amp_milli_log_checkpoint", write_amp as f64);
        m.set("s36:checkpoint_over_log_milli", ckpt_over_log_milli);
        m.set("s36:log_padding_pct", padding_pct);
    } else {
        m.note(
            "s36: write_amp_milli_log_checkpoint is UNDEFINED (no checkpoint published during              the leg) — the gate reads PENDING (ADR-0060 D3), never PASS",
        );
    }
    if model_absent {
        m.note(
            "s36: the device model is ABSENT on the device arm (no schema-2 io-properties.toml              and no --device-write-mbps): background I/O unbudgeted — this is the pre-S36              baseline arm, not the budgeted one",
        );
    }
    s35_idle(idle_s, "s36 offered-rate leg");
    let paced = s36_write_leg(&server, port, cells, duration, Some(offered))?;
    raw.push_str(&format!(
        "device offered-rate c32 P16 target={offered} everysec achieved ops/s={:<8.0} p99_us={:<7.0} max_us={:<8.0} server_cpu_pct={:<5.0} parked={}
",
        paced.ops_per_sec, paced.p99_us, paced.max_us, paced.cpu_pct, paced.parked
    ));
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);

    // ---- tmpfs control arm (the denominator).
    let control_dir = control_root.join(format!("inf-s36-control-{}", std::process::id()));
    let control_dir_s = control_dir.to_string_lossy().into_owned();
    let (server, port) = s36_spawn(flags, infinityd, cells, &control_dir_s, Some("flush"))?;
    let control = s36_write_leg(&server, port, cells, duration, None)?;
    raw.push_str(&format!(
        "tmpfs control ({control_fstype}) closed-loop c32 everysec ops/s={:<8.0} p99_us={:<7.0} max_us={:<8.0} server_cpu_pct={:<5.0}
",
        control.ops_per_sec, control.p99_us, control.max_us, control.cpu_pct
    ));
    drop(server);
    let _ = std::fs::remove_dir_all(&control_dir);

    m.set("s36:everysec_ops_per_sec", device.ops_per_sec);
    m.set("s36:everysec_max_us", device.max_us);
    m.set("s36:server_cpu_pct", device.cpu_pct);
    m.set("s36:everysec_tmpfs_ops_per_sec", control.ops_per_sec);
    m.set("s36:everysec_vs_tmpfs_x", device.ops_per_sec / control.ops_per_sec.max(1.0));
    m.set("s36:max_ms_offered_rate", paced.max_us / 1000.0);
    m.set("s36:offered_rate_achieved_x", paced.ops_per_sec / offered.max(1) as f64);
    m.set("s36:offered_ops", offered as f64);
    if paced.ops_per_sec < 0.9 * offered as f64 {
        m.note(format!(
            "s36: the offered-rate leg achieved {:.0} of {offered} ops/s (< 90 %) — the server              could not absorb the offered rate; its max is a saturation number, disclosed",
            paced.ops_per_sec
        ));
    }
    if !crate::gaterun::is_memory_fs(&control_fstype) {
        m.note(format!(
            "s36: WARNING — the tmpfs control root is {control_fstype}, not a memory filesystem;              the 0.85× denominator is not the device-free arm it is meant to be"
        ));
    }
    m.note(format!(
        "s36 row: flat everysec, {FILL_KEYS}-key space × 1 KiB, 32 conns; device leg          {}s closed-loop with server CPU from /proc/<pid>/stat ({} ticks/s); write-amp scraped          at its end; offered-rate leg {duration}s at {offered} ops/s (pipeline 16, latency from          the intended send); tmpfs control {duration}s on {} ({control_fstype}, flush class);          {idle_s}s idle before every device leg",
        2 * duration,
        crate::gaterun::CLOCK_TICKS_PER_S,
        control_root.display()
    ));
    m.row_open("device-budget");
    m.row_write_amp(
        "measured by this row as write_amp_milli_log_checkpoint (ADR-0088 D7: log frames +          checkpoint + MANIFEST bytes over encoded record bytes, cell scope, boot life; zero-fill          disclosed beside it) — the tier figure stays the M4 S16 row's",
    );
    m.raw_section("s36 per-leg samples", &raw);
    Ok(())
}

/// Spawns the S36 row's server: the campaign's arms, plus a barrier-
/// class override for the tmpfs control (FUA on tmpfs is a memcpy; the
/// control runs the flush class and says so).
fn s36_spawn(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    barrier_override: Option<&str>,
) -> Result<(crate::gaterun::ServerGuard, u16), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    // The device model (ADR-0088 D6): the campaign root's probe file is
    // copied into the device arm's fresh dir; the tmpfs control never
    // gets it (a memory fs has no device to model) and runs unpaced.
    let probe_present = barrier_override.is_none()
        && crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    let mut arms = crate::m2rows::pipeline_args(flags);
    if barrier_override.is_none() {
        arms.extend(crate::m2rows::seal_pace_args(flags, probe_present));
    }
    if let Some(class) = barrier_override {
        if let Some(at) = arms.iter().position(|a| a == "--barrier-class") {
            arms.drain(at..at + 2);
        }
        arms.push("--barrier-class".into());
        arms.push(class.to_string());
    }
    extra.extend(arms);
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    create_ns(
        port,
        &[b"INF.NS", b"CREATE", b"s36esec", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    await_fan(port, "s36esec", cells)?;
    Ok((server, port))
}

/// One S36 100 %-write leg (32 conns, 1 KiB) — closed loop, or at an
/// offered rate with pipeline 16 — with the server's CPU sampled across
/// it and the parked/stall counters scraped.
fn s36_write_leg(
    server: &crate::gaterun::ServerGuard,
    port: u16,
    cells: u16,
    duration: u64,
    offered: Option<u64>,
) -> Result<S36Leg, String> {
    let before = scrape_cells(port, cells)?;
    let ticks_before = crate::gaterun::cpu_ticks_of(server.pid());
    let wall = Instant::now();
    let report = run_load(&LoadSpec {
        port,
        conns: 32,
        pipeline: if offered.is_some() { 16 } else { 1 },
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: "s36esec:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s36esec".to_vec()]],
        target_ops_per_sec: offered,
        ..LoadSpec::default()
    })?;
    let elapsed = wall.elapsed().as_secs_f64().max(1e-9);
    let ticks = crate::gaterun::cpu_ticks_of(server.pid()).saturating_sub(ticks_before);
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s36 leg: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let after = scrape_cells(port, cells)?;
    Ok(S36Leg {
        ops_per_sec: report.ops_per_sec,
        p99_us: report.p99_us as f64,
        max_us: report.max_us as f64,
        cpu_pct: ticks as f64 / crate::gaterun::CLOCK_TICKS_PER_S as f64 / elapsed * 100.0,
        parked: sum_field(&after, "log_admission_parked_total")
            .saturating_sub(sum_field(&before, "log_admission_parked_total")),
        write_stall_p99_us: crate::gaterun::max_field(&after, "log_write_stall_p99_us"),
    })
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
    // ADR-0088 D6/D2b: the S35 row carries the device model and the
    // seal-pace arm too (the @256 A/B the pacer was designed for).
    let probe_present = crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    extra.extend(crate::m2rows::seal_pace_args(flags, probe_present));
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
    let before = scrape_cells(port, cells)?;
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
    let acks = sum_field(&infos, "acks_gated").saturating_sub(sum_field(&before, "acks_gated"));
    let fsyncs = sum_field(&infos, "fsyncs_completed")
        .saturating_sub(sum_field(&before, "fsyncs_completed"));
    let frames = sum_field(&infos, "log_frames_queued")
        .saturating_sub(sum_field(&before, "log_frames_queued"));
    let parked = sum_field(&infos, "log_admission_parked_total")
        .saturating_sub(sum_field(&before, "log_admission_parked_total"));
    let frame_bytes =
        sum_field(&infos, "log_frame_bytes").saturating_sub(sum_field(&before, "log_frame_bytes"));
    let padding = sum_field(&infos, "log_padding_bytes")
        .saturating_sub(sum_field(&before, "log_padding_bytes"));
    let waits_fill = sum_field(&infos, "frame_waits_fill")
        .saturating_sub(sum_field(&before, "frame_waits_fill"));
    // M4.5-S43: the group hold's engagement (a pre-S43 binary reports 0).
    let waits_group = sum_field(&infos, "frame_waits_group")
        .saturating_sub(sum_field(&before, "frame_waits_group"));
    let round_target = sum_field(&infos, "group_round_target");
    let mut p50s: Vec<f64> =
        infos.iter().filter_map(|c| c.get(p50_key).and_then(|v| v.parse::<f64>().ok())).collect();
    if p50s.is_empty() {
        return Err(format!("s35: INFO field {p50_key} absent — pre-S34 binary?"));
    }
    Ok(S35Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        mean_us: report.mean_us,
        p99_us: report.p99_us as f64,
        max_us: report.max_us as f64,
        barrier_p50_us: median(&mut p50s),
        barrier_p99_us: crate::gaterun::max_field(&infos, p99_key) as f64,
        frames_in_flight_max: crate::gaterun::max_field(&infos, "frames_in_flight_max"),
        acks_per_fsync: if fsyncs > 0 { acks as f64 / fsyncs as f64 } else { 0.0 },
        frames,
        parked,
        write_stall_p99_us: crate::gaterun::max_field(&infos, "log_write_stall_p99_us"),
        padding_pct: padding as f64 * 100.0 / frame_bytes.max(1) as f64,
        waits_fill,
        waits_group,
        round_target,
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

// ---- M4.5-S39b: segment recycling (ADR-0090 D6 as amended) -----------------

/// Default closed-loop write-leg length of the S39b row (seconds): at
/// ~18–25 MB/s of frames per cell and the product's 256 MiB segments
/// it yields ≥ 8 rotations per cell after the first generation
/// (ADR-0090 D6: "the row runs 150 s"); `--duration` overrides.
const S39B_DEFAULT_DURATION_S: u64 = 150;

/// Segment size the S39b row runs at unless `--segment-bytes` says
/// otherwise: the product default. Smaller segments do not measure the
/// mechanism at this row's dataset (ADR-0090 A5): the derived checkpoint
/// trigger is `max(2 × dataset, floor)` ≈ 100 MB per cell here, so an
/// 8 MiB segment sees truncation in bursts of ~12 per checkpoint and a
/// 1-slot pool serves one of them (the smoke read 0.64 warmed share) —
/// a row fact, not a mechanism fact. `--ckpt-interval-bytes` defaults
/// to the segment size (the floor; the α term binds either way).
const S39B_DEFAULT_SEGMENT_BYTES: u64 = 256 << 20; // = inf_log::DEFAULT_SEGMENT_BYTES (inf-bench is zero-dep)

/// One counter snapshot of every cell (summed) plus the block device's
/// sectors-written, taken at a named instant of the leg.
#[derive(Clone, Debug, Default)]
struct S39bSnap {
    at_s: f64,
    zero_fill: u64,
    frame_bytes: u64,
    padding: u64,
    host_bytes: u64,
    ckpt_bytes: u64,
    recycled: u64,
    rotations: u64,
    misses: u64,
    fallbacks: u64,
    pool_full: u64,
    truncated: u64,
    /// ADR-0090 D9: the pool wait's three outcomes and the two MAINTAIN
    /// facts the wait must never move (a rotation onto an un-zeroed
    /// segment, a rotation that found no next segment), plus ENOSPC.
    waits_started: u64,
    waits_satisfied: u64,
    waits_expired: u64,
    rotations_unzeroed: u64,
    inline_preallocs: u64,
    prealloc_failures: u64,
    /// Minimum over cells — the per-cell validity facts.
    rotations_min_cell: u64,
    truncated_min_cell: u64,
    /// Maximum over cells of `rotations − recycled` at this instant.
    deficit_max_cell: u64,
    device_sectors_written: u64,
}

fn s39b_snap(
    port: u16,
    cells: u16,
    device_stat: Option<&str>,
    t0: Instant,
) -> Result<S39bSnap, String> {
    let infos = scrape_cells(port, cells)?;
    let per = |field: &str| -> Vec<u64> {
        infos.iter().map(|c| c.get(field).and_then(|v| v.parse().ok()).unwrap_or(0)).collect()
    };
    let rotations = per("segment_rotations");
    let recycled = per("segments_recycled");
    let truncated = per("segments_truncated");
    Ok(S39bSnap {
        at_s: t0.elapsed().as_secs_f64(),
        zero_fill: sum_field(&infos, "zero_fill_bytes"),
        frame_bytes: sum_field(&infos, "log_frame_bytes"),
        padding: sum_field(&infos, "log_padding_bytes"),
        host_bytes: sum_field(&infos, "accounted_host_write_bytes"),
        ckpt_bytes: sum_field(&infos, "ckpt_bytes_total"),
        recycled: recycled.iter().sum(),
        rotations: rotations.iter().sum(),
        misses: sum_field(&infos, "recycle_misses"),
        fallbacks: sum_field(&infos, "recycle_fallbacks"),
        pool_full: sum_field(&infos, "recycle_pool_full"),
        truncated: truncated.iter().sum(),
        waits_started: sum_field(&infos, "recycle_waits_started"),
        waits_satisfied: sum_field(&infos, "recycle_waits_satisfied"),
        waits_expired: sum_field(&infos, "recycle_waits_expired"),
        rotations_unzeroed: sum_field(&infos, "rotations_unzeroed"),
        inline_preallocs: sum_field(&infos, "segment_inline_preallocs"),
        prealloc_failures: sum_field(&infos, "segment_prealloc_failures"),
        rotations_min_cell: rotations.iter().copied().min().unwrap_or(0),
        truncated_min_cell: truncated.iter().copied().min().unwrap_or(0),
        deficit_max_cell: rotations
            .iter()
            .zip(&recycled)
            .map(|(r, c)| r.saturating_sub(*c))
            .max()
            .unwrap_or(0),
        device_sectors_written: device_stat.map_or(0, device_sectors_written),
    })
}

/// Sectors written on `/sys/block/<dev>/stat` (field 7) — host-to-device
/// traffic including journal and metadata writes, still blind to NAND
/// amplification (ADR-0090 A3: disclosed as such). 0 when unreadable.
fn device_sectors_written(dev: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/block/{dev}/stat"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(6).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

/// One S39b leg's facts: the whole-leg client figures, the first-
/// generation snapshot (cumulative at the trigger) and the warmed deltas
/// (end − trigger), the read leg, and the crash-restart recovery time.
struct S39bLeg {
    arm: &'static str,
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    max_us: f64,
    barrier_p50_us: f64,
    barrier_p99_us: f64,
    parked: u64,
    read_ops_per_sec: f64,
    first_gen: S39bSnap,
    end: S39bSnap,
    /// `None` when the trigger never fired (the leg was too short for
    /// a truncation + rotation on every cell — the row is then invalid).
    warmed: bool,
    /// Crash-restart recovery: exactly one first boot of the fresh crashed
    /// image, after the row's drive-state idle (`--leg-idle-s`). A second
    /// boot would time an image recovery already classified and cleaned.
    recovery_s: f64,
    recover_residue_slacks: u64,
    recover_residue_stops: u64,
}

impl S39bLeg {
    fn d(&self, f: impl Fn(&S39bSnap) -> u64) -> u64 {
        f(&self.end).saturating_sub(f(&self.first_gen))
    }
    fn warmed_zero_fill_share(&self) -> f64 {
        self.d(|s| s.zero_fill) as f64 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn first_gen_zero_fill_share(&self) -> f64 {
        self.first_gen.zero_fill as f64 / self.first_gen.frame_bytes.max(1) as f64
    }
    fn warmed_padding_pct(&self) -> f64 {
        self.d(|s| s.padding) as f64 * 100.0 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn warmed_host_per_log(&self) -> f64 {
        self.d(|s| s.host_bytes) as f64 / self.d(|s| s.frame_bytes).max(1) as f64
    }
    fn warmed_device_per_log(&self) -> f64 {
        (self.d(|s| s.device_sectors_written) * 512) as f64
            / self.d(|s| s.frame_bytes).max(1) as f64
    }
    /// Waits ended `satisfied` over waits ended at all, warmed window
    /// (ADR-0090 A9: "waits end predominantly satisfied"); `None` when no
    /// wait ended (the wait-off arm, or recycling off).
    fn warmed_wait_satisfied_share(&self) -> Option<f64> {
        let ended = self.d(|s| s.waits_satisfied) + self.d(|s| s.waits_expired);
        (ended > 0).then(|| self.d(|s| s.waits_satisfied) as f64 / ended as f64)
    }
    fn warmed_deficit(&self) -> u64 {
        // `rotations − recycled` over the warmed window, worst cell: the
        // end snapshot's worst-cell deficit less what the first
        // generation owed by construction (its rotations were never
        // recyclable).
        self.end.deficit_max_cell.saturating_sub(self.first_gen.deficit_max_cell)
    }
}

/// Spawns the S39b row's server on `dir` (fresh unless `reuse`) with the
/// row's segment size, checkpoint floor and the arm's recycling knob, and
/// — fresh only — creates + fans the flat `always` namespace.
fn s39b_spawn(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
    reuse: bool,
) -> Result<(crate::gaterun::ServerGuard, u16), String> {
    if !reuse {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    }
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    let ckpt_floor = flags.u64_or("ckpt-interval-bytes", segment_bytes)?;
    let mut extra: Vec<String> = vec![
        "--data-dir".into(),
        dir.to_string(),
        "--segment-bytes".into(),
        segment_bytes.to_string(),
        "--ckpt-interval-bytes".into(),
        ckpt_floor.to_string(),
    ];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    extra.extend(arm_args.iter().map(|s| (*s).to_string()));
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    if !reuse {
        create_ns(
            port,
            &[b"INF.NS", b"CREATE", b"s39alw", b"MODE", b"durable", b"FSYNC", b"always"],
        )?;
        await_fan(port, "s39alw", cells)?;
    }
    Ok((server, port))
}

/// Crash-restart recovery time: the server is SIGKILLed (the guard's
/// drop), respawned on the same data dir, and timed to `loading:0` on
/// every cell (the recycled log's residue is what the reader meets).
fn s39b_recovery(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
) -> Result<(f64, u64, u64), String> {
    let t0 = Instant::now();
    let (mut server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, true)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = server.try_exited() {
            return Err(format!("s39b: server exited during recovery ({status})"));
        }
        if let Ok(infos) = scrape_cells(port, cells)
            && sum_field(&infos, "loading") == 0
        {
            let elapsed = t0.elapsed().as_secs_f64();
            let slacks = sum_field(&infos, "recover_recycled_residue_slacks");
            let stops = sum_field(&infos, "recover_segment_residue_stops");
            return Ok((elapsed, slacks, stops));
        }
        if Instant::now() >= deadline {
            return Err("s39b: recovery never reached loading:0 within 120 s".into());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One S39b leg: the 32-conn closed-loop `always` write leg polled every
/// second for the first-generation trigger (every cell truncated ≥ 1
/// and rotated ≥ 2 — uniform across arms, independent of recycling),
/// the end snapshot, a read leg, then the crash-restart recovery timing.
#[allow(clippy::too_many_arguments)]
fn s39b_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    idle_s: u64,
    dir: &str,
    arm: &'static str,
    arm_args: &[&str],
    device_stat: Option<&str>,
) -> Result<S39bLeg, String> {
    let (server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, false)?;
    let t0 = Instant::now();
    let spec = LoadSpec {
        port,
        conns: S35_CONNS_AC,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys: FILL_KEYS,
        key_prefix: "s39alw:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    };
    let before = s39b_snap(port, cells, device_stat, t0)?;
    let load = std::thread::spawn(move || run_load(&spec));
    // The poller: one INFO scrape per second until the trigger fires.
    let mut first_gen: Option<S39bSnap> = None;
    while !load.is_finished() {
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(1000));
        if first_gen.is_none()
            && let Ok(snap) = s39b_snap(port, cells, device_stat, t0)
            && snap.truncated_min_cell >= 1
            && snap.rotations_min_cell >= 2
        {
            first_gen = Some(snap);
        }
    }
    let report = load.join().map_err(|_| "s39b: load thread panicked".to_string())??;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s39b {arm}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let end = s39b_snap(port, cells, device_stat, t0)?;
    let infos = scrape_cells(port, cells)?;
    let mut p50s: Vec<f64> = infos
        .iter()
        .filter_map(|c| c.get("fua_latency_p50_us").and_then(|v| v.parse::<f64>().ok()))
        .collect();
    if p50s.is_empty() {
        return Err("s39b: INFO field fua_latency_p50_us absent — pre-S34 binary?".into());
    }
    let parked = sum_field(&infos, "log_admission_parked_total");
    let warmed = first_gen.is_some();
    let first_gen = first_gen.unwrap_or_else(|| before.clone());
    // Read leg (the ±2 % non-regression AC of every E4.7 story).
    let read = run_load(&LoadSpec {
        port,
        conns: 64,
        pipeline: 16,
        duration: Duration::from_secs(duration.min(10)),
        warmup: Duration::from_secs(1),
        set_weight: 0,
        get_weight: 1,
        keys: FILL_KEYS,
        key_prefix: "s39alw:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    })?;
    // Crash (SIGKILL), leave the fresh image untouched during the
    // drive-state idle, then time its first and only recovery boot.
    drop(server);
    s35_idle(idle_s, &format!("s39b {arm} fresh-image recovery boot"));
    let (recovery_s, recover_residue_slacks, recover_residue_stops) =
        s39b_recovery(flags, infinityd, cells, dir, arm_args)?;
    Ok(S39bLeg {
        arm,
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        max_us: report.max_us as f64,
        barrier_p50_us: median(&mut p50s),
        barrier_p99_us: crate::gaterun::max_field(&infos, "fua_latency_p99_us") as f64,
        parked,
        read_ops_per_sec: read.ops_per_sec,
        first_gen,
        end,
        warmed,
        recovery_s,
        recover_residue_slacks,
        recover_residue_stops,
    })
}

/// The M4.5-S39b segment-recycling row (ADR-0090 D6 as amended): the
/// S35 shape — a flat `always` namespace so zero-fill engages — run long
/// enough for ≥ 8 rotations per cell, baseline (`--no-segment-recycle`)
/// against the arm (`--segment-recycle-slots N`, row default 1), interleaved
/// per replicate (ABBA), every counter snapshotted at the end of the
/// first generation and at the end of the leg so the warmed figures are
/// **deltas**; the block device's sectors-written sampled beside the
/// accounted host bytes; a read leg and a crash-restart recovery timing
/// per leg. Gates bind on the warmed zero-fill share, the per-cell
/// recycle deficit, the padding control (S39c's term untouched), the
/// `always` p50/p99 and read ratios, and the recovery-time ratio.
fn s39b_recycle_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let slots = flags.u64_or("segment-recycle-slots", 1)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    let class = flags.str_or("barrier-class", "flush");
    if class != "fua" {
        return Err("s39b: the row needs --barrier-class fua (recycling is a Direct-class \
                    mechanism; under FLUSH nothing is pre-zeroed and nothing recycles)"
            .into());
    }
    let slots_arg = slots.to_string();
    // ADR-0090 A9: the arm's pool wait (`--recycle-wait`, the server's
    // default when absent) and which baseline the row pairs it with —
    // `recycle-off` (D6: `--no-segment-recycle`) or `wait-off` (D9: the
    // same pool bound with `--recycle-wait off`, the causal A/B for the
    // wait alone).
    let wait = flags.get("recycle-wait").map(str::to_string);
    let baseline = flags.str_or("s39b-baseline", "recycle-off");
    let mut arm_args: Vec<&str> = vec!["--segment-recycle-slots", &slots_arg];
    if let Some(wait) = wait.as_deref() {
        arm_args.extend(["--recycle-wait", wait]);
    }
    let base_args: Vec<&str> = match baseline.as_str() {
        "recycle-off" => vec!["--no-segment-recycle"],
        "wait-off" => vec!["--segment-recycle-slots", &slots_arg, "--recycle-wait", "off"],
        other => {
            return Err(format!("s39b: --s39b-baseline {other}: expected recycle-off|wait-off"));
        }
    };
    m.note(format!(
        "s39b row: {cells} cells · {replicates} replicates (ABBA) · write leg {duration} s at \
         {S35_CONNS_AC} conns · segment-bytes {segment_bytes} · ckpt floor {} · arm \
         --segment-recycle-slots {slots} --recycle-wait {} vs baseline {} · device stat {} · \
         first-generation trigger: every cell truncated ≥ 1 and rotated ≥ 2",
        flags.u64_or("ckpt-interval-bytes", segment_bytes)?,
        wait.as_deref().unwrap_or("(server default)"),
        base_args.join(" "),
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, S39bLeg)> = Vec::new();
    for rep in 0..replicates {
        let order: [(&'static str, &[&str]); 2] = if rep % 2 == 0 {
            [("base", &base_args), ("arm", &arm_args)]
        } else {
            [("arm", &arm_args), ("base", &base_args)]
        };
        for (arm, args) in order {
            let dir = format!("{data_root}/s39b-{arm}-rep{rep}");
            s35_idle(idle_s, &format!("s39b {arm} rep{rep}"));
            let leg = s39b_leg(
                flags,
                infinityd,
                cells,
                duration,
                idle_s,
                &dir,
                arm,
                args,
                device_stat.as_deref(),
            )?;
            raw.push_str(&format!(
                "rep{rep} {arm} c32 ops={:.0} p50_us={:.0} p99_us={:.0} max_us={:.0} \
                 barrier_p50_us={:.0} barrier_p99_us={:.0} parked={} read_ops={:.0} \
                 rotations={} recycled={} misses={} fallbacks={} pool_full={} truncated={} \
                 waits[started={} satisfied={} expired={}] rotations_unzeroed={} \
                 inline_preallocs={} prealloc_failures={} \
                 rotations_min_cell={} trigger_at_s={:.1} warmed={} \
                 firstgen[zero_fill={} frame_bytes={} share={:.3}] \
                 warmed[zero_fill={} frame_bytes={} share={:.3} padding_pct={:.1} \
                 host_per_log={:.3} device_per_log={:.3} ckpt={} deficit={}] \
                 recovery_first_boot_s={:.3} recover_residue_slacks={} \
                 recover_residue_stops={}\n",
                leg.ops_per_sec,
                leg.p50_us,
                leg.p99_us,
                leg.max_us,
                leg.barrier_p50_us,
                leg.barrier_p99_us,
                leg.parked,
                leg.read_ops_per_sec,
                leg.end.rotations,
                leg.end.recycled,
                leg.end.misses,
                leg.end.fallbacks,
                leg.end.pool_full,
                leg.end.truncated,
                leg.end.waits_started,
                leg.end.waits_satisfied,
                leg.end.waits_expired,
                leg.end.rotations_unzeroed,
                leg.end.inline_preallocs,
                leg.end.prealloc_failures,
                leg.end.rotations_min_cell,
                leg.first_gen.at_s,
                leg.warmed,
                leg.first_gen.zero_fill,
                leg.first_gen.frame_bytes,
                leg.first_gen_zero_fill_share(),
                leg.d(|s| s.zero_fill),
                leg.d(|s| s.frame_bytes),
                leg.warmed_zero_fill_share(),
                leg.warmed_padding_pct(),
                leg.warmed_host_per_log(),
                leg.warmed_device_per_log(),
                leg.d(|s| s.ckpt_bytes),
                leg.warmed_deficit(),
                leg.recovery_s,
                leg.recover_residue_slacks,
                leg.recover_residue_stops,
            ));
            println!("  s39b rep{rep} {arm}: {}", raw.lines().last().unwrap_or(""));
            let _ = std::fs::remove_dir_all(&dir);
            legs.push((rep, leg));
        }
    }
    // Per-replicate pairs (the same rep's base and arm ran back to back).
    let pair = |rep: usize, arm: &str| legs.iter().find(|(r, l)| *r == rep && l.arm == arm);
    let mut zf_arm = Vec::new();
    let mut zf_base = Vec::new();
    let mut zf_first_arm = Vec::new();
    let mut deficit = Vec::new();
    let mut pad_delta = Vec::new();
    let mut pad_base = Vec::new();
    let mut pad_arm = Vec::new();
    let mut p50_x = Vec::new();
    let mut p99_x = Vec::new();
    let mut barrier_x = Vec::new();
    let mut ops_x = Vec::new();
    let mut read_x = Vec::new();
    let mut rec_x = Vec::new();
    let mut host_arm = Vec::new();
    let mut host_base = Vec::new();
    let mut dev_arm = Vec::new();
    let mut dev_base = Vec::new();
    let mut rot_min = Vec::new();
    let mut wait_share = Vec::new();
    let mut unzeroed_arm = Vec::new();
    let mut inline_arm = Vec::new();
    let mut nospace_arm = Vec::new();
    let mut invalid = 0usize;
    for rep in 0..replicates {
        let (Some((_, b)), Some((_, a))) = (pair(rep, "base"), pair(rep, "arm")) else { continue };
        if !a.warmed || !b.warmed {
            invalid += 1;
            continue;
        }
        zf_arm.push(a.warmed_zero_fill_share());
        zf_base.push(b.warmed_zero_fill_share());
        zf_first_arm.push(a.first_gen_zero_fill_share());
        deficit.push(a.warmed_deficit() as f64);
        pad_base.push(b.warmed_padding_pct());
        pad_arm.push(a.warmed_padding_pct());
        pad_delta.push((a.warmed_padding_pct() - b.warmed_padding_pct()).abs());
        p50_x.push(a.p50_us / b.p50_us.max(1.0));
        p99_x.push(a.p99_us / b.p99_us.max(1.0));
        barrier_x.push(a.barrier_p50_us / b.barrier_p50_us.max(1.0));
        ops_x.push(a.ops_per_sec / b.ops_per_sec.max(1.0));
        read_x.push(a.read_ops_per_sec / b.read_ops_per_sec.max(1.0));
        rec_x.push(a.recovery_s / b.recovery_s.max(1e-6));
        host_arm.push(a.warmed_host_per_log());
        host_base.push(b.warmed_host_per_log());
        dev_arm.push(a.warmed_device_per_log());
        dev_base.push(b.warmed_device_per_log());
        rot_min.push(a.end.rotations_min_cell.min(b.end.rotations_min_cell) as f64);
        if let Some(share) = a.warmed_wait_satisfied_share() {
            wait_share.push(share);
        }
        unzeroed_arm.push(a.end.rotations_unzeroed as f64);
        inline_arm.push(a.end.inline_preallocs as f64);
        nospace_arm.push(a.end.prealloc_failures as f64);
    }
    if invalid > 0 {
        m.note(format!(
            "s39b: {invalid} replicate pair(s) never reached the first-generation trigger on \
             every cell — excluded; lengthen --duration or shrink --segment-bytes"
        ));
    }
    if zf_arm.is_empty() {
        m.note("s39b: no valid replicate pair — every gate key is absent (PENDING)");
    } else {
        m.set("s39b:warmed_zero_fill_share_arm", median(&mut zf_arm));
        m.set("s39b:warmed_zero_fill_share_base", median(&mut zf_base));
        m.set("s39b:first_gen_zero_fill_share_arm", median(&mut zf_first_arm));
        m.set("s39b:recycle_deficit_per_cell", median(&mut deficit));
        m.set("s39b:padding_pct_base", median(&mut pad_base));
        m.set("s39b:padding_pct_arm", median(&mut pad_arm));
        m.set("s39b:padding_delta_pts", median(&mut pad_delta));
        m.set("s39b:always_c32_p50_x", median(&mut p50_x));
        m.set("s39b:always_c32_p99_x", median(&mut p99_x));
        m.set("s39b:barrier_p50_x", median(&mut barrier_x));
        m.set("s39b:always_c32_ops_x", median(&mut ops_x));
        m.set("s39b:read_c64p16_x", median(&mut read_x));
        m.set("s39b:recovery_time_x", median(&mut rec_x));
        m.set("s39b:host_bytes_per_log_byte_arm", median(&mut host_arm));
        m.set("s39b:host_bytes_per_log_byte_base", median(&mut host_base));
        m.set("s39b:device_bytes_per_log_byte_arm", median(&mut dev_arm));
        m.set("s39b:device_bytes_per_log_byte_base", median(&mut dev_base));
        m.set("s39b:rotations_per_cell_min", rot_min.iter().copied().fold(f64::MAX, f64::min));
        // ADR-0090 A9 (the D9 row): the wait's outcome and the three facts
        // it must not move, worst replicate (a max, never a median — one
        // un-zeroed rotation is the falsifier).
        if wait_share.is_empty() {
            m.note("s39b: no pool wait ended on the arm — the wait-satisfied share is absent");
        } else {
            m.set("s39b:wait_satisfied_share_arm", median(&mut wait_share));
        }
        let worst = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
        m.set("s39b:rotations_unzeroed_arm_max", worst(&unzeroed_arm));
        m.set("s39b:inline_preallocs_arm_max", worst(&inline_arm));
        m.set("s39b:prealloc_failures_arm_max", worst(&nospace_arm));
    }
    if device_stat.is_none() {
        m.note("s39b: --device-stat not given — device bytes per log byte read 0 (not sampled)");
    }
    m.row_open("segment-recycling");
    m.row_write_amp(
        "the warmed figures are deltas between the first-generation snapshot and the leg's \
         end (ADR-0090 A4); `host_bytes_per_log_byte` is accounted host bytes (log + zero-fill \
         + checkpoint + MANIFEST) over log frame bytes, `device_bytes_per_log_byte` the block \
         device's sectors-written × 512 over the same — journal and metadata included, NAND \
         amplification not (A3)",
    );
    m.raw_section("s39b per-leg samples", &raw);
    Ok(())
}

// ---- M4.5-S39d: fixed-work recovery attribution (ADR-0090 A10) -------------

/// Warm-phase records per leg unless `--s39d-warm-records` says
/// otherwise: at 1 KiB values and the product's 256 MiB segments ~3 M
/// records are ≈ 3.3 GB of frames across the cells — ≥ 3 rotations per
/// cell at 4 cells, so the first generation truncates and the arm's pool
/// feeds before the checkpoint boundary (the S39b trigger, checked per
/// leg and disclosed).
const S39D_DEFAULT_WARM_RECORDS: u64 = 3_000_000;

/// Tail records applied after the checkpoint boundary (the replay work)
/// unless `--s39d-tail-records` says otherwise: ≈ 220 MB of frames across
/// the cells — inside one segment per cell, so the tail and the slack
/// behind it sit in the segment the arm recycled and the baseline
/// pre-zeroed.
const S39D_DEFAULT_TAIL_RECORDS: u64 = 200_000;

/// One cell's boot decomposed, as `INFO persistence` reports it after
/// `loading:0` (M4.5-S39d; the engine credits the loop clock between
/// consecutive recovery steps to the phase that ran — the µs sum to the
/// total within rounding).
#[derive(Clone, Copy, Debug, Default)]
struct S39dPhases {
    start_us: u64,
    ckpt_us: u64,
    ckpt_bytes: u64,
    replay_us: u64,
    replay_bytes: u64,
    replay_frames: u64,
    audit_us: u64,
    audit_bytes: u64,
    audit_valid_frames: u64,
    audit_foreign_frames: u64,
    finish_us: u64,
    stale_files: u64,
    records: u64,
    total_us: u64,
    residue_slacks: u64,
    residue_stops: u64,
}

impl S39dPhases {
    fn from_info(info: &std::collections::BTreeMap<String, String>) -> S39dPhases {
        let f = |k: &str| info.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
        S39dPhases {
            start_us: f("recover_start_us"),
            ckpt_us: f("recover_ckpt_us"),
            ckpt_bytes: f("recover_ckpt_bytes"),
            replay_us: f("recover_replay_us"),
            replay_bytes: f("recover_replay_bytes"),
            replay_frames: f("recover_replay_frames"),
            audit_us: f("recover_audit_us"),
            audit_bytes: f("recover_audit_bytes"),
            audit_valid_frames: f("recover_audit_valid_frames"),
            audit_foreign_frames: f("recover_audit_foreign_frames"),
            finish_us: f("recover_finish_us"),
            stale_files: f("recover_stale_files_removed"),
            records: f("recover_records"),
            total_us: f("recover_total_us"),
            residue_slacks: f("recover_recycled_residue_slacks"),
            residue_stops: f("recover_segment_residue_stops"),
        }
    }

    /// Durations in pipeline order.
    const fn phase_us(&self) -> [u64; 5] {
        [self.start_us, self.ckpt_us, self.replay_us, self.audit_us, self.finish_us]
    }

    /// The phase that took longest (pipeline order on ties).
    fn dominating(&self) -> &'static str {
        let mut best = 0;
        for (i, us) in self.phase_us().iter().enumerate() {
            if *us > self.phase_us()[best] {
                best = i;
            }
        }
        S39D_PHASE_NAMES[best]
    }
}

const S39D_PHASE_NAMES: [&str; 5] = ["start", "ckpt", "replay", "audit", "finish"];

/// One S39d leg: the fixed work it wrote (identical across arms by
/// construction — the counts are asserted, the bytes disclosed), the
/// warm-phase recycling facts, and the one first boot's decomposition:
/// per cell from `INFO`, the slowest cell (the boot's critical path),
/// the sum over cells (the work), the harness wall to `loading:0`, and
/// the process's device reads from `/proc/<pid>/io`.
struct S39dLeg {
    arm: &'static str,
    warm_ops_per_sec: f64,
    tail_ops_per_sec: f64,
    warm: S39bSnap,
    end: S39bSnap,
    /// Every cell truncated ≥ 1 and rotated ≥ 2 before the boundary.
    warmed: bool,
    boot_wall_s: f64,
    proc_read_bytes: u64,
    cells: Vec<S39dPhases>,
}

impl S39dLeg {
    /// The slowest cell by engine total — recovery runs per cell in
    /// parallel, so the boot's critical path is the max, not the sum.
    fn slowest(&self) -> S39dPhases {
        self.cells.iter().copied().max_by_key(|c| c.total_us).unwrap_or_default()
    }
    fn sum(&self, f: impl Fn(&S39dPhases) -> u64) -> u64 {
        self.cells.iter().map(f).sum()
    }
    fn records_recovered(&self) -> u64 {
        self.sum(|c| c.records)
    }
}

/// `INF.CKPT WAIT` on its own long-timeout connection: the boundary
/// checkpoint streams the whole warm dataset under the device budget,
/// which at the row's size is tens of seconds.
fn s39d_checkpoint_wait(port: u16) -> Result<f64, String> {
    let t0 = Instant::now();
    let mut stream = connect("127.0.0.1", port)?;
    stream.set_read_timeout(Some(Duration::from_secs(900))).map_err(|e| format!("timeout: {e}"))?;
    let reply = request(&mut stream, &[b"INF.CKPT", b"WAIT"])?;
    if !reply.starts_with(b"+OK") {
        return Err(format!("s39d: INF.CKPT WAIT: {}", String::from_utf8_lossy(&reply)));
    }
    Ok(t0.elapsed().as_secs_f64())
}

/// Polls until `segments_truncated` holds still for `settle` (the
/// post-checkpoint truncation slices — what feeds the arm's pool — ran
/// to completion) or `bound` elapses; returns the settled snapshot.
fn s39d_settle_truncation(
    port: u16,
    cells: u16,
    device_stat: Option<&str>,
    t0: Instant,
    settle: Duration,
    bound: Duration,
) -> Result<S39bSnap, String> {
    let deadline = Instant::now() + bound;
    let mut last = s39b_snap(port, cells, device_stat, t0)?;
    let mut still_since = Instant::now();
    loop {
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(250));
        let snap = s39b_snap(port, cells, device_stat, t0)?;
        if snap.truncated != last.truncated || snap.recycled != last.recycled {
            still_since = Instant::now();
        }
        last = snap;
        if still_since.elapsed() >= settle || Instant::now() >= deadline {
            return Ok(last);
        }
    }
}

/// `read_bytes` of `/proc/<pid>/io` — bytes the process caused to be
/// fetched from the storage layer (0 when unreadable; disclosed).
fn proc_read_bytes(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("read_bytes:"))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// Exactly one first boot of the untouched crashed image: wall time to
/// `loading:0` on every cell, the per-cell phase decomposition, and the
/// process's storage reads at that instant.
fn s39d_boot(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    dir: &str,
    arm_args: &[&str],
) -> Result<(f64, u64, Vec<S39dPhases>), String> {
    let t0 = Instant::now();
    let (mut server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = server.try_exited() {
            return Err(format!("s39d: server exited during recovery ({status})"));
        }
        if let Ok(infos) = scrape_cells(port, cells)
            && sum_field(&infos, "loading") == 0
        {
            let wall = t0.elapsed().as_secs_f64();
            let reads = proc_read_bytes(server.pid());
            return Ok((wall, reads, infos.iter().map(S39dPhases::from_info).collect()));
        }
        if Instant::now() >= deadline {
            return Err("s39d: recovery never reached loading:0 within 300 s".into());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One S39d leg (ADR-0090 A10): warm with exactly `warm` records (fill
/// mode — every key once, partitioned, pipelined), force and await the
/// boundary checkpoint, let truncation settle, apply exactly `tail`
/// records and wait for every ack (`always`: acked = durable), SIGKILL,
/// idle the untouched image, boot once.
#[allow(clippy::too_many_arguments)]
fn s39d_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    warm: u64,
    tail: u64,
    idle_s: u64,
    dir: &str,
    arm: &'static str,
    arm_args: &[&str],
    device_stat: Option<&str>,
) -> Result<S39dLeg, String> {
    let (server, port) = s39b_spawn(flags, infinityd, cells, dir, arm_args, false)?;
    let t0 = Instant::now();
    let fill = |prefix: &str, n: u64| LoadSpec {
        port,
        conns: S35_CONNS_AC,
        pipeline: 16,
        fill: Some(n),
        keys: n,
        key_prefix: prefix.to_string(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s39alw".to_vec()]],
        ..LoadSpec::default()
    };
    let check = |report: &crate::load::LoadReport, leg: &str| -> Result<(), String> {
        if report.errors > report.busy_retryable {
            return Err(format!(
                "s39d {arm} {leg}: {} non-BUSY errors (first: {:?})",
                report.errors - report.busy_retryable,
                report.error_samples.first()
            ));
        }
        Ok(())
    };
    let warm_report = run_load(&fill("s39d:", warm))?;
    check(&warm_report, "warm")?;
    let ckpt_s = s39d_checkpoint_wait(port)?;
    let warm_snap = s39d_settle_truncation(
        port,
        cells,
        device_stat,
        t0,
        Duration::from_secs(2),
        Duration::from_secs(60),
    )?;
    let warmed = warm_snap.truncated_min_cell >= 1 && warm_snap.rotations_min_cell >= 2;
    println!(
        "  s39d {arm}: warm {warm} records at {:.0} ops/s, boundary checkpoint {ckpt_s:.1} s, \
         truncated {} recycled {} rotations_min_cell {} (warmed={warmed})",
        warm_report.ops_per_sec,
        warm_snap.truncated,
        warm_snap.recycled,
        warm_snap.rotations_min_cell
    );
    let tail_report = run_load(&fill("s39dt:", tail))?;
    check(&tail_report, "tail")?;
    let end = s39b_snap(port, cells, device_stat, t0)?;
    // Crash (SIGKILL), leave the fresh image untouched during the
    // drive-state idle, then time its first and only recovery boot.
    drop(server);
    s35_idle(idle_s, &format!("s39d {arm} fresh-image recovery boot"));
    let (boot_wall_s, proc_read_bytes, cells_phases) =
        s39d_boot(flags, infinityd, cells, dir, arm_args)?;
    Ok(S39dLeg {
        arm,
        warm_ops_per_sec: warm_report.ops_per_sec,
        tail_ops_per_sec: tail_report.ops_per_sec,
        warm: warm_snap,
        end,
        warmed,
        boot_wall_s,
        proc_read_bytes,
        cells: cells_phases,
    })
}

/// The M4.5-S39d fixed-work recovery row (ADR-0090 A10): baseline
/// (`--no-segment-recycle`) against the arm (the server's default —
/// one slot + the D9 pool wait, or `--segment-recycle-slots` /
/// `--recycle-wait` when given), interleaved per replicate (ABBA), both
/// arms given the same records, the same checkpoint boundary, the same
/// tail, one fresh crashed image, the same idle and exactly one boot.
/// The figure is decomposed: per-phase bytes and loop-clock time on the
/// slowest cell (the critical path) and summed over cells (the work),
/// the arm ÷ baseline ratio per phase, the dominating phase per arm,
/// the process's storage reads, and the fixed-work identities (records
/// recovered equal across arms and equal to the records written). The
/// total ratio is **diagnostic** (≤ 1.05 informational); the absolute
/// first-boot wall on the arm re-reads the S18 < 15 s gate on the
/// recycled log.
fn s39d_recovery_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let warm = flags.u64_or("s39d-warm-records", S39D_DEFAULT_WARM_RECORDS)?;
    let tail = flags.u64_or("s39d-tail-records", S39D_DEFAULT_TAIL_RECORDS)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    let segment_bytes = flags.u64_or("segment-bytes", S39B_DEFAULT_SEGMENT_BYTES)?;
    if flags.str_or("barrier-class", "flush") != "fua" {
        return Err("s39d: the row needs --barrier-class fua (recycling is a Direct-class \
                    mechanism; under FLUSH nothing is pre-zeroed and nothing recycles)"
            .into());
    }
    let slots = flags.get("segment-recycle-slots").map(str::to_string);
    let wait = flags.get("recycle-wait").map(str::to_string);
    let mut arm_args: Vec<&str> = Vec::new();
    if let Some(slots) = slots.as_deref() {
        arm_args.extend(["--segment-recycle-slots", slots]);
    }
    if let Some(wait) = wait.as_deref() {
        arm_args.extend(["--recycle-wait", wait]);
    }
    let base_args: Vec<&str> = vec!["--no-segment-recycle"];
    m.note(format!(
        "s39d row: {cells} cells · {replicates} replicates (ABBA) · fixed work: {warm} warm + \
         {tail} tail records × 1 KiB at {S35_CONNS_AC} conns pipeline 16 · segment-bytes \
         {segment_bytes} · ckpt floor {} · boundary = INF.CKPT WAIT after the warm fill, \
         truncation settled 2 s · SIGKILL after the tail's last ack · {idle_s} s idle · one \
         boot · arm {} vs baseline {} · device stat {}",
        flags.u64_or("ckpt-interval-bytes", segment_bytes)?,
        if arm_args.is_empty() { "(server default)".to_string() } else { arm_args.join(" ") },
        base_args.join(" "),
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, S39dLeg)> = Vec::new();
    for rep in 0..replicates {
        let order: [(&'static str, &[&str]); 2] = if rep % 2 == 0 {
            [("base", &base_args), ("arm", &arm_args)]
        } else {
            [("arm", &arm_args), ("base", &base_args)]
        };
        for (arm, args) in order {
            let dir = format!("{data_root}/s39d-{arm}-rep{rep}");
            s35_idle(idle_s, &format!("s39d {arm} rep{rep}"));
            let leg = s39d_leg(
                flags,
                infinityd,
                cells,
                warm,
                tail,
                idle_s,
                &dir,
                arm,
                args,
                device_stat.as_deref(),
            )?;
            let slow = leg.slowest();
            raw.push_str(&format!(
                "rep{rep} {arm} warm_ops={:.0} tail_ops={:.0} warmed={} \
                 warm[rotations={} recycled={} truncated={} rotations_min_cell={} \
                 waits_expired={}] end[rotations={} recycled={} truncated={} frame_bytes={} \
                 zero_fill={}] boot_wall_s={:.3} proc_read_bytes={} records_recovered={} \
                 slowest_cell[total_ms={:.1} start_ms={:.1} ckpt_ms={:.1} replay_ms={:.1} \
                 audit_ms={:.1} finish_ms={:.1} dominating={}] \
                 sum_cells[total_ms={:.1} ckpt_ms={:.1} replay_ms={:.1} audit_ms={:.1} \
                 finish_ms={:.1} ckpt_bytes={} replay_bytes={} replay_frames={} audit_bytes={} \
                 audit_valid_frames={} audit_foreign_frames={} stale_files={} \
                 residue_slacks={} residue_stops={}]\n",
                leg.warm_ops_per_sec,
                leg.tail_ops_per_sec,
                leg.warmed,
                leg.warm.rotations,
                leg.warm.recycled,
                leg.warm.truncated,
                leg.warm.rotations_min_cell,
                leg.warm.waits_expired,
                leg.end.rotations,
                leg.end.recycled,
                leg.end.truncated,
                leg.end.frame_bytes,
                leg.end.zero_fill,
                leg.boot_wall_s,
                leg.proc_read_bytes,
                leg.records_recovered(),
                slow.total_us as f64 / 1e3,
                slow.start_us as f64 / 1e3,
                slow.ckpt_us as f64 / 1e3,
                slow.replay_us as f64 / 1e3,
                slow.audit_us as f64 / 1e3,
                slow.finish_us as f64 / 1e3,
                slow.dominating(),
                leg.sum(|c| c.total_us) as f64 / 1e3,
                leg.sum(|c| c.ckpt_us) as f64 / 1e3,
                leg.sum(|c| c.replay_us) as f64 / 1e3,
                leg.sum(|c| c.audit_us) as f64 / 1e3,
                leg.sum(|c| c.finish_us) as f64 / 1e3,
                leg.sum(|c| c.ckpt_bytes),
                leg.sum(|c| c.replay_bytes),
                leg.sum(|c| c.replay_frames),
                leg.sum(|c| c.audit_bytes),
                leg.sum(|c| c.audit_valid_frames),
                leg.sum(|c| c.audit_foreign_frames),
                leg.sum(|c| c.stale_files),
                leg.sum(|c| c.residue_slacks),
                leg.sum(|c| c.residue_stops),
            ));
            println!("  s39d rep{rep} {arm}: {}", raw.lines().last().unwrap_or(""));
            let _ = std::fs::remove_dir_all(&dir);
            legs.push((rep, leg));
        }
    }
    let pair = |rep: usize, arm: &str| legs.iter().find(|(r, l)| *r == rep && l.arm == arm);
    let mut total_x = Vec::new();
    let mut wall_x = Vec::new();
    let mut wall_arm = Vec::new();
    let mut wall_base = Vec::new();
    let mut phase_x: [Vec<f64>; 5] = Default::default();
    let mut audit_bytes_x = Vec::new();
    let mut reads_x = Vec::new();
    let mut frame_bytes_x = Vec::new();
    let mut records_match = Vec::new();
    let mut dominating_arm = Vec::new();
    let mut dominating_base = Vec::new();
    let mut foreign_arm = Vec::new();
    let mut invalid = 0usize;
    for rep in 0..replicates {
        let (Some((_, b)), Some((_, a))) = (pair(rep, "base"), pair(rep, "arm")) else { continue };
        if !a.warmed || !b.warmed {
            invalid += 1;
            continue;
        }
        let (sa, sb) = (a.slowest(), b.slowest());
        total_x.push(sa.total_us as f64 / sb.total_us.max(1) as f64);
        wall_x.push(a.boot_wall_s / b.boot_wall_s.max(1e-6));
        wall_arm.push(a.boot_wall_s);
        wall_base.push(b.boot_wall_s);
        // Per phase the work ratio is over the cell sums (a phase that
        // is 0 on both arms reads 1.0, never a division by nothing).
        let sums = |l: &S39dLeg| {
            [
                l.sum(|c| c.start_us),
                l.sum(|c| c.ckpt_us),
                l.sum(|c| c.replay_us),
                l.sum(|c| c.audit_us),
                l.sum(|c| c.finish_us),
            ]
        };
        let (pa, pb) = (sums(a), sums(b));
        for i in 0..5 {
            phase_x[i].push(if pb[i] == 0 && pa[i] == 0 {
                1.0
            } else {
                pa[i] as f64 / pb[i].max(1) as f64
            });
        }
        audit_bytes_x
            .push(a.sum(|c| c.audit_bytes) as f64 / b.sum(|c| c.audit_bytes).max(1) as f64);
        reads_x.push(a.proc_read_bytes as f64 / b.proc_read_bytes.max(1) as f64);
        frame_bytes_x.push(a.end.frame_bytes as f64 / b.end.frame_bytes.max(1) as f64);
        let fixed = warm + tail;
        records_match.push(f64::from(u8::from(
            a.records_recovered() == fixed && b.records_recovered() == fixed,
        )));
        dominating_arm.push(sa.dominating());
        dominating_base.push(sb.dominating());
        foreign_arm.push(a.sum(|c| c.audit_foreign_frames) as f64);
    }
    if invalid > 0 {
        m.note(format!(
            "s39d: {invalid} replicate pair(s) never reached the first-generation trigger on \
             every cell before the boundary — excluded; raise --s39d-warm-records or shrink \
             --segment-bytes"
        ));
    }
    if total_x.is_empty() {
        m.note("s39d: no valid replicate pair — every gate key is absent (PENDING)");
    } else {
        m.set("s39d:recovery_total_x", median(&mut total_x));
        m.set("s39d:recovery_wall_x", median(&mut wall_x));
        m.set("s39d:recovery_first_boot_s_arm", median(&mut wall_arm));
        m.set("s39d:recovery_first_boot_s_base", median(&mut wall_base));
        m.set("s39d:phase_start_x", median(&mut phase_x[0]));
        m.set("s39d:phase_ckpt_x", median(&mut phase_x[1]));
        m.set("s39d:phase_replay_x", median(&mut phase_x[2]));
        m.set("s39d:phase_audit_x", median(&mut phase_x[3]));
        m.set("s39d:phase_finish_x", median(&mut phase_x[4]));
        m.set("s39d:audit_bytes_x", median(&mut audit_bytes_x));
        m.set("s39d:proc_read_bytes_x", median(&mut reads_x));
        m.set("s39d:frame_bytes_x", median(&mut frame_bytes_x));
        m.set("s39d:records_recovered_match", records_match.iter().copied().fold(1.0, f64::min));
        m.set("s39d:audit_foreign_frames_arm", median(&mut foreign_arm));
        let name = |v: &[&str]| -> String {
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for d in v {
                *counts.entry(d).or_default() += 1;
            }
            counts.iter().map(|(k, n)| format!("{k}×{n}")).collect::<Vec<_>>().join(" ")
        };
        m.note(format!(
            "s39d dominating phase on the slowest cell — arm: {}; baseline: {} (per replicate)",
            name(&dominating_arm),
            name(&dominating_base)
        ));
    }
    if device_stat.is_none() {
        m.note("s39d: --device-stat not given — device sectors were not sampled");
    }
    m.row_open("recovery-attribution");
    m.row_write_amp(
        "fixed-work recovery (ADR-0090 A10): `recovery_total_x` is the slowest cell's engine \
         total (first step to completion on the loop clock) arm ÷ baseline; `recovery_wall_x` \
         the harness wall from process launch to loading:0; `phase_*_x` the per-phase work \
         ratio over the cell sums; `records_recovered_match` = 1 when both arms recovered \
         exactly the records written; `frame_bytes_x` discloses the encoded-bytes parity the \
         group formation allows",
    );
    m.raw_section("s39d per-leg samples", &raw);
    Ok(())
}

// ---- M4.5-S40: stall attribution at the memtier shape (S27 D5 `max`) ---------

/// Keys of the S40 memtier shape (`--keyspace 1000000`): random SETs
/// over 1 M keys × 1 KiB grow the dataset to ~1 GB inside the leg, so
/// the derived checkpoint trigger fires several times per minute at the
/// product's floor — the checkpoint phases the 103 ms maximum may sit in.
const S40_DEFAULT_KEYS: u64 = 1_000_000;

/// One server-side timeline sample (every cell summed / maxed) taken
/// every `S40_SAMPLE_MS` during the leg — the events a client maximum is
/// read against.
#[derive(Clone, Debug, Default)]
struct S40Sample {
    at_s: f64,
    /// Cells with a checkpoint stream open (`ckpt_buffer_bytes > 0`).
    ckpt_in_flight: u64,
    ckpts_completed: u64,
    ckpt_bytes: u64,
    manifests_published: u64,
    truncated: u64,
    rotations: u64,
    zero_fill: u64,
    parked: u64,
    stall_p99_us: u64,
    stall_p999_us: u64,
    waits_barrier: u64,
    waits_rotation: u64,
    waits_pace: u64,
    ckpt_deferrals: u64,
    /// Stop-and-copy index grows (`INFO stats index_grows`, summed over
    /// cells): the deterministic 12–17 ms maxima campaign I saw at the
    /// same seconds of every leg.
    index_grows: u64,
    /// `/sys/block/<dev>/stat`: sectors written (7), ms writing (8),
    /// ms doing I/O (10) — 0 when not sampled.
    dev_sectors_written: u64,
    dev_ms_writing: u64,
    dev_io_ms: u64,
}

const S40_SAMPLE_MS: u64 = 250;

fn s40_sample(port: u16, cells: u16, device_stat: Option<&str>, t0: Instant) -> Option<S40Sample> {
    let infos = scrape_cells(port, cells).ok()?;
    let per = |f: &str| -> Vec<u64> {
        infos.iter().map(|c| c.get(f).and_then(|v| v.parse().ok()).unwrap_or(0)).collect()
    };
    let dev =
        device_stat.and_then(|d| std::fs::read_to_string(format!("/sys/block/{d}/stat")).ok());
    let dev_field = |i: usize| -> u64 {
        dev.as_deref()
            .and_then(|s| s.split_whitespace().nth(i).and_then(|v| v.parse().ok()))
            .unwrap_or(0)
    };
    Some(S40Sample {
        at_s: t0.elapsed().as_secs_f64(),
        ckpt_in_flight: per("ckpt_buffer_bytes").iter().filter(|&&b| b > 0).count() as u64,
        ckpts_completed: sum_field(&infos, "ckpts_completed"),
        ckpt_bytes: sum_field(&infos, "ckpt_bytes_total"),
        manifests_published: sum_field(&infos, "manifests_published"),
        truncated: sum_field(&infos, "segments_truncated"),
        rotations: sum_field(&infos, "segment_rotations"),
        zero_fill: sum_field(&infos, "zero_fill_bytes"),
        parked: sum_field(&infos, "log_admission_parked_total"),
        stall_p99_us: crate::gaterun::max_field(&infos, "log_write_stall_p99_us"),
        stall_p999_us: crate::gaterun::max_field(&infos, "log_write_stall_p999_us"),
        waits_barrier: sum_field(&infos, "frame_waits_barrier"),
        waits_rotation: sum_field(&infos, "frame_waits_rotation"),
        waits_pace: sum_field(&infos, "frame_waits_pace"),
        ckpt_deferrals: sum_field(&infos, "io_budget_deferrals_checkpoint"),
        index_grows: sum_field(&infos, "index_grows"),
        dev_sectors_written: dev_field(6),
        dev_ms_writing: dev_field(7),
        dev_io_ms: dev_field(9),
    })
}

/// What the timeline says happened in the sample window that brackets
/// the client's maximum: every engine event is named, the device's
/// write time over the window disclosed, and one attribution word
/// chosen by precedence (checkpoint in flight → rotation → manifest/
/// truncation → zero-fill → index grow → device-busy → admission park → none).
fn s40_attribute(before: &S40Sample, after: &S40Sample) -> (String, String) {
    let window_ms = ((after.at_s - before.at_s) * 1000.0).max(1.0);
    let d = |f: fn(&S40Sample) -> u64| f(after).saturating_sub(f(before));
    let mut events = Vec::new();
    if before.ckpt_in_flight > 0 || after.ckpt_in_flight > 0 {
        events.push(format!(
            "checkpoint in flight ({}→{} cells, +{} bytes)",
            before.ckpt_in_flight,
            after.ckpt_in_flight,
            d(|s| s.ckpt_bytes)
        ));
    }
    if d(|s| s.ckpts_completed) > 0 {
        events.push(format!("checkpoint published (+{})", d(|s| s.ckpts_completed)));
    }
    if d(|s| s.rotations) > 0 {
        events.push(format!("rotation (+{})", d(|s| s.rotations)));
    }
    if d(|s| s.manifests_published) > 0 || d(|s| s.truncated) > 0 {
        events.push(format!(
            "manifest/truncation (+{} manifests, +{} segments)",
            d(|s| s.manifests_published),
            d(|s| s.truncated)
        ));
    }
    if d(|s| s.zero_fill) > 0 {
        events.push(format!("zero-fill (+{} bytes)", d(|s| s.zero_fill)));
    }
    if d(|s| s.parked) > 0 {
        events.push(format!("admission parks (+{})", d(|s| s.parked)));
    }
    if d(|s| s.ckpt_deferrals) > 0 {
        events.push(format!("checkpoint offers deferred (+{})", d(|s| s.ckpt_deferrals)));
    }
    if d(|s| s.index_grows) > 0 {
        events.push(format!("index grow, stop-and-copy (+{})", d(|s| s.index_grows)));
    }
    if d(|s| s.waits_rotation) > 0 || d(|s| s.waits_barrier) > 0 || d(|s| s.waits_pace) > 0 {
        events.push(format!(
            "frame waits (+{} barrier, +{} rotation, +{} pace)",
            d(|s| s.waits_barrier),
            d(|s| s.waits_rotation),
            d(|s| s.waits_pace)
        ));
    }
    let dev_busy_pct = d(|s| s.dev_io_ms) as f64 * 100.0 / window_ms;
    let dev_note = if after.dev_sectors_written > 0 {
        format!(
            "device: +{} MiB written, {} ms writing, io busy {:.0} % of the {:.0} ms window",
            (d(|s| s.dev_sectors_written) * 512) >> 20,
            d(|s| s.dev_ms_writing),
            dev_busy_pct,
            window_ms
        )
    } else {
        "device: not sampled".to_string()
    };
    let word =
        if before.ckpt_in_flight > 0 || after.ckpt_in_flight > 0 || d(|s| s.ckpts_completed) > 0 {
            "checkpoint"
        } else if d(|s| s.rotations) > 0 {
            "rotation"
        } else if d(|s| s.manifests_published) > 0 || d(|s| s.truncated) > 0 {
            "manifest/truncation"
        } else if d(|s| s.zero_fill) > 0 {
            "zero-fill"
        } else if d(|s| s.index_grows) > 0 {
            "index-grow"
        } else if dev_busy_pct >= 50.0 {
            // Campaign I's lesson (2026-08-23): the device outranks the
            // parks — a park is the staging domain filling behind frames
            // the device is not completing, a symptom of the cause above
            // it (rep1: 92 % busy, 18.5 s of queued write time in a
            // 253 ms window, 11 MiB of progress, +42 parks).
            "device-busy"
        } else if d(|s| s.parked) > 0 {
            "admission-park"
        } else {
            "unattributed"
        };
    let detail = if events.is_empty() {
        format!("no engine event in the window; {dev_note}")
    } else {
        format!("{}; {dev_note}", events.join(", "))
    };
    (word.to_string(), detail)
}

/// One S40 leg's facts.
struct S40Leg {
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
    max_at_s: f64,
    /// Seconds whose maximum exceeded 50 ms, and the top per-second maxima.
    seconds_over_50ms: u64,
    top_seconds: Vec<(usize, u64)>,
    cpu_pct: f64,
    attribution: String,
    attribution_detail: String,
    ckpts: u64,
    ckpt_bytes: u64,
    stall_p99_us: u64,
    stall_p999_us: u64,
    parked: u64,
    dev_mib: u64,
}

/// The M4.5-S40 stall-attribution leg: the memtier shape on the in-house
/// generator (1 M keys × 1 KiB, 32 conns, pipeline 1, `everysec`, an
/// offered rate with latency from the intended send), the server's
/// counters and the block device sampled every 250 ms across it, and
/// the client's maximum read against the sample window it fell in.
#[allow(clippy::too_many_arguments)]
fn s40_leg(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    offered: u64,
    keys: u64,
    dir: &str,
    device_stat: Option<&str>,
) -> Result<S40Leg, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    create_ns(
        port,
        &[b"INF.NS", b"CREATE", b"s40esec", b"MODE", b"durable", b"FSYNC", b"everysec"],
    )?;
    await_fan(port, "s40esec", cells)?;
    let warmup = Duration::from_secs(2);
    let spec = LoadSpec {
        port,
        conns: 32,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup,
        set_weight: 1,
        get_weight: 0,
        keys,
        key_prefix: "s40:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s40esec".to_vec()]],
        target_ops_per_sec: Some(offered),
        ..LoadSpec::default()
    };
    let ticks_before = crate::gaterun::cpu_ticks_of(server.pid());
    let t0 = Instant::now();
    let load = std::thread::spawn(move || run_load(&spec));
    let mut timeline: Vec<S40Sample> = Vec::new();
    while !load.is_finished() {
        if let Some(s) = s40_sample(port, cells, device_stat, t0) {
            timeline.push(s);
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(S40_SAMPLE_MS));
    }
    let wall = t0.elapsed().as_secs_f64().max(1e-9);
    let ticks = crate::gaterun::cpu_ticks_of(server.pid()).saturating_sub(ticks_before);
    let report = load.join().map_err(|_| "s40: load thread panicked".to_string())??;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s40: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let (first, last) = match (timeline.first(), timeline.last()) {
        (Some(f), Some(l)) => (f.clone(), l.clone()),
        _ => return Err("s40: no timeline sample".into()),
    };
    // The max's send instant is measured from warmup's end; the timeline
    // from the leg's start.
    let max_at_leg = report.max_at_s + warmup.as_secs_f64();
    let after_idx =
        timeline.iter().position(|s| s.at_s >= max_at_leg).unwrap_or(timeline.len() - 1);
    let before_idx = after_idx.saturating_sub(1);
    let (attribution, attribution_detail) =
        s40_attribute(&timeline[before_idx], &timeline[after_idx]);
    let mut top: Vec<(usize, u64)> = report.max_per_second.iter().copied().enumerate().collect();
    top.sort_by_key(|&(_, m)| std::cmp::Reverse(m));
    top.truncate(5);
    drop(server);
    Ok(S40Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        p999_us: report.p999_us as f64,
        max_us: report.max_us as f64,
        max_at_s: report.max_at_s,
        seconds_over_50ms: report.max_per_second.iter().filter(|&&m| m > 50_000).count() as u64,
        top_seconds: top,
        cpu_pct: ticks as f64 / crate::gaterun::CLOCK_TICKS_PER_S as f64 / wall * 100.0,
        attribution,
        attribution_detail,
        ckpts: last.ckpts_completed.saturating_sub(first.ckpts_completed),
        ckpt_bytes: last.ckpt_bytes.saturating_sub(first.ckpt_bytes),
        stall_p99_us: last.stall_p99_us,
        stall_p999_us: last.stall_p999_us,
        parked: last.parked.saturating_sub(first.parked),
        dev_mib: (last.dev_sectors_written.saturating_sub(first.dev_sectors_written) * 512) >> 20,
    })
}

/// The M4.5-S40 stall-attribution row: `--replicates` legs of the memtier
/// shape on the in-house generator, the client maximum of each read
/// against the server/device timeline window it fell in. Reports the
/// worst and median maximum (the S27 D5 `max ≤ 50 ms` wording at this
/// shape), the seconds over 50 ms per leg, and the attribution word per
/// leg — a note, never a gate.
fn s40_stall_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    let offered = flags.u64_or("offered-ops", 100_000)?;
    let keys = flags.u64_or("s40-keys", S40_DEFAULT_KEYS)?;
    let device_stat = flags.get("device-stat").map(str::to_string);
    m.note(format!(
        "s40 row: {cells} cells · {replicates} legs · memtier shape on the in-house generator: \
         {keys} keys × 1 KiB, 32 conns, pipeline 1, everysec, {offered} offered ops/s for \
         {duration} s (latency from the intended send) · INFO + device sampled every \
         {S40_SAMPLE_MS} ms · {idle_s} s idle before every leg · device stat {}",
        device_stat.as_deref().unwrap_or("(not sampled)")
    ));
    let mut raw = String::new();
    let mut maxes = Vec::new();
    let mut achieved = Vec::new();
    let mut p99s = Vec::new();
    let mut p999s = Vec::new();
    let mut over = Vec::new();
    let mut words: Vec<String> = Vec::new();
    for rep in 0..replicates {
        let dir = format!("{data_root}/s40-rep{rep}");
        s35_idle(idle_s, &format!("s40 rep{rep}"));
        let leg = s40_leg(
            flags,
            infinityd,
            cells,
            duration,
            offered,
            keys,
            &dir,
            device_stat.as_deref(),
        )?;
        raw.push_str(&format!(
            "rep{rep} achieved={:.0} ({:.3} of offered) p50_us={:.0} p99_us={:.0} p999_us={:.0} \
             max_us={:.0} max_at_s={:.2} seconds_over_50ms={} top_seconds={:?} cpu_pct={:.0} \
             ckpts={} ckpt_bytes={} stall_p99_us={} stall_p999_us={} parked={} dev_mib={} \
             attribution={} [{}]\n",
            leg.ops_per_sec,
            leg.ops_per_sec / offered.max(1) as f64,
            leg.p50_us,
            leg.p99_us,
            leg.p999_us,
            leg.max_us,
            leg.max_at_s,
            leg.seconds_over_50ms,
            leg.top_seconds,
            leg.cpu_pct,
            leg.ckpts,
            leg.ckpt_bytes,
            leg.stall_p99_us,
            leg.stall_p999_us,
            leg.parked,
            leg.dev_mib,
            leg.attribution,
            leg.attribution_detail,
        ));
        println!("  s40 rep{rep}: {}", raw.lines().last().unwrap_or(""));
        let _ = std::fs::remove_dir_all(&dir);
        maxes.push(leg.max_us / 1000.0);
        achieved.push(leg.ops_per_sec / offered.max(1) as f64);
        p99s.push(leg.p99_us);
        p999s.push(leg.p999_us);
        over.push(leg.seconds_over_50ms as f64);
        words.push(leg.attribution);
    }
    m.set("s40:max_ms_worst", maxes.iter().copied().fold(0.0, f64::max));
    m.set("s40:max_ms_median", median(&mut maxes));
    m.set("s40:offered_rate_achieved_x_min", achieved.iter().copied().fold(f64::MAX, f64::min));
    m.set("s40:p99_us_median", median(&mut p99s));
    m.set("s40:p999_us_median", median(&mut p999s));
    m.set("s40:seconds_over_50ms_total", over.iter().sum());
    m.note(format!("s40 attribution per leg (max's sample window): {}", words.join(" / ")));
    if achieved.iter().any(|a| *a < 0.9) {
        m.note("s40: a leg achieved < 0.90 of the offered rate — its max is a saturation number");
    }
    m.row_open("stall-attribution");
    m.row_write_amp(
        "S40 stall attribution: the client's maximum is read against the 250 ms server/device \
         sample window its send instant fell in; the attribution word is by precedence \
         (checkpoint → rotation → manifest/truncation → zero-fill → index grow → device \
         busy ≥ 50 % → admission park → unattributed) and is a note, never a gate",
    );
    m.raw_section("s40 per-leg samples", &raw);
    Ok(())
}

// ---- M4.5-S37 step 1: the cold-overwrite ceiling (bench-diagnostics arm) ------

/// Keys of the S37 ceiling row unless `--s37-keys` says otherwise:
/// 1 M × 1 KiB ≈ 250 MB per cell against the row's 128 MB tiered budget
/// — a beyond-RAM table where roughly half of every SET's candidates
/// are cold, so the verifying read the ceiling arm skips is on the
/// path of a large share of the leg (the share is measured, not
/// assumed: `cold_reads_issued` on A, `blind_overwrites_ceiling` on B).
const S37_DEFAULT_KEYS: u64 = 1_000_000;

/// One S37 leg: the S29 tiered shape (closed loop, pipeline 1, 1 KiB)
/// with the cold-resolve and blind-overwrite counts of the leg.
struct S37Leg {
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    sets: u64,
    cold_resolves: u64,
    blind: u64,
}

fn s37_leg(
    port: u16,
    cells: u16,
    conns: usize,
    duration: u64,
    keys: u64,
) -> Result<S37Leg, String> {
    let before = scrape_cells(port, cells)?;
    let report = run_load(&LoadSpec {
        port,
        conns,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys,
        key_prefix: "s37tier:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
        ..LoadSpec::default()
    })?;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s37 c{conns}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let after = scrape_cells(port, cells)?;
    let d = |f: &str| sum_field(&after, f).saturating_sub(sum_field(&before, f));
    Ok(S37Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        p999_us: report.p999_us as f64,
        sets: report.ops,
        // `cold_reads_issued`, never `tiering_cold_resolves` (that one
        // counts address classifications — candidate probes and retries,
        // ~2.9 per SET in campaign J — not reads).
        cold_resolves: d("cold_reads_issued"),
        blind: d("blind_overwrites_ceiling"),
    })
}

/// The M4.5-S37 step-1 row: the beyond-RAM tiered `always` write legs
/// (64 and 256 conns) on the shipping path (A) against the blind-
/// overwrite ceiling arm (B, `infinityd --blind-overwrite-ceiling` — a
/// `bench-diagnostics` build, the same binary for both arms), ABBA per
/// replicate, fresh server + fill per leg. B is an upper bound from an
/// unsound build: its gain is what removing the verifying cold read
/// could ever buy; the predeclared rule (plan S37) reads "< 15 %
/// throughput and < 20 % p99 ⇒ step 2 `Rejected`".
fn s37_ceiling_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let keys = flags.u64_or("s37-keys", S37_DEFAULT_KEYS)?;
    let idle_s = flags.u64_or("leg-idle-s", 0)?;
    m.note(format!(
        "s37 row: {cells} cells · {replicates} replicates (ABBA) · tiered always, MEM-BUDGET \
         {MEM_BUDGET}/cell, {keys} keys × 1 KiB filled per leg, then 100 % SET closed-loop \
         pipeline 1 at {CONNS_LOW} and {CONNS_HIGH} conns for {duration} s · arm B = \
         --blind-overwrite-ceiling (unsound ceiling instrument; bench-diagnostics build)"
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, &'static str, usize, S37Leg)> = Vec::new();
    for rep in 0..replicates {
        let order: [(&'static str, &[&str]); 2] = if rep % 2 == 0 {
            [("A", &[]), ("B", &["--blind-overwrite-ceiling"])]
        } else {
            [("B", &["--blind-overwrite-ceiling"]), ("A", &[])]
        };
        for (arm, arm_args) in order {
            let dir = format!("{data_root}/s37-{arm}-rep{rep}");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
            crate::m2rows::copy_probe_file(flags, std::path::Path::new(&dir))?;
            let mut extra: Vec<String> = vec!["--data-dir".into(), dir.clone()];
            if let Some(pin) = flags.get("pin-start") {
                extra.push("--pin-start".into());
                extra.push(pin.to_string());
            }
            extra.extend(crate::m2rows::pipeline_args(flags));
            extra.extend(arm_args.iter().map(|s| (*s).to_string()));
            let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
            s35_idle(idle_s, &format!("s37 {arm} rep{rep}"));
            let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
            let port = server.port;
            create_ns(
                port,
                &[
                    b"INF.NS",
                    b"CREATE",
                    b"s37tier",
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
            await_fan(port, "s37tier", cells)?;
            let infos = scrape_cells(port, cells)?;
            if !infos.iter().all(|c| c.contains_key("blind_overwrites_ceiling")) {
                return Err("s37: INFO has no blind_overwrites_ceiling — the binary is not a \
                            bench-diagnostics build (cargo build --release --features \
                            bench-diagnostics -p infinityd)"
                    .into());
            }
            let fill = run_load(&LoadSpec {
                port,
                conns: 64,
                pipeline: 4,
                fill: Some(keys),
                keys,
                key_prefix: "s37tier:".into(),
                value_size: 1024,
                setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
                ..LoadSpec::default()
            })?;
            if fill.errors > 0 {
                return Err(format!("s37 {arm} rep{rep} fill: {} errors", fill.errors));
            }
            for conns in [CONNS_LOW, CONNS_HIGH] {
                let leg = s37_leg(port, cells, conns, duration, keys)?;
                raw.push_str(&format!(
                    "rep{rep} {arm} c{conns:<3} ops/s={:<8.0} p50_us={:<6.0} p99_us={:<7.0} \
                     p999_us={:<7.0} sets={} cold_resolves={} ({:.3}/set) blind={} ({:.3}/set)\n",
                    leg.ops_per_sec,
                    leg.p50_us,
                    leg.p99_us,
                    leg.p999_us,
                    leg.sets,
                    leg.cold_resolves,
                    leg.cold_resolves as f64 / leg.sets.max(1) as f64,
                    leg.blind,
                    leg.blind as f64 / leg.sets.max(1) as f64,
                ));
                println!("  s37 {}", raw.lines().last().unwrap_or(""));
                legs.push((rep, arm, conns, leg));
            }
            drop(server);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let find = |rep: usize, arm: &str, conns: usize| {
        legs.iter().find(|(r, a, c, _)| *r == rep && *a == arm && *c == conns).map(|(_, _, _, l)| l)
    };
    for conns in [CONNS_LOW, CONNS_HIGH] {
        let mut ops_x = Vec::new();
        let mut p50_gain = Vec::new();
        let mut p99_gain = Vec::new();
        let mut blind_share = Vec::new();
        let mut cold_share = Vec::new();
        for rep in 0..replicates {
            let (Some(a), Some(b)) = (find(rep, "A", conns), find(rep, "B", conns)) else {
                continue;
            };
            ops_x.push(b.ops_per_sec / a.ops_per_sec.max(1.0));
            p50_gain.push(a.p50_us / b.p50_us.max(1.0));
            p99_gain.push(a.p99_us / b.p99_us.max(1.0));
            blind_share.push(b.blind as f64 / b.sets.max(1) as f64);
            cold_share.push(a.cold_resolves as f64 / a.sets.max(1) as f64);
        }
        if ops_x.is_empty() {
            continue;
        }
        let tag: &'static str = if conns == CONNS_LOW { "c64" } else { "c256" };
        let key = |name: &str| -> &'static str {
            Box::leak(format!("s37:{name}_{tag}").into_boxed_str())
        };
        m.set(key("ceiling_ops_x"), median(&mut ops_x));
        m.set(key("ceiling_p50_gain_x"), median(&mut p50_gain));
        m.set(key("ceiling_p99_gain_x"), median(&mut p99_gain));
        m.set(key("blind_share_arm_b"), median(&mut blind_share));
        m.set(key("cold_resolve_share_arm_a"), median(&mut cold_share));
    }
    m.row_open("cold-overwrite-ceiling");
    m.row_write_amp(
        "S37 step 1 (plan rule): B ÷ A throughput and A ÷ B p99 on the beyond-RAM tiered \
         always write legs; B is an UNSOUND upper bound (the cold record is orphaned) — \
         \"< 15 % throughput and < 20 % p99 ⇒ step 2 Rejected\"; `blind_share_arm_b` is the \
         share of B's SETs that skipped a cold read (0 = the instrument never engaged), \
         `cold_resolve_share_arm_a` the share of A's SETs that paid one",
    );
    m.raw_section("s37 per-leg samples", &raw);
    Ok(())
}
