//! The control thread (M2-S08, ADR-0015 D3) — the slow plane the
//! architecture always reserved (§4). Its first job: **single writer of
//! the node catalog `META` file**. Cells must never block on file I/O, so
//! DDL persists asynchronously: the origin cell's pump sends a request
//! over a bounded channel and parks on a cell-local waitlist; this thread
//! performs the write-new + fsync + rename + dir-fsync swap (`inf-log`'s
//! `meta` protocol) and publishes a monotone **persist epoch**; every
//! cell's MAINTAIN observes the epoch and wakes its parked pumps.
//!
//! The thread also owns namespace-id allocation (ids are node-unique,
//! allocated once, never reused — ADR-0015 D2): a shared `AtomicU32`
//! seeded from the catalog's `next_id` at boot. One `fetch_add` per DDL is
//! control-plane traffic; L1's no-shared-atomics rule binds the data plane.
//!
//! Since ADR-0100 D2 the writer also owns the **drop-tombstone set**: a
//! durable namespace's `DROP` adds its id to every payload the writer
//! emits until every cell has published a `MANIFEST` past the drop (the
//! origin requests that checkpoint and stamps the tombstone with its
//! epoch; the writer retires stamped tombstones at the next persist once
//! every cell's published epoch covers them). Recovery reads the set to
//! tell dropped residue from corruption.
//!
//! Checkpoint requests are issued, not counted (ADR-0159 A1): a boot's
//! partition mints one quota per cell, one for this writer and one final
//! credit per cell, and only a credit advances the checkpoint clock, so an
//! epoch can neither wrap nor repeat. An exhausted quota refuses before
//! any effect.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
// denylist-allow: the cell->control DDL channel (ADR-0015 D3), drained on this thread only.
use std::sync::mpsc;
use std::time::Duration;

use inf_foundation::issue::{
    self, FinalCredit, IssueClock, IssueCredit, IssueExhausted, IssueQuota, IssueSpace, Issued,
    Partition, PartitionRefused, RequestWord,
};
use inf_foundation::{CellCount, CellId};
use inf_log::fs::StdSegmentFs;
use inf_log::meta::{read_meta, write_meta};
use inf_store::{FIRST_INDEX_GENERATION, FIRST_INDEX_ID, FIRST_NAMED_NS_ID, NsCatalog, NsSpec};

use crate::limits::CKPT_BOARD_VISITS_PER_TURN;

/// Recycled-life residue recovery proved at boot (M4.5-S39b, ADR-0090
/// D2/D4 as amended): how many segments ended their data at a foreign-
/// segment frame and how many slacks the audit classified as recycled
/// residue. Zero on every log that never recycled.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveredResidue {
    pub segment_residue_stops: u64,
    pub recycled_residue_slacks: u64,
    /// Slacks the audit classified as a discarded life's residue by
    /// epoch (ADR-0031 D5 as amended) — each one lifted a residue stop,
    /// so the segments beyond it replayed as the live log. The DST's
    /// observable for the lift regime (review 2026-08-30, F-L14-01).
    pub stale_residue_slacks: u64,
}

/// One cell's boot-recovery slot on the [`RecoveryBoard`] (M2-S15).
/// Single writer (the owning cell, relaxed stores); readers are the
/// control thread's progress printer and any cell's `INFO persistence`
/// aggregation. Control-plane observability, not shared data-plane state
/// (the L1 carve-out class the park board documents).
#[derive(Debug, Default)]
pub struct CellRecoverySlot {
    /// 0 = recovering, 1 = ready (recovery complete, cell serving).
    state: AtomicU8,
    bytes_total: AtomicU64,
    bytes_done: AtomicU64,
    segments_total: AtomicU64,
    segments_done: AtomicU64,
    /// Records recovered (checkpoint + tail) — the ready log line.
    records: AtomicU64,
    /// Packed torn-truncation LSN + 1 (0 = no torn tail) for the boot line.
    torn_at: AtomicU64,
    /// M4.5-S39b (ADR-0090 D2 as amended): recycled-life residue the
    /// audit proved — replay ends at a foreign-segment frame, slacks
    /// classified as recycled residue. The boot line and `INFO
    /// persistence` (`recover_segment_residue_stops`,
    /// `recover_recycled_residue_slacks`) carry them.
    segment_residue_stops: AtomicU64,
    recycled_residue_slacks: AtomicU64,
    stale_residue_slacks: AtomicU64,
    /// ADR-0103 D4: tail records skipped for an id no drop tombstone
    /// explains — the C14 verifier (zero on every honest boot).
    skipped_unknown_ns: AtomicU64,
    /// M4.5-S39d: the boot's per-phase loop-clock time (ns, pipeline
    /// order: start, checkpoint, replay, audit, finish) and the bytes the
    /// checkpoint, replay and audit phases read — the boot line's
    /// decomposition.
    phase_ns: [AtomicU64; 5],
    phase_bytes: [AtomicU64; 3],
    /// Recovery phase about to run (M2.5-S01): published *before* each
    /// step so a step stalled inside the kernel names itself — the
    /// ADR-0022 D7 wedge was invisible precisely because nothing was
    /// published around the blocking section. 0 = not started; codes per
    /// `Recovery::phase_code`.
    phase: AtomicU8,
}

impl CellRecoverySlot {
    /// Publishes one progress sample (owning cell only).
    pub fn publish(&self, bytes_done: u64, bytes_total: u64, segs_done: u64, segs_total: u64) {
        self.bytes_done.store(bytes_done, Ordering::Relaxed);
        self.bytes_total.store(bytes_total, Ordering::Relaxed);
        self.segments_done.store(segs_done, Ordering::Relaxed);
        self.segments_total.store(segs_total, Ordering::Relaxed);
    }

    /// Marks this cell recovered (owning cell only, once).
    pub fn mark_ready(
        &self,
        records: u64,
        torn_truncated_at: Option<inf_log::Lsn>,
        residue: RecoveredResidue,
        phases: crate::recover::RecoverPhases,
        skipped_unknown_ns: u64,
    ) {
        debug_assert_eq!(self.state.load(Ordering::Relaxed), 0, "mark_ready called twice");
        self.records.store(records, Ordering::Relaxed);
        self.skipped_unknown_ns.store(skipped_unknown_ns, Ordering::Relaxed);
        self.torn_at.store(torn_truncated_at.map_or(0, |lsn| lsn.to_u64() + 1), Ordering::Relaxed);
        self.segment_residue_stops.store(residue.segment_residue_stops, Ordering::Relaxed);
        self.recycled_residue_slacks.store(residue.recycled_residue_slacks, Ordering::Relaxed);
        self.stale_residue_slacks.store(residue.stale_residue_slacks, Ordering::Relaxed);
        for (slot, ns) in self.phase_ns.iter().zip(phases.phase_ns()) {
            slot.store(ns, Ordering::Relaxed);
        }
        for (slot, bytes) in self.phase_bytes.iter().zip([
            phases.ckpt_bytes,
            phases.replay_bytes,
            phases.audit_bytes,
        ]) {
            slot.store(bytes, Ordering::Relaxed);
        }
        self.state.store(1, Ordering::Release);
    }

    /// The boot's phase decomposition (M4.5-S39d), valid once ready:
    /// `(phase_ns in pipeline order, [ckpt, replay, audit] bytes)`.
    pub fn phases(&self) -> ([u64; 5], [u64; 3]) {
        (
            std::array::from_fn(|i| self.phase_ns[i].load(Ordering::Relaxed)),
            std::array::from_fn(|i| self.phase_bytes[i].load(Ordering::Relaxed)),
        )
    }

    /// Tail records this boot skipped for a namespace id no tombstone
    /// explains (ADR-0103 D4), valid once ready.
    pub fn records_skipped_unknown_ns(&self) -> u64 {
        self.skipped_unknown_ns.load(Ordering::Relaxed)
    }

    /// The recycled-residue facts of this cell's boot (ADR-0090 D4).
    pub fn residue(&self) -> RecoveredResidue {
        RecoveredResidue {
            segment_residue_stops: self.segment_residue_stops.load(Ordering::Relaxed),
            recycled_residue_slacks: self.recycled_residue_slacks.load(Ordering::Relaxed),
            stale_residue_slacks: self.stale_residue_slacks.load(Ordering::Relaxed),
        }
    }

    /// Publishes the phase the next recovery step will run (owning cell).
    pub fn publish_phase(&self, code: u8) {
        self.phase.store(code, Ordering::Relaxed);
    }

    /// Last published phase code (0 = recovery never stepped).
    pub fn phase(&self) -> u8 {
        self.phase.load(Ordering::Relaxed)
    }

    /// Human name for a phase code — the stuck-cell narration. Codes
    /// 1–6 are `Recovery::phase_code`; 10+ are the assembly's setup
    /// steps (published by `cell_main` so a pre-loop stall names itself —
    /// the 500-cycle storm caught exactly that class).
    #[must_use]
    pub fn phase_name(code: u8) -> &'static str {
        match code {
            0 => "spawned",
            1 => "start",
            2 => "checkpoint",
            3 => "replay",
            4 => "audit",
            5 => "finish",
            6 => "complete",
            10 => "setup:listen",
            11 => "setup:pool",
            12 => "setup:driver",
            13 => "setup:register",
            14 => "setup:keyspace",
            15 => "setup:plane",
            16 => "setup:loop",
            _ => "unknown",
        }
    }

    pub fn ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == 1
    }

    pub fn bytes(&self) -> (u64, u64) {
        (self.bytes_done.load(Ordering::Relaxed), self.bytes_total.load(Ordering::Relaxed))
    }

    pub fn segments(&self) -> (u64, u64) {
        (self.segments_done.load(Ordering::Relaxed), self.segments_total.load(Ordering::Relaxed))
    }

    /// Records recovered (checkpoint + tail), valid once [`ready`](Self::ready).
    pub fn records(&self) -> u64 {
        self.records.load(Ordering::Relaxed)
    }

    /// `Some(lsn)` when this cell truncated a torn final write (M2-S14).
    pub fn torn_truncated_at(&self) -> Option<inf_log::Lsn> {
        match self.torn_at.load(Ordering::Relaxed) {
            0 => None,
            packed => Some(inf_log::Lsn::from_u64(packed - 1)),
        }
    }
}

/// Node-wide boot-recovery progress (M2-S15): one slot per cell, created
/// before any cell thread spawns. `-LOADING` gates on [`all_ready`]
/// (Redis semantics: the node loads until *every* cell is live); `INFO
/// persistence` and the control thread's progress lines aggregate it.
///
/// [`all_ready`]: RecoveryBoard::all_ready
#[derive(Debug)]
pub struct RecoveryBoard {
    cells: Vec<CellRecoverySlot>,
    /// Unix ms at board creation (boot start) — `loading_start_time`/ETA.
    start_unix_ms: u64,
}

impl RecoveryBoard {
    #[must_use]
    pub fn new(cells: u16, start_unix_ms: u64) -> RecoveryBoard {
        RecoveryBoard {
            cells: (0..cells).map(|_| CellRecoverySlot::default()).collect(),
            start_unix_ms,
        }
    }

    /// This cell's slot (single writer: the owning cell).
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &CellRecoverySlot {
        &self.cells[usize::from(cell)]
    }

    /// True once every cell finished recovery — the `-LOADING` gate drops.
    #[must_use]
    pub fn all_ready(&self) -> bool {
        self.cells.iter().all(CellRecoverySlot::ready)
    }

    #[must_use]
    pub fn ready_cells(&self) -> u64 {
        self.cells.iter().filter(|slot| slot.ready()).count() as u64
    }

    #[must_use]
    pub fn cell_count(&self) -> u64 {
        self.cells.len() as u64
    }

