//! The M4.6-S20 contract model (ADR-0116): cross-partition intent
//! acquisition, the reads-versus-intents rule, WATCH history, and the
//! durable prepare → decision → checkpoint rule, as executable adversarial
//! histories. No engine code runs here — a design story produces an ADR
//! and a model, not code (M4.6 §3); M6-S04/S06/S09/S16/S17 turn each
//! history into a DST scenario against the real cells.
//!
//! Every safety test runs the chosen rules by default and the withdrawn
//! rules under `INF_TXMODEL_VARIANT=withdrawn` — the red the review
//! harness records. The `withdrawn_*` twins pin those reds as positive
//! controls, so the model cannot pass vacuously.
//!
//! Eight models, one withdrawn rule set each:
//!
//! - [`acquisition`]: master plan §6.3 as written — a parallel lock fan,
//!   grants on arrival, waiters sorted by txid — deadlocks on the F2
//!   two-key history; canonical acquisition (ADR-0116 D2) cannot.
//!   Dragonfly's reschedule rule completes too, with counted retries.
//!   Readers that bypass intents observe uncommitted writes; a WATCH-only
//!   R intent released after validation lets a mutation land before the
//!   decision. (ADR-0116 A4, the 2026-09-10 review's F09:) a leg that
//!   writes live values leaves an aborted transaction's write published;
//!   a cancellation honoured past the in-memory decision publishes half a
//!   commit or answers a false abort; private staging and the A4 phase
//!   table never do, at any of the seven phases a cancellation can land.
//!   (ADR-0116 A6, F04:) D2.5's "fixed arguments make per-owner legs
//!   independent" renames from the source's pre-transaction value and
//!   publishes half an `MSETNX`; coordinator stages (gather → combine →
//!   apply) under the held intents give the serial outcome and abort
//!   cleanly at the combine step.
//! - [`watch`]: endpoint version equality (`Version | MISSING`) accepts
//!   the F3 absent → present → absent history and a delete/recreate;
//!   the owner's registration (ADR-0116 D4) agrees with the history
//!   oracle on every history, including eviction and owner restart.
//!   (ADR-0116 A5, F07:) re-registering on a repeated `WATCH` launders
//!   the mutation between the two; the additive rule agrees with the
//!   sticky-window oracle and with Redis 8.0.5's verdicts on every line
//!   of `seeds/watch-redis-oracle.txt`.
//! - [`durable`]: a decision that does not wait for durable prepares, or
//!   a checkpoint that streams a published record whose decision is not
//!   yet durable, leaves half a transaction after a crash; the ADR-0116
//!   D3 rules never do.
//! - [`lineage`] (ADR-0116 A1/A2, the 2026-09-10 review's F01/F02): a
//!   plain successor of a published-undecided record that outlives its
//!   dropped source, and a checkpoint that streams the immediate
//!   predecessor when that predecessor is itself undecided, both recover
//!   a state no serial history produces; inherited dependencies, the
//!   decision-qualified pinned image and dependency-gated `always` acks
//!   never do. (ADR-0116 A8, the fix validation's F01 reopening:)
//!   per-record dependencies recover half of a transaction whose legs
//!   inherited different sets — partial overlap, a read leg on one owner
//!   and a write on another, a chain two hops from its dropped root — and
//!   a decision gated on prepares alone outlives a plain write its read
//!   leg observed on another log; one transaction-wide closure gathered
//!   at the grants, carried by every prepare, and read-watermark gating
//!   of the decision recover every transaction whole.
//! - [`identity`] (ADR-0116 A3, F03): resuming `local_seq` above the
//!   coordinator's replayed maximum reissues a txid a remote participant's
//!   durable prepare still carries; a durable reservation carried by the
//!   checkpoint never does.
//! - [`revision`] (ADR-0116 A7, F05): a `RecordRevision` whose incarnation
//!   changes only at creation repeats after the u24 version wraps, after a
//!   checkpoint drops a dead incarnation, and after the u32 counter wraps;
//!   re-incarnation on wrap, a durable checkpoint-carried reservation and
//!   a typed refusal at exhaustion never repeat a durable token.
//! - [`credits`] (ADR-0116 A9, F06): D2.3's `Queued` reply plus a grant
//!   callback returns one credit twice and overflows the pair's reply
//!   headroom; keeping the request's credit until the grant, hop by hop,
//!   deadlocks a holder behind its own contenders on a saturated pair;
//!   one deferred terminal reply per `LockOp` and every hop's credit
//!   reserved at Admit complete every storm and drain the pair.
//! - [`retention`] (ADR-0116 A10, F08): "pinned bytes ≤ retention cap ×
//!   frame bound" is false — a 24 B tombstone pins a 1 MiB image or a
//!   cold extent; charging the live image at the grant against separate
//!   pinned budgets, returned or converted at publication and released
//!   with the pin, conserves the counter, holds the bound and leaks
//!   nothing.

/// The rules under test: `chosen` by default, `withdrawn` under the env
/// hook the review harness uses to record the red.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Variant {
    Chosen,
    Withdrawn,
}

impl Variant {
    /// `INF_TXMODEL_VARIANT=withdrawn` selects the withdrawn rules for
    /// every safety test (the harness's red); anything else is chosen.
    pub fn from_env() -> Variant {
        match std::env::var("INF_TXMODEL_VARIANT") {
            Ok(v) if v == "withdrawn" => Variant::Withdrawn,
            _ => Variant::Chosen,
        }
    }
}

/// xorshift64* — the model's only entropy; seeded, so every failure is a
/// replayable seed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

// ---------------------------------------------------------------------
// Model 1 — intent acquisition, staging, decision, cancellation
// ---------------------------------------------------------------------

pub mod acquisition;

// ---------------------------------------------------------------------
// Model 2 — WATCH history on one owner
// ---------------------------------------------------------------------

pub mod watch;

// ---------------------------------------------------------------------
// Model 3 — durable prepare, decision, checkpoint, crash
// ---------------------------------------------------------------------

pub mod durable;

// ---------------------------------------------------------------------
// Model 4 — pending lineage: successors, chains, the qualified image
// ---------------------------------------------------------------------

pub mod lineage;

// ---------------------------------------------------------------------
// Model 5 — txid identity across boots
// ---------------------------------------------------------------------

pub mod identity;

// ---------------------------------------------------------------------
// Model 6 — RecordRevision non-repetition (ADR-0116 A7)
// ---------------------------------------------------------------------

pub mod revision;

// ---------------------------------------------------------------------
// Model 7 — fabric credits for queued grants (ADR-0116 A9, review F06)
// ---------------------------------------------------------------------

pub mod credits;

// ---------------------------------------------------------------------
// Model 8 — retained-byte budget for pinned images (ADR-0116 A10, review F08)
// ---------------------------------------------------------------------

pub mod retention;

