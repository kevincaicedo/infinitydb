//! M0-S20 ACs + the DST items E3/E5 deferred here.

use inf_sim::net::Plant;
use inf_sim::{Scenario, run_scenario};

fn small(seed: u64) -> Scenario {
    Scenario {
        cells: 3,
        connections: 8,
        commands: 1_500,
        key_space: 64,
        pipelined_every: 4,
        ..Scenario::m0_smoke(seed)
    }
}

/// AC: same seed ⇒ byte-identical event traces across two runs (the
/// debug-vs-release leg runs via the CLI in CI / the artifact script).
#[test]
fn same_seed_same_trace() {
    let a = run_scenario(&small(0xC0FFEE));
    let b = run_scenario(&small(0xC0FFEE));
    assert!(a.ok(), "violations: {:?}", a.oracle_violations);
    assert_eq!(a.trace, b.trace, "trace must be byte-identical");
    assert_eq!(a.trace_hash, b.trace_hash);
    // And different seeds genuinely differ (the trace isn't degenerate).
    let c = run_scenario(&small(0xBEEF));
    assert_ne!(a.trace_hash, c.trace_hash);
}

/// AC: scenario `m0-smoke` — 3 cells, 100 sim connections, 10⁵ mixed
/// commands incl. cross-cell — oracle green. (The full-size run also
/// executes via the CLI for the artifact; this keeps a CI-sized guard.)
#[test]
fn m0_smoke_oracle_green() {
    let mut scenario = Scenario::m0_smoke(0xC0FFEE);
    scenario.commands = if cfg!(debug_assertions) { 20_000 } else { 100_000 };
    let report = run_scenario(&scenario);
    assert!(!report.stalled, "smoke stalled");
    assert_eq!(report.oracle_violations, Vec::<String>::new());
    assert_eq!(report.commands_done, scenario.commands);
    assert!(report.events >= scenario.commands, "apply events cover every command");
}

/// AC: a planted lost-wakeup bug is caught by a seed within 1000 runs —
/// the harness has teeth. (Stall detection IS the catch: the suppressed
/// readiness edge starves a reply-waiting client forever.)
#[test]
fn planted_lost_wakeup_is_caught_within_1000_seeds() {
    for seed in 0..1_000u64 {
        let scenario = Scenario {
            cells: 2,
            connections: 4,
            commands: 400,
            key_space: 32,
            pipelined_every: 0,
            plant: Plant::LostWakeup,
            ..Scenario::m0_smoke(seed)
        };
        let report = run_scenario(&scenario);
        if report.stalled {
            println!("lost wakeup caught at seed {seed}");
            return;
        }
    }
    panic!("planted lost-wakeup survived 1000 seeds — the harness is blind");
}

/// Deferred from M0-S09: deadlock battery — all-to-all saturation under
/// tiny credit budgets, 10⁷ ops equivalent, no progress stall. Heavy:
/// release-tier run for the artifact (`cargo test -p inf-sim --release
/// -- --ignored`).
#[test]
#[ignore = "artifact run: release tier, ~10^7 ops"]
fn deadlock_battery_all_to_all_saturation() {
    let scenario = Scenario {
        cells: 4,
        connections: 64,
        commands: 10_000_000,
        key_space: 512,     // hot keyspace ⇒ heavy cross-cell traffic
        pipelined_every: 1, // every client pipelined ⇒ sustained pressure
        ..Scenario::m0_smoke(0xD00D)
    };
    let report = run_scenario(&scenario);
    assert!(!report.stalled, "deadlock battery stalled");
    assert_eq!(report.oracle_violations, Vec::<String>::new());
    assert_eq!(report.commands_done, scenario.commands);
}

/// Deferred from M0-S10/S16: 10⁶ ops under randomized key placement vs the
/// single-store oracle.
#[test]
#[ignore = "artifact run: release tier, 10^6 ops"]
fn cross_cell_million_op_oracle() {
    let scenario = Scenario {
        cells: 4,
        connections: 100,
        commands: 1_000_000,
        key_space: 10_000,
        pipelined_every: 3,
        ..Scenario::m0_smoke(0x5EED)
    };
    let report = run_scenario(&scenario);
    assert!(report.ok(), "violations: {:?}", report.oracle_violations);
    assert_eq!(report.commands_done, scenario.commands);
}

/// M1-S15 AC seed: the m1-cache scenario runs the pub/sub delivery oracle
/// (confirmed ⇒ reachable; per-publisher FIFO; exact final ledger), the TTL
/// slice (PEXPIRE in the mix under the apply oracle), and the quiescence
/// accounting reconciliation — all green, deterministically.
#[test]
fn m1_cache_oracle_green() {
    let mut scenario = Scenario::m1_cache(0xCAFE);
    scenario.commands = if cfg!(debug_assertions) { 12_000 } else { 60_000 };
    let report = run_scenario(&scenario);
    assert!(!report.stalled, "m1-cache stalled (delivery loss parks phase C)");
    assert_eq!(report.oracle_violations, Vec::<String>::new());
    assert_eq!(report.commands_done, scenario.commands);
    assert!(report.published > 0, "mix produced no PUBLISH traffic");
    // Every channel has ≥ 2 watchers (subscription plan), so deliveries
    // strictly exceed publishes when anything was published.
    assert!(report.delivered > report.published, "fan-out did not fan");
}

/// M1-S15: determinism holds with the pub/sub plane + subscribers active.
#[test]
fn m1_cache_same_seed_same_trace() {
    let mut scenario = Scenario::m1_cache(0xC0FFEE);
    scenario.commands = 6_000;
    let a = run_scenario(&scenario);
    let b = run_scenario(&scenario);
    assert!(a.ok(), "violations: {:?}", a.oracle_violations);
    assert_eq!(a.trace, b.trace, "trace must be byte-identical");
    assert_eq!((a.published, a.delivered), (b.published, b.delivered));
}
