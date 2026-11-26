//! The retirement walk (ADR-0156 D1, D2, D4): once a MAINTAIN, after the
//! namespaces' pass, the cell consumes its transport's ready results, then
//! visits its pending positions in cyclic order from `head`, offering each
//! eligible owner to the control thread's worker as a job; the cell decides
//! eligibility and the worker closes and unlinks, so no tier file is
//! unlinked on a cell.
//!
//! | Position, owner | Condition | Effect | Charge |
//! |---|---|---|---|
//! | Draining(Round) | a sent op not terminal | next position | 1 visit |
//! | Draining(Round) | every sent op terminal | the round finished; Files; continue | 1 visit |
//! | Draining(Files) | release not durable | next position | 1 visit |
//! | its residue slot, first | Held, due, offers < 8 | offered as `Residue` | 1 visit, 1 offer |
//! | the same | Held, not due | the position's next owner | 1 visit |
//! | Draining(Files), catalog's last | pinned | onto `behind` | 1 visit |
//! | the same | unpinned, offers < 8 | offered with its handle | 1 visit, 1 offer |
//! | the same | catalog emptied this visit | next position (its list next turn) | — |
//! | the list, `ahead`'s last | pinned, or not due | onto `behind` | 1 visit |
//! | the same | eligible, offers < 8 | offered with its handle | 1 visit, 1 offer |
//! | any offer | no job position Free | it stays; the turn ends | 1 visit, 1 offer |
//! | any offer | offers = 8 | the turn ends | — |
//! | the list | `ahead` empty, `behind` not | a pass ends; next position | — |
//! | Live | list empty, slot not Held | bit cleared | — |
//! | Draining(Files) | nothing left, no job out, holds clear | Vacant, receipt | 1 visit |
//!
//! A turn makes at most 64 visits, its results' included, and 8 offers;
//! its offers publish once and ring the node's bell once; `head` moves one
//! past the turn's first position, so a position pending throughout is a
//! turn's first within 64 turns.

use inf_foundation::grants::{GrantPos, POSITIONS, ReceiptWriter};
use inf_foundation::time::Nanos;
use inf_log::flush::ResidueState;
use inf_log::fs::SegmentFs;
use inf_log::{NsId, TierFileMeta};
use inf_runtime::{ColdReads, OwnerPhase, TierFileId, Timers};

use super::{
    CellGrants, DrainPhase, Position, RetiringFile, TierCell, TierNs, TierRetireStats, drained,
    take_handle,
};
use crate::limits::{
    TIER_RETIRE_OFFERS_PER_TURN, TIER_RETIRE_RETRY_NS, TIER_RETIRE_VISITS_PER_TURN,
};
use crate::tier_retire::{Consumed, JobKind, OfferBatch, RetireOutcome};

/// The cell's retirement gauges (`INFO`, cell scope).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RetireGauges {
    pub(crate) stats: TierRetireStats,
    /// Job positions Out, 0–8.
    pub(crate) jobs_out: u64,
    /// Retiring owners plus the dropped namespaces' catalogs.
    pub(crate) pending: u64,
    /// Residue slots Held or Out.
    pub(crate) residues: u64,
}

/// What keeps a cell's tiered half from quiescence, each a count: flush
/// rounds out, cold reads queued or in flight, extent readers' holds,
/// extent fences the commit ledger has not settled, and dropped owners
/// whose drop is not yet durable. A harness opens a retirement window only
/// where every count is zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TierQuiescence {
    pub rounds_out: u32,
    pub cold_reads: u64,
    pub extent_holds: u32,
    pub fences_unsettled: u32,
    pub releases_pending: u32,
}

impl TierQuiescence {
    /// Every count zero.
    #[must_use]
    pub fn quiet(&self) -> bool {
        *self == TierQuiescence::default()
    }
}

/// One turn's allowance and what it examined.
struct Turn {
    visits: u32,
    offers: u32,
    /// Owners and residues whose pin or due the turn read (F8's count).
    examined: u32,
}

impl Turn {
    /// A visit charged: one Maintenance unit. A planted canary
    /// (`inf_canary_retire_walk_unpaced`) leaves a pinned or not-yet-due
    /// owner's visit uncharged, so a turn's work grows with them.
    fn charge_pass_over(&mut self) {
        if !cfg!(inf_canary_retire_walk_unpaced) {
            self.visits += 1;
        }
    }