#[cfg(test)]
mod tests {
    use super::Variant;
    use super::acquisition::{
        self, Acquisition, Cancel, CancelPhase, CancelUntil, Cmd, Dependent, Outcome, Reply, Rules,
    };
    use super::credits;
    use super::durable;
    use super::identity;
    use super::lineage;
    use super::retention;
    use super::revision;
    use super::watch::{self, Event, RepeatedWatch, Verdict};

    fn rules() -> Rules {
        match Variant::from_env() {
            Variant::Chosen => Rules::chosen(),
            Variant::Withdrawn => Rules::withdrawn(),
        }
    }

    fn reschedule(max_retries: u32) -> Rules {
        Rules { acquisition: Acquisition::Reschedule { max_retries }, ..Rules::chosen() }
    }

    // ---- acquisition ----

    #[test]
    fn two_key_history_completes_without_deadlock() {
        let r = acquisition::two_key_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(r.committed, 2);
    }

    #[test]
    fn withdrawn_rule_deadlocks_on_the_two_key_history() {
        let r = acquisition::two_key_history(Rules::withdrawn());
        assert_eq!(r.stuck.len(), 2, "{r:?}");
        assert_eq!(
            r.violations[0],
            "ACQUISITION DEADLOCK: 2 transaction(s) stuck with no message in flight — T1 waits on \
                 key 1@cell1 behind T2; T2 waits on key 0@cell0 behind T1"
        );
    }

    #[test]
    fn reschedule_rule_completes_the_two_key_history_with_a_counted_retry() {
        let r = acquisition::two_key_history(reschedule(8));
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(r.committed, 2);
        assert!(r.retries >= 1, "{r:?}");
    }

