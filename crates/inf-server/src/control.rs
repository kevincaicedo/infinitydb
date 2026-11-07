//! The control thread (M2-S08, ADR-0015 D3) — the slow plane the
//! architecture always reserved (§4). Its first job: **single writer of
//! the node catalog `META` file**. Cells must never block on file I/O, so
//! DDL persists asynchronously: the origin cell's pump sends a request
//! over a bounded channel and parks on a cell-local waitlist. This thread
//! numbers each request as it dequeues it (the **catalog epoch**, its
//! alone), performs the write-new + fsync + rename + dir-fsync swap
//! (`inf-log`'s `meta` protocol) and answers the request through its own
//! slot after the swap returned, then publishes the **catalog word**, the
//! highest answered epoch (ADR-0189 D2); every cell's MAINTAIN observes the
//! word and wakes its parked pumps, and each pump reads its own answer.
//!
//! The writer also moves the namespace-id counter (ids are node-unique,
//! issued once, never reused — ADR-0015 D2): a shared `AtomicU32` seeded
//! from the catalog's `next_id` at boot. A `CREATE` reserves the counter's
//! value under the DDL ticket without moving it, and only the writer's
//! acceptance of that `CREATE` moves it, by one, in the persist that names
//! the id (ADR-0148 D4): a refused `CREATE` leaves it where it was, and a
//! counter at `u32::MAX` issues no id. One load and one store per DDL are
//! control-plane traffic; L1's no-shared-atomics rule binds the data plane.
//!
//! The writer also owns the **drop-tombstone set** (ADR-0100 D2): a
//! durable namespace's `DROP` adds its id to every payload the writer
//! emits until every cell has published a `MANIFEST` past the drop (the
//! origin requests that checkpoint and posts its epoch with the drop's
//! notice; the writer retires stamped tombstones at the next accepted
//! request once every cell's published epoch covers them). Recovery reads
//! the set to tell dropped residue from corruption. And it owns the **one
//! pending DDL** (ADR-0189 D3): an accepted `CREATE` or `DROP` whose fan
//! may not have reached every cell, kept in every payload until the
//! origin's notice retires it.
//!
//! Checkpoint requests are issued, not counted (ADR-0159 A1): a boot's
//! partition mints one quota per cell, one for this writer and one final
//! credit per cell, and only a credit advances the checkpoint clock, so an
//! epoch can neither wrap nor repeat. An exhausted quota refuses before
//! any effect.

use core::cell::Cell;
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
use inf_store::ddl::{
    self, AnswerSlot, CatalogEpochs, CatalogWord, CreatePersisted, DdlRefusal, DdlVerdict,
    DropPersisted, Requested, SetPersisted,
};
use inf_store::{
    FIRST_INDEX_GENERATION, FIRST_INDEX_ID, FIRST_NAMED_NS_ID, NsCatalog, NsId, NsSpec,
};
use inf_wire::{Domain, Refusal};

