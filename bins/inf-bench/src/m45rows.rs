//! `inf-bench gate-run m4.5` — the M4.5-S29 scaling row: tiered
//! `FSYNC always` must scale with client concurrency like the
//! non-tiered `always` path does.
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

    let env_ok = env_gate(flags)?;
    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    if !reference_box {
        m.note("dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box");
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
                }
                (_, CONNS_LOW) => {
                    f_low.push(sample.ops_per_sec);
                }
                _ => {
                    f_high.push(sample.ops_per_sec);
                    f_high_p99.push(sample.p99_us);
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

    finish_report(
        "m4.5",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!("binary {infinityd} · cells {cells} · {replicates} replicates"),
    )
}

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
    raw.push_str(&format!(
        "rep{rep} {ns:<9} c{conns:<3} ops/s={:<8.0} p50_us={:<6} p99_us={:<7} busy={} \
         acks/fsync={group:.2}\n",
        report.ops_per_sec, report.p50_us, report.p99_us, report.busy_retryable
    ));
    Ok(LegSample { ops_per_sec: report.ops_per_sec, p99_us: report.p99_us as f64 })
}
