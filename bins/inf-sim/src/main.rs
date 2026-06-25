//! `inf-sim` CLI (M0-S20): run a deterministic scenario, optionally twice,
//! comparing event traces byte-for-byte.
//!
//! ```text
//! inf-sim --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
//! ```
#![forbid(unsafe_code)]

use inf_sim::net::{
    Plant,
    crash_matrix::{
        public_fsync_err_process_fail_stop,
        public_live_checkpoint_wait_dir_fsync_process_fail_stop,
        public_log_append_write_fault_process_fail_stop, run_m2_crash_matrix_rows,
        run_public_durable_recovery_sweep, run_public_everysec_recovery_sweep,
        run_public_everysec_workload_sweep,
    },
};
use inf_sim::{DurabilitySweepConfig, Scenario, run_durability_sweep, run_scenario};

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
    let mut sweep_seeds: Option<u64> = None;
    let mut sweep_shard_count: Option<u64> = None;
    let mut sweep_shard_index: Option<u64> = None;
    let mut writes_per_seed: Option<u64> = None;
    let mut trace_out: Option<String> = None;

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
                        other => return Err(format!("unknown plant {other}")),
                    }
                }
                "--cells" | "--connections" | "--commands" | "--key-space" => {
                    let value = take(&flag)?.parse().map_err(|e| format!("{flag}: {e}"))?;
                    overrides.push((flag.clone(), value));
                }
                "--sweep-seeds" => {
                    sweep_seeds =
                        Some(take("--sweep-seeds")?.parse().map_err(|e| format!("{flag}: {e}"))?);
                }
                "--sweep-shard-count" => {
                    sweep_shard_count = Some(
                        take("--sweep-shard-count")?.parse().map_err(|e| format!("{flag}: {e}"))?,
                    );
                }
                "--sweep-shard-index" => {
                    sweep_shard_index = Some(
                        take("--sweep-shard-index")?.parse().map_err(|e| format!("{flag}: {e}"))?,
                    );
                }
                "--writes-per-seed" => {
                    writes_per_seed = Some(
                        take("--writes-per-seed")?.parse().map_err(|e| format!("{flag}: {e}"))?,
                    );
                }
                "--trace-out" => trace_out = Some(take("--trace-out")?),
                "--help" | "-h" => {
                    println!(
                        "inf-sim --scenario m0-smoke|m1-cache|m2-durability-oracle \
                         |m2-public-durability-sweep|m2-public-everysec-sweep \
                         |m2-public-everysec-workload-sweep|m2-crash-matrix \
                         |m2-fsync-err-process-fail-stop \
                         |m2-log-append-write-fault-process-fail-stop \
                         |m2-live-checkpoint-dir-fsync-process-fail-stop \
                         [--seed N|0xN] \
                         [--verify-determinism] [--plant lost-wakeup] [--cells N] \
                         [--connections N] [--commands N] [--sweep-seeds N] \
                         [--sweep-shard-index N --sweep-shard-count N] \
                         [--writes-per-seed N] [--trace-out FILE]"
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

    if scenario_name == "m2-fsync-err-process-fail-stop" {
        public_fsync_err_process_fail_stop(seed);
        eprintln!("inf-sim: scenario {scenario_name} unexpectedly returned");
        std::process::exit(1);
    }

    if scenario_name == "m2-log-append-write-fault-process-fail-stop" {
        public_log_append_write_fault_process_fail_stop(seed);
        eprintln!("inf-sim: scenario {scenario_name} unexpectedly returned");
        std::process::exit(1);
    }

    if scenario_name == "m2-live-checkpoint-dir-fsync-process-fail-stop" {
        public_live_checkpoint_wait_dir_fsync_process_fail_stop(seed);
        eprintln!("inf-sim: scenario {scenario_name} unexpectedly returned");
        std::process::exit(1);
    }

    if scenario_name == "m2-durability-oracle" {
        let mut config = DurabilitySweepConfig::ci(seed);
        if let Some(seeds) = sweep_seeds {
            config.seeds = seeds;
        }
        if let Some(writes) = writes_per_seed {
            config.writes_per_seed = writes;
        }
        for (flag, value) in overrides {
            match flag.as_str() {
                "--key-space" => config.key_space = value,
                "--commands" => config.writes_per_seed = value,
                "--cells" | "--connections" => {}
                _ => unreachable!(),
            }
        }
        if sweep_shard_index.is_some() || sweep_shard_count.is_some() {
            let Some(shard_index) = sweep_shard_index else {
                eprintln!("inf-sim: --sweep-shard-index is required with --sweep-shard-count");
                std::process::exit(2);
            };
            let Some(shard_count) = sweep_shard_count else {
                eprintln!("inf-sim: --sweep-shard-count is required with --sweep-shard-index");
                std::process::exit(2);
            };
            if let Err(error) = config.apply_seed_shard(shard_index, shard_count) {
                eprintln!("inf-sim: {error}");
                std::process::exit(2);
            }
        }
        let report = run_durability_sweep(&config);
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} seeds, offset {}, \
             stride {}, {} writes/seed, {} mixed-policy batches, manifest {} bytes, \
             hash {:#018x}",
            report.seeds,
            report.seed_offset,
            report.seed_stride,
            report.writes_per_seed,
            report.mixed_policy_batches,
            report.manifest.len(),
            report.manifest_hash
        );
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.manifest)
        {
            eprintln!("inf-sim: --trace-out {path}: {e}");
            std::process::exit(2);
        }
        for violation in report.violations.iter().take(5) {
            eprintln!("inf-sim: ORACLE VIOLATION: {violation}");
        }
        if !report.ok() {
            std::process::exit(1);
        }
        if verify {
            let second = run_durability_sweep(&config);
            if second.manifest != report.manifest {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — manifests differ ({} vs {} bytes, \
                     {:#x} vs {:#x})",
                    report.manifest.len(),
                    second.manifest.len(),
                    report.manifest_hash,
                    second.manifest_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run manifest byte-identical");
        }
        return;
    }

    if scenario_name == "m2-public-durability-sweep" {
        let mut config = DurabilitySweepConfig {
            seeds: 8,
            writes_per_seed: 24,
            key_space: 8,
            ..DurabilitySweepConfig::ci(seed)
        };
        if let Some(seeds) = sweep_seeds {
            config.seeds = seeds;
        }
        if let Some(writes) = writes_per_seed {
            config.writes_per_seed = writes;
        }
        for (flag, value) in overrides {
            match flag.as_str() {
                "--key-space" => config.key_space = value,
                "--commands" => config.writes_per_seed = value,
                "--cells" | "--connections" => {}
                _ => unreachable!(),
            }
        }
        if sweep_shard_index.is_some() || sweep_shard_count.is_some() {
            let Some(shard_index) = sweep_shard_index else {
                eprintln!("inf-sim: --sweep-shard-index is required with --sweep-shard-count");
                std::process::exit(2);
            };
            let Some(shard_count) = sweep_shard_count else {
                eprintln!("inf-sim: --sweep-shard-count is required with --sweep-shard-index");
                std::process::exit(2);
            };
            if let Err(error) = config.apply_seed_shard(shard_index, shard_count) {
                eprintln!("inf-sim: {error}");
                std::process::exit(2);
            }
        }
        let report = run_public_durable_recovery_sweep(&config);
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} seeds, offset {}, \
             stride {}, {} writes/seed, key-space {}, manifest {} bytes, hash {:#018x}",
            report.seeds,
            report.seed_offset,
            report.seed_stride,
            report.writes_per_seed,
            report.key_space,
            report.manifest.len(),
            report.manifest_hash
        );
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.manifest)
        {
            eprintln!("inf-sim: --trace-out {path}: {e}");
            std::process::exit(2);
        }
        if verify {
            let second = run_public_durable_recovery_sweep(&config);
            if second.manifest != report.manifest {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — manifests differ ({} vs {} bytes, \
                     {:#x} vs {:#x})",
                    report.manifest.len(),
                    second.manifest.len(),
                    report.manifest_hash,
                    second.manifest_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run manifest byte-identical");
        }
        return;
    }

    if scenario_name == "m2-public-everysec-sweep" {
        let mut config = DurabilitySweepConfig {
            seeds: 32,
            writes_per_seed: 1,
            key_space: 1,
            ..DurabilitySweepConfig::ci(seed)
        };
        if let Some(seeds) = sweep_seeds {
            config.seeds = seeds;
        }
        if writes_per_seed.is_some() || !overrides.is_empty() {
            eprintln!(
                "inf-sim: m2-public-everysec-sweep is a fixed single-write loss-window sweep"
            );
            std::process::exit(2);
        }
        if sweep_shard_index.is_some() || sweep_shard_count.is_some() {
            let Some(shard_index) = sweep_shard_index else {
                eprintln!("inf-sim: --sweep-shard-index is required with --sweep-shard-count");
                std::process::exit(2);
            };
            let Some(shard_count) = sweep_shard_count else {
                eprintln!("inf-sim: --sweep-shard-count is required with --sweep-shard-index");
                std::process::exit(2);
            };
            if let Err(error) = config.apply_seed_shard(shard_index, shard_count) {
                eprintln!("inf-sim: {error}");
                std::process::exit(2);
            }
        }
        let report = run_public_everysec_recovery_sweep(&config);
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} seeds, offset {}, \
             stride {}, pre-timer lost {}, pre-timer survived {}, post-timer survived {}, \
             manifest {} bytes, hash {:#018x}",
            report.seeds,
            report.seed_offset,
            report.seed_stride,
            report.pre_timer_loss_cases,
            report.pre_timer_survival_cases,
            report.post_timer_survival_cases,
            report.manifest.len(),
            report.manifest_hash
        );
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.manifest)
        {
            eprintln!("inf-sim: --trace-out {path}: {e}");
            std::process::exit(2);
        }
        if !report.ok() {
            eprintln!(
                "inf-sim: everysec sweep did not prove both loss-window loss and post-timer survival"
            );
            std::process::exit(1);
        }
        if verify {
            let second = run_public_everysec_recovery_sweep(&config);
            if second.manifest != report.manifest {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — manifests differ ({} vs {} bytes, \
                     {:#x} vs {:#x})",
                    report.manifest.len(),
                    second.manifest.len(),
                    report.manifest_hash,
                    second.manifest_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run manifest byte-identical");
        }
        return;
    }

    if scenario_name == "m2-public-everysec-workload-sweep" {
        let mut config = DurabilitySweepConfig {
            seeds: 32,
            writes_per_seed: 24,
            key_space: 8,
            ..DurabilitySweepConfig::ci(seed)
        };
        if let Some(seeds) = sweep_seeds {
            config.seeds = seeds;
        }
        if let Some(writes) = writes_per_seed {
            config.writes_per_seed = writes;
        }
        for (flag, value) in overrides {
            match flag.as_str() {
                "--key-space" => config.key_space = value,
                "--commands" => config.writes_per_seed = value,
                "--cells" | "--connections" => {}
                _ => unreachable!(),
            }
        }
        if sweep_shard_index.is_some() || sweep_shard_count.is_some() {
            let Some(shard_index) = sweep_shard_index else {
                eprintln!("inf-sim: --sweep-shard-index is required with --sweep-shard-count");
                std::process::exit(2);
            };
            let Some(shard_count) = sweep_shard_count else {
                eprintln!("inf-sim: --sweep-shard-count is required with --sweep-shard-index");
                std::process::exit(2);
            };
            if let Err(error) = config.apply_seed_shard(shard_index, shard_count) {
                eprintln!("inf-sim: {error}");
                std::process::exit(2);
            }
        }
        let report = run_public_everysec_workload_sweep(&config);
        println!(
            "inf-sim: scenario {scenario_name} seed {seed:#x}: {} seeds, offset {}, \
             stride {}, {} writes/seed, key-space {}, loss-window truncated {}, \
             loss-window full {}, full-flush survived {}, manifest {} bytes, hash {:#018x}",
            report.seeds,
            report.seed_offset,
            report.seed_stride,
            report.writes_per_seed,
            report.key_space,
            report.loss_window_truncated_cases,
            report.loss_window_full_survival_cases,
            report.full_flush_survival_cases,
            report.manifest.len(),
            report.manifest_hash
        );
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.manifest)
        {
            eprintln!("inf-sim: --trace-out {path}: {e}");
            std::process::exit(2);
        }
        if !report.ok() {
            eprintln!(
                "inf-sim: everysec workload sweep did not prove loss-window truncation and full-flush survival"
            );
            std::process::exit(1);
        }
        if verify {
            let second = run_public_everysec_workload_sweep(&config);
            if second.manifest != report.manifest {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — manifests differ ({} vs {} bytes, \
                     {:#x} vs {:#x})",
                    report.manifest.len(),
                    second.manifest.len(),
                    report.manifest_hash,
                    second.manifest_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run manifest byte-identical");
        }
        return;
    }

    if scenario_name == "m2-crash-matrix" {
        let report = run_m2_crash_matrix_rows();
        println!(
            "inf-sim: scenario {scenario_name}: {} runner rows, manifest {} bytes, hash {:#018x}",
            report.rows,
            report.manifest.len(),
            report.manifest_hash
        );
        if let Some(path) = &trace_out
            && let Err(e) = std::fs::write(path, &report.manifest)
        {
            eprintln!("inf-sim: --trace-out {path}: {e}");
            std::process::exit(2);
        }
        for violation in report.violations.iter().take(5) {
            eprintln!("inf-sim: CRASH-MATRIX VIOLATION: {violation}");
        }
        if !report.ok() {
            std::process::exit(1);
        }
        if verify {
            let second = run_m2_crash_matrix_rows();
            if second.manifest != report.manifest {
                eprintln!(
                    "inf-sim: DETERMINISM VIOLATION — manifests differ ({} vs {} bytes, \
                     {:#x} vs {:#x})",
                    report.manifest.len(),
                    second.manifest.len(),
                    report.manifest_hash,
                    second.manifest_hash
                );
                std::process::exit(1);
            }
            println!("inf-sim: determinism verified — second run manifest byte-identical");
        }
        return;
    }

    let mut scenario = match scenario_name.as_str() {
        "m0-smoke" => Scenario::m0_smoke(seed),
        "m1-cache" => Scenario::m1_cache(seed),
        other => {
            eprintln!(
                "inf-sim: unknown scenario {other} \
                 (have: m0-smoke, m1-cache, m2-durability-oracle, \
                 m2-public-durability-sweep, m2-public-everysec-sweep, \
                 m2-public-everysec-workload-sweep, m2-crash-matrix, \
                 m2-fsync-err-process-fail-stop, \
                 m2-log-append-write-fault-process-fail-stop, \
                 m2-live-checkpoint-dir-fsync-process-fail-stop)"
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