use crate::limits::{CKPT_BOARD_VISITS_PER_TURN, NS_IDS_EXHAUSTED};

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
    /// ADR-0174 D6: this cell's boot replay of its tiered namespaces, in
    /// [`TierReplayStats::FIELDS`](crate::recover::TierReplayStats::FIELDS)
    /// order — `INFO persistence` renders the node fold.
    tier_replay: [AtomicU64; crate::recover::TIER_REPLAY_FIELDS],
    /// ADR-0174 A1.2: this cell's step facts — the largest work one step
    /// did and the liveness counters — in
    /// [`StepFacts::FIELDS`](crate::recover::StepFacts::FIELDS) order.
    step_facts: [AtomicU64; crate::recover::STEP_FACT_FIELDS],
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

    /// Publishes this cell's tiered boot replay facts (owning cell, once,
    /// before [`mark_ready`](Self::mark_ready), whose release store makes
    /// them visible to a reader that saw the cell ready).
    pub fn publish_tier_replay(&self, stats: crate::recover::TierReplayStats) {
        for (slot, value) in self.tier_replay.iter().zip(stats.to_array()) {
            slot.store(value, Ordering::Relaxed);
        }
    }

    /// Publishes this cell's step facts (owning cell, once, before
    /// [`mark_ready`](Self::mark_ready), as the tiered facts are).
    pub fn publish_step_facts(&self, facts: crate::recover::StepFacts) {
        for (slot, value) in self.step_facts.iter().zip(facts.to_array()) {
            slot.store(value, Ordering::Relaxed);
        }
    }

    /// This cell's step facts, valid once ready.
    pub fn step_facts(&self) -> crate::recover::StepFacts {
        crate::recover::StepFacts::from_array(std::array::from_fn(|i| {
            self.step_facts[i].load(Ordering::Relaxed)
        }))
    }

    /// This cell's tiered boot replay facts, valid once ready.
    pub fn tier_replay(&self) -> crate::recover::TierReplayStats {
        crate::recover::TierReplayStats::from_array(std::array::from_fn(|i| {
            self.tier_replay[i].load(Ordering::Relaxed)
        }))
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
    /// steps, labels rather than boot order (published by `cell_main` so a
    /// pre-loop stall names itself — the 500-cycle storm caught that class).
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
            17 => "setup:ring-barrier",
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

    /// The node fold of the cells' tiered boot replay facts (ADR-0174
    /// D6): counters summed, the step-charge gauge the largest — valid
    /// once [`all_ready`](Self::all_ready). Bound: one pass over the cells.
    #[must_use]
    pub fn tier_replay_totals(&self) -> crate::recover::TierReplayStats {
        self.cells.iter().fold(crate::recover::TierReplayStats::default(), |total, slot| {
            total.fold(slot.tier_replay())
        })
    }

    /// The node fold of the cells' step facts (ADR-0174 A1.2): the work
    /// gauge the largest, the counters summed — valid once
    /// [`all_ready`](Self::all_ready). Bound: one pass over the cells.
    #[must_use]
    pub fn step_facts_totals(&self) -> crate::recover::StepFacts {
        self.cells
            .iter()
            .fold(crate::recover::StepFacts::default(), |total, slot| total.fold(slot.step_facts()))
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

    /// Slot `me`'s published epoch (`Acquire`). Total: `None` for a cell the
    /// board does not hold, which `set_control` refuses to wire.
    fn own_published(&self, me: usize) -> Option<u64> {
        self.cells.get(me).map(CkptSlot::published)
    }

    /// Whether the sweep's own cell published since the start of its last
    /// completed sweep: the park guard's own-slot term (`interfaces-m2.md`,
    /// "Cells never fold the whole board"). Until a sweep that began after
    /// the publication completes, the cell's observation may lack it. The
    /// cell publishes after its sweep step in a MAINTAIN, so a cell with a
    /// checkpoint waiter does not park while this holds; it ends within
    /// `2 * ceil(N / 64)` sweep steps of the publication.
    #[must_use]
    pub fn own_unobserved(&self, sweep: &BoardSweep) -> bool {
        self.own_published(sweep.me).is_some_and(|published| published != sweep.own_seen)
    }

    /// One bounded step of a cell's sweep (ADR-0159 D4, A1.4): at most
    /// [`CKPT_BOARD_VISITS_PER_TURN`] slots from the sweep's cursor, each
    /// read `published` (`Acquire`) then its publication time. A sweep of
    /// `N` cells completes in `ceil(N / 64)` steps and then publishes its
    /// observation, and with it the cell's `lastsave`. A sweep's first step
    /// loads its own cell's slot before any visit.
    pub fn sweep_step(&self, sweep: &mut BoardSweep, lastsave: &LastSave) -> SweepStep {
        let begin = sweep.cursor;
        let end = self.cells.len().min(begin + CKPT_BOARD_VISITS_PER_TURN);
        debug_assert!(begin < end, "a sweep's cursor stays below the board's cell count");
        // The sweep's start, before any visit: a sweep whose `own_seen`
        // reaches a publication of this cell read every slot after it.
        #[cfg(not(inf_canary_ckpt_own_loaded_at_cursor))]
        let loads_own = begin == 0;
        // Canary: the own slot loaded as the cursor passes it, over slots
        // the sweep read before the publication.
        #[cfg(inf_canary_ckpt_own_loaded_at_cursor)]
        let loads_own = (begin..end).contains(&sweep.me);
        if loads_own {
            sweep.partial_own = self.own_published(sweep.me).unwrap_or(0);
        }
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
        // Canary: a completed sweep leaves `own_seen` behind.
        #[cfg(not(inf_canary_ckpt_own_seen_not_advanced))]
        {
            sweep.own_seen = sweep.partial_own;
        }
        sweep.write_lastsave(lastsave);
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
    /// Newest publication time (unix ms): one term of the cell's
    /// [`LastSave`].
    pub max_unix_ms: u64,
}

/// One cell's `LASTSAVE` value (ADR-0159 A1.4), unix ms: the newest
/// publication time in the cell's completed board sweep, raised by every
/// slot a `WAIT CELL k` on the cell confirmed; 0 before the first. The
/// `LASTSAVE` command and INFO's `rdb_last_save_time` both read it
/// (`interfaces-m2.md`, "Cells never fold the whole board"), so on one
/// cell they cannot differ. It is written only by the two steps that
/// change one of its terms ([`CkptBoard::sweep_step`] and
/// [`BoardSweep::confirm_cell_wait`]): no setter leaves this module.
#[derive(Debug, Default)]
pub struct LastSave {
    unix_ms: Cell<u64>,
    /// Canary: the completed sweep's term alone, for an INFO gauge that
    /// reads its own source again.
    #[cfg(inf_canary_info_lastsave_from_observation)]
    observed_unix_ms: Cell<u64>,
}

impl LastSave {
    /// The value in unix seconds: what `LASTSAVE` and `rdb_last_save_time`
    /// answer.
    #[must_use]
    pub fn unix_s(&self) -> u64 {
        self.unix_ms.get() / 1000
    }

    /// Canary: the completed sweep's term alone, in unix seconds.
    #[cfg(inf_canary_info_lastsave_from_observation)]
    #[must_use]
    pub fn observed_unix_s(&self) -> u64 {
        self.observed_unix_ms.get() / 1000
    }
}

/// One cell's resumable sweep over the board (ADR-0159 D4): a cursor into
/// the fixed boot board and the partial folds. No slot reference crosses a
/// turn; the board's membership never changes within a boot.
#[derive(Debug)]
pub struct BoardSweep {
    /// The sweeping cell's own slot index, fixed at construction.
    me: usize,
    cursor: usize,
    partial: BoardObservation,
    /// Slot `me`'s published epoch, loaded when the sweep in progress began.
    partial_own: u64,
    observed: BoardObservation,
    /// Slot `me`'s published epoch at the start of the last completed sweep
    /// (0 before the first).
    own_seen: u64,
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

    /// The sweep of `cell`, whose own slot it watches.
    #[must_use]
    pub fn new(cell: CellId) -> BoardSweep {
        BoardSweep {
            me: usize::from(cell.0),
            cursor: 0,
            partial: BoardSweep::EMPTY_PARTIAL,
            partial_own: 0,
            observed: BoardObservation::default(),
            own_seen: 0,
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
    /// load that satisfied the check raises this cell's `LASTSAVE` floor and
    /// `lastsave` in the same step, so a `WAIT CELL k` that returns leaves
    /// `LASTSAVE` and `rdb_last_save_time` covering the checkpoint it
    /// fenced, even while the sweep trails.
    pub fn confirm_cell_wait(
        &mut self,
        slot: &CkptSlot,
        epoch: CheckpointEpoch,
        lastsave: &LastSave,
    ) -> bool {
        let Some(unix_ms) = slot.covered_at(epoch) else { return false };
        // Canary: the confirmation without its floor raise.
        #[cfg(not(inf_canary_lastsave_floor_skipped))]
        {
            self.lastsave_floor_ms = self.lastsave_floor_ms.max(unix_ms);
        }
        #[cfg(inf_canary_lastsave_floor_skipped)]
        let _ = unix_ms;
        // Canary: the confirmation without its write of the cell's value.
        #[cfg(not(inf_canary_lastsave_cell_stale))]
        self.write_lastsave(lastsave);
        #[cfg(inf_canary_lastsave_cell_stale)]
        let _ = lastsave;
        true
    }

    /// The cell's `LASTSAVE` value from its two terms. The two steps that
    /// change a term call it, so the value never lags either.
    fn write_lastsave(&self, lastsave: &LastSave) {
        lastsave.unix_ms.set(self.observed.max_unix_ms.max(self.lastsave_floor_ms));
        #[cfg(inf_canary_info_lastsave_from_observation)]
        lastsave.observed_unix_ms.set(self.observed.max_unix_ms);
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

/// One catalog request as the writer dequeues it (ADR-0189 D2): the change
/// it asks for, the export it was shaped from, and the writer half of its
/// answer slot. Cell 0's readiness persist asks for no change and is
/// answered through no slot.
struct CatalogRequest {
    intent: Option<DdlIntent>,
    /// The requester's catalog export, before the writer's state applies.
    export: NsCatalog,
    slot: Option<AnswerSlot>,
    /// The planted canary's epoch, taken at send (the shape the writer's
    /// numbering at dequeue replaces).
    #[cfg(inf_canary_ns_epoch_at_send)]
    numbered: u64,
}

impl CatalogRequest {
    fn new(intent: Option<DdlIntent>, export: NsCatalog, slot: Option<AnswerSlot>) -> Self {
        CatalogRequest {
            intent,
            export,
            slot,
            #[cfg(inf_canary_ns_epoch_at_send)]
            numbered: 0,
        }
    }
}

/// The change a DDL request asks the writer for.
enum DdlIntent {
    /// A new namespace, its id the requester reserved: committed only if
    /// the writer accepts it, and only when it is the counter's next.
    Create { spec: NsSpec, reserved: NsId },
    /// Namespace `id` goes; `tombstone` for a durable one.
    Drop { id: u32, tombstone: bool },
}

/// The one DDL the writer accepted and has not yet seen fanned
/// (ADR-0189 D3): its effect stays in every payload until its notice
/// arrives, so no lagging export drops or revives it.
#[derive(Debug)]
enum PendingDdl {
    /// An accepted `CREATE`, until its `CreateApplied`.
    Create(NsSpec),
    /// An accepted `DROP` or rollback, until its `DropFanned`.
    Drop(u32),
}

/// A DDL's notice to the writer (ADR-0189 D3, D5): it retires the pending
/// DDL of its own kind and id, and changes nothing else.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DdlNotice {
    /// A `CREATE`'s fan completed, or the `CREATE` stopped after its answer:
    /// every cell's export carries what `META` holds of it from now on.
    CreateApplied { id: u32 },
    /// A `DROP`'s or a rollback's fan completed, or it stopped after its
    /// answer; `stamp` is the epoch of the checkpoint that retires a
    /// durable drop's tombstone, `None` when the drop leaves it unstamped.
    DropFanned { id: u32, stamp: Option<u64> },
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

    /// Whether `id` is tombstoned: ids never reuse, so an entry an export
    /// carries for it is replica lag from a concurrent DDL, never a live
    /// namespace.
    fn contains(&self, id: u32) -> bool {
        self.entries.binary_search_by_key(&id, |t| t.0).is_ok()
    }

    /// The tombstoned ids, ascending (the payload's `dropped`).
    fn ids(&self) -> Vec<u32> {
        self.entries.iter().map(|e| e.0).collect()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Control-thread work (single receiver, bounded — L3).
enum ControlMsg {
    /// A catalog request, answered in queue order.
    Request(CatalogRequest),
    /// A DDL's notice: it retires the writer's pending DDL.
    Notice(DdlNotice),
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
    /// The next namespace id (ADR-0148 D4): stored only by the catalog
    /// writer, when it accepts the `CREATE` that reserved it;
    /// [`NS_IDS_EXHAUSTED`] once the last id is issued.
    next_ns_id: AtomicU32,
    /// Index-id + generation allocators (M4.5-S03, ADR-0075 D1): both
    /// node-unique, allocated once, never reused — the ns-id discipline.
    next_index_id: AtomicU32,
    next_index_generation: AtomicU64,
    /// Catalog requests sent this boot (tests: a DDL parked on its answer
    /// has sent its request). Not an epoch: the writer numbers requests
    /// at dequeue.
    requests_sent: AtomicU64,
    /// The planted canary's send-time numbering.
    #[cfg(inf_canary_ns_epoch_at_send)]
    numbered: AtomicU64,
    /// The catalog word (ADR-0189 D2), stored by the writer alone.
    word: Arc<CatalogWord>,
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
    /// Node gauge: 1 while the writer holds a pending DDL, else 0.
    ddl_pending: AtomicU64,
    /// Node counter: notices that matched no pending DDL (zero by design;
    /// the ticket admits one DDL and each posts its own notice).
    notices_stale: AtomicU64,
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

impl DdlTicket {
    /// Reserves the next namespace id for this program's `CREATE`, moving
    /// nothing (ADR-0148 D4): the writer moves the counter past it only
    /// when it accepts that `CREATE`. One program holds the ticket, so the
    /// value it reads is the next one the writer will commit; the previous
    /// program's commit is ordered before it by that program's read of its
    /// answer and its ticket's release.
    ///
    /// # Errors
    /// `IdentityExhausted(Namespace)` once the last id, `u32::MAX − 1`, is
    /// issued: before any effect, for the data directory's life.
    pub fn reserve_ns_id(&self) -> Result<NsIdReservation, Refusal> {
        let next = self.handle.next_ns_id.load(Ordering::Acquire);
        // Canary `inf_canary_ns_id_wraps`: the exhaustion check is off, so a
        // spent counter issues `u32::MAX` and the commit wraps (the wire's
        // ids oracle sees an id with no successor).
        if next == NS_IDS_EXHAUSTED && !cfg!(inf_canary_ns_id_wraps) {
            return Err(Refusal::IdentityExhausted(Domain::Namespace));
        }
        // Canary `inf_canary_ns_id_spent_on_refusal`: the reservation commits
        // when made, as an allocator would, so every refusal after it spends
        // the id (the wire's ids oracle sees the next id skip).
        if cfg!(inf_canary_ns_id_spent_on_refusal) {
            self.handle.next_ns_id.store(next.wrapping_add(1), Ordering::Release);
        }
        Ok(NsIdReservation { id: NsId(next) })
    }
}

/// A namespace id one `CREATE` reserved under the DDL ticket (ADR-0148
/// D4): the counter's next value, not moved. Made only by
/// [`DdlTicket::reserve_ns_id`] and consumed by
/// [`ControlHandle::request_create`]; dropped, it changes nothing, so a
/// `CREATE` refused before its request spends no id. Not `Clone`.
#[derive(Debug)]
pub struct NsIdReservation {
    id: NsId,
}

impl NsIdReservation {
    /// The reserved id.
    #[must_use]
    pub fn id(&self) -> NsId {
        self.id
    }
}

impl ControlHandle {
    /// Live drop tombstones in the catalog writer (ADR-0100 D3/D7).
    pub fn drop_tombstones(&self) -> usize {
        self.drop_tombstones.load(Ordering::Relaxed)
    }

    /// Node gauge: 1 while the writer holds a pending DDL (ADR-0189 D11).
    pub fn ddl_pending(&self) -> u64 {
        self.ddl_pending.load(Ordering::Relaxed)
    }

    /// Node counter: notices that retired no pending DDL (ADR-0189 D11).
    pub fn ddl_notices_stale(&self) -> u64 {
        self.notices_stale.load(Ordering::Relaxed)
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

    /// A DDL's notice to the writer (ADR-0189 D5), a blocking send like
    /// every request: it retires the pending DDL of its own kind and id.
    pub(crate) fn post_notice(&self, notice: DdlNotice) {
        self.tx.send(ControlMsg::Notice(notice)).expect("control thread alive (fail-stop)");
    }
    /// The next namespace id (the catalog's `next_id`): the id a
    /// `CREATE` reserves, [`NS_IDS_EXHAUSTED`] once none is left.
    pub fn next_ns_id(&self) -> u32 {
        self.next_ns_id.load(Ordering::Acquire)
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

    /// Asks the writer for the `META` that names `spec` (ADR-0103 D1,
    /// ADR-0189 D2), its id the one `reservation` holds; the token reads
    /// the answer. The writer commits the id only if it accepts.
    #[must_use]
    pub fn request_create(
        &self,
        export: NsCatalog,
        spec: NsSpec,
        reservation: NsIdReservation,
    ) -> Requested<CreatePersisted> {
        let (slot, requested) = ddl::answer_slot(spec.id);
        let intent = Some(DdlIntent::Create { spec, reserved: reservation.id });
        self.send_request(CatalogRequest::new(intent, export, Some(slot)));
        requested
    }

    /// Asks the writer for the `META` that drops namespace `id`, with its
    /// tombstone when `tombstone` (a durable namespace, ADR-0100 D2).
    #[must_use]
    pub fn request_drop(
        &self,
        export: NsCatalog,
        id: u32,
        tombstone: bool,
    ) -> Requested<DropPersisted> {
        let (slot, requested) = ddl::answer_slot(NsId(id));
        let intent = Some(DdlIntent::Drop { id, tombstone });
        self.send_request(CatalogRequest::new(intent, export, Some(slot)));
        requested
    }

    /// Asks the writer for a `META` shaped from `export`, the new
    /// configuration of namespace `id` already in it (`SET` applies and
    /// fans first, ADR-0015 D3).
    #[must_use]
    pub fn request_set(&self, export: NsCatalog, id: NsId) -> Requested<SetPersisted> {
        let (slot, requested) = ddl::answer_slot(id);
        self.send_request(CatalogRequest::new(None, export, Some(slot)));
        requested
    }

    /// Queues a `META` shaped from `export` with no answer to wait for
    /// (cell 0's index-readiness persist, ADR-0075 D4). The bounded channel
    /// makes admission explicit: a full queue blocks the *sender* briefly
    /// (DDL-rate traffic, never the data path of other connections — the
    /// pump yields between commands).
    pub fn request_persist(&self, export: NsCatalog) {
        self.send_request(CatalogRequest::new(None, export, None));
    }

    /// The one send of every catalog request: a blocking send on the
    /// bounded channel, counted once sent. No epoch is taken here: the
    /// writer numbers requests as it dequeues them (ADR-0189 D2).
    fn send_request(&self, req: CatalogRequest) {
        #[cfg(inf_canary_ns_epoch_at_send)]
        let req =
            CatalogRequest { numbered: self.numbered.fetch_add(1, Ordering::Relaxed) + 1, ..req };
        // Test builds only: a test holds this sender here to order it
        // after another (the point's row in `crate::fault`).
        if inf_foundation::fault::fire(crate::fault::DDL_REQUEST_HELD) {
            inf_foundation::fault::hold(crate::fault::DDL_REQUEST_HELD);
        }
        self.tx.send(ControlMsg::Request(req)).expect("control thread alive (fail-stop)");
        self.requests_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Catalog requests sent this boot (tests: a DDL parked on its answer
    /// has sent its request, answered or not).
    #[must_use]
    pub fn persist_requests_sent(&self) -> u64 {
        self.requests_sent.load(Ordering::Relaxed)
    }
    /// Delegates one blocking unlink to the control thread (M2-S11/S12).
    /// Non-blocking: `false` = queue full — the caller keeps the path and
    /// retries next MAINTAIN slice (bounded local queue, never a stall).
    pub fn request_unlink(&self, path: PathBuf) -> bool {
        self.tx.try_send(ControlMsg::Unlink(path)).is_ok()
    }

    /// True once the writer has answered every request numbered at or
    /// below `epoch` (the word covers it): each accepted one is durable.
    /// A teardown hold reads it for a drop's epoch (ADR-0100 D5).
    pub fn persisted(&self, epoch: u64) -> bool {
        self.word.covers(epoch)
    }

    /// The catalog word (the MAINTAIN edge-detection input, and the
    /// `catalog_epoch` gauge).
    pub fn persisted_epoch(&self) -> u64 {
        self.word.load()
    }

    /// The word a request's token reads its answer against.
    #[must_use]
    pub fn catalog_word(&self) -> &CatalogWord {
        &self.word
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

/// The catalog writer (ADR-0189 D3): `META`'s single writer and the
/// catalog epoch's single owner, with the tombstone set and the one
/// pending DDL. Both hosts (the control thread, the simulator's inline
/// inbox) drive this one value, so the simulator runs the writer the node
/// runs.
struct CatalogWriter {
    handle: Arc<ControlHandle>,
    epochs: CatalogEpochs,
    tombstones: DropTombstones,
    /// One at a time: the DDL ticket admits one program, whose notice
    /// precedes its release.
    pending: Option<PendingDdl>,
}

impl CatalogWriter {
    /// Answers one request: number it and decide; a refusal is answered
    /// with nothing written and nothing moved but the epoch and the word;
    /// an acceptance moves the state, shapes the payload from it, writes
    /// `META`, and only then answers.
    ///
    /// # Errors
    /// The `META` swap failed: the host's fail-stop. The moved state dies
    /// with the node.
    fn answer<F: inf_log::fs::SegmentFs>(
        &mut self,
        req: CatalogRequest,
        fs: &F,
        data_dir: &Path,
    ) -> std::io::Result<()> {
        #[cfg(not(inf_canary_ns_epoch_at_send))]
        let taken = self.epochs.take();
        #[cfg(inf_canary_ns_epoch_at_send)]
        let taken = self.epochs.take_numbered(req.numbered);
        let min_published = self.handle.ckpt_board().min_published();
        let CatalogRequest { intent, export, slot, .. } = req;
        let verdict = self.decide(intent.as_ref(), &export);
        match verdict {
            DdlVerdict::Accepted => {}
            DdlVerdict::Refused(_) => {
                if cfg!(inf_canary_ns_refusal_writes_meta) {
                    write_meta(fs, data_dir, &self.shape(export).encode())?;
                }
                self.epochs.answer(taken, slot, verdict);
                return Ok(());
            }
        }
        self.tombstones.retire(min_published);
        let payload = if cfg!(inf_canary_ns_intent_after_shape) {
            let payload = self.shape(export).encode();
            self.accept(intent);
            payload
        } else {
            self.accept(intent);
            self.shape(export).encode()
        };
        if cfg!(inf_canary_ns_answer_before_swap) {
            self.publish_gauges();
            self.epochs.answer(taken, slot, DdlVerdict::Accepted);
            return write_meta(fs, data_dir, &payload);
        }
        write_meta(fs, data_dir, &payload)?;
        self.publish_gauges();
        self.epochs.answer(taken, slot, DdlVerdict::Accepted);
        Ok(())
    }

    /// The verdict, pure on the writer's state and the request: the first
    /// row of ADR-0189 D3's table that holds, without the tombstone cap's
    /// rows, after a `CREATE`'s own check that its reserved id is the
    /// counter's next (ADR-0148 D4).
    fn decide(&self, intent: Option<&DdlIntent>, export: &NsCatalog) -> DdlVerdict {
        let Some(intent) = intent else { return DdlVerdict::Accepted };
        match intent {
            DdlIntent::Create { spec, reserved } => {
                let next = self.handle.next_ns_id();
                // The canaries' reservation moved the counter or issued its
                // spent value (`reserve_ns_id`): their writer commits as it.
                let not_next = reserved.0 != next && !cfg!(inf_canary_ns_id_spent_on_refusal);
                let spent = next == NS_IDS_EXHAUSTED && !cfg!(inf_canary_ns_id_wraps);
                if *reserved != spec.id || not_next || spent {
                    return DdlVerdict::Refused(DdlRefusal::IdNotNext);
                }
                if self.pending.is_some() {
                    return DdlVerdict::Refused(DdlRefusal::PendingConflict);
                }
                let taken = export
                    .entries
                    .iter()
                    .any(|e| e.name == spec.name && !self.tombstones.contains(e.id.0));
                if taken {
                    return DdlVerdict::Refused(DdlRefusal::NameExists);
                }
                DdlVerdict::Accepted
            }
            DdlIntent::Drop { id, .. } => match &self.pending {
                // A rollback replaces its own `CREATE`: never refused.
                Some(PendingDdl::Create(spec)) if spec.id.0 == *id => DdlVerdict::Accepted,
                Some(PendingDdl::Create(_) | PendingDdl::Drop(_)) => {
                    DdlVerdict::Refused(DdlRefusal::PendingConflict)
                }
                None => DdlVerdict::Accepted,
            },
        }
    }

    /// An accepted request's state change (infallible): a `CREATE` commits
    /// its id and becomes the pending DDL, a `DROP` becomes it (a rollback's
    /// `DROP` replaces its `CREATE`), and a durable drop's tombstone joins
    /// the set, unstamped.
    fn accept(&mut self, intent: Option<DdlIntent>) {
        match intent {
            None => {}
            Some(DdlIntent::Create { spec, .. }) => {
                self.commit_ns_id(spec.id);
                self.pending = Some(PendingDdl::Create(spec));
            }
            Some(DdlIntent::Drop { id, tombstone }) => {
                self.pending = Some(PendingDdl::Drop(id));
                if tombstone {
                    self.tombstones.add(id);
                }
            }
        }
    }

    /// Moves the namespace-id counter past `id`, the `CREATE` just
    /// accepted (ADR-0148 D4), before the payload is shaped, so the `META`
    /// that names `id` carries `next_id = id + 1`. `decide` refused any id
    /// that is not the counter's next or a counter with none left, so
    /// `id < u32::MAX` and the successor is representable.
    fn commit_ns_id(&self, id: NsId) {
        if cfg!(inf_canary_ns_id_spent_on_refusal) {
            return;
        }
        let next = if cfg!(inf_canary_ns_id_wraps) { id.0.wrapping_add(1) } else { id.0 + 1 };
        self.handle.next_ns_id.store(next, Ordering::Release);
    }

    /// The payload, from an export and the writer's state (ADR-0189 D3
    /// step 5; ADR-0182 D1's strip): counters cover their allocators so
    /// ids and generations never regress across restart (ADR-0015 D2,
    /// ADR-0075 D1); a pending `CREATE` joins the entries if its id is
    /// absent; a pending `DROP`'s id and every tombstoned id lose their
    /// entry and every index record naming them; `dropped` is the set.
    fn shape(&self, mut catalog: NsCatalog) -> NsCatalog {
        let handle = &self.handle;
        catalog.next_id = catalog.next_id.max(handle.next_ns_id());
        catalog.index.next_id = catalog.index.next_id.max(handle.next_index_id());
        catalog.index.next_generation =
            catalog.index.next_generation.max(handle.next_index_generation());
        let pending_drop = match &self.pending {
            Some(PendingDdl::Create(spec)) => {
                if !catalog.entries.iter().any(|e| e.id == spec.id) {
                    catalog.entries.push(spec.clone());
                }
                None
            }
            Some(PendingDdl::Drop(id)) => Some(*id),
            None => None,
        };
        let gone = |id: u32| pending_drop == Some(id) || self.tombstones.contains(id);
        catalog.entries.retain(|e| !gone(e.id.0));
        if !cfg!(inf_canary_ns_index_record_kept) {
            catalog.index.entries.retain(|r| !gone(r.ns.0));
        }
        catalog.dropped = self.tombstones.ids();
        catalog
    }

    /// A notice from a DDL's origin: it retires the pending DDL of its own
    /// kind and id (a `DropFanned` with a stamp stamps the tombstone); any
    /// other changes nothing and is counted.
    fn notice(&mut self, notice: DdlNotice) {
        let retires = match (&self.pending, notice) {
            (Some(PendingDdl::Create(spec)), DdlNotice::CreateApplied { id }) => spec.id.0 == id,
            (Some(PendingDdl::Drop(pending)), DdlNotice::DropFanned { id, .. }) => *pending == id,
            (Some(PendingDdl::Create(_)), DdlNotice::DropFanned { .. })
            | (Some(PendingDdl::Drop(_)), DdlNotice::CreateApplied { .. })
            | (None, DdlNotice::CreateApplied { .. } | DdlNotice::DropFanned { .. }) => false,
        };
        if !retires {
            self.handle.notices_stale.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.pending = None;
        match notice {
            DdlNotice::DropFanned { id, stamp: Some(epoch) } => self.tombstones.stamp(id, epoch),
            DdlNotice::DropFanned { stamp: None, .. } | DdlNotice::CreateApplied { .. } => {}
        }
        self.publish_gauges();
    }

    /// The writer's node gauges (ADR-0189 D11).
    fn publish_gauges(&self) {
        self.handle.drop_tombstones.store(self.tombstones.len(), Ordering::Relaxed);
        self.handle.ddl_pending.store(u64::from(self.pending.is_some()), Ordering::Relaxed);
    }
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
    /// The writer itself — inline, deterministic.
    writer: CatalogWriter,
    /// A request [`ControlInbox::drain_persists`] took past its bound:
    /// first in line for the next drain, so queue order holds.
    next: Option<ControlMsg>,
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
        // Unbounded in persists; bounded by what the queue holds (256
        // messages: senders block on a full queue).
        self.drain_persists(fs, data_dir, u32::MAX).map(|_| ())
    }

    /// [`drain`](Self::drain), answering at most `persists_max` catalog
    /// requests: a request past the bound stays queued, first in line for
    /// the next drain. Returns the requests answered. Tests: a harness
    /// lets one swap land and holds the next.
    ///
    /// # Errors
    /// As [`drain`](Self::drain).
    pub fn drain_persists<F: inf_log::fs::SegmentFs>(
        &mut self,
        fs: &F,
        data_dir: &Path,
        persists_max: u32,
    ) -> std::io::Result<u32> {
        let mut written = 0u32;
        while let Some(msg) = self.next.take().or_else(|| self.rx.try_recv().ok()) {
            let is_persist = match &msg {
                ControlMsg::Request(_) => true,
                ControlMsg::Notice(_) | ControlMsg::Unlink(_) => false,
            };
            if is_persist && written == persists_max {
                self.next = Some(msg);
                break;
            }
            written += u32::from(is_persist);
            match msg {
                ControlMsg::Request(req) => self.writer.answer(req, fs, data_dir)?,
                ControlMsg::Notice(notice) => self.writer.notice(notice),
                ControlMsg::Unlink(path) => {
                    let _ = fs.remove_file(&path);
                }
            }
        }
        Ok(written)
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
        let (handle, rx, writer, issuers) = ControlHandle::new_parts(seed, space, start_unix_ms);
        let inbox = ControlInbox { rx, handle: Arc::clone(&handle), writer, next: None };
        (handle, inbox, issuers)
    }

    /// The handle, the writer's inbox, the writer with its tombstone set
    /// seeded from the boot catalog, and the boot's checkpoint issuers. Every
    /// surviving tombstone is re-stamped with one fresh all-cell request
    /// (ADR-0100 D3: cells edge-detect it after recovery, and the next
    /// persist retires what they cover), paid by the control writer's
    /// quota before any cell serves (ADR-0159 A1.2).
    fn new_parts(
        seed: Option<&NsCatalog>,
        space: CkptSpace,
        start_unix_ms: u64,
    ) -> (Arc<ControlHandle>, mpsc::Receiver<ControlMsg>, CatalogWriter, CkptIssuers) {
        let cells = space.cells.get();
        let Partition { clock, owner, participants } =
            issue::partition(CkptBoard::new(space.cells), space.space);
        let next_id = seed.map_or(FIRST_NAMED_NS_ID, |c| c.next_id.max(FIRST_NAMED_NS_ID));
        let next_index_id = seed.map_or(FIRST_INDEX_ID, |c| c.index.next_id.max(FIRST_INDEX_ID));
        let next_index_generation = seed.map_or(FIRST_INDEX_GENERATION, |c| {
            c.index.next_generation.max(FIRST_INDEX_GENERATION)
        });
        let (tx, rx) = mpsc::sync_channel::<ControlMsg>(256);
        let (epochs, word) = CatalogEpochs::new();
        let handle = Arc::new(ControlHandle {
            tx,
            next_ns_id: AtomicU32::new(next_id),
            next_index_id: AtomicU32::new(next_index_id),
            next_index_generation: AtomicU64::new(next_index_generation),
            requests_sent: AtomicU64::new(0),
            #[cfg(inf_canary_ns_epoch_at_send)]
            numbered: AtomicU64::new(0),
            word,
            ckpt_clock: clock,
            recovery: Arc::new(RecoveryBoard::new(cells, start_unix_ms)),
            memory_board: Arc::new(MemoryBoard::new(cells)),
            index_board: Arc::new(IndexBoard::new(cells)),
            drop_tombstones: AtomicUsize::new(0),
            ddl_pending: AtomicU64::new(0),
            notices_stale: AtomicU64::new(0),
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
        // No DDL is pending at boot: `META` holds every answered one.
        let writer =
            CatalogWriter { handle: Arc::clone(&handle), epochs, tombstones, pending: None };
        (handle, rx, writer, CkptIssuers { host, cells: cell_issuers })
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
    let (handle, rx, writer, issuers) = ControlHandle::new_parts(seed, space, start_unix_ms);
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
                control_main(&data_dir, &board, &rx, cells, writer);
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
    board: &Arc<RecoveryBoard>,
    rx: &mpsc::Receiver<ControlMsg>,
    cells: u16,
    mut writer: CatalogWriter,
) {
    {
        let mut handle_msg = |msg: ControlMsg| match msg {
            ControlMsg::Request(req) => {
                if let Err(err) = writer.answer(req, &StdSegmentFs, data_dir) {
                    // §8.4 fail-stop: a DDL was acked against this
                    // swap. `process::exit`, never `panic!` — this
                    // thread is detached (the JoinHandle is dropped),
                    // so an unwind would kill only the control plane
                    // and leave cells serving with catalog
                    // persistence, checkpoint epochs, and delegated
                    // unlinks silently dead (the ADR-0026 D2
                    // swallowed-death class; M2.5-S16 audit fix).
                    eprintln!("FATAL: catalog META swap failed (fail-stop): {err}");
                    std::process::exit(crate::EXIT_DURABLE_FAILSTOP);
                }
            }
            ControlMsg::Notice(notice) => writer.notice(notice),
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
    use inf_store::ddl::{Answer, DdlRefusal};
    use inf_store::{IndexCatalog, NsCatalog, NsMode, NsSpec};

    use super::{
        CatalogRequest, CatalogWriter, CkptSpace, ControlHandle, DdlIntent, DdlNotice,
        DropTombstones, PendingDdl,
    };

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

    fn catalog(entries: Vec<NsSpec>) -> NsCatalog {
        NsCatalog { next_id: 64, entries, index: IndexCatalog::default(), dropped: Vec::new() }
    }

    fn writer() -> CatalogWriter {
        let cells = inf_foundation::CellCount::new(2).expect("two cells");
        let (_handle, _rx, writer, _issuers) =
            ControlHandle::new_parts(None, CkptSpace::full(cells), 0);
        writer
    }

    /// `spec`'s `CREATE`, its id reserved: the counter is set to it first,
    /// as the ticket's reservation finds it.
    fn create(w: &CatalogWriter, spec: NsSpec) -> DdlIntent {
        w.handle.next_ns_id.store(spec.id.0, std::sync::atomic::Ordering::Relaxed);
        DdlIntent::Create { reserved: spec.id, spec }
    }

    /// ADR-0100 D2/D3: add → stamp → retire once every cell published the
    /// stamped epoch; unstamped tombstones never retire; the set answers
    /// membership and its ascending ids.
    #[test]
    fn tombstones_add_stamp_retire() {
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
        assert!(t.contains(18));
        assert!(!t.contains(17));
        assert_eq!(t.ids(), vec![18, 20]);
    }

    /// ADR-0189 D3's verdict table, row by row, and the state each row
    /// leaves: a request with no change is accepted; a `CREATE` is refused
    /// while a DDL is pending and when the export holds its name (a
    /// tombstoned id's entry aside); a `DROP` of the pending `CREATE`'s id
    /// (the rollback) is accepted and replaces it; any other `DROP` is
    /// refused while a DDL is pending.
    #[test]
    fn the_writer_decides_each_row_of_its_table() {
        let mut w = writer();
        let drop_of = |id: u32| DdlIntent::Drop { id, tombstone: true };
        let export = catalog(vec![entry(16)]);
        assert_eq!(w.decide(None, &export), super::DdlVerdict::Accepted);
        let mut taken = entry(40);
        taken.name = b"ns16".to_vec();
        let name_exists = super::DdlVerdict::Refused(DdlRefusal::NameExists);
        assert_eq!(w.decide(Some(&create(&w, taken.clone())), &export), name_exists);
        w.tombstones.add(16);
        assert_eq!(
            w.decide(Some(&create(&w, taken)), &export),
            super::DdlVerdict::Accepted,
            "a tombstoned id's lagging entry holds no name"
        );
        assert_eq!(w.decide(Some(&create(&w, entry(41))), &export), super::DdlVerdict::Accepted);
        let intent = create(&w, entry(41));
        w.accept(Some(intent));
        assert_eq!(w.handle.next_ns_id(), 42, "the acceptance commits the id");
        let conflict = super::DdlVerdict::Refused(DdlRefusal::PendingConflict);
        assert_eq!(w.decide(Some(&create(&w, entry(42))), &export), conflict);
        assert_eq!(w.decide(Some(&drop_of(42)), &export), conflict);
        assert_eq!(
            w.decide(Some(&drop_of(41)), &export),
            super::DdlVerdict::Accepted,
            "the rollback's drop of its own pending CREATE"
        );
        w.accept(Some(drop_of(41)));
        assert!(matches!(w.pending, Some(PendingDdl::Drop(41))), "the drop replaced it");
        assert!(w.tombstones.contains(41));
        assert_eq!(w.decide(Some(&drop_of(41)), &export), conflict);
    }

    /// ADR-0148 D4 at the writer: a `CREATE` whose reserved id is not the
    /// counter's next (a second `CREATE` on one reservation, a reservation
    /// whose spec names another id), or a counter with no id left, is
    /// refused before any other row, and commits nothing.
    #[test]
    fn a_create_whose_id_is_not_the_counter_next_is_refused() {
        let w = writer();
        let export = catalog(Vec::new());
        let not_next = super::DdlVerdict::Refused(DdlRefusal::IdNotNext);
        let next = w.handle.next_ns_id();
        let stale = DdlIntent::Create { reserved: entry(next - 1).id, spec: entry(next - 1) };
        assert_eq!(w.decide(Some(&stale), &export), not_next, "an id below the counter");
        let ahead = DdlIntent::Create { reserved: entry(next + 1).id, spec: entry(next + 1) };
        assert_eq!(w.decide(Some(&ahead), &export), not_next, "an id past the counter");
        let mismatched = DdlIntent::Create { reserved: entry(next).id, spec: entry(next + 1) };
        assert_eq!(w.decide(Some(&mismatched), &export), not_next, "a spec of another id");
        let exhausted = create(&w, entry(super::NS_IDS_EXHAUSTED));
        assert_eq!(w.decide(Some(&exhausted), &export), not_next, "a counter with none left");
        assert_eq!(w.handle.next_ns_id(), super::NS_IDS_EXHAUSTED, "nothing committed");
    }

    /// A notice retires the pending DDL of its own kind and id and nothing
    /// else: a stale one changes nothing and is counted.
    #[test]
    fn a_notice_retires_only_its_own_kind_and_id() {
        let mut w = writer();
        let intent = create(&w, entry(40));
        w.accept(Some(intent));
        w.notice(DdlNotice::DropFanned { id: 40, stamp: Some(7) });
        w.notice(DdlNotice::CreateApplied { id: 41 });
        assert_eq!(w.handle.ddl_notices_stale(), 2);
        assert!(matches!(&w.pending, Some(PendingDdl::Create(spec)) if spec.id.0 == 40));
        w.notice(DdlNotice::CreateApplied { id: 40 });
        assert!(w.pending.is_none());
        assert_eq!(w.handle.ddl_pending(), 0);
        w.accept(Some(DdlIntent::Drop { id: 40, tombstone: true }));
        w.notice(DdlNotice::DropFanned { id: 40, stamp: Some(7) });
        assert!(w.pending.is_none());
        assert_eq!(w.tombstones.entries, vec![(40, Some(7))], "the notice's stamp");
        w.notice(DdlNotice::CreateApplied { id: 40 });
        assert_eq!(w.handle.ddl_notices_stale(), 3, "none pending: stale");
    }

    /// A refused request writes nothing and moves nothing but the epoch
    /// and the word: the tombstone set keeps a tombstone every cell has
    /// published past, and the token reads the refusal. The next accepted
    /// request retires that tombstone, writes `META`, and its token reads
    /// its receipt.
    #[test]
    #[allow(clippy::disallowed_methods, reason = "test-only: the writer's scratch directory")]
    fn a_refusal_moves_nothing_but_the_epoch_and_the_word() {
        let dir = std::env::temp_dir().join(format!("inf-ctl-refusal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut w = writer();
        let handle = std::sync::Arc::clone(&w.handle);
        // A stamped tombstone the board's fold covers: any accepted request
        // retires it.
        w.tombstones.add(18);
        w.tombstones.stamp(18, 3);
        let board = handle.ckpt_board();
        (0..2).for_each(|cell| board.slot(cell).publish(3, 1, 1));
        assert_eq!(board.min_published(), 3, "every cell published past the stamp");
        let (slot, requested) =
            inf_store::ddl::answer_slot::<inf_store::ddl::CreatePersisted>(entry(40).id);
        let mut taken = entry(40);
        taken.name = b"ns16".to_vec();
        let req =
            CatalogRequest::new(Some(create(&w, taken)), catalog(vec![entry(16)]), Some(slot));
        w.answer(req, &inf_log::fs::StdSegmentFs, &dir).expect("a refusal writes nothing");
        assert!(!dir.join("META").exists() && !dir.join("META.new").exists(), "no META write");
        assert!(w.pending.is_none());
        assert_eq!(handle.next_ns_id(), 40, "the refusal committed no id");
        assert_eq!(w.tombstones.entries, vec![(18, Some(3))], "the refusal retired nothing");
        assert_eq!(handle.persisted_epoch(), 1, "the word moved");
        assert!(matches!(
            requested.answer(handle.catalog_word()),
            Answer::Refused(DdlRefusal::NameExists)
        ));
        let (slot, requested) =
            inf_store::ddl::answer_slot::<inf_store::ddl::CreatePersisted>(entry(41).id);
        // The export a program sends carries the counter it reserved from.
        let mut export = catalog(vec![entry(16)]);
        export.next_id = 41;
        let req = CatalogRequest::new(Some(create(&w, entry(41))), export, Some(slot));
        w.answer(req, &inf_log::fs::StdSegmentFs, &dir).expect("the META write");
        assert!(dir.join("META").exists(), "an acceptance writes META");
        assert_eq!(handle.next_ns_id(), 42, "the acceptance committed its id");
        let written = super::load_catalog(&dir).expect("META decodes").expect("META");
        assert_eq!(written.next_id, 42, "the META that names id 41 carries its successor");
        assert!(w.tombstones.entries.is_empty(), "the acceptance retired the covered tombstone");
        assert_eq!(handle.persisted_epoch(), 2);
        match requested.answer(handle.catalog_word()) {
            Answer::Accepted(receipt) => {
                assert_eq!(receipt.id(), entry(41).id);
                assert_eq!(receipt.fan_epoch(), 2, "the receipt carries its own epoch");
            }
            Answer::Pending(_) | Answer::Refused(_) => panic!("an accepted request's receipt"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroU64;

    use inf_foundation::time::Nanos;
    use inf_foundation::{CellCount, CellId, SLOT_COUNT};
    use inf_store::{Keyspace, StoreConfig};
    use inf_wire::{Protocol, ReplyStream, RespWriter};

    use super::{
        BoardObservation, BoardSweep, CkptBoard, CkptSlot, CkptSpace, CkptTarget, ControlHandle,
        LastSave, SweepStep,
    };
    use crate::exec::NodeInfo;

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
            match board.sweep_step(sweep, &LastSave::default()) {
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
        let mut sweep = BoardSweep::new(CellId(0));
        assert_eq!(
            board.sweep_step(&mut sweep, &LastSave::default()),
            SweepStep::Progressed,
            "the wake fires"
        );
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
        let mut sweep = BoardSweep::new(CellId(0));
        let mut turns = 0u32;
        loop {
            let before = sweep.visits();
            let step = board.sweep_step(&mut sweep, &LastSave::default());
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
            board.sweep_step(&mut sweep, &LastSave::default());
        }
        board.slot(3).publish(7, 1, 1_000);
        let seen_within = observed_within(board, &mut sweep, 512, |board, sweep| {
            board.sweep_step(sweep, &LastSave::default());
        });
        assert!(seen_within.is_some(), "an advance is observed within two sweeps");
    }

    /// Canary: a stalled observer. The liveness counter stays flat and
    /// the two-sweep oracle goes red.
    #[test]
    fn canary_a_stalled_sweep_is_caught() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(SLOT_COUNT), 0);
        let board = handle.ckpt_board();
        let mut sweep = BoardSweep::new(CellId(0));
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
            let mut sweep = BoardSweep::new(CellId(0));
            let (steps, _) = complete_sweep(board, &mut sweep);
            assert_eq!(steps, 1, "one turn at {count} cells");
            let (min, sum, max) = old_fold(board);
            let observed = sweep.observed();
            assert_eq!(observed.min_published, min);
            assert_eq!(observed.published_sum, u128::from(sum));
            assert_eq!(observed.max_unix_ms, max);
        }
    }

    /// The park guard's own-slot term through the product's own methods
    /// (`interfaces-m2.md`, "Cells never fold the whole board"): a
    /// publication of the sweep's own cell stays unobserved until a sweep
    /// that began after it completes, and no longer. `S = ceil(N / 64)`
    /// steps make a sweep, from one step (`N` = 1, 64) through three
    /// (`N` = 130) to the topology limit's 256, and the own cell is the
    /// board's first slot, its last, and slots on both sides of the cursor
    /// (63, 64, 100). The expected turns are the test's own count of where
    /// it published: `S` when the next step starts a sweep, else the rest
    /// of the sweep in progress plus `S`, never above `2 * S`. Planted,
    /// each must fail this test: `inf_canary_ckpt_own_seen_not_advanced`,
    /// and `inf_canary_ckpt_own_loaded_at_cursor` (the own slot loaded as
    /// the cursor passes it, which ends the term over a peer the sweep
    /// read before that peer published).
    #[test]
    fn an_own_publication_is_unobserved_until_a_sweep_begun_after_it_completes() {
        let mut ahead_of_the_cursor = 0;
        for count in [1u16, 64, 65, 130, SLOT_COUNT] {
            let sweep_turns = u32::from(count).div_ceil(64);
            for me in [0u16, 63, 64, 100, count - 1] {
                if me >= count {
                    continue;
                }
                let mut worst = 0;
                for steps_in in [0, 1, sweep_turns - 1] {
                    if steps_in >= sweep_turns {
                        continue; // a one-step sweep is never part-way
                    }
                    let turns = turns_until_an_own_publication_is_observed(count, me, steps_in);
                    let expected = if steps_in == 0 {
                        sweep_turns
                    } else {
                        (sweep_turns - steps_in) + sweep_turns
                    };
                    let case = format!("{count} cells, cell {me}, published {steps_in} steps in");
                    assert_eq!(turns, expected, "{case}");
                    worst = worst.max(turns);
                    ahead_of_the_cursor +=
                        u32::from(steps_in > 0 && u32::from(me) >= steps_in * 64);
                }
                assert!(worst <= 2 * sweep_turns, "{count} cells, cell {me}: {worst} turns");
            }
        }
        // Engagement: only a slot the sweep in progress has yet to read
        // separates a load at the sweep's start from one at the cursor.
        assert!(ahead_of_the_cursor > 0, "VACUOUS: no own slot lay ahead of a part-way cursor");
    }

    /// Sweep steps from a publication of cell `me`, made `steps_in` steps
    /// into a sweep of a `count`-cell board, until `own_unobserved` ends. A
    /// peer publishes just before `me` does, in a slot the sweep in
    /// progress already read (any other slot when the sweep is at its
    /// start): the observation that ends the term must carry both.
    fn turns_until_an_own_publication_is_observed(count: u16, me: u16, steps_in: u32) -> u32 {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(count), 0);
        let board = handle.ckpt_board();
        let lastsave = LastSave::default();
        let mut sweep = BoardSweep::new(CellId(me));
        assert!(!board.own_unobserved(&sweep), "nothing is published yet");
        let sweep_turns = complete_sweep(board, &mut sweep).0;
        for _ in 0..steps_in {
            assert_eq!(board.sweep_step(&mut sweep, &lastsave), SweepStep::InProgress);
        }
        // Slots 0 and 1 are behind every part-way cursor (64 and above).
        let peer_epoch = if count > 1 {
            let peer = if me == 0 { 1 } else { 0 };
            board.slot(peer).publish(5, 1, 1_000);
            5
        } else {
            0
        };
        assert!(!board.own_unobserved(&sweep), "a peer's slot is not slot {me}");
        board.slot(me).publish(7, 2, 2_000);
        let mut turns = 0;
        while board.own_unobserved(&sweep) {
            assert!(turns < 2 * sweep_turns, "the own-slot term never ended at {count} cells");
            board.sweep_step(&mut sweep, &lastsave);
            turns += 1;
        }
        // The observation that ended the term read every slot after the
        // publication: it carries both epochs.
        assert_eq!(
            sweep.observed().published_sum,
            peer_epoch + 7,
            "the term ended over a slot read before its publication ({count} cells, cell {me}, \
             {steps_in} steps in, {turns} turns)"
        );
        assert!(!sweep.in_progress(), "the term ends as a sweep completes");
        turns
    }

    /// `own_unobserved` is total: a sweep built for a cell the board does
    /// not hold answers `false` and still sweeps (`set_control` refuses to
    /// wire such a pair).
    #[test]
    fn a_sweep_of_a_cell_off_the_board_has_no_own_slot_term() {
        let (handle, _inbox, _issuers) = ControlHandle::detached(cells(2), 0);
        let board = handle.ckpt_board();
        let mut sweep = BoardSweep::new(CellId(2));
        board.slot(0).publish(3, 1, 1_000);
        board.slot(1).publish(4, 1, 1_000);
        assert!(!board.own_unobserved(&sweep));
        assert_eq!(board.sweep_step(&mut sweep, &LastSave::default()), SweepStep::Progressed);
        assert!(!board.own_unobserved(&sweep));
        assert_eq!(sweep.observed().min_published, 3);
    }

    /// ADR-0159 A1.4 through the product's own step: with this cell's
    /// sweep stalled (it never read the board), the `WAIT CELL 1`
    /// confirmation raises its `LASTSAVE` floor to slot 1's publication
    /// second. `wait_for_ckpt` calls exactly this method and `LASTSAVE`
    /// reads exactly this `LastSave`; the planted
    /// `inf_canary_lastsave_floor_skipped` (the confirmation without its
    /// raise) must fail this test.
    #[test]
    fn lastsave_after_wait_cell_covers_the_fenced_checkpoint() {
        let (handle, _inbox, issuers) = ControlHandle::detached(cells(2), 0);
        let mut quota = issuers.cells.into_iter().next().expect("cell 0").quota;
        let epoch = quota.request(CkptTarget::Cell(CellId(1))).expect("a unit");
        let board = handle.ckpt_board();
        assert_eq!(board.requested(1), epoch.get());
        assert_eq!(board.requested(0), 0, "a targeted request reaches one slot");
        let mut sweep = BoardSweep::new(CellId(0));
        let lastsave = LastSave::default();
        assert!(!sweep.confirm_cell_wait(board.slot(1), epoch, &lastsave), "not yet published");
        let published_ms = 1_700_000_123_456;
        board.slot(1).publish(epoch.get(), 9, published_ms);
        assert!(sweep.confirm_cell_wait(board.slot(1), epoch, &lastsave), "slot 1 published");
        // Engagement: the observation alone would answer 0.
        assert_eq!(sweep.observed(), BoardObservation::default(), "the sweep trails the board");
        let answered = lastsave.unix_s();
        assert!(answered >= published_ms / 1000, "LASTSAVE {answered} trails the WAIT");
        // The floor only rises: an older publication it confirms leaves it.
        board.slot(0).publish(epoch.get(), 1, 1_000);
        assert!(sweep.confirm_cell_wait(board.slot(0), epoch, &lastsave));
        assert_eq!(lastsave.unix_s(), answered);
    }

    /// `rdb_last_save_time` of the product's `INFO persistence` for `node`.
    fn info_rdb_last_save_time(node: &NodeInfo) -> u64 {
        let keyspace = Keyspace::new(StoreConfig::default());
        let argv: [&[u8]; 2] = [b"INFO", b"persistence"];
        let mut reply = ReplyStream::new(Vec::new());
        let mut writer = RespWriter::new(reply.open(), Protocol::Resp2);
        crate::admin::info(&argv[..], &keyspace, node, Nanos(1), &mut writer);
        let text = String::from_utf8(reply.into_vec()).expect("INFO is text");
        text.lines()
            .find_map(|line| line.strip_prefix("rdb_last_save_time:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("no rdb_last_save_time in {text:?}"))
    }

    /// One `LASTSAVE` value per cell (`interfaces-m2.md`, "Cells never fold
    /// the whole board"): after a `WAIT CELL 1` confirmed with the sweep
    /// stalled on an older observation, the value the `LASTSAVE` command
    /// answers (`node.lastsave`) and the
    /// `rdb_last_save_time` the product's INFO renders are the same second,
    /// at or after slot 1's publication second, which the test reads from
    /// the board. Planted, each must fail this test: the confirmation
    /// without its write of the cell's value
    /// (`inf_canary_lastsave_cell_stale`), and an INFO gauge that renders
    /// the sweep's term alone (`inf_canary_info_lastsave_from_observation`).
    #[test]
    fn lastsave_and_the_info_gauge_answer_one_value_after_a_wait_cell() {
        let (handle, _inbox, issuers) = ControlHandle::detached(cells(2), 0);
        let mut quota = issuers.cells.into_iter().next().expect("cell 0").quota;
        let board = handle.ckpt_board();
        let node = NodeInfo::try_default().expect("fixture cache allocation");
        let mut sweep = BoardSweep::new(CellId(0));
        assert_eq!((node.lastsave.unix_s(), info_rdb_last_save_time(&node)), (0, 0));
        // One completed sweep over an older publication: both surfaces
        // answer its second.
        let older_ms = 1_700_000_100_000;
        board.slot(0).publish(1, 1, older_ms);
        assert_eq!(board.sweep_step(&mut sweep, &node.lastsave), SweepStep::Progressed);
        assert_eq!(node.lastsave.unix_s(), older_ms / 1000);
        assert_eq!(info_rdb_last_save_time(&node), older_ms / 1000);
        // The sweep stalls there; slot 1 publishes and the WAIT confirms.
        let epoch = quota.request(CkptTarget::Cell(CellId(1))).expect("a unit");
        board.slot(1).publish(epoch.get(), 9, 1_700_000_123_456);
        assert!(sweep.confirm_cell_wait(board.slot(1), epoch, &node.lastsave));
        // Engagement: the observation still holds the older second.
        assert_eq!(sweep.observed().max_unix_ms, older_ms, "the sweep trails the board");
        let fenced_s = board.slot(1).last_unix_ms() / 1000;
        let command = node.lastsave.unix_s();
        let info = info_rdb_last_save_time(&node);
        assert!(command >= fenced_s, "LASTSAVE {command} is below slot 1's second {fenced_s}");
        assert_eq!(info, command, "rdb_last_save_time and LASTSAVE differ on one cell");
        // The next completed sweep changes neither: it reads the same slot.
        assert_eq!(board.sweep_step(&mut sweep, &node.lastsave), SweepStep::Progressed);
        assert_eq!((node.lastsave.unix_s(), info_rdb_last_save_time(&node)), (fenced_s, fenced_s));
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