    /// Aggregate (done, total) bytes across cells.
    #[must_use]
    pub fn bytes(&self) -> (u64, u64) {
        self.cells.iter().fold((0, 0), |(d, t), slot| {
            let (done, total) = slot.bytes();
            (d + done, t + total)
        })
    }

    #[must_use]
    pub fn start_unix_ms(&self) -> u64 {
        self.start_unix_ms
    }
}

/// One cell's checkpoint request/publication slot (M2-S20, ADR-0021 D6).
/// Same L1 control-plane carve-out class as the recovery board: the
/// request side is raised by an issuer (a targeted or final request —
/// ADR-0159 A1.3), the publication side by the owning cell when the
/// MANIFEST swap's dir-fsync commits (the durability point — ADR-0017).
/// Readers are the owning cell's MAINTAIN, `WAIT CELL k` and the cells'
/// board sweeps.
#[derive(Debug, Default)]
pub struct CkptSlot {
    /// Requests aimed at this cell alone (`INF.CKPT CELL k`, its stop).
    req: RequestWord,
    /// Highest epoch whose checkpoint reached a *durable* MANIFEST.
    published: AtomicU64,
    /// The published checkpoint id / unix ms (the `LASTSAVE` currency).
    ckpt_id: AtomicU64,
    unix_ms: AtomicU64,
}

impl CkptSlot {
    /// Publication (owning cell, at the swap's dir-fsync commit).
    pub fn publish(&self, epoch: u64, ckpt_id: u64, unix_ms: u64) {
        self.ckpt_id.store(ckpt_id, Ordering::Relaxed);
        self.unix_ms.store(unix_ms, Ordering::Relaxed);
        self.published.fetch_max(epoch, Ordering::Release);
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn last_unix_ms(&self) -> u64 {
        self.unix_ms.load(Ordering::Relaxed)
    }

    pub fn last_ckpt_id(&self) -> u64 {
        self.ckpt_id.load(Ordering::Relaxed)
    }

    /// Once this slot published `epoch`, the publication time read after
    /// the `Acquire` load that satisfied it. Private: the one caller is
    /// [`BoardSweep::confirm_cell_wait`], which raises the `LASTSAVE` floor
    /// with it in the same step.
    fn covered_at(&self, epoch: CheckpointEpoch) -> Option<u64> {
        (self.published() >= epoch.get()).then(|| self.last_unix_ms())
    }
}

/// Node-wide checkpoint board (ADR-0159 A1.3): the all-cell request word
/// and one slot per cell. The boot's issue clock owns it, so only a credit
/// of this boot raises a request (ADR-0159 A1.2).
#[derive(Debug)]
pub struct CkptBoard {
    /// All-cell requests: one `fetch_max` per request, never a fan.
    request_all: RequestWord,
    cells: Box<[CkptSlot]>,
}

impl CkptBoard {
    fn new(cells: CellCount) -> CkptBoard {
        CkptBoard {
            request_all: RequestWord::new(),
            cells: (0..cells.get()).map(|_| CkptSlot::default()).collect(),
        }
    }

    /// This cell's slot.
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &CkptSlot {
        &self.cells[usize::from(cell)]
    }

    /// The checkpoint epoch `cell` owes: the larger of the all-cell word
    /// and its own slot (A1.3), both read with `Acquire` (A1.6). The owning
    /// cell's MAINTAIN edge-detects it.
    #[must_use]
    pub fn requested(&self, cell: u16) -> u64 {
        self.request_all.read().max(self.slot(cell).req.read())
    }

    /// Cells on the board (the boot's `CellCount`).
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    fn raise(&self, target: CkptTarget, issued: Issued) {
        match target {
            CkptTarget::All => self.request_all.raise(issued),
            CkptTarget::Cell(cell) => self.slot(cell.0).req.raise(issued),
        }
    }

    /// The lowest published epoch across cells: the catalog writer's retire
    /// fold (ADR-0100 D3), one pass of `N <= 16,384` `Acquire` loads per
    /// persist on the control thread, never on a cell. Cells read their
    /// sweep's observation instead (ADR-0159 D4).
    fn min_published(&self) -> u64 {
        self.cells.iter().map(CkptSlot::published).min().unwrap_or(0)
    }

    /// One bounded step of a cell's sweep (ADR-0159 D4, A1.4): at most
    /// [`CKPT_BOARD_VISITS_PER_TURN`] slots from the sweep's cursor, each
    /// read `published` (`Acquire`) then its publication time. A sweep of
    /// `N` cells completes in `ceil(N / 64)` steps and then publishes its
    /// observation.
    pub fn sweep_step(&self, sweep: &mut BoardSweep) -> SweepStep {
        let begin = sweep.cursor;
        let end = self.cells.len().min(begin + CKPT_BOARD_VISITS_PER_TURN);
        debug_assert!(begin < end, "a sweep's cursor stays below the board's cell count");
        for slot in &self.cells[begin..end] {
            let published = slot.published();
            let unix_ms = slot.last_unix_ms();
            let partial = &mut sweep.partial;
            partial.min_published = partial.min_published.min(published);
            // ADR-0159 D4: at most 2^14 epochs below 2^64 each — the sum is
            // below 2^78.
            partial.published_sum += u128::from(published);
            partial.max_unix_ms = partial.max_unix_ms.max(unix_ms);
        }
        let visits = end - begin;
        debug_assert!(visits <= CKPT_BOARD_VISITS_PER_TURN, "A1.4: at most 64 visits per turn");
        sweep.visits += visits as u64;
        if end < self.cells.len() {
            sweep.cursor = end;
            return SweepStep::InProgress;
        }
        sweep.cursor = 0;
        sweep.sweeps_completed += 1;
        let previous = core::mem::replace(&mut sweep.observed, sweep.partial);
        sweep.partial = BoardSweep::EMPTY_PARTIAL;
        if sweep.observed.published_sum == previous.published_sum {
            SweepStep::Unchanged
        } else {
            SweepStep::Progressed
        }
    }
}

/// A completed sweep's view of the board (ADR-0159 D4). The sweep reads
/// slots at different times, so `min_published` is a lower bound on every
/// slot's current publication — never early for `WAIT` or DROP
/// pacing — and the sum of a later sweep is never smaller.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardObservation {
    /// Lowest published epoch: `WAIT` (all cells) and DROP pacing.
    pub min_published: u64,
    /// Sum of published epochs, in `u128` so it cannot wrap (D4): the
    /// wake signal for parked `WAIT`s.
    pub published_sum: u128,
    /// Newest publication time (unix ms): the `rdb_last_save_time` gauge.
    pub max_unix_ms: u64,
}

/// One cell's resumable sweep over the board (ADR-0159 D4): a cursor into
/// the fixed boot board and the partial folds. No slot reference crosses a
/// turn; the board's membership never changes within a boot.
#[derive(Debug)]
pub struct BoardSweep {
    cursor: usize,
    partial: BoardObservation,
    observed: BoardObservation,
    /// Slot visits so far — the sweep's liveness counter (per cell).
    visits: u64,
    /// Completed sweeps so far (per cell).
    sweeps_completed: u64,
    /// The newest publication time (unix ms) a `WAIT CELL k` on this cell
    /// confirmed: `LASTSAVE`'s floor (ADR-0159 A1.4).
    lastsave_floor_ms: u64,
}

/// What one sweep step did (ADR-0159 D4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SweepStep {
    /// The sweep resumes at its cursor next turn.
    InProgress,
    /// A sweep completed with the previous observation's sum.
    Unchanged,
    /// A sweep completed and some slot published since the last one: wake
    /// the parked `WAIT`s.
    Progressed,
}

impl BoardSweep {
    const EMPTY_PARTIAL: BoardObservation =
        BoardObservation { min_published: u64::MAX, published_sum: 0, max_unix_ms: 0 };

    #[must_use]
    pub fn new() -> BoardSweep {
        BoardSweep {
            cursor: 0,
            partial: BoardSweep::EMPTY_PARTIAL,
            observed: BoardObservation::default(),
            visits: 0,
            sweeps_completed: 0,
            lastsave_floor_ms: 0,
        }
    }

    /// The last completed sweep's observation (all zero before the first).
    #[must_use]
    pub fn observed(&self) -> BoardObservation {
        self.observed
    }

    /// `WAIT CELL k`'s check (ADR-0159 A1.4): whether `slot` published
    /// `epoch`. When it did, the publication time read after the `Acquire`
    /// load that satisfied the check raises this cell's `LASTSAVE` floor in
    /// the same step, so a `WAIT CELL k` that returns leaves `LASTSAVE`
    /// covering the checkpoint it fenced, even while the sweep trails.
    pub fn confirm_cell_wait(&mut self, slot: &CkptSlot, epoch: CheckpointEpoch) -> bool {
        let Some(unix_ms) = slot.covered_at(epoch) else { return false };
        // Canary: the confirmation without its floor raise.
        #[cfg(not(inf_canary_lastsave_floor_skipped))]
        {
            self.lastsave_floor_ms = self.lastsave_floor_ms.max(unix_ms);
        }
        #[cfg(inf_canary_lastsave_floor_skipped)]
        let _ = unix_ms;
        true
    }

    /// `LASTSAVE` (ADR-0159 A1.4): the newest publication this cell has
    /// observed, from its completed sweep or a slot a `WAIT CELL k` on
    /// this cell confirmed, in unix seconds.
    #[must_use]
    pub fn lastsave_unix_s(&self) -> u64 {
        self.observed.max_unix_ms.max(self.lastsave_floor_ms) / 1000
    }

    /// A sweep is part-way through the board.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.cursor != 0
    }

    #[must_use]
    pub fn visits(&self) -> u64 {
        self.visits
    }

    #[must_use]
    pub fn sweeps_completed(&self) -> u64 {
        self.sweeps_completed
    }
}

impl Default for BoardSweep {
    fn default() -> BoardSweep {
        BoardSweep::new()
    }
}

/// A checkpoint request's epoch (ADR-0159 D1): built only by issuing a
/// credit of this boot's clock, so it is nonzero and above every earlier
/// request of the boot. `WAIT` compares a published epoch against it.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CheckpointEpoch(Issued);

impl CheckpointEpoch {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Which cells a checkpoint request asks (ADR-0159 A1.3).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CkptTarget {
    /// Every cell: one raise of the board's request word.
    All,
    /// One cell (`INF.CKPT CELL k`, a cell's stop): a raise of its slot.
    /// The id is below the board's count: the command's `CELL` check, or
    /// the cell's own id.
    Cell(CellId),
}

/// One ordinary checkpoint issue (ADR-0159 D1): not `Clone`, consumed by
/// value; dropped unused, it is spent.
#[derive(Debug)]
pub struct CkptCredit(IssueCredit<CkptBoard>);

impl CkptCredit {
    /// Issues the request: the epoch is assigned now, late, then raised
    /// into `target` (A1.3, A1.6).
    pub fn issue(self, target: CkptTarget) -> CheckpointEpoch {
        CheckpointEpoch(self.0.issue(|board, issued| board.raise(target, issued)))
    }
}

/// An owner's checkpoint quota (ADR-0159 A1.2): a cell's, or the control
/// writer's, which the host holds after construction.
#[derive(Debug)]
pub struct CkptQuota(IssueQuota<CkptBoard>);

impl CkptQuota {
    /// Takes `K` credits, or none.
    ///
    /// # Errors
    /// [`IssueExhausted`]: fewer than `K` units remain; nothing changed.
    pub fn reserve<const K: usize>(&mut self) -> Result<[CkptCredit; K], IssueExhausted> {
        self.0.reserve::<K>().map(|credits| credits.map(CkptCredit))
    }

