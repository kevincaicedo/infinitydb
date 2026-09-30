//! A store's expiry schedule (ADR-0008 A1): the one owner of the timing
//! wheel, its membership table, the expiry sweep, the `owed` sequence and
//! the TTL census. Every field is private; the store reaches it only from
//! its record choke points (`store/lifecycle.rs`), so no command path
//! passes TTL facts (I2).
//!
//! A record with a deadline is either **scheduled** — its key hash has
//! exactly one live wheel node filed no later than the group's least
//! deadline + 1 — or **swept**: it has no node and the sweep owes it a
//! visit (rule 1). The transitions are A1's state table:
//!
//! | State | Event | Next |
//! |---|---|---|
//! | none | write with `D` | scheduled (`Filed`), or swept (`Refused`, sweep owed) |
//! | scheduled at `w` | write with `D` | `Held` if `w ≤ D + 1`, else `Moved` (swept if refused) |
//! | scheduled | cleared or death, walk `Complete` | kept if a member has a deadline, else gone |
//! | scheduled | cleared or death, walk `Over` | node removed, sweep owed |
//! | scheduled | fire | reap every expired member; re-file, release, or release and owe |
//! | swept | sweep visit | reaped if expired, else placed as from none |
//! | any | `FLUSH*`, `reserve_keys` on an empty store | schedule reset, sweep idle |
//!
//! The sweep walks index slots in passes (rule 6). A pass records the
//! `owed` sequence and the index's rebuild count when it begins and is
//! **clean** when both still match at its end: between rebuilds no record
//! changes slot, and every event that left a record with a deadline
//! without a node advanced `owed`, so a clean pass leaves every record
//! with a deadline scheduled (I7) and the sweep goes idle.

use crate::store::SweepState;
use crate::wheel::{Due, ExpiryBudget, FireNext, Placement, TickStats, TtlWheel, WheelNodesMax};

/// The removal question's answer about the rest of a key hash's group
/// (ADR-0139 D9's enumeration; ADR-0008 A1 rule 4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Survivors {
    /// The enumeration completed and a member with a deadline remains.
    WithDeadline,
    /// The enumeration completed and no member has a deadline.
    WithoutDeadline,
    /// The enumeration crossed a D9 budget: the question is unanswered.
    Over,
}

/// What a deadline cleared, or a death, did to a hash's node.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The hash had no node (a swept record): no enumeration ran.
    NoNode,
    /// A member with a deadline keeps the node.
    Kept,
    /// No member with a deadline remains: the node was removed.
    Removed,
    /// The group could not be enumerated: the node was removed and the
    /// sweep owed (`expiry_alias_over`).
    Over,
}

/// What a written record's deadline change did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    /// The deadline did not change.
    Unchanged,
    Placed(Placement),
    Cleared(Removal),
}

/// One fire's result from the store: the members it reaped, the node's
/// next state (rule 5), and the least deadline its enumeration saw among
/// every member, reaped or not (`None` on `Over` or with no deadline) —
/// what the I4 check at fire compares the node's deadline with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fire {
    pub reaped: u32,
    pub next: FireNext,
    pub least_deadline_ms: Option<u64>,
}

/// Where a sweep pass stands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SweepAt {
    /// The next index slot the pass walks (masked by the capacity).
    pub cursor: usize,
    /// Slots left before the pass ends.
    pub slots_left: usize,
}

/// How a sweep pass ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PassEnd {
    /// No owed event and no rebuild since it began: the sweep is idle.
    Clean,
    /// An owed event since it began: another pass is owed.
    Dirty,
    /// An index rebuild since it began voided it; another pass is owed
    /// (`sweep_passes_voided`).
    Voided,
}

struct Pass {
    began_ms: u64,
    begin_slot: usize,
    slots_left: usize,
    /// `owed` when the pass began (under `inf_canary_sweep_owed_by_own_refusals`,
    /// the sweep's own refusal count instead).
    owed: u64,
    rebuilds: u64,
}

enum SweepPhase {
    Idle,
    /// A pass is owed; `None` until the next slice begins it.
    Walking(Option<Pass>),
}

