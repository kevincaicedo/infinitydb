//! M2.5-S14 combined-scenario ACs: determinism of the unified
//! durable+memory+pub/sub+expiry shape, and a CI-sized green sweep that
//! also proves the new oracles actually *ran* (an oracle that audits zero
//! keys is decoration — L10). The planted-bug catches for the two new
//! oracles (everysec ack-deferral under the stall device; L2 memory
//! volatility) are demonstrated in
//! `.artifacts/m2.5/s14-dst-20260707/planted-catches.md` (source mutations,
//! plant → red → revert → green).

use inf_sim::{CombinedScenario, run_combined_scenario};

/// L7: same seed ⇒ byte-identical traces, including the post-recovery
/// audit reads (durable admissible-set + memory volatility) in the trace.
#[test]
fn combined_same_seed_same_trace() {
    for seed in [0x00C0_FFEEu64, 0x5EED_0003 /* a double-cut seed */] {
        let a = run_combined_scenario(&CombinedScenario::m2_combined(seed));
        let b = run_combined_scenario(&CombinedScenario::m2_combined(seed));
        assert!(a.ok(), "seed {seed:#x} violations: {:?}", a.violations);
        assert_eq!(a.trace, b.trace, "seed {seed:#x}: trace must be byte-identical");
        assert_eq!(a.trace_hash, b.trace_hash);
    }
    let a = run_combined_scenario(&CombinedScenario::m2_combined(1));
    let c = run_combined_scenario(&CombinedScenario::m2_combined(2));
    assert_ne!(a.trace_hash, c.trace_hash, "different seeds must diverge");
}

/// CI-sized green sweep: zero violations across fresh seeds, AND every new
/// oracle demonstrably exercised — durable keys audited, memory keys
/// audited (L2 volatility), messages published+delivered (pub/sub), and
/// the stall device engaged (a non-zero max always-ack latency proves the
/// device has service time, else the everysec-deferral oracle is dead).
#[test]
fn combined_sweep_ci_slice_is_green_and_oracles_ran() {
    let seeds = if cfg!(debug_assertions) { 24u64 } else { 64 };
    let mut memory_audited = 0u64;
    let mut delivered = 0u64;
    let mut stall_engaged = false;
    let mut double_cuts = 0u32;
    for i in 0..seeds {
        let scenario = CombinedScenario::m2_combined(0x00CB_0000 + i);
        double_cuts += u32::from(scenario.durable.double_cut);
        let report = run_combined_scenario(&scenario);
        assert!(
            report.ok(),
            "seed {:#x}: stalled={} violations={:?}",
            scenario.durable.seed,
            report.stalled,
            report.violations
        );
        assert!(
            report.audited_keys > 0,
            "seed {:#x}: durable audit ran on no keys",
            scenario.durable.seed
        );
        memory_audited += report.memory_keys_audited;
        delivered += report.delivered;
        stall_engaged |= report.always_ack_latency_ms_max > 0;
    }
    assert!(double_cuts > 0, "the slice must include interrupted-recovery (double-cut) seeds");
    assert!(memory_audited > 0, "the L2 memory-volatility oracle audited zero keys — decoration");
    assert!(delivered > 0, "pub/sub delivered nothing — the fan-out oracle never ran");
    assert!(
        stall_engaged,
        "the stall device never showed a non-zero always-ack latency — the everysec-deferral \
         oracle has no signal"
    );
}
