//! One in-flight flush round's plane bookkeeping (ADR-0178 T4): its phase,
//! its group cursor and the identity its completion tokens carry. The
//! cursor is this module's alone: [`FlushRound::place_cursor`] is the one
//! writer of `next_unsent` after the open, so the rule it keeps — the op at
//! the cursor is unsent, or the phase is spent — cannot be broken by a
//! second writer elsewhere in the tier cell.

use inf_log::fs::SegmentFs;
use inf_log::{OpState, TierFlush};

/// Plane bookkeeping of one in-flight flush round (M4.5-S31): the group
/// cursor, the phase, and the identity its completion tokens carry. The
/// ops' states and positions live in the staged ops themselves
/// (`TierFlush::round_op`, ADR-0167 A1), so no second table beside them
/// exists to keep in step.
pub(crate) struct FlushRound {
    /// Ops submitted, not yet terminal.
    pub(super) pending: u32,
    /// Writes until every write is done, then barriers (ADR-0178 T4: the
    /// two are distinct states, so a barrier group is never built with a
    /// write unsent or outstanding).
    phase: RoundPhase,
    /// The group builder's cursor: the first unsent op of the phase's
    /// range, or the range's end once the phase is spent — the one rule
    /// every writer of it keeps through [`FlushRound::place_cursor`] (the
    /// open, a retry, the barrier phase, each emit), so the walk starts
    /// at an unsent op and no run of done ops ever stands between the
    /// cursor and the next group.
    next_unsent: usize,
    /// When the group at the cursor became ready, on the injected clock
    /// in microseconds: the open, a grant that left unsent ops, a retry,
    /// the barrier phase. `None` while nothing is unsent.
    pub(super) ready_since_us: Option<u64>,
    /// The unsent ops are a retry of failed writes (the ungranted-retry
    /// plant reads it).
    pub(super) retry: bool,
    pub(super) round_seq: u32,
    pub(super) staged_at_us: u64,
    /// Worst write error of the current attempt (ENOSPC dominates).
    pub(super) write_error: Option<i32>,
    /// A barrier failed — fatal at the next MAINTAIN (fsync failure is
    /// fail-stop, ADR-0056 D4).
    pub(super) fatal: Option<i32>,
}

/// A round's phase: its writes, then its barriers.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum RoundPhase {
    Writes,
    Barriers,
}

impl FlushRound {
    pub(super) fn new(round_seq: u32, staged_at_us: u64) -> FlushRound {
        FlushRound {
            pending: 0,
            phase: RoundPhase::Writes,
            next_unsent: 0,
            ready_since_us: Some(staged_at_us),
            retry: false,
            round_seq,
            staged_at_us,
            write_error: None,
            fatal: None,
        }
    }

    /// The round's phase.
    pub(super) fn phase(&self) -> RoundPhase {
        self.phase
    }

    /// The group builder's cursor (a test's diagnostics read it).
    #[cfg(test)]
    pub(super) fn cursor(&self) -> usize {
        self.next_unsent
    }

    /// Every write is done (or, under the early-barrier plant, sent): the
    /// barrier phase begins, its cursor placed at the first barrier — the
    /// phase and the cursor move together.
    pub(super) fn enter_barriers<F: SegmentFs>(&mut self, flush: &TierFlush<F>) {
        self.phase = RoundPhase::Barriers;
        let writes = flush.round_write_count();
        self.place_cursor(writes, flush);
    }

    /// The op range the current phase may issue from, from the cursor.
    pub(super) fn phase_range(&self, writes: usize, op_count: usize) -> core::ops::Range<usize> {
        match self.phase {
            RoundPhase::Writes => self.next_unsent..writes,
            RoundPhase::Barriers => self.next_unsent.max(writes)..op_count,
        }
    }

    /// The first unsent op of the current phase's range — the cursor, by
    /// the rule its writers keep — or `None` once the phase is spent.
    /// One primitive for "is a group ready" and "where does it start".
    pub(super) fn first_unsent<F: SegmentFs>(&self, flush: &TierFlush<F>) -> Option<usize> {
        let range = self.phase_range(flush.round_write_count(), flush.round_op_count());
        let first = range.clone().find(|&index| unsent(flush, index));
        debug_assert_eq!(
            first.unwrap_or(range.end),
            range.start,
            "the op at the cursor is unsent, or the phase is spent"
        );
        first
    }

    /// Whether an unsent op lies in the current phase's range: a group
    /// is ready to be built and offered.
    pub(super) fn has_ready_group<F: SegmentFs>(&self, flush: &TierFlush<F>) -> bool {
        self.first_unsent(flush).is_some()
    }

    /// Places the cursor at the first unsent op at or above `from` in the
    /// current phase's range, or at the range's end when none is left:
    /// the one writer of `next_unsent`. A retry calls it from 0 (a failed
    /// write may lie below the old cursor), the barrier phase from the
    /// first barrier, an emit from its group's end — so a run of done
    /// ops is passed here, once, never charged to a turn's visits.
    pub(super) fn place_cursor<F: SegmentFs>(&mut self, from: usize, flush: &TierFlush<F>) {
        self.next_unsent = from;
        let range = self.phase_range(flush.round_write_count(), flush.round_op_count());
        self.next_unsent = range.clone().find(|&index| unsent(flush, index)).unwrap_or(range.end);
    }
}

/// Whether round op `index` is unsent, read from the staged op itself.
pub(super) fn unsent<F: SegmentFs>(flush: &TierFlush<F>, index: usize) -> bool {
    flush.round_op_state(index) == Some(OpState::Unsent)
}
