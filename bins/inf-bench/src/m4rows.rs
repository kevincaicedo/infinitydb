//! `inf-bench gate-run m4` (M4-S03 today; S22/S24 add the tiered YCSB and
//! campaign rows): the **memory-mode degenerate-case A/B** — the hard §22
//! sub-gate. An M3-tip `infinityd` (`--baseline-bin`) vs the M4 build over
//! the same memory-only gate mixes the C15/M2-S09 zero-cost A/B used,
//! crossover replicates (S24 instrument fix — see [`degenerate_row`]),
//! Δ ≤ 1% on every metric (throughput,
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

use std::collections::BTreeMap;
use std::time::Duration;

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, median, scrape_cells,
    spawn_infinityd, sum_field,
};
use crate::load::{LoadSpec, render, run as run_load};
use crate::m2rows::{delta_pct, rel_spread_pct};
use crate::writeamp;

/// Measurement keys for one degenerate A/B row.
struct RowKeys {
    ops_delta: &'static str,
    p999_delta: &'static str,
}

/// Per-leg sample vectors for one binary across a row's replicates.
#[derive(Default)]
struct LegSamples {
    ops: Vec<f64>,
    p999: Vec<f64>,
    rss: Vec<f64>,
}

/// What one A/B row hands back to the campaign loop: the per-leg RSS
/// medians (None without a baseline) and the last replicate's M4-leg
/// scrape, which the write-amplification disposition reads.
struct RowResult {
    rss_medians: Option<(f64, f64)>,
    m4_scrape: Vec<BTreeMap<String, String>>,
}

/// Runs one leg against `server` and records its report into `samples`.
fn run_leg(
    m: &mut Measurements,
    name: &str,
    label: &str,
    rep: usize,
    server: &ServerGuard,
    spec: &LoadSpec,
    samples: &mut LegSamples,
) -> Result<(), String> {
    let report = run_load(spec)?;
    println!("  rep {rep} {label}: {:.0} ops/s, p999 {} µs", report.ops_per_sec, report.p999_us);
    m.raw_section(&format!("{name} {label} rep {rep}"), &render(&report));
    samples.ops.push(report.ops_per_sec);
    samples.p999.push(report.p999_us as f64);
    samples.rss.push(server.rss_bytes() as f64);
    Ok(())
}

/// One crossover A/B row — the S24 instrument fix. The week-4 A/A control
/// (`.artifacts/m4/s03/week-4-risk-gate/verdict.md`) showed the
/// first-spawned slot's unpipelined p99.9 reading one LogHistogram bucket
/// high with *identical binaries*: the bias follows the slot (spawn order,
/// port draw, process lifetime), not the binary. Fix: servers respawn
/// fresh **per replicate** and the binary↔slot assignment alternates; legs
/// run in spawn order, so the slot and load-order nuisances move together
/// and alternate sign against the binary — both cancel in the leg medians
/// over an even replicate count. Both legs stay equally fresh within a
/// replicate (the fresh-server wire effect hits them symmetrically).
#[allow(clippy::too_many_arguments)] // orchestration row: linear, not branchy
fn degenerate_row(
    m: &mut Measurements,
    name: &str,
    keys: &RowKeys,
    replicates: usize,
    infinityd_bin: &str,
    baseline_bin: Option<&str>,
    cells: u16,
    server_extra: &[&str],
    spec_for: impl Fn(u16) -> LoadSpec,
) -> Result<RowResult, String> {
    println!("\n== row: {name} (crossover A/B × {replicates}) ==");
    let mut base = LegSamples::default();
    let mut m4 = LegSamples::default();
    let mut m4_scrape: Vec<BTreeMap<String, String>> = Vec::new();
    for rep in 0..replicates {
        // Slot order this replicate: (binary, is_m4) in spawn order.
        let order: Vec<(&str, bool)> = match baseline_bin {
            Some(base_bin) if rep.is_multiple_of(2) => {
                vec![(infinityd_bin, true), (base_bin, false)]
            }
            Some(base_bin) => vec![(base_bin, false), (infinityd_bin, true)],
            None => vec![(infinityd_bin, true)],
        };
        // Spawn every slot before any leg runs: the pre-fix shape kept
        // both servers resident on the same cpu set while one served —
        // the crossover changes assignment, never the concurrency shape.
        let servers = order
            .iter()
            .map(|(bin, _)| spawn_infinityd(bin, cells, server_extra))
            .collect::<Result<Vec<_>, String>>()?;
        for ((_, is_m4), server) in order.iter().zip(&servers) {
            let (label, samples) =
                if *is_m4 { ("m4", &mut m4) } else { ("m3-baseline", &mut base) };
            run_leg(m, name, label, rep, server, &spec_for(server.port), samples)?;
            if *is_m4 {
                // Audit the M4 leg before its server drops: both zero
                // tripwires bind per replicate now (each server lifetime
                // owns its own zero), and the last scrape feeds the row's
                // write-amplification disposition.
                let scrape = scrape_cells(server.port, cells)?;
                let appended = sum_field(&scrape, "log_records_appended");
                if appended != 0 {
                    return Err(format!(
                        "memory-only row `{name}` appended {appended} log records — the \
                         M2-S09 zero-cost contract is broken"
                    ));
                }
                assert_zero_tiering(&scrape, name)?;
                m4_scrape = scrape;
            }
        }
    }
    if baseline_bin.is_none() {
        return Ok(RowResult { rss_medians: None, m4_scrape });
    }
    let (a_ops, b_ops) = (median(&mut base.ops), median(&mut m4.ops));
    let (a_p999, b_p999) = (median(&mut base.p999), median(&mut m4.p999));
    let (a_rss, b_rss) = (median(&mut base.rss), median(&mut m4.rss));
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
        rel_spread_pct(&mut base.ops),
        rel_spread_pct(&mut m4.ops),
        delta_pct(a_rss, b_rss),
    ));
    Ok(RowResult { rss_medians: Some((a_rss, b_rss)), m4_scrape })
}