    #[test]
    fn storms_never_deadlock_and_reads_are_serializable() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::storm(rules, 4, 6, 24, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
            assert_eq!(r.committed + r.aborted, 24, "seed {seed}: {r:?}");
        }
    }

    #[test]
    fn withdrawn_rule_deadlocks_some_storm_within_64_seeds() {
        let deadlocked = (1..=64u64)
            .filter(|seed| {
                !acquisition::storm(Rules::withdrawn(), 4, 6, 24, *seed).stuck.is_empty()
            })
            .count();
        assert!(deadlocked > 0, "the withdrawn rule survived 64 storms — the model lost its teeth");
    }

    #[test]
    fn reschedule_retries_scale_with_contention_and_canonical_has_none() {
        let hot: u32 =
            (1..=32u64).map(|s| acquisition::storm(reschedule(64), 4, 2, 24, s).retries).sum();
        let cold: u32 =
            (1..=32u64).map(|s| acquisition::storm(reschedule(64), 4, 64, 24, s).retries).sum();
        let canonical: u32 =
            (1..=32u64).map(|s| acquisition::storm(Rules::chosen(), 4, 2, 24, s).retries).sum();
        eprintln!(
            "reschedule retries over 32 storms of 24 txns on 4 cells: hot (2 keys) {hot}, cold (64 \
                 keys) {cold}; canonical hot {canonical}"
        );
        assert_eq!(canonical, 0);
        assert!(hot > cold, "hot {hot} retries vs cold {cold}");
        for seed in 1..=32u64 {
            let r = acquisition::storm(reschedule(64), 4, 2, 24, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
        }
    }

    #[test]
    fn readers_never_observe_an_uncommitted_write() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::read_pair_history(rules, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
        }
    }

    #[test]
    fn withdrawn_read_bypass_observes_an_uncommitted_write_within_64_seeds() {
        let hits = (1..=64u64)
            .filter(|s| {
                acquisition::read_pair_history(Rules::withdrawn(), *s)
                    .violations
                    .iter()
                    .any(|v| v.starts_with("READ UNCOMMITTED"))
            })
            .count();
        assert!(hits > 0);
    }

    #[test]
    fn a_watch_only_key_is_held_through_the_decision() {
        let r = acquisition::watch_interval_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
    }

    #[test]
    fn withdrawn_validate_and_release_commits_over_a_mutated_watch() {
        let r = acquisition::watch_interval_history(Rules {
            hold_watch_intents: false,
            ..Rules::chosen()
        });
        assert_eq!(
            r.violations,
            vec![
                "WATCH INTERVAL VIOLATION: T1 committed after key 1 was mutated between the \
                     owner's validation and the decision"
                    .to_string()
            ]
        );
    }

    #[test]
    fn every_terminal_path_releases_every_intent_and_every_staged_set() {
        let leak = |v: &String| v.starts_with("INTENT LEAK") || v.starts_with("STAGING LEAK");
        for seed in 1..=32u64 {
            for faults in [false, true] {
                let r = acquisition::storm_with(Rules::chosen(), 3, 4, 16, seed, faults);
                assert!(!r.violations.iter().any(leak), "seed {seed}: {r:?}");
                let r = acquisition::storm_with(reschedule(2), 3, 4, 16, seed, faults);
                assert!(!r.violations.iter().any(leak), "seed {seed}: {r:?}");
            }
        }
    }

    #[test]
    fn outcome_is_typed() {
        let mut m = acquisition::Model::new(Rules::chosen(), 1, 1);
        let t = m.submit(acquisition::TxnSpec {
            coordinator: 0,
            writes: vec![0],
            ..Default::default()
        });
        m.run(100);
        assert_eq!(m.outcome(t), Outcome::Committed);
    }

    // ---- staging, decision functions, cancellation (ADR-0116 A4) ----

    #[test]
    fn a_failed_native_condition_publishes_nothing() {
        let (r, outcome, published) = acquisition::native_failure_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("condition failed"));
        assert_eq!(published, None);
    }

    #[test]
    fn withdrawn_live_writes_survive_a_failed_native_condition() {
        let (r, outcome, published) = acquisition::native_failure_history(Rules {
            stage_privately: false,
            ..Rules::chosen()
        });
        assert_eq!(outcome, Outcome::Aborted("condition failed"));
        assert_eq!(published, Some(0));
        assert_eq!(
            r.violations,
            vec![
                "STAGING VIOLATION: T1 aborted (condition failed) but its write to key 0@cell0 is \
                     published"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_failed_exec_command_is_embedded_and_the_rest_commits() {
        let (r, outcome, published) = acquisition::exec_failure_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [Some(0), None]);
    }

    #[test]
    fn cancellation_aborts_before_the_decision_and_completes_after_it() {
        let rules = rules();
        for phase in acquisition::PHASES {
            for kind in [Cancel::Disconnect, Cancel::Timeout] {
                for seed in 1..=16u64 {
                    let (r, outcome, published) =
                        acquisition::cancellation_history(rules, phase, kind, seed);
                    assert!(
                        r.violations.is_empty(),
                        "{phase:?} {kind:?} seed {seed}: {}",
                        r.violations.join("\n")
                    );
                    if acquisition::before_decision(phase) {
                        let reason = match kind {
                            Cancel::Disconnect => "disconnect",
                            Cancel::Timeout => "timeout",
                        };
                        assert_eq!(outcome, Outcome::Aborted(reason), "{phase:?} {kind:?}");
                        assert_eq!(published, [false, false], "{phase:?} {kind:?}");
                    } else {
                        assert_eq!(outcome, Outcome::Committed, "{phase:?} {kind:?}");
                        assert_eq!(published, [true, true], "{phase:?} {kind:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn withdrawn_cancel_until_durable_publishes_half_a_commit_within_64_seeds() {
        let rules = Rules { cancel_until: CancelUntil::DurableDecision, ..Rules::chosen() };
        let mut partial = Vec::new();
        let mut false_abort = 0;
        for seed in 1..=64u64 {
            let (r, _, _) = acquisition::cancellation_history(
                rules,
                CancelPhase::Decided,
                Cancel::Disconnect,
                seed,
            );
            partial.extend(r.violations.iter().filter(|v| v.starts_with("PARTIAL")).cloned());
            false_abort += usize::from(r.violations.iter().any(|v| v.starts_with("FALSE ABORT")));
        }
        assert!(
            !partial.is_empty(),
            "no partial publication in 64 seeds — the model lost its teeth"
        );
        eprintln!(
            "cancel-until-durable: {} partial, {false_abort} false aborts over 64 seeds",
            partial.len()
        );
        assert_eq!(
            partial[0],
            "PARTIAL PUBLICATION: T1 (disconnect after the decision) published on cell(s) [0] and \
                 discarded on cell(s) [1]"
        );
        assert!(false_abort > 0, "no false abort in 64 seeds");
    }

    #[test]
    fn withdrawn_live_writes_outlive_a_cancel_between_two_legs() {
        let rules = Rules { stage_privately: false, ..Rules::chosen() };
        let (r, outcome, _) = acquisition::cancellation_history(
            rules,
            CancelPhase::Executed(0, 1),
            Cancel::Timeout,
            3,
        );
        assert_eq!(outcome, Outcome::Aborted("timeout"));
        assert_eq!(r.violations.len(), 1, "{r:?}");
        assert!(
            r.violations[0]
                .starts_with("STAGING VIOLATION: T1 aborted (timeout) but its write to key"),
            "{}",
            r.violations[0]
        );
    }

    #[test]
    fn fault_storms_publish_all_or_nothing_and_leak_nothing() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::storm_with(rules, 4, 6, 24, seed, true);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
            assert_eq!(r.committed + r.aborted, 24, "seed {seed}: {r:?}");
        }
    }

    #[test]
    fn withdrawn_rules_violate_some_fault_storm_within_256_seeds() {
        let staging = Rules { stage_privately: false, ..Rules::chosen() };
        let cancel = Rules { cancel_until: CancelUntil::DurableDecision, ..Rules::chosen() };
        let hits = |rules: Rules, prefix: &str, seeds: u64| {
            (1..=seeds)
                .filter(|s| {
                    acquisition::storm_with(rules, 4, 6, 24, *s, true)
                        .violations
                        .iter()
                        .any(|v| v.starts_with(prefix))
                })
                .count()
        };
        let staging_hits = hits(staging, "STAGING VIOLATION", 64);
        // A cancellation planted at `Decided` reaches the half-published
        // interleaving in roughly one storm in fifty (the CancelAt must
        // land between two owners' unlocks) — 256 seeds for that arm.
        let partial_hits = hits(cancel, "PARTIAL PUBLICATION", 256);
        let false_aborts = hits(cancel, "FALSE ABORT", 256);
        eprintln!(
            "fault storms: live-write {staging_hits}/64, cancel-until-durable partial \
                 {partial_hits}/256, false abort {false_aborts}/256"
        );
        assert!(staging_hits > 0 && partial_hits > 0 && false_aborts > 0);
    }

    // ---- dependent multi-owner commands (ADR-0116 A6) ----

    #[test]
    fn a_cross_owner_rename_ships_the_value_the_transaction_staged() {
        for native in [false, true] {
            let (r, outcome, published) = acquisition::rename_history(rules(), native, false);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(published, [None, Some(0)], "native {native}: a absent, b = T's value");
        }
    }

    #[test]
    fn a_refused_destination_never_strands_the_source() {
        let (r, outcome, published) = acquisition::rename_history(rules(), false, true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed, "EXEC embeds the refusal");
        assert_eq!(published, [Some(0), None], "the source keeps the SET; nothing at b");
        let (r, outcome, published) = acquisition::rename_history(rules(), true, true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("destination refused"));
        assert_eq!(published, [None, None]);
    }

    #[test]
    fn withdrawn_independent_legs_rename_from_the_pre_transaction_value() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, published) = acquisition::rename_history(rules, false, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [None, None], "the source is deleted, the destination never set");
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's RENAME 0→1 replied no such key where the serial history \
                     replies OK"
                    .to_string(),
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 1@cell1 = absent where the serial \
                     history gives T1"
                    .to_string(),
            ]
        );
        let (r, outcome, _) = acquisition::rename_history(rules, true, false);
        assert_eq!(outcome, Outcome::Aborted("no such key"));
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT LEG: T1 aborted (no such key) but the serial history commits its RENAME \
                     0→1"
                    .to_string()
            ]
        );
        let (r, _, published) = acquisition::rename_history(rules, false, true);
        assert_eq!(published, [None, None], "the refused destination lost the source");
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 0@cell0 = absent where the serial \
                     history gives T1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_cross_owner_msetnx_is_one_condition_over_every_owner() {
        for native in [false, true] {
            let (r, outcome, published) = acquisition::msetnx_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            let want =
                if native { Outcome::Aborted("condition failed") } else { Outcome::Committed };
            assert_eq!(outcome, want, "native {native}");
            assert_eq!(published, [None, Some(0)], "native {native}: nothing set");
        }
    }

    #[test]
    fn withdrawn_independent_legs_publish_half_an_msetnx() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, published) = acquisition::msetnx_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [Some(1), Some(0)], "cell 0 set its key; cell 1 refused");
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT COMMAND: T2's MSETNX [0, 1] left key 0@cell0 = T2 where the serial \
                     history gives absent"
                    .to_string()
            ]
        );
    }

    #[test]
    fn dependent_stages_abort_at_the_combine_step_and_complete_after_the_decision() {
        let rules = rules();
        for phase in acquisition::DEP_PHASES {
            for kind in [Cancel::Disconnect, Cancel::Timeout] {
                for seed in 1..=16u64 {
                    let (r, outcome, moved) =
                        acquisition::dependent_cancellation_history(rules, phase, kind, seed);
                    assert!(
                        r.violations.is_empty(),
                        "{phase:?} {kind:?} seed {seed}: {}",
                        r.violations.join("\n")
                    );
                    if acquisition::before_decision(phase) {
                        assert!(
                            matches!(outcome, Outcome::Aborted(_)),
                            "{phase:?} {kind:?}: {outcome:?}"
                        );
                        assert!(!moved, "{phase:?} {kind:?}: published after an abort");
                    } else {
                        assert_eq!(outcome, Outcome::Committed, "{phase:?} {kind:?}");
                        assert!(moved, "{phase:?} {kind:?}: the move did not complete");
                    }
                }
            }
        }
    }

    #[test]
    fn withdrawn_independent_legs_violate_some_fault_storm_within_64_seeds() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let hits = (1..=64u64)
            .filter(|s| {
                acquisition::storm_with(rules, 4, 6, 24, *s, true)
                    .violations
                    .iter()
                    .any(|v| v.starts_with("DEPENDENT"))
            })
            .count();
        eprintln!("fault storms: independent-legs {hits}/64");
        assert!(hits > 0, "no dependent-command violation in 64 storms — the model lost its teeth");
    }

    #[test]
    fn a_dependent_command_never_spans_one_owner() {
        let mut m = acquisition::Model::new(Rules::chosen(), 2, 1);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            m.submit(acquisition::TxnSpec {
                coordinator: 0,
                dependent: Some(Dependent::Move { source: 0, target: 2 }),
                ..Default::default()
            })
        }));
        assert!(r.is_err(), "keys 0 and 2 share cell 0: not a cross-owner command");
    }

    // ---- ordered programs: replies in queue order, read-your-writes
    // across a dependent command (the fix validation's F04 follow-up) ----

    #[test]
    fn the_ordered_rename_history_replies_in_queue_order_and_reads_its_own_move() {
        for native in [false, true] {
            let (r, outcome, reply, published) =
                acquisition::ordered_rename_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(
                reply,
                Some(vec![Reply::Ok, Reply::Ok, Reply::Value(Some(0))]),
                "native {native}: SET, RENAME, then GET b = the moved value"
            );
            assert_eq!(published, [None, Some(0)], "native {native}");
        }
    }

    #[test]
    fn withdrawn_independent_legs_answer_the_ordered_rename_from_published_state() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, reply, published) = acquisition::ordered_rename_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Err("no such key"), Reply::Value(None)]),
            "the destination never saw the staged value and the GET read the pre-state"
        );
        assert_eq!(published, [None, None]);
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's RENAME 0→1 replied no such key where the serial history \
                     replies OK"
                    .to_string(),
                "REPLY MISMATCH: T1's GET 1 replied absent where the serial history replies T1"
                    .to_string(),
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 1@cell1 = absent where the serial \
                     history gives T1"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn withdrawn_published_reads_miss_the_transactions_own_writes() {
        let rules = Rules { read_your_writes: false, ..Rules::chosen() };
        let (r, outcome, reply, published) = acquisition::ordered_rename_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [None, Some(0)], "the state is right — only the reply is wrong");
        assert_eq!(reply, Some(vec![Reply::Ok, Reply::Ok, Reply::Value(None)]));
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's GET 1 replied absent where the serial history replies T1"
                    .to_string()
            ]
        );
        let (r, _, reply, _) = acquisition::reply_order_history(rules, false);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Ok, Reply::Value(None), Reply::Value(None)]),
            "a GET after the transaction's own SET answers the pre-state"
        );
        assert_eq!(r.violations.len(), 2, "{}", r.violations.join("\n"));
    }

    #[test]
    fn consecutive_dependent_commands_each_see_the_previous_apply() {
        for native in [false, true] {
            let (r, outcome, reply, published) =
                acquisition::consecutive_dependents_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(
                reply,
                Some(vec![Reply::Int(1), Reply::Ok, Reply::Value(Some(0)), Reply::Value(None)]),
                "native {native}: MSETNX set both, RENAME moved b to c, GET c = T, GET b absent"
            );
            assert_eq!(published, [Some(0), None, Some(0)], "native {native}");
        }
    }

    #[test]
    fn withdrawn_independent_legs_break_consecutive_dependent_commands() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, reply, published) =
            acquisition::consecutive_dependents_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![
                Reply::Int(1),
                Reply::Err("no such key"),
                Reply::Value(None),
                Reply::Value(None)
            ]),
            "the RENAME read b as published (absent) and both GETs answered the pre-state"
        );
        assert_eq!(published, [Some(0), None, None], "c never received the value");
        assert!(
            r.violations.iter().any(|v| v.starts_with("REPLY MISMATCH: T1's RENAME 1→2"))
                && r.violations.iter().any(|v| v.starts_with("REPLY MISMATCH: T1's GET 2"))
                && r.violations
                    .iter()
                    .any(|v| v.starts_with("DEPENDENT COMMAND: T1's RENAME 1→2 left key 2@cell2")),
            "{}",
            r.violations.join("\n")
        );
    }

    #[test]
    fn a_refused_command_followed_by_reads_embeds_under_exec_and_aborts_under_native() {
        let (r, outcome, reply, published) = acquisition::refused_then_read_history(rules(), false);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![
                Reply::Ok,
                Reply::Err("destination refused"),
                Reply::Value(Some(0)),
                Reply::Value(None)
            ]),
            "the SET is kept, the refusal embedded, the reads see exactly that"
        );
        assert_eq!(published, [Some(0), None]);
        let (r, outcome, reply, published) = acquisition::refused_then_read_history(rules(), true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("destination refused"));
        assert_eq!(reply, None, "an aborted INF.TX has no reply array");
        assert_eq!(published, [None, None]);
    }

    #[test]
    fn replies_come_back_in_queue_order_whatever_the_legs_arrival_order() {
        let (r, outcome, reply, published) = acquisition::reply_order_history(rules(), false);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Ok, Reply::Value(Some(0)), Reply::Value(Some(0))])
        );
        assert_eq!(published, [Some(0), Some(0)]);
        let (r, outcome, reply, published) = acquisition::reply_order_history(rules(), true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed, "EXEC embeds the failed condition");
        assert_eq!(
            reply,
            Some(vec![
                Reply::Ok,
                Reply::Err("condition failed"),
                Reply::Value(None),
                Reply::Value(Some(0))
            ]),
            "the failed SET staged nothing, so GET b is absent and GET a is the SET"
        );
        assert_eq!(published, [Some(0), None]);
    }

    #[test]
    fn a_stage_compiles_monotonically_in_queue_order() {
        let program = vec![
            Cmd::Get(0),
            Cmd::Dep(Dependent::Move { source: 0, target: 1 }),
            Cmd::Set(1),
            Cmd::Dep(Dependent::SetIfNoneExist { keys: vec![2, 3] }),
            Cmd::Get(3),
        ];
        assert_eq!(acquisition::stages_of(&program), vec![(0, true), (1, true), (2, false)]);
        assert_eq!(acquisition::stages_of(&[Cmd::Set(0)]), vec![(0, false)]);
        assert_eq!(acquisition::stages_of(&[]), Vec::<(usize, bool)>::new());
    }

    // ---- watch ----

    fn watch_rules() -> watch::Rules {
        match Variant::from_env() {
            Variant::Chosen => watch::Rules::chosen(),
            Variant::Withdrawn => watch::Rules::withdrawn(),
        }
    }

    /// The chosen registration with the batch-22 repeated-WATCH rule.
    fn reregister() -> watch::Rules {
        watch::Rules { repeated_watch: RepeatedWatch::Reregister, ..watch::Rules::chosen() }
    }

    #[test]
    fn absent_present_absent_aborts() {
        let (verdict, oracle) = watch::run(watch_rules(), &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: absent → present → absent accepted by {:?}",
            watch_rules()
        );
    }

    #[test]
    fn withdrawn_endpoint_equality_accepts_absent_present_absent() {
        let (verdict, oracle) =
            watch::run(watch::Rules::withdrawn(), &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
        let (verdict, oracle) = watch::run(watch::Rules::withdrawn(), &watch::DELETE_RECREATE);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
    }

    #[test]
    fn delete_recreate_aborts() {
        let (verdict, oracle) = watch::run(watch_rules(), &watch::DELETE_RECREATE);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: delete/recreate accepted by {:?}",
            watch_rules()
        );
    }

    #[test]
    fn eviction_and_owner_restart_fail_closed() {
        for h in [
            [Event::Set(0), Event::Watch(0), Event::Evict(0), Event::Exec],
            [Event::Set(0), Event::Watch(0), Event::Restart, Event::Exec],
        ] {
            let (verdict, oracle) = watch::run(watch_rules(), &h);
            assert_eq!(oracle, Verdict::Abort);
            assert_eq!(
                verdict,
                Verdict::Abort,
                "WATCH HISTORY VIOLATION: {h:?} accepted by {:?}",
                watch_rules()
            );
        }
    }

    // ---- repeated WATCH (ADR-0116 A5, the review's F07) ----

    #[test]
    fn a_repeated_watch_keeps_the_first_registrations_history() {
        for h in [
            &watch::REPEATED_WATCH[..],
            &watch::ADDITIONAL_KEY_AFTER_CHANGE,
            &watch::REPEATED_WATCH_AFTER_EVICTION,
            &watch::REPEATED_WATCH_AFTER_RESTART,
        ] {
            let (verdict, oracle) = watch::run(watch_rules(), h);
            assert_eq!(oracle, Verdict::Abort, "{h:?}");
            assert_eq!(
                verdict,
                Verdict::Abort,
                "WATCH ORACLE RESET: {h:?} accepted by {:?}",
                watch_rules()
            );
        }
        // UNWATCH is the reset; the oracle is not vacuously strict.
        assert_eq!(
            watch::run(watch_rules(), &watch::RESET_THEN_WATCH),
            (Verdict::Commit, Verdict::Commit)
        );
    }

    #[test]
    fn withdrawn_reregistration_launders_the_mutation_between_two_watches() {
        assert_eq!(
            watch::run(reregister(), &watch::REPEATED_WATCH),
            (Verdict::Commit, Verdict::Abort),
            "WATCH ORACLE RESET: a second WATCH must not clear an earlier modification"
        );
        for h in [&watch::REPEATED_WATCH_AFTER_EVICTION, &watch::REPEATED_WATCH_AFTER_RESTART] {
            assert_eq!(watch::run(reregister(), h), (Verdict::Commit, Verdict::Abort), "{h:?}");
        }
        // The per-key rule is not what a second key exercises: the
        // additional-key history aborts under both.
        assert_eq!(
            watch::run(reregister(), &watch::ADDITIONAL_KEY_AFTER_CHANGE),
            (Verdict::Abort, Verdict::Abort)
        );
    }

    #[test]
    fn registration_agrees_with_the_history_oracle_on_random_histories() {
        let rules = watch_rules();
        let mut disagreements = Vec::new();
        for seed in 1..=2000u64 {
            let h = watch::random_history(seed, 7);
            let (verdict, oracle) = watch::run(rules, &h);
            if verdict != oracle {
                disagreements.push(format!("seed {seed} {h:?}: {verdict:?} vs oracle {oracle:?}"));
            }
        }
        assert!(
            disagreements.is_empty(),
            "WATCH HISTORY VIOLATION on {} of 2000 histories under {rules:?}; first: {}",
            disagreements.len(),
            disagreements[0]
        );
    }

    #[test]
    fn withdrawn_watch_rules_disagree_on_random_histories() {
        let count = |rules: watch::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    let (v, o) = watch::run(rules, &watch::random_history(*s, 7));
                    v != o
                })
                .count()
        };
        let withdrawn = count(watch::Rules::withdrawn());
        let reregister = count(reregister());
        eprintln!(
            "watch disagreements over 2000 histories: withdrawn {withdrawn}, reregister-only \
                 {reregister}"
        );
        assert!(withdrawn > 0 && reregister > 0);
    }

    /// Redis 8.0.5's own verdicts (`seeds/watch-redis-oracle.txt`,
    /// regenerated by `scripts/txmodel-watch-redis-oracle.py`): the
    /// chosen rules and the sticky-window oracle agree with Redis on
    /// every line; the batch-22 repeated-WATCH rule does not.
    #[test]
    fn chosen_rules_and_oracle_agree_with_redis_on_every_fixture_line() {
        let lines: Vec<(Vec<Event>, Verdict)> = watch::REDIS_FIXTURE
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| watch::parse_fixture_line(l).unwrap_or_else(|| panic!("fixture line {l:?}")))
            .collect();
        assert!(lines.len() >= 400, "fixture holds {} histories", lines.len());
        let mut disagreements = Vec::new();
        let mut reregister_disagreements = 0;
        for (h, redis) in &lines {
            let (verdict, oracle) = watch::run(watch::Rules::chosen(), h);
            if verdict != *redis || oracle != *redis {
                disagreements
                    .push(format!("{h:?}: model {verdict:?}, oracle {oracle:?}, Redis {redis:?}"));
            }
            let (verdict, _) = watch::run(reregister(), h);
            reregister_disagreements += usize::from(verdict != *redis);
        }
        assert!(
            disagreements.is_empty(),
            "REDIS ORACLE MISMATCH on {} of {} fixture lines; first: {}",
            disagreements.len(),
            lines.len(),
            disagreements[0]
        );
        eprintln!(
            "redis fixture: {} lines, chosen 0 disagreements, reregister \
                 {reregister_disagreements}",
            lines.len()
        );
        assert!(reregister_disagreements > 0, "the fixture does not reach the repeated-WATCH rule");
    }

    // ---- durable ----

    fn durable_rules() -> durable::Rules {
        match Variant::from_env() {
            Variant::Chosen => durable::Rules::chosen(),
            Variant::Withdrawn => durable::Rules::withdrawn(),
        }
    }

    #[test]
    fn everysec_checkpoint_never_holds_half_a_transaction() {
        let r = durable::run(durable_rules(), &durable::EVERYSEC_HALF_CHECKPOINT);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_rules_leave_half_a_transaction_in_the_checkpoint() {
        let r = durable::run(durable::Rules::withdrawn(), &durable::EVERYSEC_HALF_CHECKPOINT);
        assert_eq!(
            r,
            Err("PARTIAL COMMIT after crash: key0 = 10, key1 = 100 (decision durable: false)"
                .to_string())
        );
        // Each rule alone is insufficient.
        let only_wait = durable::Rules {
            decision_waits_for_durable_prepares: true,
            checkpoint_streams_predecessor: false,
        };
        let only_pin = durable::Rules {
            decision_waits_for_durable_prepares: false,
            checkpoint_streams_predecessor: true,
        };
        let wait_partials = (1..=2000u64)
            .filter(|s| durable::run(only_wait, &durable::random_steps(*s, 8)).is_err())
            .count();
        let pin_partials = (1..=2000u64)
            .filter(|s| durable::run(only_pin, &durable::random_steps(*s, 8)).is_err())
            .count();
        assert!(
            wait_partials > 0 && pin_partials > 0,
            "wait-only {wait_partials}, pin-only {pin_partials}"
        );
    }

    #[test]
    fn random_crash_interleavings_are_all_or_nothing() {
        let rules = durable_rules();
        let mut partials = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = durable::run(rules, &durable::random_steps(seed, 8)) {
                partials.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            partials.is_empty(),
            "{} of 2000 interleavings partial under {rules:?}; first: {}",
            partials.len(),
            partials[0]
        );
    }

    // ---- lineage (ADR-0116 A1/A2) ----

    fn lineage_rules() -> lineage::Rules {
        match Variant::from_env() {
            Variant::Chosen => lineage::Rules::chosen(),
            Variant::Withdrawn => lineage::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_durable_successor_never_outlives_its_dependency() {
        let r = lineage::run(lineage_rules(), &lineage::Shape::full_overlap(), &lineage::SUCCESSOR);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_plain_successor_outlives_its_dropped_source() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::withdrawn(), &shape, &lineage::SUCCESSOR);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [10, 101] is no serial subset of \
                 [Tx(1), Incr { cell: 1, id: 1 }] (decisions durable: {})".to_string())
        );
        // The qualified image alone does not repair it: the successor is still plain.
        let only_pin =
            lineage::Rules { successors_inherit_dependencies: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_pin, &shape, &lineage::SUCCESSOR).is_err());
    }

    #[test]
    fn a_checkpoint_streams_the_decision_qualified_image() {
        let r = lineage::run(lineage_rules(), &lineage::Shape::full_overlap(), &lineage::CHAIN);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_immediate_predecessor_leaks_half_of_the_older_transaction() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::withdrawn(), &shape, &lineage::CHAIN);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [100, 11] is no serial subset of \
                 [Tx(1), Tx(2)] (decisions durable: {})".to_string())
        );
        // Inheritance alone does not repair it: the image is still T1's.
        let only_inherit =
            lineage::Rules { pin_decision_qualified_image: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_inherit, &shape, &lineage::CHAIN).is_err());
    }

    #[test]
    fn an_acked_dependent_transaction_is_never_dropped() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::chosen(), &shape, &lineage::DEPENDENT_ACK);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn without_ack_gating_an_acked_dependent_transaction_is_dropped() {
        let rules =
            lineage::Rules { ack_waits_for_dependencies: false, ..lineage::Rules::chosen() };
        let r = lineage::run(rules, &lineage::Shape::full_overlap(), &lineage::DEPENDENT_ACK);
        assert_eq!(
            r,
            Err("ACKED WRITE LOST after crash: [Tx(2)] acked under `always` but recovered [10, 11] \
                 needs a subset without one of them (decisions durable: {2})".to_string())
        );
    }

    // ADR-0116 A8 — the fix validation's F01: a transaction's dependencies
    // are one set, or half of it recovers.

    #[test]
    fn a_partially_overlapping_transaction_recovers_whole() {
        let shape = lineage::Shape::partial_overlap();
        let r = lineage::run(lineage_rules(), &shape, &lineage::PARTIAL_OVERLAP);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::PARTIAL_OVERLAP),
            Ok(vec![10, 11, 12])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_recover_half_of_a_partially_overlapping_transaction() {
        let shape = lineage::Shape::partial_overlap();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::PARTIAL_OVERLAP);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [10, 11, 200] is no serial \
                 subset of [Tx(1), Tx(2)] (decisions durable: {2})"
                .to_string())
        );
        // Neither decision durable, T1's only, and both: the controls pass.
        let mut t1_only = lineage::PARTIAL_OVERLAP[..9].to_vec();
        t1_only.extend([lineage::Step::Fsync(0), lineage::Step::Crash]);
        let mut both = lineage::PARTIAL_OVERLAP[..10].to_vec();
        both.extend([lineage::Step::Fsync(0), lineage::Step::Crash]);
        for control in [lineage::PARTIAL_OVERLAP[..9].to_vec(), t1_only, both] {
            let r = lineage::run(lineage::Rules::per_record(), &shape, &control);
            assert!(r.is_ok(), "{}", r.unwrap_err());
        }
    }

    #[test]
    fn a_read_leg_makes_the_whole_transaction_dependent() {
        let shape = lineage::Shape::asymmetric_read();
        let r = lineage::run(lineage_rules(), &shape, &lineage::ASYMMETRIC_READ);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::ASYMMETRIC_READ),
            Ok(vec![10, 11, 12])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_keep_a_write_whose_read_was_dropped() {
        let shape = lineage::Shape::asymmetric_read();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::ASYMMETRIC_READ);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 11, 200] matches only subsets \
                 of [Tx(1), Tx(2)] that keep Tx(2) without Tx(1) it observed (decisions durable: \
                 {2})"
                .to_string())
        );
    }

    #[test]
    fn a_decision_never_outlives_a_plain_write_its_read_leg_observed() {
        let shape = lineage::Shape::read_watermark();
        let r = lineage::run(lineage_rules(), &shape, &lineage::READ_WATERMARK);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::READ_WATERMARK),
            Ok(vec![10, 11])
        );
    }

    #[test]
    fn withdrawn_prepare_only_watermarks_let_a_decision_outlive_what_it_read() {
        let shape = lineage::Shape::read_watermark();
        let rules = lineage::Rules {
            decision_waits_for_read_watermarks: false,
            ..lineage::Rules::chosen()
        };
        let r = lineage::run(rules, &shape, &lineage::READ_WATERMARK);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 100] matches only subsets of \
                 [Incr { cell: 0, id: 1 }, Tx(1)] that keep Tx(1) without Incr { cell: 0, id: 1 } \
                 it observed (decisions durable: {1})"
                .to_string())
        );
    }

    #[test]
    fn a_transitive_successor_inherits_the_whole_closure() {
        let shape = lineage::Shape::transitive();
        let r = lineage::run(lineage_rules(), &shape, &lineage::TRANSITIVE);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::TRANSITIVE),
            Ok(vec![10, 11, 12, 13])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_recover_a_transaction_two_hops_from_its_dropped_root() {
        let shape = lineage::Shape::transitive();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::TRANSITIVE);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 11, 300, 300] matches only \
                 subsets of [Tx(1), Tx(2), Tx(3)] that keep Tx(3) without Tx(1) it observed \
                 (decisions durable: {2, 3})"
                .to_string())
        );
    }

    #[test]
    fn random_lineage_interleavings_are_serializable_and_keep_every_ack() {
        let rules = lineage_rules();
        let shape = lineage::Shape::full_overlap();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = lineage::run(rules, &shape, &lineage::random_steps(&shape, seed, 20)) {
                violations.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 interleavings violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn random_lineage_shapes_are_serializable_and_keep_every_ack() {
        let rules = lineage_rules();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            let shape = lineage::Shape::random(seed);
            if let Err(e) = lineage::run(rules, &shape, &lineage::random_program(&shape, seed)) {
                violations.push(format!("seed {seed} {shape:?}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 shapes violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn withdrawn_lineage_rules_violate_some_random_interleavings() {
        let shape = lineage::Shape::full_overlap();
        let count = |rules: lineage::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    lineage::run(rules, &shape, &lineage::random_steps(&shape, *s, 20)).is_err()
                })
                .count()
        };
        let withdrawn = count(lineage::Rules::withdrawn());
        let only_inherit = count(lineage::Rules {
            pin_decision_qualified_image: false,
            ..lineage::Rules::chosen()
        });
        let only_pin = count(lineage::Rules {
            successors_inherit_dependencies: false,
            ..lineage::Rules::chosen()
        });
        let no_ack_gate =
            count(lineage::Rules { ack_waits_for_dependencies: false, ..lineage::Rules::chosen() });
        eprintln!(
            "lineage violations over 2000 interleavings: withdrawn {withdrawn}, inherit-only \
                 {only_inherit}, pin-only {only_pin}, no-ack-gate {no_ack_gate}"
        );
        assert!(withdrawn > 0 && only_inherit > 0 && only_pin > 0 && no_ack_gate > 0);
    }

    #[test]
    fn withdrawn_per_record_dependencies_violate_some_random_shapes() {
        let count = |rules: lineage::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    let shape = lineage::Shape::random(*s);
                    lineage::run(rules, &shape, &lineage::random_program(&shape, *s)).is_err()
                })
                .count()
        };
        let per_record = count(lineage::Rules::per_record());
        let prepare_only = count(lineage::Rules {
            decision_waits_for_read_watermarks: false,
            ..lineage::Rules::chosen()
        });
        let withdrawn = count(lineage::Rules::withdrawn());
        eprintln!(
            "lineage violations over 2000 random shapes: per-record {per_record}, prepare-only \
                 watermarks {prepare_only}, withdrawn {withdrawn}"
        );
        assert!(per_record > 0 && prepare_only > 0 && withdrawn > 0, "the model lost its teeth");
    }

    // ---- identity (ADR-0116 A3) ----

    fn identity_rules() -> identity::Rules {
        match Variant::from_env() {
            Variant::Chosen => identity::Rules::chosen(),
            Variant::Withdrawn => identity::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_remote_prepare_never_meets_a_reissued_txid() {
        let r = identity::run(identity_rules(), &identity::remote_prepare_history());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_replay_maximum_reissues_the_remote_prepares_txid() {
        let r = identity::run(identity::Rules::withdrawn(), &identity::remote_prepare_history());
        assert_eq!(
            r,
            Err("TXID REISSUE: (0, 41) issued again while a remote participant's durable prepare \
                 still carries it"
                .to_string())
        );
    }

    #[test]
    fn reservation_without_the_checkpoint_field_reissues_after_truncation() {
        let rules =
            identity::Rules { checkpoint_carries_reservation: false, ..identity::Rules::chosen() };
        let steps = [
            identity::Step::Issue,
            identity::Step::RemoteSurvives,
            identity::Step::Checkpoint,
            identity::Step::Crash,
            identity::Step::Issue,
        ];
        assert!(identity::run(rules, &steps).is_err(), "truncation dropped the reservation");
        assert!(identity::run(identity::Rules::chosen(), &steps).is_ok());
    }

    #[test]
    fn random_identity_histories_never_reissue() {
        let rules = identity_rules();
        let mut reissues = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = identity::run(rules, &identity::random_steps(seed, 24)) {
                reissues.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            reissues.is_empty(),
            "TXID REISSUE on {} of 2000 histories under {rules:?}; first: {}",
            reissues.len(),
            reissues[0]
        );
    }

    #[test]
    fn withdrawn_identity_reissues_within_2000_seeds() {
        let n = (1..=2000u64)
            .filter(|s| {
                identity::run(identity::Rules::withdrawn(), &identity::random_steps(*s, 24))
                    .is_err()
            })
            .count();
        assert!(
            n > 0,
            "the withdrawn identity rule survived 2000 histories — the model lost its teeth"
        );
    }

    // ---- revision (ADR-0116 A7) ----

    fn revision_rules() -> revision::Rules {
        match Variant::from_env() {
            Variant::Chosen => revision::Rules::chosen(),
            Variant::Withdrawn => revision::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_revision_token_never_repeats_across_the_version_wrap() {
        let stats = revision::run(revision_rules(), &revision::wrap_history());
        let stats = stats.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stats.issued, 1 + (1 << revision::VERSION_BITS));
        assert_eq!(stats.reincarnations, 1, "the wrapping mutation re-incarnated");
    }

    #[test]
    fn withdrawn_incarnation_at_creation_repeats_after_the_version_wraps() {
        let r = revision::run(revision::Rules::withdrawn(), &revision::wrap_history());
        assert_eq!(
            r.map(|_| ()),
            Err("REVISION REPEAT: (1, 0) issued again after 8 mutations of a record that never \
                 left — the u3 version wrapped under incarnation 1"
                .to_string())
        );
    }

    #[test]
    fn a_dead_incarnation_is_never_reissued_after_a_checkpointed_restart() {
        let stats = revision::run(revision_rules(), &revision::dead_incarnation_history());
        assert!(stats.is_ok(), "{}", stats.unwrap_err());
    }

    #[test]
    fn withdrawn_replayed_maximum_reissues_a_dead_incarnation() {
        let rules =
            revision::Rules { resume_from_durable_reservation: false, ..revision::Rules::chosen() };
        let r = revision::run(rules, &revision::dead_incarnation_history());
        assert_eq!(
            r.map(|_| ()),
            Err(
                "REVISION REPEAT: (1, 0) issued again for a new incarnation — the counter resumed \
                 below a dead incarnation after a restart"
                    .to_string()
            )
        );
    }

    #[test]
    fn an_exhausted_incarnation_counter_refuses_instead_of_repeating() {
        let stats = revision::run(revision_rules(), &revision::exhaustion_history());
        let stats = stats.unwrap_or_else(|e| panic!("{e}"));
        assert!(stats.refused >= 1, "{stats:?}");
        let rules = revision::Rules { refuse_at_exhaustion: false, ..revision::Rules::chosen() };
        let r = revision::run(rules, &revision::exhaustion_history());
        assert!(
            r.as_ref().is_err_and(|e| e.ends_with("the u4 incarnation counter wrapped")),
            "{r:?}"
        );
    }

    #[test]
    fn random_revision_histories_never_repeat_a_durable_token() {
        let rules = revision_rules();
        let mut repeats = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = revision::run(rules, &revision::random_steps(seed, 48)) {
                repeats.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            repeats.is_empty(),
            "REVISION REPEAT on {} of 2000 histories under {rules:?}; first: {}",
            repeats.len(),
            repeats[0]
        );
    }

    #[test]
    fn withdrawn_revision_rules_repeat_within_2000_seeds() {
        let count = |rules: revision::Rules| {
            (1..=2000u64)
                .filter(|s| revision::run(rules, &revision::random_steps(*s, 48)).is_err())
                .count()
        };
        let wrap =
            count(revision::Rules { reincarnate_on_wrap: false, ..revision::Rules::chosen() });
        let restart = count(revision::Rules {
            resume_from_durable_reservation: false,
            ..revision::Rules::chosen()
        });
        let exhaustion =
            count(revision::Rules { refuse_at_exhaustion: false, ..revision::Rules::chosen() });
        eprintln!(
            "revision repeats over 2000 histories: wrap {wrap}, restart {restart}, exhaustion \
                 {exhaustion}"
        );
        assert!(wrap > 0 && restart > 0 && exhaustion > 0, "the model lost its teeth");
    }

    // ---- credits (ADR-0116 A9, review F06) ----

    fn credits_rules() -> credits::Rules {
        match Variant::from_env() {
            Variant::Chosen => credits::Rules::chosen(),
            Variant::Withdrawn => credits::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_saturated_pair_with_contenders_behind_a_holder_completes() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits_rules(), cfg, &actions);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats = credits::run_script(credits::Rules::chosen(), cfg, &actions).expect("chosen");
        assert_eq!((stats.committed, stats.aborted, stats.refused), (2, 0, 7));
        assert!(stats.max_parked <= 1 && stats.max_ring <= cfg.data_credits as usize);
    }

    #[test]
    fn withdrawn_queued_grant_returns_one_credit_twice() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits::Rules::withdrawn(), cfg, &actions);
        assert_eq!(
            r,
            Err("CREDIT OVERFLOW: the coordinator holds 9 credits toward the owner of 8 — a second \
                 reply to one request returned its credit twice".to_string())
        );
        // Reserving at Admit does not repair a second reply.
        let queued_reserved = credits::Rules {
            grant_is_the_deferred_terminal_reply: false,
            ..credits::Rules::chosen()
        };
        let r = credits::run_script(queued_reserved, cfg, &actions);
        assert!(r.as_ref().is_err_and(|e| e.starts_with("CREDIT OVERFLOW")), "{r:?}");
    }

    #[test]
    fn withdrawn_hop_by_hop_credits_deadlock_a_holder_behind_its_own_contenders() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits::Rules::deferred_hop_by_hop(), cfg, &actions);
        assert_eq!(
            r,
            Err("CREDIT DEADLOCK: 9 transaction(s) stuck with no message in flight and 0 \
                 credit(s) toward the owner — T1 holds key 0@owner and waits for ExecOp; 8 parked \
                 LockOp(s) (T2, T3, T4, T5, T6, T7, T8, T9) hold every credit"
                .to_string())
        );
    }

    #[test]
    fn credit_storms_complete_and_drain_the_pair() {
        let rules = credits_rules();
        let mut violations = Vec::new();
        let mut committed = 0;
        for seed in 1..=64u64 {
            match credits::run(rules, credits::storm(), seed) {
                Ok(stats) => committed += stats.committed,
                Err(e) => violations.push(format!("seed {seed}: {e}")),
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 64 storms violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
        assert!(committed > 0);
    }

    #[test]
    fn withdrawn_credit_rules_violate_some_storm_within_64_seeds() {
        let count = |rules: credits::Rules| {
            (1..=64u64).filter(|s| credits::run(rules, credits::storm(), *s).is_err()).count()
        };
        let withdrawn = count(credits::Rules::withdrawn());
        let hop_by_hop = count(credits::Rules::deferred_hop_by_hop());
        eprintln!("credit storms: queued-grant {withdrawn}/64, hop-by-hop {hop_by_hop}/64");
        assert!(withdrawn > 0 && hop_by_hop > 0, "the model lost its teeth");
    }

    // ---- retention (ADR-0116 A10, review F08) ----

    fn retention_rules() -> retention::Rules {
        match Variant::from_env() {
            Variant::Chosen => retention::Rules::chosen(),
            Variant::Withdrawn => retention::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_tombstone_storm_is_refused_at_the_pinned_budget_and_admitted_after_the_decisions() {
        let (images, txns, steps) = retention::tombstone_storm_then_decisions();
        let r = retention::run(retention_rules(), retention::budget(), &images, &txns, &steps);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats =
            retention::run(retention::Rules::chosen(), retention::budget(), &images, &txns, &steps)
                .expect("chosen");
        assert_eq!(stats.committed, 4);
        assert_eq!(stats.refused.get(&retention::Refusal::Retain), Some(&2));
        assert_eq!(stats.max_retained, 2 * retention::MIB);
    }

    #[test]
    fn withdrawn_frame_bound_does_not_bound_what_a_tombstone_pins() {
        let (images, txns, steps) = retention::tombstone_storm();
        let r = retention::run(
            retention::Rules::withdrawn(),
            retention::budget(),
            &images,
            &txns,
            &steps,
        );
        assert_eq!(
            r,
            Err(
                "RETAINED BYTES EXCEED THE CLAIMED BOUND: 1 pending transaction(s) staged at most \
                 24 B each but retain 1048576 B of images and 0 B of extents — above the retention \
                 cap × frame bound of 256 B"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_cold_extent_is_charged_and_dependent_writes_pin_nothing_new() {
        let (images, txns, steps) = retention::cold_extent_and_dependents();
        let r = retention::run(retention_rules(), retention::budget(), &images, &txns, &steps);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats =
            retention::run(retention::Rules::chosen(), retention::budget(), &images, &txns, &steps)
                .expect("chosen");
        assert_eq!((stats.committed, stats.max_retained), (2, 4 * retention::MIB));
        assert!(stats.refused.is_empty());
    }

    #[test]
    fn random_retention_histories_are_conserved_bounded_and_leak_free() {
        let rules = retention_rules();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            let (images, txns, steps) = retention::random(seed);
            if let Err(e) = retention::run(rules, retention::budget(), &images, &txns, &steps) {
                violations.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 histories violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn withdrawn_retention_rules_exceed_the_claimed_bound_within_2000_seeds() {
        let count = (1..=2000u64)
            .filter(|s| {
                let (images, txns, steps) = retention::random(*s);
                retention::run(
                    retention::Rules::withdrawn(),
                    retention::budget(),
                    &images,
                    &txns,
                    &steps,
                )
                .is_err()
            })
            .count();
        eprintln!("retention claimed-bound violations over 2000 histories: {count}");
        assert!(count > 0, "the model lost its teeth");
    }
}