pub(crate) struct ExpirySchedule {
    wheel: TtlWheel,
    nodes_max: WheelNodesMax,
    phase: SweepPhase,
    /// The next index slot the sweep walks. A new pass begins here, never
    /// at slot 0, so no slot range starves under voided passes.
    sweep_cursor: usize,
    /// When the last clean or dirty pass since the sweep left idle began
    /// (a voided pass completes nothing).
    completed_began_ms: Option<u64>,
    /// Events that owe the sweep a pass: every `Refused` placement (a
    /// write, a `Moved`, the sweep's own) and every `Over`. u64 cannot
    /// wrap: at most one per operation, 584 years at one per nanosecond.
    owed: u64,
    /// The sweep's own refusals — read only by the canary that stamps a
    /// pass with them alone (I11's canary).
    own_refusals: u64,
    /// Records with a deadline (`INFO keyspace` `expires=`), exact (I8).
    ttl_live: u64,
}

impl ExpirySchedule {
    pub(crate) fn new(start_ms: u64, nodes_max: WheelNodesMax) -> ExpirySchedule {
        ExpirySchedule {
            wheel: TtlWheel::new(start_ms, nodes_max),
            nodes_max,
            phase: SweepPhase::Idle,
            sweep_cursor: 0,
            completed_began_ms: None,
            owed: 0,
            own_refusals: 0,
            ttl_live: 0,
        }
    }

    /// The record table was replaced by an empty one: no record has a
    /// deadline, so nothing is scheduled or owed.
    pub(crate) fn reset(&mut self, start_ms: u64) {
        let owed = self.owed;
        *self = ExpirySchedule::new(start_ms, self.nodes_max);
        self.owed = owed;
        self.check_census();
    }

    pub(crate) fn ttl_live(&self) -> u64 {
        self.ttl_live
    }

    pub(crate) fn armed(&self) -> u64 {
        self.wheel.armed()
    }

    pub(crate) fn tombstones(&self) -> u64 {
        self.wheel.tombstones()
    }

    pub(crate) fn cursor_ms(&self) -> u64 {
        self.wheel.cursor_ms()
    }

    pub(crate) fn resident_bytes(&self) -> u64 {
        self.wheel.resident_bytes()
    }

