//! `inf-bench gate-run m4` (M4-S03 today; S22/S24 add the tiered YCSB and
//! campaign rows): the **memory-mode degenerate-case A/B** — the hard §22
//! sub-gate. An M3-tip `infinityd` (`--baseline-bin`) vs the M4 build over
//! the same memory-only gate mixes the C15/M2-S09 zero-cost A/B used,
//! interleaved ABBA replicates, Δ ≤ 1% on every metric (throughput,
//! p99.9, RSS) — plus two report-enforced tripwires on the M4 leg:
//! `log_records_appended == 0` (the M2 posture, carried) and **every
//! `tiering_*` counter identically zero** (M4 §3.3: cache-profile users
//! provably never execute tiering code; a nonzero counter aborts the run,
//! not a human eyeball).
//!
//! Tier honesty (L10): dev runs report measured values with non-binding
//! verdicts on reference-box gates; the two zero tripwires are
//! box-independent and always bind. Failing the delta gate is a **STOP**
//! (release blocker), not a backlog item — the M4 plan says so by name.
//! Baseline provenance (the C15 lesson): both binaries are fingerprinted
//! into the report notes; rebuild the baseline from the M3 tip commit and
//! keep the fingerprint stable across the week-4 and final re-runs.

use std::time::Duration;

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, median, scrape_cells,
    spawn_infinityd, sum_field,
};
use crate::load::{LoadSpec, render, run as run_load};
use crate::m2rows::{delta_pct, rel_spread_pct};

/// Measurement keys for one degenerate A/B row.
struct RowKeys {
    ops_delta: &'static str,
    p999_delta: &'static str,
}

/// One interleaved A/B row (the m2rows ABBA shape, with per-leg peak-RSS
/// capture): `replicates` alternating runs against two live servers;
/// the second leg flips every replicate so drift/thermal bias cancels in
/// the medians. Returns the per-leg RSS medians `(m3, m4)` for the
/// worst-row RSS gate.
#[allow(clippy::too_many_arguments)] // orchestration row: linear, not branchy
fn degenerate_row(
    m: &mut Measurements,
    name: &str,
    keys: &RowKeys,
    replicates: usize,
    baseline: Option<&ServerGuard>,
    m4: &ServerGuard,
    spec_for: impl Fn(u16) -> LoadSpec,
) -> Result<Option<(f64, f64)>, String> {
    println!("\n== row: {name} (interleaved ABBA × {replicates}) ==");
    let mut base_ops: Vec<f64> = Vec::new();
    let mut base_p999: Vec<f64> = Vec::new();
    let mut base_rss: Vec<f64> = Vec::new();
    let mut m4_ops: Vec<f64> = Vec::new();
    let mut m4_p999: Vec<f64> = Vec::new();
    let mut m4_rss: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        let baseline_first = rep % 2 == 0;
        for leg in 0..2 {
            if (leg == 0) == baseline_first {
                let Some(server) = baseline else { continue };
                let report = run_load(&spec_for(server.port))?;
                println!(
                    "  rep {rep} m3-baseline: {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("{name} m3-baseline rep {rep}"), &render(&report));
                base_ops.push(report.ops_per_sec);
                base_p999.push(report.p999_us as f64);
                base_rss.push(server.rss_bytes() as f64);
            } else {
                let report = run_load(&spec_for(m4.port))?;
                println!(
                    "  rep {rep} m4: {:.0} ops/s, p999 {} µs",
                    report.ops_per_sec, report.p999_us
                );
                m.raw_section(&format!("{name} m4 rep {rep}"), &render(&report));
                m4_ops.push(report.ops_per_sec);
                m4_p999.push(report.p999_us as f64);
                m4_rss.push(m4.rss_bytes() as f64);
            }
        }
    }
    if baseline.is_none() {
        return Ok(None);
    }
    let (a_ops, b_ops) = (median(&mut base_ops), median(&mut m4_ops));
    let (a_p999, b_p999) = (median(&mut base_p999), median(&mut m4_p999));
    let (a_rss, b_rss) = (median(&mut base_rss), median(&mut m4_rss));
    // Regression bound ("pays nothing"): a faster/leaner M4 build clamps
    // to 0; the signed deltas are disclosed — improvements are findings
    // to explain, never gate passes to bank.
    let ops_signed = delta_pct(a_ops, b_ops); // negative = m4 slower
    let p999_signed = delta_pct(a_p999, b_p999); // positive = m4 worse tail
    m.set(keys.ops_delta, (-ops_signed).max(0.0));
    m.set(keys.p999_delta, p999_signed.max(0.0));
    m.note(format!(
        "{name}: m3 {a_ops:.0} ops/s (spread {:.2}%) vs m4 {b_ops:.0} ops/s (spread {:.2}%) — \
         signed ops delta {ops_signed:+.2}% · p999 {a_p999:.0} → {b_p999:.0} µs \
         ({p999_signed:+.2}%) · peak-RSS {a_rss:.0} → {b_rss:.0} B ({:+.2}%)",
        rel_spread_pct(&mut base_ops),
        rel_spread_pct(&mut m4_ops),
        delta_pct(a_rss, b_rss),
    ));
    Ok(Some((a_rss, b_rss)))
}