    fn spent(&self) -> bool {
        self.visits >= TIER_RETIRE_VISITS_PER_TURN
    }

    fn offers_spent(&self) -> bool {
        self.offers >= TIER_RETIRE_OFFERS_PER_TURN
    }
}

/// How one position's visit ended.
enum Visited {
    /// The walk goes on to the next pending position.
    Next,
    /// The turn ends: its visits or offers are spent, or no job position is
    /// Free.
    End,
    /// The position holds no catalog file, no retiring owner and no held
    /// residue.
    Empty,
}

/// What a turn's visit of one owner reads beside the owner.
struct WalkCx<'w, 'b, J> {
    cold: Option<&'w ColdReads>,
    batch: &'w mut OfferBatch<'b, J>,
    turn: &'w mut Turn,
    now: Nanos,
}

impl<J> WalkCx<'_, '_, J> {
    fn pinned(&self, id: u32) -> bool {
        self.cold.is_some_and(|c| c.inflight_on(TierFileId::new(id)) > 0)
    }

    /// Offers one owner charging `bytes`, `room` checked first: `Err` hands
    /// it back whole.
    fn offer(
        &mut self,
        t: &mut TierNs<impl SegmentFs<File = J>>,
        kind: JobKind,
        (meta, bytes): (TierFileMeta, u64),
        handle: Option<J>,
    ) -> Result<(), (TierFileMeta, Option<J>)> {
        self.batch.offer(t.pos, t.ns, kind, meta, handle)?;
        self.turn.visits += 1;
        self.turn.offers += 1;
        t.jobs_out += 1;
        if kind == JobKind::Retired {
            t.out_bytes += bytes;
        }
        Ok(())
    }

    /// Whether the turn may offer now; `Some` ends the visit. A deferred
    /// offer is spent (ADR-0156 D4) and ends the turn: no position frees
    /// before the next turn's results.
    fn offer_blocked(&mut self) -> Option<Visited> {
        if self.turn.offers_spent() {
            return Some(Visited::End);
        }
        if !self.batch.room() {
            self.turn.visits += 1;
            self.turn.offers += 1;
            return Some(Visited::End);
        }
        None
    }
}

impl<F: SegmentFs + Clone> TierCell<F> {
    /// The retirement walk's turn (ADR-0156 D4), each MAINTAIN after the
    /// namespaces' pass: the transport's ready results first, at most 8, a
    /// visit each; then every pending position from `head`, cyclic, each at
    /// most once, under the turn's 64 visits and 8 offers. `persisted` is
    /// the drop epoch's durability (ADR-0100 D5: no dropped file goes
    /// before its drop is durable) and `settled` the commit ledger's extent
    /// fence. Returns the turn's visits, its Maintenance units.
    pub fn retire_walk(
        &mut self,
        now: Nanos,
        persisted: impl Fn(u64) -> bool,
        settled: impl Fn(u64) -> bool,
    ) -> u32 {
        #[cfg(inf_canary_grant_return_at_drop)]
        self.parked.retain(|t| t.round.as_ref().is_some_and(|r| r.pending > 0));
        #[cfg(test)]
        {
            self.examined = 0;
        }
        let mut turn = Turn { visits: self.consume_results(now), offers: 0, examined: 0 };
        let TierCell { fs, cold, positions, grants, pending, head, .. } = self;
        let Some(CellGrants { part, receipts, board, wake, .. }) = grants.as_mut() else {
            return turn.visits;
        };
        if *pending == 0 {
            return turn.visits;
        }
        let first = first_pending(*pending, *head);
        let snapshot = *pending;
        let mut batch = part.offers();
        for k in 0..POSITIONS {
            let at = (first + k) % POSITIONS;
            if snapshot & (1 << at) == 0 {
                continue;
            }
            if turn.spent() {
                break;
            }
            let cx = &mut WalkCx { cold: cold.as_ref(), batch: &mut batch, turn: &mut turn, now };
            let slot = &mut positions[at];
            match visit_position(fs, slot, cx, &persisted, &settled, receipts) {
                Visited::Next => {}
                Visited::Empty => *pending &= !(1 << at),
                Visited::End => break,
            }
        }
        *head = (first + 1) % POSITIONS;
        if batch.publish() > 0 {
            board.bell().ring(&*wake);
        }
        #[cfg(test)]
        {
            self.examined = turn.examined;
        }
        turn.visits
    }

