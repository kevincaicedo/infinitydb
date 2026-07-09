//! `inf-sim` CLI (M0-S20): run a deterministic scenario, optionally twice,
//! comparing event traces byte-for-byte.
//!
//! ```text
//! inf-sim --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
//! ```
#![forbid(unsafe_code)]

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
                "--help" | "-h" => {
                    println!(
                        "inf-sim --scenario m0-smoke|m1-cache|m2-durable|m2-combined|boot-storm \
                         [--seed N|0xN] [--verify-determinism] \
                         [--plant lost-wakeup|fsync-lies] [--cells N] [--connections N] \
                         [--commands N] [--trace-out FILE] [--sweep N [--shard I/K] [--out DIR]]"
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
    if scenario_name == "m2-durable" {
        run_m2_durable(seed, plant, verify, sweep, shard, out_dir.as_deref());
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
        eprintln!("inf-sim: --sweep is an m2-durable / m2-combined mode");
        std::process::exit(2);
    }
    let mut scenario = match scenario_name.as_str() {
        "m0-smoke" => Scenario::m0_smoke(seed),
        "m1-cache" => Scenario::m1_cache(seed),
        other => {
            eprintln!(
                "inf-sim: unknown scenario {other} (have: m0-smoke, m1-cache, m2-durable, \
                 m2-combined, boot-storm)"
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

/// The m2-durable runner (M2-S19, ADR-0021 D5): one seed, or a sharded
/// sweep writing per-seed results + a manifest. Any failing seed replays
/// byte-identically via `--seed`.
fn run_m2_durable(
    seed: u64,
    plant: Plant,
    verify: bool,
    sweep: Option<u64>,
    (shard_i, shard_k): (u64, u64),
    out_dir: Option<&str>,
) {
    let run_one = |seed: u64| -> inf_sim::DurableReport {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.plant = plant;
        run_durable_scenario(&scenario)
    };

    let Some(sweep) = sweep else {
        let report = run_one(seed);
        println!(
            "inf-sim: scenario m2-durable seed {seed:#x}: {} commands, {} steps, {} keys \
             audited, {} required ops, {} allowed-lost, trace {} bytes, hash {:#018x}",
            report.commands_done,
            report.scheduler_steps,
            report.audited_keys,
            report.required_ops,
            report.allowed_lost_ops,
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
    for i in (shard_i..sweep).step_by(shard_k as usize) {
        let seed = seed.wrapping_add(i);
        let report = run_one(seed);
        ran += 1;
        sim_seconds += report.sim_seconds;
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
    println!(
        "inf-sim: m2-durable sweep shard {shard_i}/{shard_k}: {ran} seeds, {violations} \
         violations, {refused} legal taxonomy refusals"
    );
    println!("inf-sim: sim_seconds={sim_seconds:.6} published=0 delivered=0");
    if let Some(dir) = out_dir {
        std::fs::create_dir_all(dir).expect("--out dir");
        let manifest = format!(
            "scenario=m2-durable base_seed={seed:#x} sweep={sweep} shard={shard_i}/{shard_k} \
             plant={plant:?} seeds_run={ran} violations={violations} refused={refused}\n"
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
/// `run_m2_durable` (per-seed results + a manifest, `violations=0` gate);
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
