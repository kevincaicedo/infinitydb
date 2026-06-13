//! `inf-sim` CLI (M0-S20): run a deterministic scenario, optionally twice,
//! comparing event traces byte-for-byte.
//!
//! ```text
//! inf-sim --scenario m0-smoke --seed 0xC0FFEE --verify-determinism
//! ```
#![forbid(unsafe_code)]

use inf_sim::net::Plant;
use inf_sim::{Scenario, run_scenario};

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
                "--trace-out" => trace_out = Some(take("--trace-out")?),
                "--help" | "-h" => {
                    println!(
                        "inf-sim --scenario m0-smoke|m1-cache [--seed N|0xN] \
                         [--verify-determinism] [--plant lost-wakeup] [--cells N] \
                         [--connections N] [--commands N] [--trace-out FILE]"
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

    let mut scenario = match scenario_name.as_str() {
        "m0-smoke" => Scenario::m0_smoke(seed),
        "m1-cache" => Scenario::m1_cache(seed),
        other => {
            eprintln!("inf-sim: unknown scenario {other} (have: m0-smoke, m1-cache)");
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