    pub(crate) fn live_bytes(&self) -> u64 {
        self.wheel.live_bytes()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn node_deadline(&self, hash: u64) -> Option<u64> {
        self.wheel.node_deadline(hash)
    }

    // ---- the record transitions (rules 2–4) ----

    /// A written record's deadline went from `old` to `new`. `survivors`
    /// runs only when a cleared deadline's hash has a node.
    pub(crate) fn on_write(
        &mut self,
        hash: u64,
        old: Option<u64>,
        new: Option<u64>,
        survivors: impl FnOnce() -> Survivors,
    ) -> Transition {
        let transition = match (old, new) {
            (None, None) => Transition::Unchanged,
            (Some(before), Some(after)) if before == after => Transition::Unchanged,
            (None, Some(deadline_ms)) => {
                self.ttl_live += 1;
                Transition::Placed(self.place(hash, deadline_ms))
            }
            (Some(_), Some(deadline_ms)) => Transition::Placed(self.place(hash, deadline_ms)),
            (Some(_), None) => {
                self.ttl_live -= 1;
                Transition::Cleared(self.remove(hash, survivors))
            }
        };
        self.check_census();
        transition
    }

    /// A record died; `deadline` is what it carried. `survivors` runs
    /// only when its hash has a node.
    pub(crate) fn on_death(
        &mut self,
        hash: u64,
        deadline: Option<u64>,
        survivors: impl FnOnce() -> Survivors,
    ) -> Removal {
        if deadline.is_none() {
            return Removal::NoNode;
        }
        self.ttl_live -= 1;
        let removal = self.remove(hash, survivors);
        self.check_census();
        removal
    }

    /// Placement as a transition: a refusal owes the sweep.
    fn place(&mut self, hash: u64, deadline_ms: u64) -> Placement {
        let placement = self.wheel.place(hash, deadline_ms);
        // I4's placement half: whatever the outcome, the hash's node (if
        // any) files no later than the placed deadline + 1.
        debug_assert!(
            self.wheel.node_deadline(hash).is_none_or(|filed| filed <= deadline_ms),
            "a placement left its hash's node after the placed deadline (I4)"
        );
        if placement == Placement::Refused && !cfg!(inf_canary_wheel_refused_lazy) {
            self.owe();
        }
        placement
    }

    /// I5 after every transition: each live node schedules a hash with at
    /// least one record with a deadline, so `armed ≤ ttl_live`. Exempt
    /// only under the canary that plants a node per change, whose oracle
    /// is the same bound read by the tests.
    fn check_census(&self) {
        debug_assert!(
            self.wheel.armed() <= self.ttl_live || cfg!(inf_canary_wheel_arm_per_change),
            "armed {} > ttl_live {} (I5)",
            self.wheel.armed(),
            self.ttl_live
        );
    }

    fn remove(&mut self, hash: u64, survivors: impl FnOnce() -> Survivors) -> Removal {
        if self.wheel.node_deadline(hash).is_none() {
            return Removal::NoNode;
        }
        // A hash is not identity: the group is asked before any decision
        // (the canary plants removal by hash alone).
        let answer = if cfg!(inf_canary_wheel_release_by_hash) {
            Survivors::WithoutDeadline
        } else {
            survivors()
        };
        match answer {
            Survivors::WithDeadline => Removal::Kept,
            Survivors::WithoutDeadline => {
                self.wheel.remove(hash);
                Removal::Removed
            }
            Survivors::Over => {
                self.wheel.remove(hash);
                self.owe();
                Removal::Over
            }
        }
    }

    fn owe(&mut self) {
        self.owed += 1;
        if matches!(self.phase, SweepPhase::Idle) {
            self.phase = SweepPhase::Walking(None);
            self.completed_began_ms = None;
        }
    }

    // ---- the wheel's fires (rule 5) ----

    /// Advances the wheel toward `now_ms`; `fire(hash)` reaps the group's
    /// expired members and decides the node's next state.
    pub(crate) fn tick(
        &mut self,
        now_ms: u64,
        budget: ExpiryBudget,
        mut fire: impl FnMut(u64) -> Fire,
    ) -> TickStats {
        // The phase cannot change inside the wheel's tick: its owed fires
        // are applied after it returns.
        let sweep_idle = matches!(self.phase, SweepPhase::Idle);
        let mut reaped = 0u64;
        let stats = self.wheel.tick(now_ms, budget, |due: Due| {
            let answer = fire(due.hash);
            // I4's fire half, idle-scoped: a walking sweep may owe a
            // member whose hash a later placement re-filed (after an
            // `Over` or a refused `Moved`); an idle sweep owes none.
            debug_assert!(
                !sweep_idle
                    || answer.least_deadline_ms.is_none_or(|least| due.deadline_ms <= least),
                "a node fired after its group's least deadline + 1 (I4)"
            );
            reaped += u64::from(answer.reaped);
            answer.next
        });
        self.ttl_live -= reaped;
        for _ in 0..stats.owed {
            self.owe();
        }
        self.check_census();
        stats
    }

    // ---- the sweep (rule 6) ----

    /// The pass under way, beginning one at `sweep_cursor` when one is
    /// owed and none is walking. `None` when idle.
    pub(crate) fn sweep_begin(
        &mut self,
        now_ms: u64,
        capacity: usize,
        rebuilds: u64,
    ) -> Option<SweepAt> {
        let stamp = self.pass_stamp();
        let begin_slot = self.sweep_cursor & (capacity - 1);
        self.sweep_cursor = begin_slot;
        let pass = match &mut self.phase {
            SweepPhase::Idle => return None,
            SweepPhase::Walking(pass) => pass.get_or_insert(Pass {
                began_ms: now_ms,
                begin_slot,
                slots_left: capacity,
                owed: stamp,
                rebuilds,
            }),
        };
        Some(SweepAt { cursor: self.sweep_cursor, slots_left: pass.slots_left })
    }

    /// Records one walked chunk of `walked` slots ending at `next_cursor`.
    pub(crate) fn sweep_walked(&mut self, next_cursor: usize, walked: usize) -> SweepAt {
        self.sweep_cursor = next_cursor;
        let slots_left = match &mut self.phase {
            SweepPhase::Walking(Some(pass)) => {
                pass.slots_left = pass.slots_left.saturating_sub(walked);
                pass.slots_left
            }
            SweepPhase::Walking(None) | SweepPhase::Idle => {
                debug_assert!(false, "a chunk walked with no pass under way");
                0
            }
        };
        SweepAt { cursor: next_cursor, slots_left }
    }

    /// Ends the pass whose slots are walked: clean ⇒ idle; otherwise
    /// another pass is owed and begins at the next slice.
    pub(crate) fn sweep_end(&mut self, capacity: usize, rebuilds: u64) -> PassEnd {
        let stamp = self.pass_stamp();
        let SweepPhase::Walking(Some(pass)) = &self.phase else {
            debug_assert!(false, "a pass ended with none under way");
            return PassEnd::Dirty;
        };
        debug_assert_eq!(pass.slots_left, 0, "a pass ends once its slots are walked");
        debug_assert!(
            pass.rebuilds != rebuilds || self.sweep_cursor & (capacity - 1) == pass.begin_slot,
            "an unvoided pass is exactly one lap of the table"
        );
        let end = if pass.rebuilds != rebuilds {
            PassEnd::Voided
        } else if pass.owed != stamp {
            PassEnd::Dirty
        } else {
            PassEnd::Clean
        };
        // A clean or dirty pass visited every record present between
        // rebuilds; a voided one proves nothing (a rebuild moved records
        // across the cursor), so it completes no pass the drain reads.
        match end {
            PassEnd::Clean | PassEnd::Dirty => self.completed_began_ms = Some(pass.began_ms),
            PassEnd::Voided => {}
        }
        self.phase =
            if end == PassEnd::Clean { SweepPhase::Idle } else { SweepPhase::Walking(None) };
        end
    }

    /// The sweep's own placement of a live record it visited.
    pub(crate) fn sweep_place(&mut self, hash: u64, deadline_ms: u64) -> Placement {
        let placement = self.place(hash, deadline_ms);
        if placement == Placement::Refused {
            self.own_refusals += 1;
        }
        self.check_census();
        placement
    }

    /// What a pass stamps and compares: the whole `owed` sequence (I11).
    /// The canary stamps the sweep's own refusals alone, so a write
    /// refused behind the cursor cannot dirty the pass.
    fn pass_stamp(&self) -> u64 {
        if cfg!(inf_canary_sweep_owed_by_own_refusals) { self.own_refusals } else { self.owed }
    }

    pub(crate) fn sweep_state(&self) -> SweepState {
        match &self.phase {
            SweepPhase::Idle => SweepState::Idle,
            SweepPhase::Walking(pass) => SweepState::Walking {
                pass_began_ms: pass.as_ref().map(|p| p.began_ms),
                completed_pass_began_ms: self.completed_began_ms,
            },
        }
    }

    /// `(begin slot, cursor)` of the pass under way (test-support).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn sweep_pass_slots(&self) -> Option<(usize, usize)> {
        match &self.phase {
            SweepPhase::Walking(Some(pass)) => Some((pass.begin_slot, self.sweep_cursor)),
            SweepPhase::Walking(None) | SweepPhase::Idle => None,
        }
    }