    /// Reserves one unit and issues it into `target`: the direct admin,
    /// test and simulator request.
    ///
    /// # Errors
    /// [`IssueExhausted`] before any request.
    pub fn request(&mut self, target: CkptTarget) -> Result<CheckpointEpoch, IssueExhausted> {
        let [credit] = self.reserve::<1>()?;
        Ok(credit.issue(target))
    }

    /// Units left.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.0.remaining()
    }
}

/// A cell's stop-checkpoint issue (ADR-0159 D3): outside its quota, bound
/// to its own slot, issued once.
#[derive(Debug)]
pub struct CkptFinalCredit {
    credit: FinalCredit<CkptBoard>,
    cell: CellId,
}

impl CkptFinalCredit {
    /// Issues the stop request into this cell's own slot.
    pub fn issue(self) -> CheckpointEpoch {
        let target = CkptTarget::Cell(self.cell);
        CheckpointEpoch(self.credit.issue(|board, issued| board.raise(target, issued)))
    }
}

/// One cell's checkpoint allowance (ADR-0159 A1.2, D3), wired with its
/// control handle by `ServerPlane::set_control` on the cell it was minted
/// for, and only there.
#[derive(Debug)]
pub struct CellIssuer {
    pub(crate) quota: CkptQuota,
    pub(crate) final_credit: CkptFinalCredit,
}

impl CellIssuer {
    /// The cell this allowance was minted for: its final credit raises
    /// this cell's slot alone (ADR-0159 D3).
    #[must_use]
    pub fn cell(&self) -> CellId {
        self.final_credit.cell
    }

    /// The wiring check of `ServerPlane::set_control`: every fact this
    /// allowance's use relies on, against the handle and the plane it is
    /// handed to. Pure; the caller wires nothing unless it passes.
    ///
    /// # Errors
    /// The first [`IssuerMismatch`], in the order the variants list them.
    pub(crate) fn check_wiring(
        &self,
        control: &ControlHandle,
        plane: CellId,
        plane_cells: u16,
    ) -> Result<(), IssuerMismatch> {
        let clock = &*control.ckpt_clock;
        if !self.quota.0.issues_on(clock) || !self.final_credit.credit.issues_on(clock) {
            return Err(IssuerMismatch::OtherBoot);
        }
        let board = clock.board().cell_count();
        if usize::from(plane_cells) != board {
            return Err(IssuerMismatch::CellCount { plane: plane_cells, board });
        }
        if self.cell() != plane {
            return Err(IssuerMismatch::OtherCell { issuer: self.cell() });
        }
        Ok(())
    }
}

/// Why `ServerPlane::set_control` refused a checkpoint allowance: each is a
/// host's wiring error, found before anything is wired.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IssuerMismatch {
    /// Minted by another control handle's partition (ADR-0159 A1.2: a
    /// credit issues only into its own boot's board). Wired, its requests
    /// would raise a board this cell never reads: `INF.CKPT WAIT` would
    /// park for ever and the stop would wait out its deadline.
    OtherBoot,
    /// The plane routes `plane` cells over a board of `board` slots
    /// (ADR-0159 A1.1: one `CellCount` per boot). `INF.CKPT CELL k` is
    /// checked against the plane, so `k` could index past the board.
    CellCount { plane: u16, board: usize },
    /// Minted for cell `issuer` (ADR-0159 D3: a stop credit raises its own
    /// cell's slot, and a cell's stop waits on its own slot, so a crossed
    /// pair would wait until the stop deadline).
    OtherCell { issuer: CellId },
}

/// `ServerPlane::set_control` refused `issuer`. Nothing was wired; the
/// allowance comes back to the host.
#[derive(Debug)]
pub struct IssuerRefused {
    /// The cell of the plane that refused it.
    pub plane: CellId,
    /// The first fact that did not match.
    pub mismatch: IssuerMismatch,
    /// The refused allowance, unspent.
    pub issuer: CellIssuer,
}

impl std::fmt::Display for IssuerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let plane = self.plane.0;
        match self.mismatch {
            IssuerMismatch::OtherBoot => {
                write!(f, "cell {plane} was handed a checkpoint issuer of another boot")
            }
            IssuerMismatch::CellCount { plane: cells, board } => write!(
                f,
                "cell {plane} routes {cells} cells but its checkpoint board holds {board} slots"
            ),
            IssuerMismatch::OtherCell { issuer } => write!(
                f,
                "cell {plane} was handed the checkpoint issuer minted for cell {}",
                issuer.0
            ),
        }
    }
}

impl std::error::Error for IssuerRefused {}

/// What a control-plane constructor mints besides the handle (ADR-0159
/// A1.2): each cell's allowance, in cell order, and the control writer's
/// quota. The writer's quota paid for the boot restamp during
/// construction; it now belongs to the host that owns the writer, for
/// direct admin, test and simulator requests. No cell holds it.
#[derive(Debug)]
pub struct CkptIssuers {
    pub host: CkptQuota,
    pub cells: Vec<CellIssuer>,
}

/// One boot's checkpoint issuance space, fixed with its cell count
/// (ADR-0159 A1.2, A1.5).
#[derive(Copy, Clone, Debug)]
pub struct CkptSpace {
    cells: CellCount,
    space: IssueSpace,
}

impl CkptSpace {
    /// The production split: `N + 1` quotas of
    /// `floor((u64::MAX − N) / (N + 1))` units and `N` final credits.
    #[must_use]
    pub fn full(cells: CellCount) -> CkptSpace {
        CkptSpace { cells, space: IssueSpace::full(u32::from(cells.get())) }
    }

    /// The test seam (A1.5): `headroom` units per quota, the clock started
    /// so that the last credit issues `u64::MAX`.
    ///
    /// # Errors
    /// [`PartitionRefused`] when `(N + 1) × headroom + N` overflows `u64`.
    pub fn with_headroom(
        cells: CellCount,
        headroom: NonZeroU64,
    ) -> Result<CkptSpace, PartitionRefused> {
        let space = IssueSpace::with_headroom(u32::from(cells.get()), headroom)?;
        Ok(CkptSpace { cells, space })
    }

    #[must_use]
    pub fn cells(self) -> CellCount {
        self.cells
    }
}

/// One cell's memory-gauge publication payload — and, field-for-field,
/// the node-wide fold `MemoryBoard::totals` returns.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemoryGauges {
    /// The cell's `used_memory` contribution (attributed keyspace bytes +
    /// wire buffers + connection state).
    pub used_bytes: u64,
    /// The cell's `maxmemory` comparable (`Keyspace::pool_used_bytes`,
    /// ADR-0068 A2): the pool's logical bytes, rendered as
    /// `used_memory_pool` so the operator can see the distance to the limit.
    pub pool_used_bytes: u64,
    pub docs_live: u64,
    pub doc_tape_bytes: u64,
    pub doc_arena_bytes: u64,
    pub doc_resident_bytes: u64,
    pub doc_intern_bytes: u64,
    pub doc_slack_bytes: u64,
    pub doc_scratch_bytes: u64,
    pub doc_path_cache_bytes: u64,
    /// Index-tree domains (M4.5-S03, ADR-0075 D6 — L5).
    pub idx_tree_bytes: u64,
    pub idx_slack_bytes: u64,
    /// Per-db live keys and keys with a deadline (ADR-0122 A2): the
    /// `# Keyspace` fold, so `INFO keyspace` counts what `DBSIZE` counts.
    pub db_keys: [u64; inf_store::DEFAULT_DBS],
    pub db_expires: [u64; inf_store::DEFAULT_DBS],
}

/// Per-cell memory publication slot. Same L1 control-plane carve-out
/// class as the recovery/checkpoint boards: the owning cell writes its
/// slot (MAINTAIN cadence, plus fresh at its own `INFO` render); readers
/// fold across slots. Gauges are advisory — a peer's value lags its
/// publisher by at most one publish period, like every other INFO gauge.
#[derive(Debug, Default)]
pub struct MemorySlot {
    used_bytes: AtomicU64,
    pool_used_bytes: AtomicU64,
    docs_live: AtomicU64,
    doc_tape_bytes: AtomicU64,
    doc_arena_bytes: AtomicU64,
    doc_resident_bytes: AtomicU64,
    doc_intern_bytes: AtomicU64,
    doc_slack_bytes: AtomicU64,
    doc_scratch_bytes: AtomicU64,
    doc_path_cache_bytes: AtomicU64,
    idx_tree_bytes: AtomicU64,
    idx_slack_bytes: AtomicU64,
    db_keys: [AtomicU64; inf_store::DEFAULT_DBS],
    db_expires: [AtomicU64; inf_store::DEFAULT_DBS],
}

impl MemorySlot {
    pub fn publish(&self, g: MemoryGauges) {
        self.used_bytes.store(g.used_bytes, Ordering::Relaxed);
        self.pool_used_bytes.store(g.pool_used_bytes, Ordering::Relaxed);
        self.docs_live.store(g.docs_live, Ordering::Relaxed);
        self.doc_tape_bytes.store(g.doc_tape_bytes, Ordering::Relaxed);
        self.doc_arena_bytes.store(g.doc_arena_bytes, Ordering::Relaxed);
        self.doc_resident_bytes.store(g.doc_resident_bytes, Ordering::Relaxed);
        self.doc_intern_bytes.store(g.doc_intern_bytes, Ordering::Relaxed);
        self.doc_slack_bytes.store(g.doc_slack_bytes, Ordering::Relaxed);
        self.doc_scratch_bytes.store(g.doc_scratch_bytes, Ordering::Relaxed);
        self.doc_path_cache_bytes.store(g.doc_path_cache_bytes, Ordering::Relaxed);
        self.idx_tree_bytes.store(g.idx_tree_bytes, Ordering::Relaxed);
        self.idx_slack_bytes.store(g.idx_slack_bytes, Ordering::Relaxed);
        for (slot, v) in self.db_keys.iter().zip(g.db_keys) {
            slot.store(v, Ordering::Relaxed);
        }
        for (slot, v) in self.db_expires.iter().zip(g.db_expires) {
            slot.store(v, Ordering::Relaxed);
        }
    }

    fn read(&self) -> MemoryGauges {
        MemoryGauges {
            used_bytes: self.used_bytes.load(Ordering::Relaxed),
            pool_used_bytes: self.pool_used_bytes.load(Ordering::Relaxed),
            docs_live: self.docs_live.load(Ordering::Relaxed),
            doc_tape_bytes: self.doc_tape_bytes.load(Ordering::Relaxed),
            doc_arena_bytes: self.doc_arena_bytes.load(Ordering::Relaxed),
            doc_resident_bytes: self.doc_resident_bytes.load(Ordering::Relaxed),
            doc_intern_bytes: self.doc_intern_bytes.load(Ordering::Relaxed),
            doc_slack_bytes: self.doc_slack_bytes.load(Ordering::Relaxed),
            doc_scratch_bytes: self.doc_scratch_bytes.load(Ordering::Relaxed),
            doc_path_cache_bytes: self.doc_path_cache_bytes.load(Ordering::Relaxed),
            idx_tree_bytes: self.idx_tree_bytes.load(Ordering::Relaxed),
            idx_slack_bytes: self.idx_slack_bytes.load(Ordering::Relaxed),
            db_keys: core::array::from_fn(|i| self.db_keys[i].load(Ordering::Relaxed)),
            db_expires: core::array::from_fn(|i| self.db_expires[i].load(Ordering::Relaxed)),
        }
    }
}