/// The report-enforced M4-S03 tripwire: every tiering code-path counter on
/// the M4 leg reads identically zero after the row (§3.3 "provably
/// unexecuted"). Returns the summed counter total (0 on pass); any nonzero
/// value aborts the run with the same teeth as the M2 zero-record assert.
fn assert_zero_tiering(
    scrape: &[BTreeMap<String, String>],
    row: &str,
) -> Result<(u64, u64), String> {
    let tables = sum_field(scrape, "tiering_tables");
    let total = sum_field(scrape, "tiering_tail_allocs")
        + sum_field(scrape, "tiering_seal_holes")
        + sum_field(scrape, "tiering_seal_hole_bytes")
        + sum_field(scrape, "tiering_region_commit_pages")
        + sum_field(scrape, "tiering_region_decommit_pages")
        + sum_field(scrape, "tiering_cold_resolves")
        // M4-S07 demotion/backpressure counters + L5 usage attribution —
        // all identically zero when no tiered table exists.
        + sum_field(scrape, "tiering_tail_alloc_stalls")
        + sum_field(scrape, "tiering_demote_slices")
        + sum_field(scrape, "tiering_demote_sealed_bytes")
        // M4-S11 flush-pipeline counters — identically zero without a
        // tiered table.
        + sum_field(scrape, "tiering_flush_slices")
        + sum_field(scrape, "tiering_flush_confirmed_bytes")
        // M4-S15 copy-forward slices — identically zero without a
        // tiered table.
        + sum_field(scrape, "tiering_compact_slices")
        + sum_field(scrape, "tiering_reserved_bytes")
        + sum_field(scrape, "tiering_committed_bytes")
        + sum_field(scrape, "tiering_allocated_bytes")
        + sum_field(scrape, "tiering_dead_bytes")
        + sum_field(scrape, "tiering_live_bytes")
        + sum_field(scrape, "tiering_index_bytes")
        // M4-S13 write-path accounting — a memory-mode node writes no
        // user, WAL, flush, or compaction byte *through a tiered
        // namespace*, so these are zero for the same structural reason
        // (no `TieredTable` exists to hold a counter).
        + sum_field(scrape, "tiering_user_bytes")
        + sum_field(scrape, "tiering_wal_bytes")
        + sum_field(scrape, "tiering_flush_bytes")
        + sum_field(scrape, "tiering_compaction_bytes")
        + sum_field(scrape, "tiering_written_bytes")
        // M4-S16 write amplification: no tiered namespace means no ratio
        // to report and none that cannot answer — both fields read zero
        // structurally, so they belong in the same zero-assert.
        + sum_field(scrape, "tiering_write_amp_milli_max")
        + sum_field(scrape, "tiering_write_amp_undefined_ns")
        // M4-S17 blob extents (ADR-0061 D8): no table, no extents — the
        // whole blob leg reads zero for the same structural reason.
        + sum_field(scrape, "tiering_blob_user_bytes")
        + sum_field(scrape, "tiering_blob_bytes")
        + sum_field(scrape, "tiering_blob_extents_live")
        + sum_field(scrape, "tiering_blob_extent_bytes_live")
        + sum_field(scrape, "tiering_blob_extents_created")
        + sum_field(scrape, "tiering_blob_extents_reclaimed")
        + sum_field(scrape, "tiering_blob_reclaim_slices")
        + sum_field(scrape, "tiering_blob_rmw_ops")
        // M4-S18: the blob ratio aggregates and the reclaim backlog —
        // zero without a table, same contract.
        + sum_field(scrape, "tiering_blob_write_amp_milli_max")
        + sum_field(scrape, "tiering_blob_write_amp_undefined_ns")
        + sum_field(scrape, "tiering_blob_reclaimable")
        + sum_field(scrape, "tiering_blob_reclaim_deferred")
        // M4-S19: extent disk occupancy — zero without a table.
        + sum_field(scrape, "tiering_blob_disk_bytes")
        // M4-S21 (ADR-0063 D5): disk-admission observables — no table,
        // no budget, no refusal, no alarm; same structural zero.
        + sum_field(scrape, "tiering_diskfull_ns")
        + sum_field(scrape, "tiering_diskfull_refusals")
        + sum_field(scrape, "tiering_compact_idle_pressure")
        + sum_field(scrape, "tiering_disk_used_bytes");
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
    // Default is even by design: the crossover cancels the slot bias in
    // the medians only when both assignments appear equally often.
    let replicates: usize = flags.usize_or("replicates", 6)?;
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
        "p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% \
         since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans \
         ≥ 1 bucket",
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
    m.note(
        "slot crossover active (week-4 instrument fix): servers respawn per replicate and the \
         binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias \
         cancels in the leg medians over an even replicate count",
    );
    if !replicates.is_multiple_of(2) {
        m.note(format!(
            "replicates {replicates} is odd: the slot crossover is unbalanced by one \
             replicate — prefer an even count for a binding tail verdict"
        ));
    }

    // The C15 row set — M0/M1 memory-only gate mixes, unchanged (S03:
    // `inf-bench gate-run m1` workloads remain the source; no bespoke
    // workloads). Fresh servers per replicate: each replicate owns its
    // keyspace state and its slot assignment (the crossover fix).
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
    let mut worst_write_amp: f64 = 0.0;
    for (name, keys, spec) in &rows {
        m.row_open(name);
        // Both zero tripwires now bind per replicate inside the row (each
        // server lifetime owns its own zero — M2 posture + the M4 §3.3
        // contract); the row returns the last M4-leg scrape for the
        // write-amplification disposition.
        let result = degenerate_row(
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
        if let Some((m3_rss, m4_rss)) = result.rss_medians {
            worst_rss_delta = worst_rss_delta.max(delta_pct(m3_rss, m4_rss));
        }
        let (tables, total) = assert_zero_tiering(&result.m4_scrape, name)?;
        tiering_tables_seen = tiering_tables_seen.max(tables);
        tiering_counter_total += total;
        // M4-S16: the row's write-amplification disposition, derived from
        // the same scrape and cross-checked against the server's derived
        // fields. On these memory-mode rows the honest answer is "no
        // tiered namespace" — and `disposition` proves that structurally
        // (no namespace line *and* no tiered table) instead of assuming it
        // because the row is called memory-mode.
        let disposition = writeamp::disposition(&result.m4_scrape)?;
        if let Some(value) = disposition.gate_value() {
            worst_write_amp = worst_write_amp.max(value); // worst row binds
        }
        // M4-S18 (ADR-0061 D8): the blob leg renders beside the record
        // leg, never inside it — same scrape, same pair assertions. The
        // record gate above is untouched; a blended figure would let a
        // quiet blob leg dilute a runaway record leg (or vice versa).
        let blob = writeamp::blob_disposition(&result.m4_scrape)?;
        m.row_write_amp(&format!("{} · blob: {}", disposition.render(), blob.render()));
    }
    if worst_write_amp > 0.0 {
        m.set("wa:write_amp_max", worst_write_amp);
    }
    // M4-S16: a write-amplification figure measured by tooling this
    // harness cannot host binds the gate row here. It exists because the
    // tiered rows are S19/S22-owned — `infinityd` has no way to create a
    // tiered namespace yet, so the only WA a live server can report today
    // is the memory-mode "none". The store-side harness
    // (`cargo test -p inf-store --test tiered_write_amp`) measures the
    // ratio, this flag carries it, and `--campaign-note` carries where it
    // came from: a number without provenance is not evidence (L10).
    if let Some(raw) = flags.get("write-amp-milli") {
        let milli: u64 = raw
            .parse()
            .map_err(|_| format!("--write-amp-milli: `{raw}` is not an integer (milli-units)"))?;
        if flags.get("campaign-note").is_none() {
            return Err("--write-amp-milli without --campaign-note: quote the harness and \
                        artifact the figure came from (L10)"
                .into());
        }
        let value = milli as f64 / 1_000.0;
        m.set("wa:write_amp_max", value);
        m.note(format!(
            "write amplification {value:.3}× supplied externally (milli {milli}) — the gate row \
             binds this value; the memory-mode rows above report `n/a` because no tiered \
             namespace exists on a node this harness can build yet (S19/S22 own the tiered rows)"
        ));
    }
    // M4-S24 campaign carriers: §7 rows measured by tooling this harness
    // cannot host (mixed-audit sampler, soak-m4 verdict, recovery gate,
    // DST sweeps, crash matrix, storm artifacts, the M3 gate set). Each
    // flag binds its gate row into the campaign table and demands
    // --campaign-note provenance — the --write-amp-milli precedent (L10:
    // a number without provenance is not evidence).
    for (flag, key) in [
        ("mixed-attribution-pct", "mixed_attribution_divergence_pct"),
        ("cache-isolation-pct", "cache_isolation_p99_delta_pct"),
        ("recovery-gbps-per-cell", "recovery:tiered_gbps_per_cell"),
        ("recovery-boot-s", "recovery:tiered_10gb_boot_s"),
        // ADR-0070 D7: Phase::Start overhead, split out of the replay row.
        ("recovery-setup-s", "recovery:tiered_setup_s"),
        ("dst-violations", "dst:never_none_violations"),
        ("crash-failures", "crash:matrix_failures"),
        ("m3-regression-pct", "m3:regression_worst_pct"),
        ("foreground-p999-ms", "storm:foreground_p999_ms"),
        ("endurance-rss-slope-pct", "endurance:rss_slope_pct_per_24h"),
        ("endurance-crashes", "endurance:crashes"),
        // Hot-set split deltas: computed from the two ycsb legs (tiered
        // vs --dataset-multiple 1 reference) per the S24 runbook.
        ("hot-set-p50-pct", "ycsb:hot_set_p50_delta_pct"),
        ("hot-set-p99-pct", "ycsb:hot_set_p99_delta_pct"),
        ("hot-set-p999-pct", "ycsb:hot_set_p999_delta_pct"),
        ("cold-read-p99-ms", "ycsb:cold_read_p99_ms"),
    ] {
        let Some(raw) = flags.get(flag) else { continue };
        let value: f64 = raw.parse().map_err(|_| format!("--{flag}: `{raw}` is not a number"))?;
        if flags.get("campaign-note").is_none() {
            return Err(format!(
                "--{flag} without --campaign-note: quote the harness and artifact the \
                 figure came from (L10)"
            ));
        }
        m.set(key, value);
        m.note(format!("{key} = {value} supplied externally (--{flag}; see --campaign-note)"));
    }
    if let Some(note) = flags.get("campaign-note") {
        m.note(format!("campaign: {note}"));
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