    /// The transport's ready results, at most 8, each resolved by its
    /// position and namespace id to its owner (ADR-0156 D2). Returns the
    /// results consumed.
    pub(super) fn consume_results(&mut self, now: Nanos) -> u32 {
        let TierCell { positions, grants, retire_stats, pending, .. } = self;
        let Some(grants) = grants.as_mut() else { return 0 };
        // With no job out no result can wait: the turn reads only cell-local
        // words, never the result ring's shared index.
        if grants.part.jobs_out() == 0 {
            return 0;
        }
        let mut due_last = None;
        let (taken, stale) = grants.part.consume(|consumed| {
            if let Some(due) = consume_result(positions, retire_stats, pending, consumed, now) {
                due_last = Some(due);
            }
        });
        retire_stats.stale += stale as u64;
        // Canary `inf_canary_retire_timer_unarmed`: a consumed `Retry` arms
        // nothing, so an idle cell waits for its next tick to retry.
        if let Some(due) = due_last
            && !cfg!(inf_canary_retire_timer_unarmed)
        {
            self.retry_due_last = due;
            self.arm_wanted = true;
        }
        taken as u32
    }

    /// The retirement role's arm (ADR-0154 A1): after a turn whose consumed
    /// `Retry` set a due, the role is armed at it when Idle; Armed, its
    /// deadline is the earlier and its callback's successor reaches the
    /// latest. A refused arm (the clock's range) leaves the role Idle, and
    /// the walk offers each retry at the first turn past its due.
    pub(crate) fn arm_retire_role(&mut self, timers: &mut Timers<'_>) {
        if !std::mem::take(&mut self.arm_wanted) {
            return;
        }
        let Some(owner) = self.retire_timer.as_ref() else { return };
        if timers.phase(owner) == OwnerPhase::Idle {
            let _ = timers.arm(owner, self.retry_due_last);
        }
    }

    /// The role's callback: one compare of the latest due with the turn, no
    /// I/O. `Some(due)` when a retry is not yet due (the successor's
    /// deadline); `None` when every retry is due, which this iteration's
    /// walk offers.
    pub(crate) fn retire_role_due(&self, now: Nanos) -> Option<Nanos> {
        (self.retry_due_last > now).then_some(self.retry_due_last)
    }

    /// The retirement role's phase on `timers` and the latest due, `None`
    /// with no role.
    pub(crate) fn retire_role(
        &self,
        timers: &inf_runtime::TimerSet,
    ) -> Option<(OwnerPhase, Nanos)> {
        self.retire_timer.as_ref().map(|owner| (timers.phase(owner), self.retry_due_last))
    }

    /// Hands the cell its retirement role's owner (`enable_durable`).
    pub(crate) fn set_retire_timer(&mut self, owner: inf_runtime::TimerOwner) {
        self.retire_timer = Some(owner);
    }

    /// The cell's retirement gauges.
    pub(crate) fn retire_gauges(&self) -> RetireGauges {
        let jobs_out = self.grants.as_ref().map_or(0, |grants| grants.part.jobs_out()) as u64;
        let (mut pending, mut residues) = (0u64, 0u64);
        for position in &self.positions {
            let (t, catalog) = match position {
                Position::Live(t) => (t, 0),
                Position::Draining { ns: t, .. } => (t, t.flush.catalog_len()),
                Position::Vacant { .. } => continue,
            };
            pending += (t.retiring.len() + catalog) as u64;
            residues += u64::from(!matches!(t.flush.residue(), ResidueState::Empty));
        }
        RetireGauges { stats: self.retire_stats, jobs_out, pending, residues }
    }

