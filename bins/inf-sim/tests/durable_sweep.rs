//! M2-S19 durability-oracle ACs (ADR-0021): determinism of the durable
//! scenario, a CI-sized honest sweep (zero violations; taxonomy refusals
//! legal with a clean survival audit), and the planted-bug canary — a
//! lying fsync must be caught by the oracle within 1,000 seeds (it is
//! caught orders of magnitude sooner; the assert keeps the AC's bound).

use inf_sim::net::Plant;
use inf_sim::{DurableScenario, run_durable_scenario};

/// L7: same seed ⇒ byte-identical traces, on a plain seed and on a
/// taxonomy-refusal seed (the audit path is part of the trace contract).
#[test]
fn durable_same_seed_same_trace() {
    for seed in [0xC0FFEEu64, 0x5EED_005D] {
        let a = run_durable_scenario(&DurableScenario::m2_durable(seed));
        let b = run_durable_scenario(&DurableScenario::m2_durable(seed));
        assert!(a.ok(), "seed {seed:#x} violations: {:?}", a.violations);
        assert_eq!(a.trace, b.trace, "seed {seed:#x}: trace must be byte-identical");
        assert_eq!(a.trace_hash, b.trace_hash);
    }
    let a = run_durable_scenario(&DurableScenario::m2_durable(1));
    let c = run_durable_scenario(&DurableScenario::m2_durable(2));
    assert_ne!(a.trace_hash, c.trace_hash, "different seeds must diverge");
}

/// CI-sized honest sweep: no durability violations across fresh seeds;
/// double-cut seeds (interrupted recovery) included by construction.
#[test]
fn durable_sweep_ci_slice_is_green() {
    let seeds = if cfg!(debug_assertions) { 24u64 } else { 64 };
    let mut refused = 0u32;
    let mut double_cuts = 0u32;
    for i in 0..seeds {
        let scenario = DurableScenario::m2_durable(0x00D5_0000 + i);
        double_cuts += u32::from(scenario.double_cut);
        let report = run_durable_scenario(&scenario);
        assert!(
            report.ok(),
            "seed {:#x}: stalled={} violations={:?}",
            scenario.seed,
            report.stalled,
            report.violations
        );
        refused += u32::from(report.refused_boot);
        assert!(report.audited_keys > 0, "seed {:#x}: the audit ran on no keys", scenario.seed);
    }
    assert!(double_cuts > 0, "the slice must include interrupted-recovery seeds");
    // Refusals are legal but must stay the exception, not the rule.
    assert!(
        u64::from(refused) < seeds / 4,
        "taxonomy refusals dominate the sweep ({refused}/{seeds}) — the disk model or \
         recovery regressed"
    );
}

/// The AC: a planted ack-ahead-of-durability bug (the lying fsync,
/// ADR-0021 D4) is caught within 1,000 seeds. The survival audit is what
/// gives the oracle teeth — a refusal cannot hide the loss.
#[test]
fn planted_lying_fsync_is_caught_within_1000_seeds() {
    for seed in 0..1_000u64 {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.plant = Plant::FsyncLies;
        let report = run_durable_scenario(&scenario);
        if report.violations.iter().any(|v| v.contains("VIOLATION")) {
            eprintln!("canary caught at seed {seed:#x} (within 1000)");
            return;
        }
    }
    panic!("the lying-fsync canary survived 1,000 seeds — the oracle has no teeth");
}
