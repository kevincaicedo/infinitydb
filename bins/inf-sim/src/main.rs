//! `inf-sim` CLI (M0-S20): run a deterministic scenario, optionally twice,
//! comparing event traces byte-for-byte.
//!
//! ```text
//! inf-sim --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
//! ```
#![forbid(unsafe_code)]

use inf_sim::RecoveryScenario;
use inf_sim::net::Plant;
use inf_sim::{
    CombinedScenario, DurableScenario, Scenario, run_combined_scenario, run_durable_scenario,
    run_scenario,
};

fn parse_seed(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| format!("seed: {e}"))
    } else {
        text.parse().map_err(|e| format!("seed: {e}"))
    }
}

fn main() {
    let mut scenario_name = "m0-smoke".to_string();
    let mut seed = 0xC0FFEEu64;
    let mut verify = false;
    let mut plant = Plant::None;
    let mut overrides: Vec<(String, u64)> = Vec::new();
    let mut trace_out: Option<String> = None;

    let mut sweep: Option<u64> = None;
    let mut shard = (0u64, 1u64);
    let mut out_dir: Option<String> = None;
    let mut replay_canary = false;
    let mut ops_override: Option<u64> = None;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut take = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        let result: Result<(), String> = (|| {
            match flag.as_str() {
                "--scenario" => scenario_name = take("--scenario")?,
                "--seed" => seed = parse_seed(&take("--seed")?)?,
                "--verify-determinism" => verify = true,
                "--plant" => {
                    plant = match take("--plant")?.as_str() {
                        "lost-wakeup" => Plant::LostWakeup,
                        "fsync-lies" => Plant::FsyncLies,
                        other => return Err(format!("unknown plant {other}")),
                    }
                }
                "--cells" | "--connections" | "--commands" | "--key-space" => {
                    let value = take(&flag)?.parse().map_err(|e| format!("{flag}: {e}"))?;
                    overrides.push((flag.clone(), value));
                }
                "--trace-out" => trace_out = Some(take("--trace-out")?),
                "--sweep" => {
                    sweep = Some(take("--sweep")?.parse().map_err(|e| format!("--sweep: {e}"))?)
                }
                "--shard" => {
                    let spec = take("--shard")?;
                    let (i, k) = spec
                        .split_once('/')
                        .ok_or_else(|| format!("--shard wants I/K, got {spec}"))?;
                    shard = (
                        i.parse().map_err(|e| format!("--shard: {e}"))?,
                        k.parse().map_err(|e| format!("--shard: {e}"))?,
                    );
                }
                "--out" => out_dir = Some(take("--out")?),
                "--replay-canary" => replay_canary = true,
                // m4-cold: total op count (the AC's 10⁶ run sets it; the
                // smoke default is lighter).
                "--ops" => {
                    ops_override = Some(take("--ops")?.parse().map_err(|e| format!("--ops: {e}"))?);
                }
                "--help" | "-h" => {
                    println!(
                        "inf-sim --scenario m0-smoke|m1-cache|m2-durable|m3-document|m2-combined|boot-storm \
                         [--seed N|0xN] [--verify-determinism] \
                         [--plant lost-wakeup|fsync-lies] [--replay-canary] [--cells N] \
                         [--connections N] [--commands N] [--trace-out FILE] \
                         [--sweep N [--shard I/K] [--out DIR]]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown flag {other}")),
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("inf-sim: {e}");
            std::process::exit(2);
        }
    }

    // The M2-S19 durable scenario has its own runner (power cuts, the
    // durability oracle, the sweep mode).
    if matches!(scenario_name.as_str(), "m2-durable" | "m3-document") {
        run_durable(
            &scenario_name,
            seed,
            plant,
            replay_canary,
            verify,
            sweep,
            shard,
            out_dir.as_deref(),
        );
        return;
    }
    // The M2.5-S14 combined scenario (durable + memory + pub/sub + expiry
    // on one node; its own runner — power cuts, the composed oracles, the
    // sweep mode). `--plant` does not apply (its planted bugs are source
    // mutations, per the S14 demonstrations).
    if scenario_name == "m2-combined" {
        run_m2_combined(seed, verify, sweep, shard, out_dir.as_deref());
        return;
    }
    // The M4-S04 steel-thread scenario (tiered lifecycle + cold reads
    // through suspension + the S06 crash/replay content oracle).
    if scenario_name == "m4-steel" {
        let scenario = inf_sim::SteelScenario::m4_steel(seed);
        let report = inf_sim::run_steel_scenario(&scenario);
        println!(
            "inf-sim: scenario m4-steel seed {seed:#x}: {} cold reads, {} promotions, \
             hash {:#018x}",
            report.cold_reads, report.promotions, report.trace_hash
        );
        if verify {
            let second = inf_sim::run_steel_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "m4-steel determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    // The M4.5-S05 backfill scenario (crash at every backfill phase;
    // ready only with oracle-verified contents — ADR-0077).
    if scenario_name == "m45-backfill" {
        let scenario = inf_sim::BackfillScenario::m45_backfill(seed);
        let report = inf_sim::run_backfill_scenario(&scenario);
        println!(
            "inf-sim: scenario m45-backfill seed {seed:#x}: {} boots, cuts {:?}, \
             {} ready checks, {} refused bindings, {} raced mutations, {} steps, \
             hash {:#018x}",
            report.boots,
            report.cuts,
            report.ready_checks,
            report.refused_bindings,
            report.raced_mutations,
            report.scheduler_steps,
            report.trace_hash
        );
        if verify {
            let second = inf_sim::run_backfill_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "m45-backfill determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    // The M4-S21 disk-budget admission scenario (typed DISKFULL, refusal
    // purity, the compaction reserve, automatic recovery — ADR-0063).
    if scenario_name == "m4-diskfull" {
        let scenario = inf_sim::DiskfullScenario::m4_diskfull(seed);
        let report = inf_sim::run_diskfull_scenario(&scenario);
        println!(
            "inf-sim: scenario m4-diskfull seed {seed:#x}: {} refusals, {} reopens, \
             {} B relocated, {} files retired, peak disk {} B, {} keys verified, hash {:#018x}",
            report.refusals,
            report.reopens,
            report.relocated_bytes,
            report.retired_files,
            report.peak_disk_used,
            report.keys_verified,
            report.trace_hash
        );
        if verify {
            let second = inf_sim::run_diskfull_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "m4-diskfull determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    // The M4-S07 throttled-device backpressure scenario (budget bound,
    // typed stall timeouts, deadlock freedom — ADR-0053).
    if scenario_name == "m4-pressure" {
        let scenario = inf_sim::PressureScenario::m4_pressure(seed);
        let report = inf_sim::run_pressure_scenario(&scenario);
        println!(
            "inf-sim: scenario m4-pressure seed {seed:#x}: {} stalls (p50 {} µs, p99 {} µs), \
             {} timeouts, peak committed {} B, hash {:#018x}",
            report.stalls,
            report.stall_p50_ns / 1_000,
            report.stall_p99_ns / 1_000,
            report.stall_timeouts,
            report.peak_committed_bytes,
            report.trace_hash
        );
        if verify {
            let second = inf_sim::run_pressure_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "m4-pressure determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    // The M4-S08 cold-read storm (placement, relocation races, chunked
    // staging, cancellation, pin-deferred unlinks).
    if scenario_name == "m4-recovery" {
        let run_one = |seed: u64| {
            let scenario = RecoveryScenario::m4_recovery(seed);
            inf_sim::run_recovery_scenario(&scenario)
        };
        if let Some(sweep) = sweep {
            let (shard_i, shard_k) = shard;
            assert!(shard_k > 0 && shard_i < shard_k, "--shard I/K wants I < K");
            let mut lines = Vec::new();
            let mut violations = 0u64;
            let mut ran = 0u64;
            let mut refs = 0u64;
            let mut images = 0u64;
            let mut cuts_before = 0u64;
            let mut flush_lag = 0u64;
            let mut live_entries = 0u64;
            let mut relocations = 0u64;
            let mut files_retired = 0u64;
            let mut files_unlinked = 0u64;
            let mut boot_gc = 0u64;
            let mut blobs = 0u64;
            let mut blob_orphans = 0u64;
            let mut blob_reclaims = 0u64;
            for i in (shard_i..sweep).step_by(shard_k as usize) {
                let seed = seed.wrapping_add(i);
                let report = run_one(seed);
                ran += 1;
                refs += report.refs_emitted;
                images += report.images_emitted;
                cuts_before += report.cut_before_publish;
                flush_lag += report.flush_lag_lives;
                live_entries += report.live_entries_emitted;
                relocations += report.relocations;
                files_retired += report.files_retired;
                files_unlinked += report.files_unlinked;
                boot_gc += report.unlinks_left_to_boot_gc;
                blobs += report.blobs_written;
                blob_orphans += report.blob_orphans_planted;
                blob_reclaims += report.blob_extents_reclaimed;
                if report.ok() {
                    lines.push(format!("{seed:#x} ok"));
                } else {
                    violations += 1;
                    let first = report.violations.first().map_or("?", |v| v.as_str());
                    lines.push(format!("{seed:#x} VIOLATION {first}"));
                    eprintln!("inf-sim: seed {seed:#x}: {first}");
                }
            }
            // Coverage disclosed, never assumed (ADR-0045 D4): the seed
            // classes must actually occur across the shard.
            println!(
                "inf-sim: m4-recovery sweep shard {shard_i}/{shard_k}: {ran} seeds, \
                 {violations} violations, {refs} refs, {images} images, \
                 {cuts_before} cut-before-publish lives, {flush_lag} flush-lag lives, \
                 {live_entries} live-set entries, {relocations} relocations, \
                 {files_retired} retired, {files_unlinked} unlinked, \
                 {boot_gc} left-to-boot-gc, {blobs} blobs, {blob_orphans} orphans-planted, \
                 {blob_reclaims} blob-reclaims"
            );
            if let Some(dir) = out_dir {
                std::fs::create_dir_all(&dir).expect("--out dir");
                let manifest = format!(
                    "scenario=m4-recovery base_seed={seed:#x} sweep={sweep} \
                     shard={shard_i}/{shard_k} seeds_run={ran} violations={violations} \
                     refs={refs} images={images} cut_before_publish={cuts_before} \
                     flush_lag_lives={flush_lag} live_set_entries={live_entries} \
                     relocations={relocations} files_retired={files_retired} \
                     files_unlinked={files_unlinked} unlinks_boot_gc={boot_gc} \
                     blobs={blobs} blob_orphans={blob_orphans} blob_reclaims={blob_reclaims}\n"
                );
                std::fs::write(format!("{dir}/manifest-shard-{shard_i}.txt"), manifest)
                    .expect("manifest");
                std::fs::write(
                    format!("{dir}/results-shard-{shard_i}.txt"),
                    lines.join("\n") + "\n",
                )
                .expect("results");
            }
            std::process::exit(if violations > 0 { 1 } else { 0 });
        }
        let report = run_one(seed);
        println!(
            "inf-sim: m4-recovery seed {seed:#x}: {} lives, {} refs, {} images, {} tail \
             records, {} cut-before-publish, {} flush-lag, {} keys audited, {} live-set \
             entries, {} relocations, {} retired, {} unlinked, {} left-to-boot-gc, \
             {} blobs, {} orphans-planted, {} blob-reclaims, trace {:#x}",
            report.lives,
            report.refs_emitted,
            report.images_emitted,
            report.tail_records,
            report.cut_before_publish,
            report.flush_lag_lives,
            report.keys_audited,
            report.live_entries_emitted,
            report.relocations,
            report.files_retired,
            report.files_unlinked,
            report.unlinks_left_to_boot_gc,
            report.blobs_written,
            report.blob_orphans_planted,
            report.blob_extents_reclaimed,
            report.trace_hash
        );
        if verify {
            let twin = run_one(seed);
            assert_eq!(report.trace_hash, twin.trace_hash, "determinism violated (L7)");
            println!("inf-sim: determinism verified (two runs, identical traces)");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }

    // The M4-S26 command-driven tiered node (RESP over the sim net
    // against the wired plane: cut → recover → audit → re-pressure →
    // DISKFULL clamp → drop race). Sweep mode mirrors m4-recovery.
    if scenario_name == "m4-tiered" {
        let run_one = |seed: u64| {
            let scenario = inf_sim::TieredScenario::m4_tiered(seed);
            inf_sim::run_tiered_scenario(&scenario)
        };
        if let Some(sweep) = sweep {
            let (shard_i, shard_k) = shard;
            assert!(shard_k > 0 && shard_i < shard_k, "--shard I/K wants I < K");
            let mut lines = Vec::new();
            let mut violations = 0u64;
            let mut refused = 0u64;
            let mut ran = 0u64;
            let mut sim_seconds = 0.0f64;
            let mut commands = 0u64;
            let mut audited = 0u64;
            let mut flushed_pre_cut = 0u64;
            let mut cold_resolves = 0u64;
            let mut blob_sets = 0u64;
            let mut diskfull_refusals = 0u64;
            let mut drop_values = 0u64;
            let mut drop_other = 0u64;
            for i in (shard_i..sweep).step_by(shard_k as usize) {
                let seed = seed.wrapping_add(i);
                let report = run_one(seed);
                ran += 1;
                sim_seconds += report.sim_seconds;
                commands += report.commands_done;
                audited += report.audited_keys;
                flushed_pre_cut += report.flushed_pre_cut_bytes;
                cold_resolves += report.cold_resolves;
                blob_sets += report.blob_sets;
                diskfull_refusals += report.diskfull_refusals;
                drop_values += report.drop_replies_value;
                drop_other += report.drop_replies_other;
                if report.refused_boot && report.ok() {
                    refused += 1;
                    lines.push(format!("{seed:#x} refused (taxonomy fail-stop)"));
                } else if report.ok() {
                    lines.push(format!("{seed:#x} ok"));
                } else {
                    violations += 1;
                    let first = report.violations.first().map_or("stall", |v| v.as_str());
                    lines.push(format!("{seed:#x} VIOLATION {first}"));
                    eprintln!("inf-sim: seed {seed:#x}: {first}");
                }
            }
            // Coverage disclosed, never assumed (ADR-0045 D4): demotion,
            // cold reads, blobs, refusals, and both drop-race outcomes
            // must actually occur across the shard.
            println!(
                "inf-sim: m4-tiered sweep shard {shard_i}/{shard_k}: {ran} seeds, {violations} \
                 violations, {refused} legal taxonomy refusals, {commands} commands, {audited} \
                 keys audited, {flushed_pre_cut} B flushed pre-cut, {cold_resolves} cold \
                 resolves, {blob_sets} blob sets, {diskfull_refusals} DISKFULL refusals, \
                 drop-race {drop_values} values / {drop_other} typed-other"
            );
            println!("inf-sim: sim_seconds={sim_seconds:.6} published=0 delivered=0");
            if let Some(dir) = out_dir {
                std::fs::create_dir_all(&dir).expect("--out dir");
                let manifest = format!(
                    "scenario=m4-tiered base_seed={seed:#x} sweep={sweep} \
                     shard={shard_i}/{shard_k} seeds_run={ran} violations={violations} \
                     refused={refused} commands={commands} keys_audited={audited} \
                     flushed_pre_cut={flushed_pre_cut} cold_resolves={cold_resolves} \
                     blob_sets={blob_sets} diskfull_refusals={diskfull_refusals} \
                     drop_values={drop_values} drop_other={drop_other}\n"
                );
                std::fs::write(format!("{dir}/manifest-shard-{shard_i}.txt"), manifest)
                    .expect("manifest");
                std::fs::write(
                    format!("{dir}/results-shard-{shard_i}.txt"),
                    lines.join("\n") + "\n",
                )
                .expect("results");
            }
            std::process::exit(if violations > 0 { 1 } else { 0 });
        }
        let report = run_one(seed);
        println!(
            "inf-sim: m4-tiered seed {seed:#x}: {} commands, {} steps, {} keys audited, {} \
             required ops, {} allowed-lost, {} B flushed pre-cut, {} B flushed final, {} cold \
             resolves, {} blob sets, {} DISKFULL refusals (reopened: {}), drop-race {} values / \
             {} typed-other, refused-boot {}, trace {} bytes, hash {:#018x}",
            report.commands_done,
            report.scheduler_steps,
            report.audited_keys,
            report.required_ops,
            report.allowed_lost_ops,
            report.flushed_pre_cut_bytes,
            report.flushed_final_bytes,
            report.cold_resolves,
            report.blob_sets,
            report.diskfull_refusals,
            report.diskfull_reopened,
            report.drop_replies_value,
            report.drop_replies_other,
            report.refused_boot,
            report.trace.len(),
            report.trace_hash
        );
        println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
        if verify {
            let second = run_one(seed);
            if second.trace != report.trace {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — traces differ ({} vs {} bytes, {:#x} vs \
                     {:#x})",
                    report.trace.len(),
                    second.trace.len(),
                    report.trace_hash,
                    second.trace_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run trace byte-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }

    if scenario_name == "m4-cold" {
        let mut scenario = inf_sim::ColdStormScenario::m4_cold(seed);
        if let Some(ops) = ops_override {
            scenario.ops = ops;
        }
        let report = inf_sim::run_cold_storm_scenario(&scenario);
        println!(
            "inf-sim: scenario m4-cold seed {seed:#x}: {} gets ({} cold, {} chunked, {} \
             restarts, {} cancelled [{} early / {} late]), {} merged waiters (queue hw {}), \
             {} unlinks ({} deferrals), hash {:#018x}",
            report.gets,
            report.cold_served,
            report.chunked_reads,
            report.restarts,
            report.cancelled,
            report.cancelled_early,
            report.cancelled_late,
            report.merged_waiters,
            report.queue_high_water,
            report.unlinks,
            report.unlink_deferrals,
            report.trace_hash
        );
        if verify {
            let second = inf_sim::run_cold_storm_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "m4-cold determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    // The M2.5-S01 boot-storm scenario (wedge-class oracle).
    if scenario_name == "boot-storm" {
        let scenario = inf_sim::BootStormScenario::m2_boot_storm(seed);
        let report = inf_sim::run_boot_storm_scenario(&scenario);
        println!(
            "inf-sim: scenario boot-storm seed {seed:#x}: {} boots, ready-steps max {}, \
             hash {:#018x}",
            report.boots, report.ready_steps_max, report.trace_hash
        );
        if verify {
            let second = inf_sim::run_boot_storm_scenario(&scenario);
            assert_eq!(
                report.trace_hash, second.trace_hash,
                "boot-storm determinism: second run diverged"
            );
            println!("inf-sim: determinism verified — second run hash-identical");
        }
        if !report.ok() {
            for v in &report.violations {
                eprintln!("inf-sim: VIOLATION: {v}");
            }
            std::process::exit(1);
        }
        return;
    }
    if sweep.is_some() {
        eprintln!("inf-sim: --sweep is an m2-durable / m3-document / m2-combined mode");
        std::process::exit(2);
    }
    let mut scenario = match scenario_name.as_str() {
        "m0-smoke" => Scenario::m0_smoke(seed),
        "m1-cache" => Scenario::m1_cache(seed),
        other => {
            eprintln!(
                "inf-sim: unknown scenario {other} (have: m0-smoke, m1-cache, m2-durable, \
                 m3-document, m2-combined, boot-storm, m4-steel, m4-pressure, m4-cold, \
                 m4-recovery, m4-diskfull, m4-tiered)"
            );
            std::process::exit(2);
        }
    };
    scenario.plant = plant;
    for (flag, value) in overrides {
        match flag.as_str() {
            "--cells" => scenario.cells = value as u16,
            "--connections" => scenario.connections = value as usize,
            "--commands" => scenario.commands = value,
            "--key-space" => scenario.key_space = value,
            _ => unreachable!(),
        }
    }

    let report = run_scenario(&scenario);
    println!(
        "inf-sim: scenario {scenario_name} seed {seed:#x}: {} commands, {} apply events, \
         {} steps, trace {} bytes, hash {:#018x}",
        report.commands_done,
        report.events,
        report.scheduler_steps,
        report.trace.len(),
        report.trace_hash
    );
    // Machine-readable line for the nightly fleet (sim-seconds budget sum).
    println!(
        "inf-sim: sim_seconds={:.6} published={} delivered={}",
        report.sim_seconds, report.published, report.delivered
    );
    if let Some(path) = &trace_out
        && let Err(e) = std::fs::write(path, &report.trace)
    {
        eprintln!("inf-sim: --trace-out {path}: {e}");
        std::process::exit(2);
    }
    for violation in report.oracle_violations.iter().take(5) {
        eprintln!("inf-sim: ORACLE VIOLATION: {violation}");
    }
    if report.stalled {
        eprintln!("inf-sim: STALL — no progress for the detector window (seed {seed:#x})");
    }
    if !report.ok() {
        std::process::exit(1);
    }

    if verify {
        let second = run_scenario(&scenario);
        if second.trace != report.trace {
            eprintln!(
                "inf-sim: DETERMINISM VIOLATION — traces differ ({} vs {} bytes, {:#x} vs {:#x})",
                report.trace.len(),
                second.trace.len(),
                report.trace_hash,
                second.trace_hash
            );
            std::process::exit(1);
        }
        println!("inf-sim: determinism verified — second run trace byte-identical");
    }
}

/// Shared durable runner: M2's key/value workload and M3's document
/// workload differ only in scenario construction. Both write replayable
/// per-seed results and a sweep manifest.
#[allow(clippy::too_many_arguments)] // one linear CLI dispatch, like main
fn run_durable(
    scenario_name: &str,
    seed: u64,
    plant: Plant,
    replay_canary: bool,
    verify: bool,
    sweep: Option<u64>,
    (shard_i, shard_k): (u64, u64),
    out_dir: Option<&str>,
) {
    let run_one = |seed: u64| -> inf_sim::DurableReport {
        let mut scenario = match scenario_name {
            "m2-durable" => DurableScenario::m2_durable(seed),
            "m3-document" => DurableScenario::m3_document(seed),
            _ => unreachable!("the caller filters durable scenario names"),
        };
        scenario.plant = plant;
        scenario.replay_canary = replay_canary;
        run_durable_scenario(&scenario)
    };

    let Some(sweep) = sweep else {
        let report = run_one(seed);
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} commands, {} steps, {} keys \
             audited, {} required ops, {} allowed-lost, {} equivalence checks, {} documents \
             compared, {} corpus docs, cut classes {:?}, trace {} bytes, hash {:#018x}",
            report.commands_done,
            report.scheduler_steps,
            report.audited_keys,
            report.required_ops,
            report.allowed_lost_ops,
            report.equivalence_checks,
            report.documents_compared,
            report.corpus_documents_used,
            report.cut_classes,
            report.trace.len(),
            report.trace_hash
        );
        println!("inf-sim: sim_seconds={:.6} published=0 delivered=0", report.sim_seconds);
        for violation in report.violations.iter().take(5) {
            eprintln!("inf-sim: ORACLE VIOLATION: {violation}");
        }
        if report.stalled {
            eprintln!("inf-sim: STALL (seed {seed:#x})");
        }
        if !report.ok() {
            std::process::exit(1);
        }
        if verify {
            let second = run_one(seed);
            if second.trace != report.trace {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — traces differ ({} vs {} bytes)",
                    report.trace.len(),
                    second.trace.len()
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run trace byte-identical");
        }
        return;
    };

    // Sweep mode: seeds base+i for i ≡ shard_i (mod shard_k).
    assert!(shard_k > 0 && shard_i < shard_k, "--shard I/K wants I < K");
    let mut lines = Vec::new();
    let mut violations = 0u64;
    let mut refused = 0u64;
    let mut ran = 0u64;
    let mut sim_seconds = 0.0f64;
    let mut equivalence_checks = 0u64;
    let mut documents_compared = 0u64;
    let mut corpus_documents = 0u64;
    let mut cut_classes: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    for i in (shard_i..sweep).step_by(shard_k as usize) {
        let seed = seed.wrapping_add(i);
        let report = run_one(seed);
        ran += 1;
        sim_seconds += report.sim_seconds;
        equivalence_checks += report.equivalence_checks;
        documents_compared += report.documents_compared;
        corpus_documents += report.corpus_documents_used;
        for class in &report.cut_classes {
            *cut_classes.entry(class).or_default() += 1;
        }
        if report.refused_boot && report.ok() {
            // Legal §8.4 refusal (ADR-0018 taxonomy) — disclosed, not a
            // violation.
            refused += 1;
            lines.push(format!("{seed:#x} refused (taxonomy fail-stop)"));
        } else if report.ok() {
            lines.push(format!("{seed:#x} ok"));
        } else {
            violations += 1;
            let first = report.violations.first().map_or("stall", |v| v.as_str());
            lines.push(format!("{seed:#x} VIOLATION {first}"));
            eprintln!("inf-sim: seed {seed:#x}: {first}");
        }
    }
    // Cut-class distribution (ADR-0045 D4): coverage is disclosed in the
    // manifest, never assumed from the random process.
    let classes: Vec<String> =
        cut_classes.iter().map(|(class, count)| format!("{class}:{count}")).collect();
    println!(
        "inf-sim: {scenario_name} sweep shard {shard_i}/{shard_k}: {ran} seeds, {violations} \
         violations, {refused} legal taxonomy refusals, {equivalence_checks} equivalence \
         checks, {documents_compared} documents compared, cut classes [{}]",
        classes.join(" ")
    );
    println!("inf-sim: sim_seconds={sim_seconds:.6} published=0 delivered=0");
    if let Some(dir) = out_dir {
        std::fs::create_dir_all(dir).expect("--out dir");
        let manifest = format!(
            "scenario={scenario_name} base_seed={seed:#x} sweep={sweep} shard={shard_i}/{shard_k} \
             plant={plant:?} replay_canary={replay_canary} seeds_run={ran} \
             violations={violations} refused={refused} equivalence_checks={equivalence_checks} \
             documents_compared={documents_compared} corpus_documents={corpus_documents} \
             cut_classes=[{}]\n",
            classes.join(" ")
        );
        std::fs::write(format!("{dir}/manifest-shard-{shard_i}.txt"), manifest).expect("manifest");
        std::fs::write(format!("{dir}/results-shard-{shard_i}.txt"), lines.join("\n") + "\n")
            .expect("results");
    }
    if violations > 0 {
        std::process::exit(1);
    }
}

/// The M2.5-S14 combined runner: one seed, or a sharded sweep. Mirrors
/// the shared durable runner (per-seed results + a manifest,
/// `violations=0` gate);
/// any failing seed replays byte-identically via `--seed`.
fn run_m2_combined(
    seed: u64,
    verify: bool,
    sweep: Option<u64>,
    (shard_i, shard_k): (u64, u64),
    out_dir: Option<&str>,
) {
    let run_one = |seed: u64| -> inf_sim::CombinedReport {
        run_combined_scenario(&CombinedScenario::m2_combined(seed))
    };

    let Some(sweep) = sweep else {
        let report = run_one(seed);
        println!(
            "inf-sim: scenario m2-combined seed {seed:#x}: {} commands, {} steps, {} keys \
             audited, {} memory keys audited, {} required ops, {} allowed-lost, {} published, \
             {} delivered, stall-max-always {} ms, trace {} bytes, hash {:#018x}",
            report.commands_done,
            report.scheduler_steps,
            report.audited_keys,
            report.memory_keys_audited,
            report.required_ops,
            report.allowed_lost_ops,
            report.published,
            report.delivered,
            report.always_ack_latency_ms_max,
            report.trace.len(),
            report.trace_hash
        );
        println!(
            "inf-sim: sim_seconds={:.6} published={} delivered={}",
            report.sim_seconds, report.published, report.delivered
        );
        for violation in report.violations.iter().take(5) {
            eprintln!("inf-sim: ORACLE VIOLATION: {violation}");
        }
        if report.stalled {
            eprintln!("inf-sim: STALL (seed {seed:#x})");
        }
        if !report.ok() {
            std::process::exit(1);
        }
        if verify {
            let second = run_one(seed);
            if second.trace != report.trace {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — traces differ ({} vs {} bytes)",
                    report.trace.len(),
                    second.trace.len()
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run trace byte-identical");
        }
        return;
    };

    assert!(shard_k > 0 && shard_i < shard_k, "--shard I/K wants I < K");
    let mut lines = Vec::new();
    let mut violations = 0u64;
    let mut refused = 0u64;
    let mut ran = 0u64;
    let mut sim_seconds = 0.0f64;
    let mut published = 0u64;
    let mut delivered = 0u64;
    for i in (shard_i..sweep).step_by(shard_k as usize) {
        let seed = seed.wrapping_add(i);
        let report = run_one(seed);
        ran += 1;
        sim_seconds += report.sim_seconds;
        published += report.published;
        delivered += report.delivered;
        if report.refused_boot && report.ok() {
            refused += 1;
            lines.push(format!("{seed:#x} refused (taxonomy fail-stop)"));
        } else if report.ok() {
            lines.push(format!("{seed:#x} ok"));
        } else {
            violations += 1;
            let first = report.violations.first().map_or("stall", |v| v.as_str());
            lines.push(format!("{seed:#x} VIOLATION {first}"));
            eprintln!("inf-sim: seed {seed:#x}: {first}");
        }
    }
    println!(
        "inf-sim: m2-combined sweep shard {shard_i}/{shard_k}: {ran} seeds, {violations} \
         violations, {refused} legal taxonomy refusals"
    );
    println!("inf-sim: sim_seconds={sim_seconds:.6} published={published} delivered={delivered}");
    if let Some(dir) = out_dir {
        std::fs::create_dir_all(dir).expect("--out dir");
        let manifest = format!(
            "scenario=m2-combined base_seed={seed:#x} sweep={sweep} shard={shard_i}/{shard_k} \
             seeds_run={ran} violations={violations} refused={refused}\n"
        );
        std::fs::write(format!("{dir}/manifest-shard-{shard_i}.txt"), manifest).expect("manifest");
        std::fs::write(format!("{dir}/results-shard-{shard_i}.txt"), lines.join("\n") + "\n")
            .expect("results");
    }
    if violations > 0 {
        std::process::exit(1);
    }
}
