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

use std::time::Duration;

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, median, scrape_cells,
    spawn_infinityd, sum_field,
};
use crate::load::{LoadSpec, render, run as run_load};

/// Measurement keys for one A/B row (`Measurements` keys are `'static`).
struct RowKeys {
    ops_delta: &'static str,
    p999_delta: &'static str,
}

/// Relative spread of a replicate set: (max − min) / median, in percent.
fn rel_spread_pct(values: &mut [f64]) -> f64 {
    let med = median(values);
    let (min, max) = (values[0], values[values.len() - 1]); // median() sorted them
    if med == 0.0 { 0.0 } else { (max - min) / med * 100.0 }
}

/// Signed delta (b − a) / a in percent.
fn delta_pct(a: f64, b: f64) -> f64 {
    if a == 0.0 { 0.0 } else { (b - a) / a * 100.0 }
}

/// One interleaved A/B row: `replicates` alternating runs of the same
/// `LoadSpec` shape against two live servers, in ABBA order (the leg that
/// runs second flips every replicate, so drift/thermal bias cancels in the
/// medians instead of always taxing the same build). With no baseline
/// server the M2 leg still runs (the counter tripwire below needs it) and
/// the delta gates stay PENDING.
fn ab_row(
    m: &mut Measurements,
    name: &str,
    keys: &RowKeys,
    replicates: usize,
    baseline: Option<&ServerGuard>,
    m2: &ServerGuard,
    spec_for: impl Fn(u16) -> LoadSpec,
) -> Result<(), String> {
    println!("\n== row: {name} (interleaved ABBA × {replicates}) ==");
    let mut base_ops: Vec<f64> = Vec::new();
    let mut base_p999: Vec<f64> = Vec::new();
    let mut m2_ops: Vec<f64> = Vec::new();
    let mut m2_p999: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        let baseline_first = rep % 2 == 0;
        for leg in 0..2 {
            if (leg == 0) == baseline_first {
                let Some(server) = baseline else { continue };
                let report = run_load(&spec_for(server.port))?;
                println!(
                    "  rep {rep} m1-baseline: {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("{name} m1-baseline rep {rep}"), &render(&report));
                base_ops.push(report.ops_per_sec);
                base_p999.push(report.p999_us as f64);
            } else {
                let report = run_load(&spec_for(m2.port))?;
                println!(
                    "  rep {rep} m2: {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("{name} m2 rep {rep}"), &render(&report));
                m2_ops.push(report.ops_per_sec);
                m2_p999.push(report.p999_us as f64);
            }
        }
    }
    if baseline.is_some() {
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
    let pin_args: Vec<String> = flags
        .get("pin-start")
        .map(|v| vec!["--pin-start".to_string(), v.to_string()])
        .unwrap_or_default();
    let server_extra: Vec<&str> = pin_args.iter().map(String::as_str).collect();

    let env_ok = env_gate(flags)?;
    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    if !reference_box {
        m.note("dev-tier run: reference-box gates report measured values, non-binding verdicts");
    }
    m.note(
        "p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): \
         0.0% = same bucket; any non-zero delta spans ≥ 1 bucket",
    );
    m.note(
        "M2 leg runs the as-shipped node assembly (no durable plane configured — infinityd \
         durable wiring lands with the release stories); the zero-record assert on a \
         durable-enabled node is the node_e2e mixed-class test",
    );
    if baseline_bin.is_none() {
        m.note(
            "--baseline-bin not given: zero-cost delta rows report PENDING \
             (build the pre-M2 commit's infinityd and pass its path)",
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
        let m2_server = spawn_infinityd(&infinityd, cells, &server_extra)?;
        let baseline_server = match &baseline_bin {
            Some(bin) => Some(spawn_infinityd(bin, cells, &server_extra)?),
            None => None,
        };
        ab_row(&mut m, name, keys, replicates, baseline_server.as_ref(), &m2_server, |port| {
            spec(port, duration)
        })?;
        assert_zero_log_records(&mut m, m2_server.port, cells, name)?;
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