/// The report-enforced M4-S03 tripwire: every tiering code-path counter on
/// the M4 leg reads identically zero after the row (§3.3 "provably
/// unexecuted"). Returns the summed counter total (0 on pass); any nonzero
/// value aborts the run with the same teeth as the M2 zero-record assert.
fn assert_zero_tiering(port: u16, cells: u16, row: &str) -> Result<(u64, u64), String> {
    let scrape = scrape_cells(port, cells)?;
    let tables = sum_field(&scrape, "tiering_tables");
    let total = sum_field(&scrape, "tiering_tail_allocs")
        + sum_field(&scrape, "tiering_seal_holes")
        + sum_field(&scrape, "tiering_seal_hole_bytes")
        + sum_field(&scrape, "tiering_region_commit_pages")
        + sum_field(&scrape, "tiering_region_decommit_pages")
        + sum_field(&scrape, "tiering_cold_resolves");
    if tables != 0 || total != 0 {
        return Err(format!(
            "memory-only row `{row}` shows tiering activity (tables {tables}, counter sum \
             {total}) — the §3.3 degenerate-case contract is broken (STOP, not a note)"
        ));
    }
    Ok((tables, total))
}

/// `hash64` fingerprint of a binary for baseline provenance (zero-dep
/// harness: not cryptographic, disclosed as such — it pins *which file
/// ran*, the commit pin lives in the ledger row).
fn binary_fingerprint(path: &str) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            format!(
                "hash64:{:016x} ({} bytes)",
                inf_foundation::hash64(&bytes, 0x4D34),
                bytes.len()
            )
        }
        Err(e) => format!("unreadable ({e})"),
    }
}

#[allow(clippy::too_many_lines)] // orchestration script: linear rows, not branchy logic
pub fn cmd_gate_run_m4(flags: &Flags) -> Result<(), String> {
    let gates_list = load_gates(flags, "m4")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m4/s03");
    let replicates: usize = flags.usize_or("replicates", 5)?;
    let duration: u64 = flags.u64_or("duration", 10)?;
    let cells: u16 = flags.u16_or("cells", 4)?;
    let infinityd = flags.str_or("infinityd-bin", "target/release/infinityd");
    let baseline_bin = flags.get("baseline-bin").map(str::to_string);
    let reference_box = flags.bool("reference-box");
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
        m.note(
            "dev-tier run: reference-box gates report measured values, non-binding verdicts — \
             the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)",
        );
    }
    m.note(
        "p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): \
         0.0% = same bucket; any non-zero delta spans ≥ 1 bucket",
    );
    m.note(format!("m4 binary {}: {}", infinityd, binary_fingerprint(&infinityd)));
    match &baseline_bin {
        Some(bin) => m.note(format!(
            "m3 baseline {}: {} — pin this fingerprint across the week-4 and S24 re-runs; \
             the commit it was built from is recorded in the ledger row (C15 lesson)",
            bin,
            binary_fingerprint(bin)
        )),
        None => m.note(
            "--baseline-bin not given: delta rows report PENDING (build the M3 tip commit's \
             infinityd and pass its path)"
                .to_string(),
        ),
    }
    if !server_extra.is_empty() {
        m.note(format!("server cells pinned: {} (same cpu set both legs)", pin_args.join(" ")));
    }

    // The C15 row set — M0/M1 memory-only gate mixes, unchanged (S03:
    // `inf-bench gate-run m1` workloads remain the source; no bespoke
    // workloads). Fresh servers per row: each row owns its keyspace state.
    type SpecFn = fn(u16, u64) -> LoadSpec;
    let rows: [(&str, RowKeys, SpecFn); 3] = [
        (
            "pipelined 1:10 (M0 gate mix)",
            RowKeys {
                ops_delta: "ab:degenerate_pipelined_ops_delta_pct",
                p999_delta: "ab:degenerate_pipelined_p999_delta_pct",
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
                ops_delta: "ab:degenerate_unpipelined_ops_delta_pct",
                p999_delta: "ab:degenerate_unpipelined_p999_delta_pct",
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
                ops_delta: "ab:degenerate_ttl_ops_delta_pct",
                p999_delta: "ab:degenerate_ttl_p999_delta_pct",
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

    let mut worst_rss_delta: f64 = 0.0;
    let mut tiering_tables_seen = 0u64;
    let mut tiering_counter_total = 0u64;
    for (name, keys, spec) in &rows {
        let m4_server = spawn_infinityd(&infinityd, cells, &server_extra)?;
        let baseline_server = match &baseline_bin {
            Some(bin) => Some(spawn_infinityd(bin, cells, &server_extra)?),
            None => None,
        };
        let rss = degenerate_row(
            &mut m,
            name,
            keys,
            replicates,
            baseline_server.as_ref(),
            &m4_server,
            |port| spec(port, duration),
        )?;
        if let Some((m3_rss, m4_rss)) = rss {
            worst_rss_delta = worst_rss_delta.max(delta_pct(m3_rss, m4_rss));
        }
        // Both zero tripwires bind on every row, every box (M2 posture +
        // the M4 §3.3 contract).
        let appended = sum_field(&scrape_cells(m4_server.port, cells)?, "log_records_appended");
        if appended != 0 {
            return Err(format!(
                "memory-only row `{name}` appended {appended} log records — the M2-S09 \
                 zero-cost contract is broken"
            ));
        }
        let (tables, total) = assert_zero_tiering(m4_server.port, cells, name)?;
        tiering_tables_seen = tiering_tables_seen.max(tables);
        tiering_counter_total += total;
    }
    if baseline_bin.is_some() {
        m.set("ab:degenerate_rss_delta_pct", worst_rss_delta.max(0.0));
    }
    m.set("tripwire:mem_only_log_records_appended", 0.0);
    m.set("tripwire:tiering_tables", tiering_tables_seen as f64);
    m.set("tripwire:tiering_counter_total", tiering_counter_total as f64);

    finish_report(
        "m4",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!(
            "cells: {cells} · duration: {duration}s · replicates: {replicates} · \
             degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)"
        ),
    )
}