    /// `(orphan membership entries, unpointed linked nodes)` (test-support).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn audit_links(&self) -> (u64, u64) {
        self.wheel.audit_links()
    }

    /// The most tombstones in one wheel list (test-support).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn list_tombstones_max(&self) -> u64 {
        self.wheel.list_tombstones_max()
    }
}

/// The I4 and I5 debug assertions fire on the states they forbid (and I4
/// stays silent on the one it scopes out). Debug builds only: release
/// compiles the checks away.
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn fire_with_least(least_deadline_ms: u64) -> impl FnMut(u64) -> Fire {
        move |_| Fire {
            reaped: 0,
            next: FireNext::Release,
            least_deadline_ms: Some(least_deadline_ms),
        }
    }

    /// A node filed at 100 for a hash scheduled under the census.
    fn scheduled_at_100() -> ExpirySchedule {
        let mut schedule = ExpirySchedule::new(0, WheelNodesMax::MAX);
        assert_eq!(
            schedule.on_write(7, None, Some(100), || Survivors::WithoutDeadline),
            Transition::Placed(Placement::Filed)
        );
        schedule
    }

    #[test]
    #[should_panic(expected = "(I5)")]
    fn a_node_no_record_accounts_for_trips_i5() {
        let mut schedule = ExpirySchedule::new(0, WheelNodesMax::MAX);
        schedule.wheel.place(7, 100);
        schedule.check_census();
    }

    #[test]
    #[should_panic(expected = "(I4)")]
    fn an_idle_fire_later_than_a_member_deadline_trips_i4() {
        let mut schedule = scheduled_at_100();
        schedule.tick(200, ExpiryBudget::UNBOUNDED, fire_with_least(50));
    }

    #[test]
    fn a_walking_sweep_scopes_the_fire_check_out() {
        let mut schedule = scheduled_at_100();
        schedule.owe();
        let stats = schedule.tick(200, ExpiryBudget::UNBOUNDED, fire_with_least(50));
        assert_eq!(stats.fired, 1, "the owed member is the sweep's, not a late node");
    }
}