    /// What keeps the cell from quiescence; `persisted` and `settled` as the
    /// walk reads them.
    pub(crate) fn quiescence(
        &self,
        persisted: impl Fn(u64) -> bool,
        settled: impl Fn(u64) -> bool,
    ) -> TierQuiescence {
        let mut q = TierQuiescence::default();
        if let Some(cold) = self.cold.as_ref() {
            q.cold_reads = (cold.queue_depth() + cold.inflight_total()) as u64;
        }
        for position in &self.positions {
            let t = match position {
                Position::Live(t) => t,
                Position::Draining { ns: t, release, .. } => {
                    q.releases_pending += u32::from(!persisted(*release));
                    t
                }
                Position::Vacant { .. } => continue,
            };
            q.rounds_out += u32::from(t.round.is_some());
            q.extent_holds += u32::from(std::rc::Rc::strong_count(&t.holds) > 1);
            let fence = t.extent_fence.get();
            q.fences_unsettled += u32::from(fence != 0 && !settled(fence));
        }
        q
    }

    /// The owners and residues the last turn examined (F8's count).
    #[cfg(test)]
    pub(crate) fn walk_examined(&self) -> u32 {
        self.examined
    }

    /// The walk's pending positions.
    #[cfg(test)]
    pub(crate) fn walk_pending(&self) -> u64 {
        self.pending
    }

    /// Puts `meta` on the retirement list of the Live owner at index `at`
    /// of the Live order, as a covering swap's commit does: eligible at
    /// once, its handle already closed, the position pending.
    #[cfg(test)]
    pub(crate) fn retire_for_test(&mut self, at: usize, meta: TierFileMeta) {
        let pos = self.live[at];
        let Some(t) = super::live_at_mut(&mut self.positions, pos) else {
            debug_assert!(false, "a Live owner at {at}");
            return;
        };
        t.retiring.reserve(1, t.jobs_out as usize);
        let bytes = meta.disk_bytes();
        t.retiring.push(RetiringFile { meta, handle: None, due: Nanos(0), bytes });
        self.pending |= 1 << pos.index();
    }

    /// The owners on the retirement list of the Live owner at `at`.
    #[cfg(test)]
    pub(crate) fn retiring_at(&self, at: usize) -> usize {
        super::live_at(&self.positions, self.live[at]).map_or(0, |t| t.retiring.len())
    }
}

/// The first pending position at or after `head`, cyclic; `pending ≠ 0`.
fn first_pending(pending: u64, head: usize) -> usize {
    let above = pending & (u64::MAX << head);
    let bits = if above != 0 { above } else { pending };
    bits.trailing_zeros() as usize
}

/// One pending position's visit.
fn visit_position<F: SegmentFs>(
    fs: &F,
    slot: &mut Position<F>,
    cx: &mut WalkCx<'_, '_, F::File>,
    persisted: &impl Fn(u64) -> bool,
    settled: &impl Fn(u64) -> bool,
    receipts: &ReceiptWriter,
) -> Visited {
    let (t, release, phase) = match slot {
        Position::Vacant { .. } => return Visited::Empty,
        Position::Live(t) => {
            return match visit_owner(fs, t, false, cx) {
                Visited::Empty if matches!(t.flush.residue(), ResidueState::Held(_)) => {
                    Visited::Next
                }
                visited @ (Visited::Next | Visited::End | Visited::Empty) => visited,
            };
        }
        Position::Draining { ns, release, phase } => (ns, *release, phase),
    };
    if *phase == DrainPhase::Round {
        cx.turn.visits += 1;
        if t.round.as_ref().is_some_and(|r| r.pending > 0) {
            return Visited::Next;
        }
        if t.round.take().is_some() {
            let _ = t.flush.finish_round();
        }
        *phase = DrainPhase::Files;
        if cx.turn.spent() {
            return Visited::End;
        }
    }
    if !persisted(release) {
        cx.turn.visits += 1;
        return Visited::Next;
    }
    match visit_owner(fs, t, true, cx) {
        Visited::Empty => {}
        visited @ (Visited::Next | Visited::End) => return visited,
    }
    if cx.turn.spent() {
        return Visited::End;
    }
    cx.turn.visits += 1;
    if !drained(t, settled) {
        return Visited::Next;
    }
    debug_assert_eq!(
        t.jobs_out, 0,
        "RECEIPT_WITH_JOBS_OUT: namespace {} published its receipt with jobs out",
        t.ns.0
    );
    let (pos, id, round_seq) = (t.pos, t.ns, t.round_seq);
    *slot = Position::Vacant { round_seq };
    receipts.publish(pos, id.0);
    Visited::Empty
}

