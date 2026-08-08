//! M3-S18/S23/S24 end-to-end document durability + replay equivalence
//! over the real server plane and simulated disk: merge-heavy multi-match
//! deltas, fuzz-corpus subtrees, cadence full images, checkpoints, power
//! cuts, ack-watermark survival, the read-only shadow-replay equivalence
//! oracle (with its planted canary), and byte-identical seed replay.

use std::collections::BTreeSet;

use inf_sim::{DurableScenario, run_durable_scenario};

#[test]
fn document_same_seed_replays_byte_identically() {
    let scenario = DurableScenario::m3_document(0xD0C0_0018);
    let first = run_durable_scenario(&scenario);
    let second = run_durable_scenario(&scenario);
    assert!(first.ok(), "violations: {:?}", first.violations);
    assert_eq!(first.trace, second.trace, "same seed must reproduce every apply/reply byte");
    assert_eq!(first.trace_hash, second.trace_hash);
    assert!(
        first.trace.windows(b"JSON.NUMINCRBY".len()).any(|w| w == b"JSON.NUMINCRBY"),
        "the trace must exercise multi-match document deltas"
    );
    assert!(
        first.trace.windows(b"JSON.MERGE".len()).any(|w| w == b"JSON.MERGE"),
        "the trace must exercise RFC 7386 merge deltas (the S14 hand-off)"
    );
    assert!(first.audited_keys > 0, "the post-cut durability audit must run");
    assert!(first.equivalence_checks > 0, "the equivalence oracle must run — dead oracles lie");
    assert_eq!(
        first.equivalence_checks, second.equivalence_checks,
        "oracle cadence is part of determinism"
    );
}

#[test]
fn document_power_cut_ci_slice_is_atomic_and_replay_equivalent() {
    let seeds = if cfg!(debug_assertions) { 24u64 } else { 64 };
    let mut double_cuts = 0u32;
    let mut equivalence_checks = 0u64;
    let mut documents_compared = 0u64;
    let mut corpus_documents = 0u64;
    let mut cut_classes: BTreeSet<&'static str> = BTreeSet::new();
    for offset in 0..seeds {
        let scenario = DurableScenario::m3_document(0xD0C0_1800 + offset);
        double_cuts += u32::from(scenario.double_cut);
        let report = run_durable_scenario(&scenario);
        assert!(
            report.ok(),
            "seed {:#x}: stalled={} violations={:?}",
            scenario.seed,
            report.stalled,
            report.violations
        );
        assert!(!report.refused_boot, "honest document power-cut images must recover");
        assert!(report.required_ops > 0, "the always watermark oracle must bind");
        assert!(report.audited_keys > 0, "the recovered document set must be audited");
        assert!(
            report.equivalence_checks > 0,
            "seed {:#x}: the equivalence oracle never ran",
            scenario.seed
        );
        assert!(
            report.documents_compared > 0,
            "seed {:#x}: the equivalence oracle compared nothing",
            scenario.seed
        );
        equivalence_checks += report.equivalence_checks;
        documents_compared += report.documents_compared;
        corpus_documents += report.corpus_documents_used;
        cut_classes.extend(report.cut_classes.iter().copied());
    }
    assert!(double_cuts > 0, "the slice must interrupt recovery as well as live traffic");
    // Post-recovery always checks; most seeds also reach both mid-run
    // quiesce instants before the cut.
    assert!(
        equivalence_checks >= 2 * seeds,
        "the slice must reach mid-run equivalence instants ({equivalence_checks} checks)"
    );
    assert!(documents_compared >= seeds, "per-document comparison must have teeth");
    assert!(corpus_documents > 0, "fuzz-corpus documents must enter the workload (M3-S24)");
    // ADR-0045 D4: cut coverage is measured, never assumed — the slice
    // must cut adjacent to both document record classes.
    assert!(
        cut_classes.contains("doc-delta") && cut_classes.contains("doc-full"),
        "cut boundary classes observed: {cut_classes:?}"
    );
}

/// The M3-S23 canary AC: a shadow replay that skips one `DocDelta` must
/// be caught by the equivalence oracle within 100 seeds — an oracle
/// nobody has watched fail proves nothing.
#[test]
fn planted_replay_skip_is_caught_within_100_seeds() {
    for offset in 0..100u64 {
        let mut scenario = DurableScenario::m3_document(0xD0C0_23CA + offset);
        scenario.replay_canary = true;
        let report = run_durable_scenario(&scenario);
        if report.violations.iter().any(|v| v.contains("REPLAY EQUIVALENCE VIOLATION")) {
            println!(
                "canary caught at seed {:#x} after {} seeds: {}",
                scenario.seed,
                offset + 1,
                report.violations.first().expect("non-empty violations")
            );
            return;
        }
    }
    panic!("the replay-skip canary survived 100 seeds — the equivalence oracle has no teeth");
}

/// The M3-S24 corpus AC: fuzz-minimized documents round-trip the full
/// stack (parse → mutate → log → crash → recover → serialize) inside the
/// DST scenario, with the durability and equivalence oracles both green.
#[test]
fn fuzz_corpus_documents_round_trip_the_full_stack() {
    let scenario = DurableScenario::m3_document(0xD0C0_2400);
    let report = run_durable_scenario(&scenario);
    assert!(report.ok(), "violations: {:?}", report.violations);
    assert!(
        report.corpus_documents_used > 0,
        "the seed must embed fuzz-corpus documents (used {})",
        report.corpus_documents_used
    );
    assert!(report.audited_keys > 0, "recovered corpus-bearing documents must be audited");
    assert!(report.equivalence_checks > 0, "the equivalence oracle must cover corpus documents");
    assert!(!report.cut_classes.is_empty(), "the cut boundary must be classified (ADR-0045 D4)");
}