/// Node-wide memory board: one slot per cell (M3-S25 finding — `INFO`
/// rendered the *serving cell's* keyspace attribution beside
/// *process-wide* RSS, so `docs_live` read 17,801 on a 71,200-document
/// node and `mem_fragmentation_ratio` mixed scopes. The memory section
/// now folds this board — §16's "control thread aggregates on scrape").
#[derive(Debug)]
pub struct MemoryBoard {
    cells: Vec<MemorySlot>,
}

impl MemoryBoard {
    #[must_use]
    pub fn new(cells: u16) -> MemoryBoard {
        MemoryBoard { cells: (0..cells).map(|_| MemorySlot::default()).collect() }
    }

    /// This cell's slot.
    ///
    /// # Panics
    /// Out-of-range cell id (assembly bug).
    #[must_use]
    pub fn slot(&self, cell: u16) -> &MemorySlot {
        &self.cells[usize::from(cell)]
    }

    /// Field-wise sum across every cell's last publication.
    #[must_use]
    pub fn totals(&self) -> MemoryGauges {
        let mut t = MemoryGauges::default();
        for slot in &self.cells {
            let g = slot.read();
            t.used_bytes += g.used_bytes;
            t.pool_used_bytes += g.pool_used_bytes;
            t.docs_live += g.docs_live;
            t.doc_tape_bytes += g.doc_tape_bytes;
            t.doc_arena_bytes += g.doc_arena_bytes;
            t.doc_resident_bytes += g.doc_resident_bytes;
            t.doc_intern_bytes += g.doc_intern_bytes;
            t.doc_slack_bytes += g.doc_slack_bytes;
            t.doc_scratch_bytes += g.doc_scratch_bytes;
            t.doc_path_cache_bytes += g.doc_path_cache_bytes;
            t.idx_tree_bytes += g.idx_tree_bytes;
            t.idx_slack_bytes += g.idx_slack_bytes;
            for (sum, v) in t.db_keys.iter_mut().zip(g.db_keys) {
                *sum += v;
            }
            for (sum, v) in t.db_expires.iter_mut().zip(g.db_expires) {
                *sum += v;
            }
        }
        t
    }
}

/// Per-cell × per-index-slot readiness board (M4.5-S03, ADR-0075 D5):
/// each owning cell publishes the **generation** it last reported ready
/// for the index occupying a slot (0 = none) — the CkptBoard L1
/// carve-out class, single writer per (cell, slot). The catalog flips
/// `backfilling → ready` only when [`fleet_ready`] holds; generation-
/// exact matching makes a stale report (a cell still on the pre-rebuild
/// generation) read as not-ready — ABA-safe by construction. Slots are
/// assigned by the control plane at create, freed at drop completion,
/// and never persisted (boot reassigns from the seeded catalog; the
/// zeroed board at boot *is* the ADR-0075 D4 readiness regression).
///
/// [`fleet_ready`]: IndexBoard::fleet_ready
#[derive(Debug)]
pub struct IndexBoard {
    /// `cells × INDEX_SLOTS` ready-generation words, row-major by cell.
    slots: Vec<AtomicU64>,
    cells: u16,
}

/// One board row per possible live index ([`inf_store::INDEXES_PER_NODE_MAX`]).
pub const INDEX_SLOTS: usize = inf_store::INDEXES_PER_NODE_MAX;

impl IndexBoard {
    #[must_use]
    pub fn new(cells: u16) -> IndexBoard {
        let n = usize::from(cells) * INDEX_SLOTS;
        IndexBoard { slots: (0..n).map(|_| AtomicU64::new(0)).collect(), cells }
    }

    fn at(&self, cell: u16, slot: usize) -> &AtomicU64 {
        assert!(cell < self.cells, "cell id validated by the assembly");
        assert!(slot < INDEX_SLOTS, "slot ids are control-plane-assigned < INDEX_SLOTS");
        &self.slots[usize::from(cell) * INDEX_SLOTS + slot]
    }

    /// This cell reports ready for the index at `slot`, at `generation`
    /// (owning cell only, from MAINTAIN — the S05 publisher).
    pub fn publish_ready(&self, cell: u16, slot: usize, generation: u64) {
        debug_assert!(generation > 0, "generation 0 is the reserved null");
        self.at(cell, slot).store(generation, Ordering::Release);
    }

    /// Clears this cell's report (drop teardown / rebuild start).
    pub fn clear(&self, cell: u16, slot: usize) {
        self.at(cell, slot).store(0, Ordering::Release);
    }

    /// The generation `cell` last reported ready for `slot` (0 = none).
    #[must_use]
    pub fn cell_ready_generation(&self, cell: u16, slot: usize) -> u64 {
        self.at(cell, slot).load(Ordering::Acquire)
    }

    /// True once **every** cell reports exactly `generation` for `slot`
    /// — the §3.1 aggregation: the catalog flips to `ready` on this and
    /// nothing else. Monotone under one generation (cells only regress
    /// through DDL, which bumps the generation first).
    #[must_use]
    pub fn fleet_ready(&self, slot: usize, generation: u64) -> bool {
        debug_assert!(generation > 0, "generation 0 is the reserved null");
        (0..self.cells).all(|cell| self.cell_ready_generation(cell, slot) == generation)
    }
}

/// One catalog snapshot to persist (the origin cell's post-apply export).
struct PersistReq {
    catalog: NsCatalog,
    /// The epoch value published once this snapshot is durable.
    epoch: u64,
    /// The durable namespace this persist drops (ADR-0100 D2): its
    /// tombstone joins the writer's set before the payload encodes.
    drop: Option<u32>,
    /// Any namespace this persist drops (ADR-0103 D2): retired from the
    /// pending-create set whatever its mode.
    dropped: Option<u32>,
    /// The `CREATE` this persist carries (ADR-0103 D1/D2): merged into
    /// the payload and the pending set, its verdict written before the
    /// epoch publishes.
    create: Option<(NsSpec, CreateVerdict)>,
}

/// The catalog writer's answer to one `CREATE` (ADR-0103 D2), read by
/// the origin after `persisted(epoch)`: one atomic the origin owns, set
/// by the writer before it publishes the epoch (Release/Acquire pairs
/// through `persisted_epoch`).
#[derive(Clone, Debug)]
pub struct CreateVerdict(Arc<AtomicU8>);

/// What the writer decided about a `CREATE`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CreateOutcome {
    /// The name was free across existing and pending entries: the
    /// definition is durable, the origin may apply and fan.
    Accepted,
    /// An existing or earlier-pending entry holds the name.
    NameExists,
    /// The pending set is at [`PENDING_CREATE_MAX`].
    AtCapacity,
}

impl CreateVerdict {
    fn new() -> CreateVerdict {
        CreateVerdict(Arc::new(AtomicU8::new(0)))
    }

    fn set(&self, outcome: CreateOutcome) {
        let code = match outcome {
            CreateOutcome::Accepted => 1,
            CreateOutcome::NameExists => 2,
            CreateOutcome::AtCapacity => 3,
        };
        self.0.store(code, Ordering::Release);
    }

    /// The writer's decision; `None` until the persist that carried the
    /// request ran (callers read after `persisted(epoch)`).
    #[must_use]
    pub fn get(&self) -> Option<CreateOutcome> {
        match self.0.load(Ordering::Acquire) {
            1 => Some(CreateOutcome::Accepted),
            2 => Some(CreateOutcome::NameExists),
            3 => Some(CreateOutcome::AtCapacity),
            _ => None,
        }
    }
}

/// Bound on creates in flight between their catalog swap and their
/// fan's completion (ADR-0103 D2). At the cap the origin answers a
/// typed `BUSY` before requesting; the writer refuses too, so a race of
/// origins past the gauge cannot overshoot.
pub const PENDING_CREATE_MAX: usize = 256;

/// The catalog writer's pending-create set (ADR-0103 D2): every
/// namespace whose definition is durable but whose fan may not have
/// reached every cell. Merged into each payload so a concurrent
/// origin's stale export cannot drop an accepted definition; retired
/// once the origin's fan completed (every export carries it from then
/// on) or a `DROP` names the id. Single owner — the thread or the sim's
/// inline inbox. Empty at boot: `META` already holds every accepted
/// create.
#[derive(Debug, Default)]
struct PendingCreates {
    /// Sorted by id.
    entries: Vec<NsSpec>,
}

impl PendingCreates {
    fn retire(&mut self, id: u32) {
        self.entries.retain(|e| e.id.0 != id);
    }

    /// Adds every pending spec the payload lacks (by id).
    fn merge(&self, catalog: &mut NsCatalog) {
        for spec in &self.entries {
            if !catalog.entries.iter().any(|e| e.id == spec.id) {
                catalog.entries.push(spec.clone());
            }
        }
    }