/// One owner's visit: its residue slot first, then (Draining) its catalog
/// from the end, then its retirement list's pass.
fn visit_owner<F: SegmentFs>(
    fs: &F,
    t: &mut TierNs<F>,
    draining: bool,
    cx: &mut WalkCx<'_, '_, F::File>,
) -> Visited {
    if let Some(end) = visit_residue(t, cx) {
        return end;
    }
    if draining && t.flush.catalog_end_id().is_some() {
        return visit_catalog(fs, t, cx);
    }
    loop {
        let Some(owner) = t.retiring.ahead_last() else {
            if t.retiring.end_pass() {
                return Visited::Next;
            }
            return Visited::Empty;
        };
        if cx.turn.spent() {
            return Visited::End;
        }
        cx.turn.examined += 1;
        // Canary `inf_canary_retire_due_ignored`: a retried owner is offered
        // before its due.
        let early = cx.now < owner.due && !cfg!(inf_canary_retire_due_ignored);
        if cx.pinned(owner.meta.id) || early {
            t.retiring.move_behind();
            cx.turn.charge_pass_over();
            continue;
        }
        if let Some(end) = cx.offer_blocked() {
            return end;
        }
        let Some(owner) = t.retiring.take_ahead_last() else { return Visited::End };
        let RetiringFile { meta, handle, bytes, .. } = owner;
        if draining && cfg!(inf_canary_drain_unlink_on_cell) {
            unlink_on_cell(fs, t, meta, handle, cx);
            continue;
        }
        if let Err((meta, handle)) = cx.offer(t, JobKind::Retired, (meta, bytes), handle) {
            // Unreachable: the batch had room above. The owner keeps its
            // file and its handle, never closed on the cell.
            t.retiring.push(RetiringFile { meta, handle, due: Nanos(0), bytes });
            return Visited::End;
        }
    }
}

/// The owner's residue slot, visited first: a held failed create's file
/// offered once due. `Some` ends the position's visit or the turn.
fn visit_residue<F: SegmentFs>(
    t: &mut TierNs<F>,
    cx: &mut WalkCx<'_, '_, F::File>,
) -> Option<Visited> {
    let ResidueState::Held(due) = t.flush.residue() else { return None };
    if cx.turn.spent() {
        return Some(Visited::End);
    }
    cx.turn.examined += 1;
    if cx.now < due && !cfg!(inf_canary_retire_due_ignored) {
        cx.turn.charge_pass_over();
        return None;
    }
    if let Some(end) = cx.offer_blocked() {
        return Some(end);
    }
    let meta = t.flush.take_residue(due.max(cx.now))?;
    if let Err((meta, _)) = cx.offer(t, JobKind::Residue, (meta, 0), None) {
        // Unreachable: the batch had room above. The slot holds it again.
        t.flush.residue_retry(meta, cx.now);
        return Some(Visited::End);
    }
    None
}

/// A dropped namespace's catalog, from its end: the active file, its staged
/// seals, its sealed files, last first. A pinned file moves onto the list
/// with its handle; the catalog's emptying ends the visit, its list taken
/// from the next turn.
fn visit_catalog<F: SegmentFs>(
    fs: &F,
    t: &mut TierNs<F>,
    cx: &mut WalkCx<'_, '_, F::File>,
) -> Visited {
    while let Some(id) = t.flush.catalog_end_id() {
        if cx.turn.spent() {
            return Visited::End;
        }
        cx.turn.examined += 1;
        let pinned = cx.pinned(id);
        if !pinned && let Some(end) = cx.offer_blocked() {
            return end;
        }
        let Some((meta, handle)) = t.flush.pop_catalog_end() else { break };
        let handle = handle.or_else(|| take_handle(&mut t.files, meta.id));
        if pinned {
            let bytes = meta.disk_bytes();
            t.retiring.push(RetiringFile { meta, handle, due: Nanos(0), bytes });
            cx.turn.charge_pass_over();
            continue;
        }
        if cfg!(inf_canary_drain_unlink_on_cell) {
            unlink_on_cell(fs, t, meta, handle, cx);
            continue;
        }
        let bytes = meta.disk_bytes();
        if let Err((meta, handle)) = cx.offer(t, JobKind::Retired, (meta, bytes), handle) {
            // Unreachable: the batch had room above.
            t.retiring.push(RetiringFile { meta, handle, due: Nanos(0), bytes });
            return Visited::End;
        }
    }
    Visited::Next
}