    /// Decides one create against the merged payload and, when
    /// accepted, adds it to both.
    fn admit(&mut self, catalog: &mut NsCatalog, spec: NsSpec) -> CreateOutcome {
        if catalog.entries.iter().any(|e| e.name == spec.name) {
            return CreateOutcome::NameExists;
        }
        if self.entries.len() >= PENDING_CREATE_MAX {
            return CreateOutcome::AtCapacity;
        }
        let at = self.entries.partition_point(|e| e.id < spec.id);
        self.entries.insert(at, spec.clone());
        catalog.entries.push(spec);
        CreateOutcome::Accepted
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The catalog writer's live tombstone set (ADR-0100 D2/D3): one entry
/// per durable namespace dropped since every cell last published a
/// `MANIFEST` past the drop. Single owner — the thread (or the sim's
/// inline inbox); cells only read the boot snapshot the catalog carries.
#[derive(Debug, Default)]
struct DropTombstones {
    /// `(namespace id, checkpoint epoch that retires it)` — unstamped
    /// entries never retire. Kept sorted by id (the wire order).
    entries: Vec<(u32, Option<u64>)>,
}

/// Bound on live tombstones (ADR-0100 D3): at the cap a `DROP` waits for
/// its own node-wide checkpoint before persisting, so `META` never
/// carries more than 1 KiB of tombstones.
pub const DROPPED_NS_MAX: usize = 256;

impl DropTombstones {
    fn seed(ids: &[u32]) -> DropTombstones {
        DropTombstones { entries: ids.iter().map(|&id| (id, None)).collect() }
    }

    fn add(&mut self, id: u32) {
        match self.entries.binary_search_by_key(&id, |e| e.0) {
            Ok(_) => {}
            Err(at) => self.entries.insert(at, (id, None)),
        }
    }

    fn stamp(&mut self, id: u32, ckpt_epoch: u64) {
        if let Ok(at) = self.entries.binary_search_by_key(&id, |e| e.0) {
            // A re-stamp only ever comes from boot (every survivor gets
            // one fresh epoch); a later stamp never lowers the bar.
            let slot = &mut self.entries[at].1;
            *slot = Some(slot.map_or(ckpt_epoch, |e| e.max(ckpt_epoch)));
        }
    }

    /// Stamps every unstamped survivor (the boot re-stamp, ADR-0100 D3).
    fn stamp_unstamped(&mut self, ckpt_epoch: u64) {
        for entry in &mut self.entries {
            if entry.1.is_none() {
                entry.1 = Some(ckpt_epoch);
            }
        }
    }

    /// Retires every tombstone whose checkpoint every cell has published.
    fn retire(&mut self, min_published: u64) {
        self.entries.retain(|(_, epoch)| !epoch.is_some_and(|e| e <= min_published));
    }

    /// Merges the set into `catalog` (ADR-0100 D2): the payload carries
    /// the ids, and an entry whose id is tombstoned is dropped — ids
    /// never reuse, so such an entry is replica lag from a concurrent
    /// DDL, never a live namespace.
    fn reconcile(&self, catalog: &mut NsCatalog) {
        catalog.dropped = self.entries.iter().map(|e| e.0).collect();
        if !self.entries.is_empty() {
            catalog
                .entries
                .retain(|e| self.entries.binary_search_by_key(&e.id.0, |t| t.0).is_err());
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Control-thread work (single receiver, bounded — L3).
enum ControlMsg {
    Persist(PersistReq),
    /// The origin fanned a `DROP` and requested the node-wide checkpoint
    /// that retires its tombstone (ADR-0100 D3).
    StampDrop {
        id: u32,
        ckpt_epoch: u64,
    },
    /// The origin's `CREATE` fan completed (or failed) — every cell's
    /// export carries the namespace from now on, so its pending entry
    /// retires (ADR-0103 D2).
    CreateApplied {
        id: u32,
    },
    /// Blocking unlink delegated by a cell (M2-S11/S12, ADR-0017):
    /// freeing a truncated segment's or stale checkpoint's pages is
    /// O(file size) in the kernel — a measured multi-ms stall when done
    /// on the loop. The named file is already outside every recovery
    /// unit (below the durable manifest's floor / not the named `.ick`),
    /// so any thread may delete it; failure is re-collected by boot GC.
    Unlink(PathBuf),
}

/// Shared handle the assembly wires into every cell's plane.
pub struct ControlHandle {
    tx: mpsc::SyncSender<ControlMsg>,
    next_ns_id: AtomicU32,
    /// Index-id + generation allocators (M4.5-S03, ADR-0075 D1): both
    /// node-unique, allocated once, never reused — the ns-id discipline.
    next_index_id: AtomicU32,
    next_index_generation: AtomicU64,
    next_epoch: AtomicU64,
    persisted_epoch: Arc<AtomicU64>,
    /// The boot's checkpoint clock and the board it owns (ADR-0159 A1):
    /// the all-cell request word and the per-cell request/publication
    /// slots. Only a partition credit advances the clock.
    ckpt_clock: Arc<IssueClock<CkptBoard>>,
    /// Boot-recovery progress + readiness (M2-S15).
    recovery: Arc<RecoveryBoard>,
    /// Per-cell memory gauges for node-scope `INFO` (M3-S25 fix).
    memory_board: Arc<MemoryBoard>,
    /// Per-cell index readiness (M4.5-S03, ADR-0075 D5).
    index_board: Arc<IndexBoard>,
    /// Live drop tombstones (ADR-0100 D7 gauge + the D3 cap input);
    /// written by the catalog writer after every change.
    drop_tombstones: AtomicUsize,
    /// Creates in flight in the writer's pending set (ADR-0103 D2 —
    /// the cap input); written by the writer after every change.
    pending_creates: AtomicUsize,
    /// The node-wide DDL ticket (ADR-0108 D1): `0` = free, `cell + 1` =
    /// the cell whose DDL program holds it. One namespace DDL program
    /// runs at a time — its fan legs never cross another's. Taken by
    /// CAS from the origin's pump, released by [`DdlTicket`]'s drop.
    ddl_holder: AtomicU16,
    /// Bumped on every ticket release; cells edge-detect it in MAINTAIN
    /// (one relaxed load — the persisted-epoch pattern) and wake their
    /// parked DDL pumps.
    ddl_generation: AtomicU64,
}

/// The DDL ticket (ADR-0108 D1): held by exactly one namespace DDL
/// program per node from before its first effect to after its reply.
/// Dropping it — on every exit path, a dropped future included —
/// releases the holder slot and bumps the generation the waiters
/// edge-detect. A deliberate exception to the "no guards across a
/// suspension" rule: it is the serialization point, not a borrow.
pub struct DdlTicket {
    handle: Arc<ControlHandle>,
}

impl Drop for DdlTicket {
    fn drop(&mut self) {
        self.handle.ddl_holder.store(0, Ordering::Release);
        self.handle.ddl_generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl ControlHandle {
    /// Live drop tombstones in the catalog writer (ADR-0100 D3/D7).
    pub fn drop_tombstones(&self) -> usize {
        self.drop_tombstones.load(Ordering::Relaxed)
    }

    /// Creates in flight in the writer's pending set (ADR-0103 D2).
    pub fn pending_creates(&self) -> usize {
        self.pending_creates.load(Ordering::Relaxed)
    }

    /// Takes the DDL ticket for `cell` (ADR-0108 D1), or `None` while
    /// another program holds it — the caller parks on its DDL waitlist
    /// and retries on the release edge.
    #[must_use]
    pub fn ddl_try_acquire(self: &Arc<Self>, cell: u16) -> Option<DdlTicket> {
        self.ddl_holder.compare_exchange(0, cell + 1, Ordering::AcqRel, Ordering::Acquire).ok()?;
        Some(DdlTicket { handle: Arc::clone(self) })
    }

    /// The cell holding the DDL ticket, if any (a gauge, never a decision).
    #[must_use]
    pub fn ddl_holder(&self) -> Option<u16> {
        self.ddl_holder.load(Ordering::Acquire).checked_sub(1)
    }

    /// Ticket releases so far (the MAINTAIN edge-detection input).
    pub fn ddl_generation(&self) -> u64 {
        self.ddl_generation.load(Ordering::Acquire)
    }

    /// The origin's post-fan notice for a `CREATE` (ADR-0103 D2): the
    /// pending entry retires — every cell's export carries it now.
    pub fn create_applied(&self, id: u32) {
        self.tx.send(ControlMsg::CreateApplied { id }).expect("control thread alive (fail-stop)");
    }

    /// The origin's post-fan notice (ADR-0100 D3): `id`'s tombstone
    /// retires once every cell publishes `ckpt_epoch`.
    pub fn stamp_drop(&self, id: u32, ckpt_epoch: u64) {
        self.tx
            .send(ControlMsg::StampDrop { id, ckpt_epoch })
            .expect("control thread alive (fail-stop)");
    }
    /// Allocates one namespace id (never reused — ADR-0015 D2).
    pub fn alloc_ns_id(&self) -> u32 {
        self.next_ns_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The id the allocator would hand out next (catalog `next_id`).
    pub fn next_ns_id(&self) -> u32 {
        self.next_ns_id.load(Ordering::Relaxed)
    }

    /// Allocates one index id (never reused — ADR-0075 D1).
    pub fn alloc_index_id(&self) -> u32 {
        self.next_index_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The index id the allocator would hand out next.
    pub fn next_index_id(&self) -> u32 {
        self.next_index_id.load(Ordering::Relaxed)
    }

    /// Allocates one index generation (create + rebuild — ADR-0075 D3).
    pub fn alloc_index_generation(&self) -> u64 {
        self.next_index_generation.fetch_add(1, Ordering::Relaxed)
    }

    /// The generation the allocator would hand out next.
    pub fn next_index_generation(&self) -> u64 {
        self.next_index_generation.load(Ordering::Relaxed)
    }

    /// The index readiness board (M4.5-S03, ADR-0075 D5).
    #[must_use]
    pub fn index_board(&self) -> &Arc<IndexBoard> {
        &self.index_board
    }

    /// Queues `catalog` for a durable swap; returns the epoch to await.
    /// The bounded channel makes DDL admission explicit: a full queue
    /// blocks the *sender* briefly (DDL-rate traffic, never the data path
    /// of other connections — the pump yields between commands).
    pub fn request_persist(&self, catalog: NsCatalog) -> u64 {
        self.send_persist(catalog, None, None, None)
    }

    /// [`request_persist`](Self::request_persist) for a `DROP` of
    /// namespace `id`: a durable namespace's tombstone joins the payload
    /// (ADR-0100 D2, `tombstone`); any mode retires from the
    /// pending-create set (ADR-0103 D2).
    pub fn request_persist_drop(&self, catalog: NsCatalog, id: u32, tombstone: bool) -> u64 {
        self.send_persist(catalog, tombstone.then_some(id), Some(id), None)
    }

    /// [`request_persist`](Self::request_persist) for a `CREATE`
    /// (ADR-0103 D1/D2): `spec` joins the payload and the writer's
    /// pending set; the returned verdict is readable once
    /// [`persisted`](Self::persisted) holds for the epoch.
    pub fn request_persist_create(&self, catalog: NsCatalog, spec: NsSpec) -> (u64, CreateVerdict) {
        let verdict = CreateVerdict::new();
        let epoch = self.send_persist(catalog, None, None, Some((spec, verdict.clone())));
        (epoch, verdict)
    }

    fn send_persist(
        &self,
        catalog: NsCatalog,
        drop: Option<u32>,
        dropped: Option<u32>,
        create: Option<(NsSpec, CreateVerdict)>,
    ) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx
            .send(ControlMsg::Persist(PersistReq { catalog, epoch, drop, dropped, create }))
            .expect("control thread alive (fail-stop)");
        epoch
    }

    /// Delegates one blocking unlink to the control thread (M2-S11/S12).
    /// Non-blocking: `false` = queue full — the caller keeps the path and
    /// retries next MAINTAIN slice (bounded local queue, never a stall).
    pub fn request_unlink(&self, path: PathBuf) -> bool {
        self.tx.try_send(ControlMsg::Unlink(path)).is_ok()
    }

    /// True once every persist up to `epoch` is durable. Cells poll this
    /// from MAINTAIN (one relaxed load) and wake their parked DDL pumps.
    pub fn persisted(&self, epoch: u64) -> bool {
        self.persisted_epoch.load(Ordering::Acquire) >= epoch
    }

    /// The highest durable epoch (the MAINTAIN edge-detection input).
    pub fn persisted_epoch(&self) -> u64 {
        self.persisted_epoch.load(Ordering::Acquire)
    }

    /// The checkpoint board (the request word and the per-cell
    /// request/publication slots). Requests reach it only through a
    /// credit ([`CkptQuota`], [`CkptFinalCredit`]).
    #[must_use]
    pub fn ckpt_board(&self) -> &CkptBoard {
        self.ckpt_clock.board()
    }

    /// The last checkpoint epoch issued this boot (tests and oracles: the
    /// engagement check of a drive to the top of the space).
    #[must_use]
    pub fn ckpt_last_issued(&self) -> u64 {
        self.ckpt_clock.last()
    }

    /// The boot-recovery board (M2-S15): cells publish their slot, the
    /// plane gates `-LOADING` on `all_ready`, `INFO` aggregates it.
    #[must_use]
    pub fn recovery_board(&self) -> &Arc<RecoveryBoard> {
        &self.recovery
    }

    /// The memory board (M3-S25): cells publish their slot from MAINTAIN
    /// and at their own `INFO` render; the memory section folds it.
    #[must_use]
    pub fn memory_board(&self) -> &Arc<MemoryBoard> {
        &self.memory_board
    }
}

/// Reads the catalog at boot (`None` = fresh node). Corruption is a typed
/// error — the node must refuse to start, never guess (§8.4).
pub fn load_catalog(data_dir: &Path) -> std::io::Result<Option<NsCatalog>> {
    load_catalog_from(&StdSegmentFs, data_dir)
}

/// [`load_catalog`] over an injected filesystem tier (M2-S19: the sim
/// boots its catalog from the `SimDisk` surviving image).
///
/// # Errors
/// Envelope or schema corruption — fail-stop, never guess (§8.4).
pub fn load_catalog_from<F: inf_log::fs::SegmentFs>(
    fs: &F,
    data_dir: &Path,
) -> std::io::Result<Option<NsCatalog>> {
    let Some(payload) = read_meta(fs, data_dir)? else {
        return Ok(None);
    };
    NsCatalog::decode(&payload)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// One catalog persist as the writer performs it (ADR-0015 D3 + ADR-0100
/// D2/D3): counters cover their allocators, the drop joins the tombstone
/// set, stamped tombstones every cell has checkpointed past retire, the
/// set merges into the payload. Shared by the thread and the sim inbox
/// so DST runs the same writer. Returns the encoded payload.
fn prepare_persist(
    handle: &ControlHandle,
    tombstones: &mut DropTombstones,
    pending: &mut PendingCreates,
    req: PersistReq,
) -> (Vec<u8>, u64) {
    let mut catalog = req.catalog;
    catalog.next_id = catalog.next_id.max(handle.next_ns_id());
    catalog.index.next_id = catalog.index.next_id.max(handle.next_index_id());
    catalog.index.next_generation =
        catalog.index.next_generation.max(handle.next_index_generation());
    if let Some(id) = req.dropped {
        pending.retire(id);
    }
    if let Some(id) = req.drop {
        tombstones.add(id);
    }
    tombstones.retire(handle.ckpt_board().min_published());
    // ADR-0103 D2: pending creates join before the tombstones prune, so
    // a since-dropped pending entry is dropped like any other; the
    // verdict is decided against the merged view and written before
    // the epoch publishes (the origin reads it after `persisted`).
    pending.merge(&mut catalog);
    if let Some((spec, verdict)) = req.create {
        verdict.set(pending.admit(&mut catalog, spec));
    }
    tombstones.reconcile(&mut catalog);
    handle.drop_tombstones.store(tombstones.len(), Ordering::Relaxed);
    handle.pending_creates.store(pending.len(), Ordering::Relaxed);
    (catalog.encode(), req.epoch)
}

/// The receiving end of a detached control plane (M2-S19, ADR-0021 D2):
/// the sim cannot spawn the control thread (single-thread determinism),
/// so [`ControlHandle::detached`] hands the message queue back and the
/// harness drains it inline once per scheduler step — catalog META swaps
/// over the *injected* fs and delegated unlinks, exactly the thread's
/// message loop. DDL, checkpoint epochs, and truncation therefore all
/// run inside the deterministic loop.
pub struct ControlInbox {
    rx: mpsc::Receiver<ControlMsg>,
    handle: Arc<ControlHandle>,
    /// The writer's tombstone set (ADR-0100 D2) — inline, deterministic.
    tombstones: DropTombstones,
    /// The writer's pending-create set (ADR-0103 D2) — inline too.
    pending: PendingCreates,
}

impl std::fmt::Debug for ControlInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlInbox").finish_non_exhaustive()
    }
}

impl ControlInbox {
    /// The lowest published checkpoint epoch across cells — the writer's
    /// own retire fold, for the host that drains this inbox (the
    /// simulator's checkpoint waits). No cell folds the whole board
    /// (ADR-0159 D4).
    #[must_use]
    pub fn ckpt_min_published(&self) -> u64 {
        self.handle.ckpt_board().min_published()
    }

    /// Drains queued control work against `fs`. A failed catalog swap is
    /// an error (the control thread's fail-stop analog — the caller owns
    /// the verdict); failed unlinks are ignored (boot GC re-collects).
    ///
    /// # Errors
    /// The catalog META swap failed — a DDL was acked against it (§8.4).
    pub fn drain<F: inf_log::fs::SegmentFs>(
        &mut self,
        fs: &F,
        data_dir: &Path,
    ) -> std::io::Result<()> {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ControlMsg::Persist(req) => {
                    let (payload, epoch) =
                        prepare_persist(&self.handle, &mut self.tombstones, &mut self.pending, req);
                    write_meta(fs, data_dir, &payload)?;
                    self.handle.persisted_epoch.store(epoch, Ordering::Release);
                }
                ControlMsg::StampDrop { id, ckpt_epoch } => {
                    self.tombstones.stamp(id, ckpt_epoch);
                }
                ControlMsg::CreateApplied { id } => {
                    self.pending.retire(id);
                    self.handle.pending_creates.store(self.pending.len(), Ordering::Relaxed);
                }
                ControlMsg::Unlink(path) => {
                    let _ = fs.remove_file(&path);
                }
            }
        }
        Ok(())
    }
}

/// A detached control plane (M2-S19): the handle, the writer's inbox the
/// host drains inline, and the boot's checkpoint issuers (ADR-0159 A1.2).
pub type DetachedControl = (Arc<ControlHandle>, ControlInbox, CkptIssuers);

impl ControlHandle {
    /// A control plane without the thread (M2-S19): the caller drains the
    /// returned [`ControlInbox`] inline. Production nodes use
    /// [`spawn`]; the sim's determinism forbids the thread.
    #[must_use]
    pub fn detached(cells: CellCount, start_unix_ms: u64) -> DetachedControl {
        ControlHandle::detached_in(None, CkptSpace::full(cells), start_unix_ms)
    }

    /// A detached control plane seeded from a recovered catalog (the
    /// reboot path: ids never regress across restart — ADR-0015 D2;
    /// index counters follow the same rule, ADR-0075 D1).
    #[must_use]
    pub fn detached_with_catalog(
        seed: Option<&NsCatalog>,
        cells: CellCount,
        start_unix_ms: u64,
    ) -> DetachedControl {
        ControlHandle::detached_in(seed, CkptSpace::full(cells), start_unix_ms)
    }

    /// [`detached_with_catalog`](Self::detached_with_catalog) over an
    /// explicit issuance space — the A1.5 headroom seam for tests.
    #[must_use]
    pub fn detached_in(
        seed: Option<&NsCatalog>,
        space: CkptSpace,
        start_unix_ms: u64,
    ) -> DetachedControl {
        let (handle, rx, tombstones, issuers) =
            ControlHandle::new_parts(seed, space, start_unix_ms);
        let inbox = ControlInbox {
            rx,
            handle: Arc::clone(&handle),
            tombstones,
            pending: PendingCreates::default(),
        };
        (handle, inbox, issuers)
    }

    /// The handle, the writer's inbox, the writer's tombstone set seeded
    /// from the boot catalog, and the boot's checkpoint issuers. Every
    /// surviving tombstone is re-stamped with one fresh all-cell request
    /// (ADR-0100 D3: cells edge-detect it after recovery, and the next
    /// persist retires what they cover), paid by the control writer's
    /// quota before any cell serves (ADR-0159 A1.2).
    fn new_parts(
        seed: Option<&NsCatalog>,
        space: CkptSpace,
        start_unix_ms: u64,
    ) -> (Arc<ControlHandle>, mpsc::Receiver<ControlMsg>, DropTombstones, CkptIssuers) {
        let cells = space.cells.get();
        let Partition { clock, owner, participants } =
            issue::partition(CkptBoard::new(space.cells), space.space);
        let next_id = seed.map_or(FIRST_NAMED_NS_ID, |c| c.next_id.max(FIRST_NAMED_NS_ID));
        let next_index_id = seed.map_or(FIRST_INDEX_ID, |c| c.index.next_id.max(FIRST_INDEX_ID));
        let next_index_generation = seed.map_or(FIRST_INDEX_GENERATION, |c| {
            c.index.next_generation.max(FIRST_INDEX_GENERATION)
        });
        let (tx, rx) = mpsc::sync_channel::<ControlMsg>(256);
        let handle = Arc::new(ControlHandle {
            tx,
            next_ns_id: AtomicU32::new(next_id),
            next_index_id: AtomicU32::new(next_index_id),
            next_index_generation: AtomicU64::new(next_index_generation),
            next_epoch: AtomicU64::new(0),
            persisted_epoch: Arc::new(AtomicU64::new(0)),
            ckpt_clock: clock,
            recovery: Arc::new(RecoveryBoard::new(cells, start_unix_ms)),
            memory_board: Arc::new(MemoryBoard::new(cells)),
            index_board: Arc::new(IndexBoard::new(cells)),
            drop_tombstones: AtomicUsize::new(0),
            pending_creates: AtomicUsize::new(0),
            ddl_holder: AtomicU16::new(0),
            ddl_generation: AtomicU64::new(0),
        });
        let mut host = CkptQuota(owner);
        let cell_issuers = participants
            .into_iter()
            .zip(0..cells)
            .map(|(participant, cell)| CellIssuer {
                quota: CkptQuota(participant.quota),
                final_credit: CkptFinalCredit {
                    credit: participant.final_credit,
                    cell: CellId(cell),
                },
            })
            .collect();
        let mut tombstones = DropTombstones::seed(seed.map_or(&[][..], |c| &c.dropped));
        if tombstones.len() > 0 {
            let epoch = host
                .request(CkptTarget::All)
                .expect("a fresh quota holds at least one unit (IssueSpace: Q >= 1)");
            tombstones.stamp_unstamped(epoch.get());
            handle.drop_tombstones.store(tombstones.len(), Ordering::Relaxed);
        }
        (handle, rx, tombstones, CkptIssuers { host, cells: cell_issuers })
    }
}

/// Spawns the control thread. `seed` is the boot-loaded catalog (its
/// `next_id` seeds the allocator; a fresh node starts at the named floor).
///
/// The thread fail-stops the process on a failed swap: the catalog is
/// durability metadata and a lost DDL after its `+OK` would be a §8.2
/// violation — same rule class as fsync failure.
pub fn spawn(
    data_dir: PathBuf,
    seed: Option<&NsCatalog>,
    cells: CellCount,
    start_unix_ms: u64,
) -> (Arc<ControlHandle>, CkptIssuers) {
    spawn_in(data_dir, seed, CkptSpace::full(cells), start_unix_ms)
}

/// [`spawn`] over an explicit issuance space — the A1.5 headroom seam for
/// tests. Returns the handle and the boot's checkpoint issuers.
pub fn spawn_in(
    data_dir: PathBuf,
    seed: Option<&NsCatalog>,
    space: CkptSpace,
    start_unix_ms: u64,
) -> (Arc<ControlHandle>, CkptIssuers) {
    let cells = space.cells.get();
    let (handle, rx, tombstones, issuers) = ControlHandle::new_parts(seed, space, start_unix_ms);
    let allocator = Arc::clone(&handle);
    let persisted = Arc::clone(&handle.persisted_epoch);
    let board = Arc::clone(&handle.recovery);
    // denylist-allow: the control thread itself (ADR-0015 D3) - the slow plane, never a cell.
    std::thread::Builder::new()
        .name("inf-control".into())
        .spawn(move || {
            // This thread is detached (the JoinHandle drops below): a
            // swallowed unwind would leave cells serving with catalog
            // persistence, checkpoint epochs, and delegated unlinks
            // silently dead — the ADR-0026 D2 class. Same boundary as
            // the cell threads (M2.5-S01/S16): panic ⇒ loud process exit.
            let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                control_main(&data_dir, &allocator, &persisted, &board, &rx, cells, tombstones);
            }));
            if body.is_err() {
                // The default hook already printed the panic.
                eprintln!("infinityd: control thread panicked — fail-stop");
                std::process::exit(101);
            }
        })
        .expect("spawn control thread");
    (handle, issuers)
}

/// The control thread's message loop + boot narration (M2-S15/M2.5-S01),
/// extracted so [`spawn`] can wrap it in the panic fail-stop boundary.
fn control_main(
    data_dir: &Path,
    allocator: &Arc<ControlHandle>,
    persisted: &Arc<AtomicU64>,
    board: &Arc<RecoveryBoard>,
    rx: &mpsc::Receiver<ControlMsg>,
    cells: u16,
    mut tombstones: DropTombstones,
) {
    let mut pending = PendingCreates::default();
    {
        let mut handle_msg = |msg: ControlMsg| match msg {
            ControlMsg::Persist(req) => {
                // The persisted counters must always cover their
                // allocators so ids and generations never regress
                // across restart, even for DDL that raced this
                // snapshot (ADR-0015 D2; index counters ADR-0075 D1);
                // the tombstone set merges in (ADR-0100 D2), the
                // pending creates too (ADR-0103 D2).
                let (payload, epoch) =
                    prepare_persist(allocator, &mut tombstones, &mut pending, req);
                if let Err(err) = write_meta(&StdSegmentFs, data_dir, &payload) {
                    // §8.4 fail-stop: a DDL was acked against this
                    // swap. `process::exit`, never `panic!` — this
                    // thread is detached (the JoinHandle is dropped),
                    // so an unwind would kill only the control plane
                    // and leave cells serving with catalog
                    // persistence, checkpoint epochs, and delegated
                    // unlinks silently dead (the ADR-0026 D2
                    // swallowed-death class; M2.5-S16 audit fix).
                    eprintln!("FATAL: catalog META swap failed (fail-stop, §8.4): {err}");
                    std::process::exit(crate::EXIT_DURABLE_FAILSTOP);
                }
                persisted.store(epoch, Ordering::Release);
            }
            ControlMsg::StampDrop { id, ckpt_epoch } => tombstones.stamp(id, ckpt_epoch),
            ControlMsg::CreateApplied { id } => {
                pending.retire(id);
                allocator.pending_creates.store(pending.len(), Ordering::Relaxed);
            }
            ControlMsg::Unlink(path) => {
                // Never fatal: the file is outside every recovery
                // unit; a survivor is re-collected at boot.
                #[allow(clippy::disallowed_methods, reason = "control-thread: retired file unlink")]
                if let Err(err) = std::fs::remove_file(&path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("control: unlink {} failed: {err}", path.display());
                }
            }
        };
        // Boot narration (M2-S15): until every cell reports ready,
        // poll the channel with a timeout and print per-cell ready
        // lines + a periodic aggregate progress line (segments/bytes/
        // ETA). Log-line clocks are ambient wall time — control-plane
        // narration only, never oracle input (those ride the
        // injected clocks).
        let mut announced = vec![false; usize::from(cells)];
        #[allow(
            clippy::disallowed_methods,
            reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
        )]
        // denylist-allow: boot-narration wall clock on the control thread, never oracle input.
        let boot_started = std::time::Instant::now();
        let mut next_progress = boot_started + Duration::from_secs(1);
        while !board.all_ready() {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(msg) => handle_msg(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            for (cell, seen) in announced.iter_mut().enumerate() {
                let slot = board.slot(cell as u16);
                if !*seen && slot.ready() {
                    *seen = true;
                    let (_, total) = slot.bytes();
                    let (segs, _) = slot.segments();
                    let torn = slot.torn_truncated_at().map_or(String::new(), |lsn| {
                        format!(", torn tail truncated at {lsn} (M2-S14)")
                    });
                    let residue = slot.residue();
                    let recycled = if residue == RecoveredResidue::default() {
                        String::new()
                    } else {
                        format!(
                            ", recycled residue: {} segment stops, {} slacks (ADR-0090)",
                            residue.segment_residue_stops, residue.recycled_residue_slacks
                        )
                    };
                    let (ns, bytes) = slot.phases();
                    let ms = |i: usize| ns[i] as f64 / 1e6;
                    let unknown = match slot.records_skipped_unknown_ns() {
                        0 => String::new(),
                        n => format!(
                            ", {n} records of untombstoned unknown namespaces skipped \
                             (foreign log or corruption — ADR-0103 D4)"
                        ),
                    };
                    eprintln!(
                        "control: cell {cell} recovered ({segs} segments, {total} bytes, {} \
                         records{torn}{recycled}{unknown}; phases ms: start {:.1}, ckpt {:.1} \
                         [{} B], replay {:.1} [{} B], audit {:.1} [{} B], finish {:.1}, \
                         total {:.1})",
                        slot.records(),
                        ms(0),
                        ms(1),
                        bytes[0],
                        ms(2),
                        bytes[1],
                        ms(3),
                        bytes[2],
                        ms(4),
                        ns.iter().sum::<u64>() as f64 / 1e6,
                    );
                }
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
            )]
            // denylist-allow: boot-narration wall clock on the control thread, never oracle input.
            let now = std::time::Instant::now();
            if now >= next_progress && !board.all_ready() {
                next_progress = now + Duration::from_secs(1);
                let (done, total) = board.bytes();
                #[allow(
                    clippy::disallowed_methods,
                    reason = "clock: boot narration, never oracle input (ADR-0015 D3)"
                )]
                let elapsed = boot_started.elapsed().as_secs_f64();
                let eta = if done > 0 {
                    (elapsed * (total.saturating_sub(done)) as f64 / done as f64).ceil()
                } else {
                    0.0
                };
                eprintln!(
                    "control: recovery {}/{} cells ready, {done}/{total} bytes ({:.1}%), eta \
                         {eta:.0}s",
                    board.ready_cells(),
                    board.cell_count(),
                    if total > 0 { done as f64 * 100.0 / total as f64 } else { 100.0 },
                );
                // Stuck-cell narration (M2.5-S01): after 5 s, name each
                // not-ready cell and the phase it published before its
                // current step — the ADR-0022 D7 wedge signature was a
                // silent cell; a stalled boot must say where it stalled.
                if elapsed >= 5.0 {
                    for cell in 0..cells {
                        let slot = board.slot(cell);
                        if !slot.ready() {
                            let (done, total) = slot.bytes();
                            eprintln!(
                                "control: cell {cell} not ready — in {} ({done}/{total} bytes) for \
                                     {elapsed:.0}s",
                                CellRecoverySlot::phase_name(slot.phase()),
                            );
                        }
                    }
                }
            }
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "clock: control-thread boot narration, never oracle input (ADR-0015 D3)"
        )]
        let boot_ms = boot_started.elapsed().as_millis();
        eprintln!(
            "control: recovery complete — {} cells serving ({boot_ms} ms)",
            board.cell_count()
        );
        while let Ok(msg) = rx.recv() {
            handle_msg(msg);
        }
        // Channel closed = node shutdown; nothing to flush (every
        // acked DDL already persisted before its reply).
    }
}