/// Canary `inf_canary_drain_unlink_on_cell`: the cell closes and unlinks a
/// dropped namespace's file itself, on its loop, as before the transport; a
/// failure other than absence keeps the file on the list.
fn unlink_on_cell<F: SegmentFs>(
    fs: &F,
    t: &mut TierNs<F>,
    meta: TierFileMeta,
    handle: Option<F::File>,
    cx: &mut WalkCx<'_, '_, F::File>,
) {
    drop(handle);
    cx.turn.visits += 1;
    cx.turn.offers += 1;
    let unlinked = inf_log::flush::unlink_tier_file(fs, &meta);
    if unlinked.is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound) {
        let bytes = meta.disk_bytes();
        t.retiring.push(RetiringFile { meta, handle: None, due: Nanos(0), bytes });
    }
}

/// One consumed result, resolved by its position and namespace id to its
/// owner: a terminal one drops the owner (its handle was closed by the
/// worker) or empties its residue slot; a `Retry` returns it onto `behind`
/// of its list, or into its residue slot, charged, counted by its errno
/// class, due `TIER_RETIRE_RETRY_NS` after `now`, its position pending.
/// Returns the due a `Retry` set.
fn consume_result<F: SegmentFs>(
    positions: &mut [Position<F>; POSITIONS],
    stats: &mut TierRetireStats,
    pending: &mut u64,
    consumed: Consumed,
    now: Nanos,
) -> Option<Nanos> {
    let Consumed { pos, ns, kind, meta, outcome } = consumed;
    let Some(owner) = owner_of(positions, pos, ns) else {
        // Unreachable: a position's owner leaves only at its terminal step,
        // which waits for every job of it consumed.
        debug_assert!(false, "a result's owner left its position");
        stats.stale += 1;
        return None;
    };
    debug_assert!(owner.jobs_out > 0, "a result answers one of the owner's jobs");
    owner.jobs_out = owner.jobs_out.saturating_sub(1);
    if kind == JobKind::Retired {
        owner.out_bytes = owner.out_bytes.saturating_sub(meta.disk_bytes());
    }
    let cause = match outcome {
        RetireOutcome::Unlinked | RetireOutcome::AlreadyAbsent => {
            stats.files_retired += 1;
            if kind == JobKind::Residue {
                owner.flush.residue_terminal();
            }
            return None;
        }
        RetireOutcome::Retry(cause) => cause,
    };
    if cause.transient() {
        stats.errors_transient += 1;
    } else {
        stats.errors_operator += 1;
    }
    // Canary `inf_canary_unlink_result_ignored`: the cell drops the owner at
    // any result, so a file whose unlink failed is never retried.
    if cfg!(inf_canary_unlink_result_ignored) {
        if kind == JobKind::Residue {
            owner.flush.residue_terminal();
        }
        return None;
    }
    let due = Nanos(now.0.saturating_add(TIER_RETIRE_RETRY_NS.0));
    match kind {
        JobKind::Retired => {
            // Canary `inf_canary_retire_charge_released_on_retry`: the
            // retried owner's bytes leave the disk sum while its file stays.
            let bytes = if cfg!(inf_canary_retire_charge_released_on_retry) {
                0
            } else {
                meta.disk_bytes()
            };
            owner.retiring.push(RetiringFile { meta, handle: None, due, bytes });
        }
        JobKind::Residue => owner.flush.residue_retry(meta, due),
    }
    *pending |= 1 << pos.index();
    Some(due)
}

/// The owner at `pos` named `ns`, Live or Draining.
fn owner_of<F: SegmentFs>(
    positions: &mut [Position<F>; POSITIONS],
    pos: GrantPos,
    ns: NsId,
) -> Option<&mut TierNs<F>> {
    match &mut positions[pos.index()] {
        Position::Draining { ns: t, .. } | Position::Live(t) if t.ns == ns => Some(t),
        Position::Draining { .. } | Position::Live(_) | Position::Vacant { .. } => None,
    }
}