#[cfg(test)]
mod drop_tombstone_tests {
    use inf_store::{IndexCatalog, NsCatalog, NsMode, NsSpec};

    use super::DropTombstones;

    fn entry(id: u32) -> NsSpec {
        NsSpec {
            id: inf_log::NsId(id),
            name: format!("ns{id}").into_bytes(),
            mode: NsMode::Durable,
            fsync: Some(inf_log::FsyncClass::Everysec),
            policy: None,
            maxmemory: None,
            tier: None,
        }
    }

    /// ADR-0100 D2/D3: add → stamp → retire once every cell published the
    /// stamped epoch; unstamped tombstones never retire; the payload
    /// carries the sorted set and loses any entry the set names.
    #[test]
    fn tombstones_add_stamp_retire_and_reconcile() {
        let mut t = DropTombstones::default();
        t.add(18);
        t.add(16);
        t.add(18);
        assert_eq!(t.entries, vec![(16, None), (18, None)]);
        t.retire(u64::MAX);
        assert_eq!(t.len(), 2, "unstamped tombstones never retire");
        t.stamp(16, 3);
        t.retire(2);
        assert_eq!(t.len(), 2, "not every cell published epoch 3 yet");
        t.retire(3);
        assert_eq!(t.entries, vec![(18, None)]);
        // Boot re-stamp: only the unstamped survivor takes the epoch; a
        // later stamp never lowers an existing bar.
        t.add(20);
        t.stamp(20, 9);
        t.stamp_unstamped(5);
        assert_eq!(t.entries, vec![(18, Some(5)), (20, Some(9))]);
        t.stamp(20, 7);
        assert_eq!(t.entries[1], (20, Some(9)));
        let mut catalog = NsCatalog {
            next_id: 21,
            entries: vec![entry(17), entry(18)],
            index: IndexCatalog::default(),
            dropped: Vec::new(),
        };
        t.reconcile(&mut catalog);
        assert_eq!(catalog.dropped, vec![18, 20]);
        assert_eq!(catalog.entries.len(), 1, "a tombstoned entry is replica lag, never live");
        assert_eq!(catalog.entries[0].id.0, 17);
        assert_eq!(NsCatalog::decode(&catalog.encode()).expect("v4 payload"), catalog);
    }

    /// ADR-0103 D2: a pending create joins every payload that lacks it,
    /// a same-name create is refused against the merged view, the cap
    /// refuses, retirement frees the name, and the verdict slot reads
    /// exactly what the writer set.
    #[test]
    fn pending_creates_merge_admit_retire_and_verdict() {
        use super::{CreateOutcome, CreateVerdict, PENDING_CREATE_MAX, PendingCreates};
        let mut pending = PendingCreates::default();
        let mut catalog = NsCatalog {
            next_id: 17,
            entries: vec![entry(16)],
            index: IndexCatalog::default(),
            dropped: Vec::new(),
        };
        // Accepted: joins the payload and the set.
        assert_eq!(pending.admit(&mut catalog, entry(17)), CreateOutcome::Accepted);
        assert_eq!(catalog.entries.len(), 2);
        assert_eq!(pending.len(), 1);
        // A concurrent origin's stale export (no 17) gets it merged in.
        let mut stale = NsCatalog {
            next_id: 18,
            entries: vec![entry(16)],
            index: IndexCatalog::default(),
            dropped: Vec::new(),
        };
        pending.merge(&mut stale);
        assert_eq!(stale.entries.iter().map(|e| e.id.0).collect::<Vec<_>>(), vec![16, 17]);
        // The same name from another origin (fresh id) is refused
        // against the merged view — existing and pending alike.
        let mut dup = entry(18);
        dup.name = b"ns17".to_vec();
        assert_eq!(pending.admit(&mut stale, dup), CreateOutcome::NameExists);
        let mut dup16 = entry(19);
        dup16.name = b"ns16".to_vec();
        assert_eq!(pending.admit(&mut stale, dup16), CreateOutcome::NameExists);
        assert_eq!(pending.len(), 1, "a refused create never joins the set");
        // Retirement frees the name.
        pending.retire(17);
        assert_eq!(pending.len(), 0);
        let mut again = NsCatalog {
            next_id: 20,
            entries: vec![entry(16)],
            index: IndexCatalog::default(),
            dropped: Vec::new(),
        };
        let mut reuse = entry(20);
        reuse.name = b"ns17".to_vec();
        assert_eq!(pending.admit(&mut again, reuse), CreateOutcome::Accepted);
        pending.retire(20);
        // The cap.
        for id in 100..(100 + PENDING_CREATE_MAX as u32) {
            let mut c = NsCatalog {
                next_id: id + 1,
                entries: Vec::new(),
                index: IndexCatalog::default(),
                dropped: Vec::new(),
            };
            assert_eq!(pending.admit(&mut c, entry(id)), CreateOutcome::Accepted);
        }
        let mut full = NsCatalog {
            next_id: 1_000,
            entries: Vec::new(),
            index: IndexCatalog::default(),
            dropped: Vec::new(),
        };
        assert_eq!(pending.admit(&mut full, entry(999)), CreateOutcome::AtCapacity);
        assert!(full.entries.is_empty(), "a refused create never joins the payload");
        // The verdict slot.
        let verdict = CreateVerdict::new();
        assert_eq!(verdict.get(), None, "unset until the writer ran");
        verdict.set(CreateOutcome::NameExists);
        assert_eq!(verdict.get(), Some(CreateOutcome::NameExists));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroU64;

    use inf_foundation::{CellCount, CellId, SLOT_COUNT};

    use super::{
        BoardObservation, BoardSweep, CkptBoard, CkptSlot, CkptSpace, CkptTarget, ControlHandle,
        SweepStep,
    };

    fn cells(count: u16) -> CellCount {
        CellCount::new(count).expect("a valid test topology")
    }

    fn headroom(units: u64) -> NonZeroU64 {
        NonZeroU64::new(units).expect("nonzero")
    }

    /// The pre-change folds, one whole-board pass at one instant: the
    /// control leg's reference. The sum is the pre-fix `u64` fold with its
    /// release behaviour (it wrapped), kept as the alias canary.
    fn old_fold(board: &CkptBoard) -> (u64, u64, u64) {
        let min = board.cells.iter().map(CkptSlot::published).min().unwrap_or(0);
        let sum = board.cells.iter().map(CkptSlot::published).fold(0u64, u64::wrapping_add);
        let max = board.cells.iter().map(CkptSlot::last_unix_ms).max().unwrap_or(0);
        (min, sum, max)
    }

    /// The alias oracle: a wake signal equal to the initial state after a
    /// publication hides the publication.
    fn aliases_the_initial_state(signal: u128) -> bool {
        signal == 0
    }

    /// Steps `sweep` until it completes; returns the steps taken.
    fn complete_sweep(board: &CkptBoard, sweep: &mut BoardSweep) -> (u32, SweepStep) {
        let mut steps = 0;
        loop {
            steps += 1;
            match board.sweep_step(sweep) {
                SweepStep::InProgress => {}
                done @ (SweepStep::Unchanged | SweepStep::Progressed) => return (steps, done),
            }
        }
    }

    /// ADR-0159 D1, A1.2 (every issue is nonzero and above every earlier
    /// one of its boot), through the A1.5 seam: every credit of a 2-cell
    /// boot — both cells' quotas, the host's and both final credits —
    /// issues a distinct, increasing, nonzero epoch, the drive reaches
    /// `u64::MAX` exactly (engagement), and past it every reservation is a
    /// typed refusal: no issue can wrap or panic.
    #[test]
    fn ckpt_issue_at_u64_max_never_repeats() {
        let space = CkptSpace::with_headroom(cells(2), headroom(2)).expect("fits");
        let (handle, _inbox, issuers) = ControlHandle::detached_in(None, space, 0);
        assert_eq!(handle.ckpt_last_issued(), u64::MAX - (3 * 2 + 2), "(N + 1)·h + N below");
        let mut ledger = BTreeSet::new();
        let mut previous = handle.ckpt_last_issued();
        let mut record = |epoch: u64| {
            assert!(epoch > previous, "epoch {epoch} is not above {previous}");
            assert!(ledger.insert(epoch), "epoch {epoch} repeats");
            previous = epoch;
        };
        let mut host = issuers.host;
        while let Ok(epoch) = host.request(CkptTarget::All) {
            record(epoch.get());
        }
        let mut finals = Vec::new();
        for (cell, issuer) in issuers.cells.into_iter().enumerate() {
            let mut quota = issuer.quota;
            let target = CkptTarget::Cell(CellId(cell as u16));
            while let Ok(epoch) = quota.request(target) {
                record(epoch.get());
            }
            assert!(quota.reserve::<1>().is_err(), "an exhausted quota refuses");
            finals.push(issuer.final_credit);
        }
        for final_credit in finals {
            record(final_credit.issue().get());
        }
        assert_eq!(handle.ckpt_last_issued(), u64::MAX, "the drive reached the top");
        assert_eq!(ledger.len(), 8);
        assert!(host.request(CkptTarget::All).is_err(), "past the top, only refusals");
        let board = handle.ckpt_board();
        assert_eq!(board.requested(1), u64::MAX, "cell 1's final request is the last epoch");
    }

    /// ADR-0159 D4 (observed sums cannot wrap): two publications at 2^63
    /// fold to 2^64 in the sweep's `u128` sum — a change from the initial
    /// observation, so the wake fires.
    #[test]
    fn published_sum_at_2_pow_63_is_not_the_initial_state() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(2), 0);
        let board = handle.ckpt_board();
        board.slot(0).publish(1 << 63, 1, 1);
        board.slot(1).publish(1 << 63, 1, 1);
        let mut sweep = BoardSweep::new();
        assert_eq!(board.sweep_step(&mut sweep), SweepStep::Progressed, "the wake fires");
        assert_eq!(sweep.observed().published_sum, 1 << 64);
        assert!(!aliases_the_initial_state(sweep.observed().published_sum));
    }

    /// Canary: the pre-fix `u64` fold aliases the initial state at
    /// 2^63 + 2^63, and the alias oracle sees it.
    #[test]
    fn canary_the_u64_fold_aliases_the_initial_state() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(2), 0);
        let board = handle.ckpt_board();
        board.slot(0).publish(1 << 63, 1, 1);
        board.slot(1).publish(1 << 63, 1, 1);
        let (_, old_sum, _) = old_fold(board);
        assert!(aliases_the_initial_state(u128::from(old_sum)), "the alias canary must go red");
    }

    /// A1.2: one cell's exhausted quota never stops another cell's.
    #[test]
    fn one_exhausted_quota_leaves_the_others_reserving() {
        let space = CkptSpace::with_headroom(cells(2), headroom(1)).expect("fits");
        let (_handle, _inbox, issuers) = ControlHandle::detached_in(None, space, 0);
        let mut quotas = issuers.cells.into_iter().map(|issuer| issuer.quota);
        let (mut first, mut second) = (quotas.next().expect("cell 0"), quotas.next().expect("1"));
        assert!(first.request(CkptTarget::All).is_ok());
        assert!(first.request(CkptTarget::All).is_err(), "cell 0 is exhausted");
        assert!(second.request(CkptTarget::All).is_ok(), "cell 1 still reserves");
    }

    /// The observer at the topology limit (ADR-0159 D4, A1.4): 256
    /// turns of at most 64 visits per sweep, and a publication made at
    /// any point is observed within two completed sweeps.
    #[test]
    fn a_full_board_sweeps_in_256_bounded_turns_and_sees_an_advance_within_two() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(SLOT_COUNT), 0);
        let board = handle.ckpt_board();
        let mut sweep = BoardSweep::new();
        let mut turns = 0u32;
        loop {
            let before = sweep.visits();
            let step = board.sweep_step(&mut sweep);
            turns += 1;
            assert!(sweep.visits() - before <= 64, "A1.4: at most 64 visits per turn");
            if step != SweepStep::InProgress {
                break;
            }
        }
        assert_eq!(turns, 256, "ceil(16,384 / 64) turns per sweep");
        assert_eq!(sweep.sweeps_completed(), 1);
        // Publish behind the cursor, part-way into a sweep: the sweep in
        // progress misses it, the next one sees it.
        for _ in 0..100 {
            board.sweep_step(&mut sweep);
        }
        board.slot(3).publish(7, 1, 1_000);
        let seen_within = observed_within(board, &mut sweep, 512, |board, sweep| {
            board.sweep_step(sweep);
        });
        assert!(seen_within.is_some(), "an advance is observed within two sweeps");
    }

    /// Canary: a stalled observer. The liveness counter stays flat and
    /// the two-sweep oracle goes red.
    #[test]
    fn canary_a_stalled_sweep_is_caught() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(SLOT_COUNT), 0);
        let board = handle.ckpt_board();
        let mut sweep = BoardSweep::new();
        board.slot(3).publish(7, 1, 1_000);
        let seen = observed_within(board, &mut sweep, 512, |_, _| {});
        assert!(seen.is_none(), "the stalled-sweep canary must go red");
        assert_eq!(sweep.visits(), 0, "the liveness counter shows the stall");
    }

    /// Turns until the sweep's observation carries a nonzero sum, up to
    /// `turns_max`.
    fn observed_within(
        board: &CkptBoard,
        sweep: &mut BoardSweep,
        turns_max: u32,
        step: impl Fn(&CkptBoard, &mut BoardSweep),
    ) -> Option<u32> {
        (1..=turns_max).find(|_| {
            step(board, sweep);
            sweep.observed().published_sum != 0
        })
    }

    /// The control leg (ADR-0159 D4): at `N <= 64` one MAINTAIN completes a
    /// sweep, and its observation equals the pre-change fold over the same
    /// board state — `WAIT`, DROP pacing and the INFO gauge are unchanged.
    #[test]
    fn at_64_cells_or_fewer_one_step_equals_the_old_fold() {
        for count in [1u16, 2, 7, 64] {
            let (handle, _inbox, _issuers) = ControlHandle::detached(cells(count), 0);
            let board = handle.ckpt_board();
            for cell in 0..count {
                let epoch = u64::from(cell) * 7 + 3;
                board.slot(cell).publish(epoch, 1, 1_000 + u64::from(cell) * 13 % 50);
            }
            let mut sweep = BoardSweep::new();
            let (steps, _) = complete_sweep(board, &mut sweep);
            assert_eq!(steps, 1, "one turn at {count} cells");
            let (min, sum, max) = old_fold(board);
            let observed = sweep.observed();
            assert_eq!(observed.min_published, min);
            assert_eq!(observed.published_sum, u128::from(sum));
            assert_eq!(observed.max_unix_ms, max);
        }
    }

    /// ADR-0159 A1.4 through the product's own step: with this cell's
    /// sweep stalled (it never read the board), the `WAIT CELL 1`
    /// confirmation raises its `LASTSAVE` floor to slot 1's publication
    /// second. `wait_for_ckpt` and `LASTSAVE` call exactly these two
    /// methods; the planted `inf_canary_lastsave_floor_skipped` (the
    /// confirmation without its raise) must fail this test.
    #[test]
    fn lastsave_after_wait_cell_covers_the_fenced_checkpoint() {
        let (handle, _inbox, issuers) = ControlHandle::detached(cells(2), 0);
        let mut quota = issuers.cells.into_iter().next().expect("cell 0").quota;
        let epoch = quota.request(CkptTarget::Cell(CellId(1))).expect("a unit");
        let board = handle.ckpt_board();
        assert_eq!(board.requested(1), epoch.get());
        assert_eq!(board.requested(0), 0, "a targeted request reaches one slot");
        let mut sweep = BoardSweep::new();
        assert!(!sweep.confirm_cell_wait(board.slot(1), epoch), "not yet published");
        let published_ms = 1_700_000_123_456;
        board.slot(1).publish(epoch.get(), 9, published_ms);
        assert!(sweep.confirm_cell_wait(board.slot(1), epoch), "slot 1 published the epoch");
        // Engagement: the observation alone would answer 0.
        assert_eq!(sweep.observed(), BoardObservation::default(), "the sweep trails the board");
        let lastsave = sweep.lastsave_unix_s();
        assert!(lastsave >= published_ms / 1000, "LASTSAVE {lastsave} trails the WAIT");
        // The floor only rises: an older publication it confirms leaves it.
        board.slot(0).publish(epoch.get(), 1, 1_000);
        assert!(sweep.confirm_cell_wait(board.slot(0), epoch));
        assert_eq!(sweep.lastsave_unix_s(), lastsave);
    }

    /// ADR-0159 A1.1 at the control-plane constructors: they take only a
    /// `CellCount`, whose constructor refuses `SLOT_COUNT + 1`, and the
    /// largest topology builds with `Q >= 2^49`. The embedded host has no
    /// assembly yet, so this covers only the constructors it will call.
    #[test]
    fn control_constructors_take_a_cell_count_up_to_the_slot_count() {
        assert!(CellCount::new(SLOT_COUNT + 1).is_err());
        let (handle, _inbox, issuers) = ControlHandle::detached(cells(SLOT_COUNT), 0);
        assert_eq!(handle.ckpt_board().cell_count(), usize::from(SLOT_COUNT));
        assert_eq!(issuers.cells.len(), usize::from(SLOT_COUNT));
        assert!(issuers.host.remaining() >= 1 << 49, "Q >= 2^49 at 16,384 cells (A1.2)");
    }
}
