//! Group commit (M2-S05/S06, ADR-0013): the cell-local policy engine and
//! fsync ledger behind the reactor's LOG step. One frame write per
//! iteration (L3), one fdatasync covering every durability class due that
//! iteration (§8.2 group commit), and a durability watermark that advances
//! **only** on fsync completions (L2) — the value `always` acks gate on
//! (L6, via `Lsn::to_u64` → `WatermarkGate`).
//!
//! [`GroupCommit`] never names the driver, ops, or sockets — it decides
//! *what is due* and accounts for *what completed*; the plane (a test
//! harness today, `inf-server`'s cell from M2-S08) translates decisions
//! into `IoOp::LogWrite`/`IoOp::Fdatasync` and routes completions back.
//! Choreography per iteration:
//!
//! ```text
//! EXECUTE   stage(effect) → note_staged(class)         (always ⇒ sync due)
//! on_timer  note_everysec_tick()                       (injected wheel, L7)
//! LOG       staging.can_seal()?
//!             ├─ rotor.begin_frame_deferred → (slot, seal?)
//!             ├─ seal? → register_seal_fsync(handoff) → IoOp::Fdatasync
//!             ├─ lease = staging.seal(slot.first_record_lsn())
//!             ├─ note_frame_queued(end, len); frame_fsync_due()?
//!             │    └─ register_linked_fsync() → LogWrite{fsync_token}
//!             └─ rotor.commit_frame_queued(slot)
//!           else standalone_fsync_due()? → register_standalone_fsync()
//! REAP      LogWritten → note_frame_written(id); staging.release(lease)
//!           Synced     → on_fsync_complete → Some(end) ⇒ gate.advance(end)
//! ```
//!
//! **K frames in flight (M4.5-S35, ADR-0087).** The staging ring may hold
//! up to K sealed frames awaiting `LogWritten`, and completions arrive in
//! any order. The ledger therefore keeps a bounded FIFO of queued frames
//! and advances `written_up_to` over the **completion-ordered prefix**
//! (ADR-0087 D2) — a later frame landing first advances nothing. The LOG
//! step asks [`GroupCommit::frame_plan`] which barrier the next frame may
//! carry (ADR-0087 D3): write-through under the prefix rule, a linked
//! fdatasync only when every earlier write has completed (`IO_LINK`
//! orders the sync after *this* frame's write alone), and — when a sync
//! is due but neither is admissible because frames are still in flight
//! below — the frame **waits** rather than accumulating behind
//! barrier-less frames (a livelock under load). At K = 1 the wait is
//! unreachable and every decision is the pre-S35 one.
//!
//! ## Coverage rules (ADR-0013 D2/D3 — each is load-bearing)
//!
//! - A **linked** fsync covers its frame's exclusive end: the kernel orders
//!   it after the write (`IOSQE_IO_LINK`; fallback tiers sync after the
//!   write completes).
//! - A **seal** fsync covers the sealed segment's exclusive end: rotation
//!   happens only while no write is in flight (the staging lease serializes
//!   writes), so the segment is complete at registration — asserted here.
//! - A **standalone** fsync (everysec tick with no frame) covers
//!   `written_up_to` at *submission* — never `queued_up_to`: an fdatasync
//!   races writes still in flight and may not include them.
//!
//! Completions may arrive out of order across fds (seal fsync on the old
//! segment vs linked fsync on the new); ledger entries are
//! submission-ordered with monotone coverage, and the watermark advances to
//! the covers value of the longest **done prefix**.
//!
//! **Write-through frames** (M4.5-S34, ADR-0086 D5): on a pre-zeroed
//! `O_DIRECT` segment an `always`-due frame is written `RWF_DSYNC` and is
//! durable at its own `LogWritten`. The plane registers a
//! [`SyncReason::WriteThrough`] ticket at seal and completes it from the
//! write's completion — the same ledger, the same done-prefix rule, so a
//! FUA frame completing ahead of an earlier FLUSH-class entry advances
//! nothing until that entry lands. Write-through tickets do **not** count
//! toward the sync-pipeline bound (ADR-0022 D3 re-scoped): they never
//! queue on the device's flush unit, and the one-frame staging lease
//! already bounds them at one per cell.
//!
//! **The prefix rule (load-bearing).** A FUA write persists *its own
//! bytes*; an fdatasync persists the whole file. A write-through ticket
//! may therefore claim coverage up to its frame's end **only if the frame
//! extends the durable prefix** — every byte below its base is already
//! durable or covered by a pending FLUSH-class entry ahead of it in the
//! ledger. An `everysec`-only frame written with no barrier breaks that
//! prefix; the next `always`-due frame then takes the linked fdatasync,
//! which covers the gap. Found by the m2-durable sweep on the first
//! `Direct` run (seed 0xd5ee00a7: an acked DEL replayed as its
//! predecessor because the plain frame before it was lost to the cut).
//!
//! The pending set is bounded by construction: at most K writes (and so
//! K write-through tickets, or one linked sync — a linked sync needs the
//! pipeline drained) are in flight, standalone syncs dedupe against the
//! ledger tail, and seal/zero-fill syncs arrive at segment cadence.
//!
//! **The reorder window (ADR-0087 D2 as amended, 2026-08-22).** The
//! staging ring bounds *unwritten* frames at K, but a frame that landed
//! behind an earlier one still in flight stays in the queue until the
//! prefix reaches it — and its released buffer lets the next frame in.
//! One wedged plain write at the front with barrier-less frames landing
//! behind it would therefore grow the queue without bound (the review of
//! `2cb6074`: memory, a linear completion search, eventually the cell).
//! The queue is bounded at [`REORDER_WINDOW_FRAMES`] by construction:
//! [`GroupCommit::frame_plan`] answers `Wait` while the window is full,
//! so the next frame holds until the front lands (≤ one write latency —
//! the same bound the barrier `Wait` already carries), the staging
//! builder absorbs the hold, and the existing staging backpressure
//! (admission parking, M4.5-S27) takes over from there. Release-asserted
//! at the queue; frames are found by ordinal arithmetic, never a scan.

use core::fmt;
use std::collections::VecDeque;

use inf_foundation::LogHistogram;
use inf_foundation::time::Nanos;

use crate::fs::SegmentFile;
use crate::lsn::Lsn;
use crate::segment::SealHandoff;
use crate::staging::MAX_FRAMES_IN_FLIGHT;

/// The most frames the completion ledger holds behind the written prefix
/// — unwritten ones plus those that landed ahead of an earlier one still
/// in flight (ADR-0087 D2 as amended). Twice the ring's in-flight cap:
/// one late write with a full pipeline landing behind it, twice over,
/// reorders without a hold; beyond that the next frame waits for the
/// front. Fixed, independent of the configured K, so the ledger's memory
/// is a constant (16 × 32 B) and never a function of device behaviour.
pub const REORDER_WINDOW_FRAMES: usize = 2 * MAX_FRAMES_IN_FLIGHT as usize;

/// Durability class of a staged effect's namespace (§8.2). `memory`
/// namespaces never reach the log, so they have no representation here —
/// zero cost by construction (M2-S09).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FsyncClass {
    /// Ack on apply; group commit each loop; fsync on the 1 s timer tick
    /// (and on segment seal). Loss window ≤ 1 s.
    Everysec,
    /// Ack **after** fsync: the response future gates on the watermark.
    Always,
}

/// Why an fsync was submitted (observability + tests).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SyncReason {
    /// Chained behind this iteration's frame write.
    Linked,
    /// Deferred segment seal (ADR-0013 D4).
    Seal,
    /// everysec tick with dirty bytes and no frame this iteration.
    Standalone,
    /// Issued at an fsync-completion CQE under the bounded sync pipeline
    /// (M2.5-S07): covers accumulated written dues immediately instead of
    /// waiting for the next LOG step, keeping the device's flush queue
    /// primed. Only fires when a slot remains reserved for the LOG step's
    /// linked sync (`flush_bound ≥ 2`).
    Completion,
    /// Boot-metadata barrier (M2.5-S01): a driver-ridden fdatasync on a
    /// boot directory handle or the fresh segment fd, registered at the
    /// head of the ledger so the done-prefix rule fences every durable
    /// ack behind boot-metadata durability — while boot-ready never
    /// blocks on the device.
    BootBarrier,
    /// Prealloc-metadata barrier (M2.5-S01): the deferred next-segment
    /// prealloc's log-dir fdatasync, registered **coverage-neutral** at
    /// the ledger tail (a dir sync promises no log-data coverage) so acks
    /// into the new segment are fenced behind its directory entry being
    /// durable — without a blocking sync_dir on the reactor.
    PreallocBarrier,
    /// Blob-extent seal barrier (M4-S17/S26, ADR-0061 D3): the extent's
    /// fdatasync registered **before** the referencing frame's linked
    /// fsync — the done-prefix watermark rule then fences the ack
    /// mechanically; `on_fsync_error`'s freeze covers the failure half.
    ExtentSeal,
    /// A write-through (FUA-class) frame (M4.5-S34, ADR-0086 D5): the
    /// frame's own `RWF_DSYNC` write is the barrier; the ticket completes
    /// at `LogWritten`. Does not occupy a sync-pipeline slot.
    WriteThrough,
    /// Zero-fill barrier (ADR-0086 D4): the fdatasync that commits a
    /// pre-zeroed next segment's extent metadata before any frame lands
    /// in it. Coverage-neutral like the prealloc dir barrier.
    ZeroFill,
}

/// Ledger key for one submitted fsync. The plane maps tickets onto
/// completion tokens (`inf-log` never names the token type).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FsyncTicket(u64);

impl FsyncTicket {
    /// Raw value for the plane's token packing.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Rebuild from the plane's token unpacking.
    #[must_use]
    pub fn from_u64(raw: u64) -> FsyncTicket {
        FsyncTicket(raw)
    }
}

/// Ordinal of a queued frame (ADR-0087 D2): returned by
/// [`GroupCommit::note_frame_queued`], presented back at
/// [`GroupCommit::note_frame_written`]. Equals the plane's write-token
/// sequence so a `LogWritten` completion maps to its frame without a
/// lookup table.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FrameId(pub u64);

/// The barrier class the next frame may carry, or the instruction to hold
/// it (ADR-0087 D3). Decided by [`GroupCommit::frame_plan`] before the
/// seal, so a frame is never sealed into a barrier it cannot have.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FramePlan {
    /// Seal and write with no barrier (no sync due, or the FLUSH slot is
    /// busy and the due accumulates — §8.2 batching).
    Plain,
    /// Seal and write `RWF_DSYNC`; register the ticket with
    /// [`GroupCommit::register_write_through`].
    WriteThrough,
    /// Seal and chain an fdatasync; register it with
    /// [`GroupCommit::register_linked_fsync`].
    LinkedFsync,
    /// A sync is due, write-through is not admissible, and frames are
    /// still in flight below this one: hold the frame until the pipeline
    /// drains (≤ one write latency).
    Wait,
}

/// Cumulative commit counters (cell-local, no atomics — L1). The frozen
/// S21 counter set grows from these names.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    /// Frames handed to the driver — the `log_writes_per_iter` numerator
    /// (tripwire: exactly one per LOG step that sealed).
    pub frames_queued: u64,
    pub frame_bytes_queued: u64,
    pub fsyncs_linked: u64,
    pub fsyncs_seal: u64,
    pub fsyncs_standalone: u64,
    /// Completion-CQE-issued syncs under the bounded pipeline (M2.5-S07).
    pub fsyncs_completion: u64,
    /// Boot-metadata barriers registered at enable-time (M2.5-S01).
    pub fsyncs_boot_barrier: u64,
    /// Deferred-prealloc dir barriers (M2.5-S01).
    pub fsyncs_prealloc_barrier: u64,
    /// Write-through (FUA-class) frame tickets (M4.5-S34, ADR-0086 D5) —
    /// the `fsyncs_fua` observable.
    pub fsyncs_write_through: u64,
    /// Zero-fill barriers (ADR-0086 D4) — one per pre-zeroed segment.
    pub fsyncs_zero_fill: u64,
    pub fsyncs_completed: u64,
    /// everysec ticks that found nothing dirty (proves idle ticks are free).
    pub idle_ticks: u64,
}

/// A file handle the ledger keeps alive until its fsync completes.
enum HeldHandle<File> {
    None,
    /// Deferred-seal write handle: dropped when the covering sync
    /// completes — seal durability and handle drop coincide (ADR-0013 D4).
    Seal(SealHandoff<File>),
    /// Boot-barrier directory handle (M2.5-S01): the fd the barrier
    /// fdatasync targets must stay open until its `Synced` arrives.
    Dir(File),
    /// Sealed blob extent's file handle (ADR-0061 D3): held open until
    /// the barrier's `Synced` — the token was constructed deferred, so
    /// this fdatasync is the extent's durability act.
    Extent(File),
}

/// One frame handed to the driver and not yet known written (ADR-0087
/// D2). `end_bytes` is the frame's exclusive end in ledger-byte space.
struct QueuedFrame {
    id: FrameId,
    end: Lsn,
    end_bytes: u64,
    written: bool,
}

struct PendingFsync<File> {
    ticket: u64,
    covers_up_to: Lsn,
    covers_bytes: u64,
    submitted_at: Nanos,
    reason: SyncReason,
    done: bool,
    /// A failed fsync freezes the watermark at the previous entry forever
    /// (§8.4 — the cell is fail-stopping; nothing past it may be claimed).
    failed: bool,
    held: HeldHandle<File>,
}

/// The group-commit engine of one cell. Single-threaded by design (L1);
/// time is injected (`now` parameters — L7); generic over the segment-file
/// tier only to hold [`SealHandoff`]s until their sync completes.
pub struct GroupCommit<File> {
    /// An fsync is due this iteration (everysec tick fired, or `always`
    /// traffic staged). Cleared by the registration that covers it.
    sync_due: bool,
    always_pending: bool,
    /// An `always` record was staged after the last frame queued — it
    /// still sits in the staging ring, so no standalone sync can cover it
    /// yet (M2.5-S07 completion-issue discharge check).
    always_unqueued: bool,
    /// Exclusive end of the last queued frame carrying `always` records:
    /// a standalone covering ≥ this discharges `always_pending`.
    always_queued_up_to: Option<Lsn>,
    /// FLUSH-class durability syncs allowed in flight at once (ADR-0022
    /// D3): 1 = the shipped discipline; 2 = the measured two-in-flight
    /// arm (M2.5-S07; never more — a queue is the batch=1.0 disease
    /// reborn). Write-through tickets never count (ADR-0086 D5).
    flush_bound: usize,
    queued_up_to: Option<Lsn>,
    queued_bytes: u64,
    /// Frames queued and not yet part of the written prefix, in queue
    /// order (ADR-0087 D2 as amended): the unwritten ones plus those that
    /// landed ahead of an earlier one still in flight. Bounded at
    /// `REORDER_WINDOW_FRAMES` by `frame_plan`'s `Wait`, release-asserted
    /// at the queue; ids are consecutive ordinals, so a frame's index is
    /// `id − front.id` (O(1) completion routing, never a scan).
    queued: VecDeque<QueuedFrame>,
    /// Queued frames without their `LogWritten` yet — bounded by the
    /// staging ring's in-flight slots (`MAX_FRAMES_IN_FLIGHT`).
    unwritten: u8,
    next_frame_id: u64,
    /// Exclusive end of the completion-ordered written prefix: every
    /// frame below it has its `LogWritten`. What an fdatasync can cover.
    written_up_to: Option<Lsn>,
    written_bytes: u64,
    durable_up_to: Option<Lsn>,
    durable_bytes: u64,
    pending: VecDeque<PendingFsync<File>>,
    next_ticket: u64,
    /// Every durability barrier's latency (all classes) — `fsync_latency_*`.
    fsync_hist_us: LogHistogram,
    /// Write-through frames only (ADR-0086 D5/D7) — `fua_latency_*`, the
    /// `barrier_class_degraded` tripwire's input.
    write_through_hist_us: LogHistogram,
    stats: CommitStats,
}

impl<File> fmt::Debug for GroupCommit<File> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupCommit")
            .field("sync_due", &self.sync_due)
            .field("always_pending", &self.always_pending)
            .field("queued_up_to", &self.queued_up_to)
            .field("written_up_to", &self.written_up_to)
            .field("durable_up_to", &self.durable_up_to)
            .field("pending_fsyncs", &self.pending.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl<File: SegmentFile> Default for GroupCommit<File> {
    fn default() -> Self {
        Self::new()
    }
}

impl<File: SegmentFile> GroupCommit<File> {
    #[must_use]
    pub fn new() -> GroupCommit<File> {
        GroupCommit::with_flush_bound(1)
    }

    /// `bound` FLUSH-class durability syncs may be in flight at once
    /// (ADR-0022 D3; the two-in-flight arm is M2.5-S07's measured shape,
    /// reachable by construction here — `--sync-pipeline` retired,
    /// ADR-0087 D5).
    ///
    /// # Panics
    /// Outside `1..=2` — the pipeline is bounded by construction, never a
    /// queue.
    #[must_use]
    pub fn with_flush_bound(bound: usize) -> GroupCommit<File> {
        assert!((1..=2).contains(&bound), "FLUSH-class bound is 1 or 2, never a queue");
        GroupCommit {
            sync_due: false,
            always_pending: false,
            always_unqueued: false,
            always_queued_up_to: None,
            flush_bound: bound,
            queued_up_to: None,
            queued_bytes: 0,
            queued: VecDeque::with_capacity(REORDER_WINDOW_FRAMES),
            unwritten: 0,
            next_frame_id: 0,
            written_up_to: None,
            written_bytes: 0,
            durable_up_to: None,
            durable_bytes: 0,
            pending: VecDeque::new(),
            next_ticket: 0,
            fsync_hist_us: LogHistogram::new(),
            write_through_hist_us: LogHistogram::new(),
            stats: CommitStats::default(),
        }
    }

    // ---- EXECUTE / timer inputs --------------------------------------------

    /// A durable effect was staged. `Always` traffic makes this iteration's
    /// frame carry a linked fsync (the ack gates on it); `Everysec` records
    /// ride the timer — the fast path pays nothing extra here.
    pub fn note_staged(&mut self, class: FsyncClass) {
        if class == FsyncClass::Always {
            self.always_pending = true;
            self.always_unqueued = true;
            self.sync_due = true;
        }
    }

    /// The everysec timer fired (plane-armed on the injected wheel — L7).
    /// Idle ticks (nothing dirty, nothing pending it) are counted and cost
    /// nothing.
    pub fn note_everysec_tick(&mut self) {
        if self.queued_bytes == self.durable_bytes && !self.always_pending {
            self.stats.idle_ticks += 1;
            return;
        }
        self.sync_due = true;
    }

    // ---- LOG-step decisions ------------------------------------------------

    /// Registered FLUSH-class fsyncs not yet completed. While the pipeline
    /// is full, new dues accumulate instead of issuing — **group commit is
    /// a bounded number of durability fsyncs in flight per cell** (§8.2:
    /// batch = what arrives during the sync window; the S22 campaign
    /// measured the per-iteration-fsync shape at ratio 16.7 / 106k w/s on
    /// a 5 ms-fsync device vs ratio ≥ 500 expected — the batch=1.0
    /// disease expressed at the device tier; ADR-0022 D3 fixed the bound
    /// at 1, M2.5-S07 evaluates 2). Write-through tickets are excluded
    /// (ADR-0086 D5): they never queue on the flush unit.
    fn syncs_in_flight(&self) -> usize {
        self.pending
            .iter()
            .filter(|p| !p.done && !p.failed && p.reason != SyncReason::WriteThrough)
            .count()
    }

    /// Is a sync owed right now (everysec tick fired or `always` traffic
    /// staged and not yet discharged)?
    #[must_use]
    pub fn sync_due(&self) -> bool {
        self.sync_due
    }

    /// True when no queued frame is still awaiting `LogWritten` — the
    /// written prefix has caught up with everything queued.
    #[must_use]
    pub fn drained(&self) -> bool {
        self.written_bytes == self.queued_bytes
    }

    /// True while the ledger holds `REORDER_WINDOW_FRAMES` frames behind
    /// the written prefix — the front write is late and the frames that
    /// landed behind it have filled the window. [`frame_plan`](Self::frame_plan)
    /// answers `Wait` in this state; the plane counts the episode
    /// (`frame_waits_reorder`) so a wedging device is visible, never
    /// absorbed.
    #[must_use]
    pub fn reorder_window_full(&self) -> bool {
        self.queued.len() >= REORDER_WINDOW_FRAMES
    }

    /// Should this iteration's frame write chain an fdatasync (the
    /// FLUSH-class barrier, bounded by the FLUSH pipeline)? A linked
    /// fdatasync is ordered after *its own* write only (`IO_LINK`), so it
    /// may claim coverage up to its frame's end only when every earlier
    /// write has completed — `drained()` (ADR-0087 D3; at K = 1 implied by
    /// `can_seal`). Call before the seal.
    #[must_use]
    pub fn frame_fsync_due(&self) -> bool {
        self.sync_due && self.drained() && self.syncs_in_flight() < self.flush_bound
    }

    /// Should the next frame be written write-through (ADR-0086 D5)?
    /// Unbounded by the FLUSH pipeline — the staging ring's
    /// `frames_in_flight` is the bound; the caller has already established
    /// the segment is pre-zeroed `O_DIRECT` and the frame is inside
    /// `fua_max_frame_bytes`. **The prefix rule:** true only when every
    /// byte below the frame's base (`queued_bytes`, the next base) is
    /// durable or covered by a pending entry — a FUA write covers itself,
    /// never the un-barriered frames before it; earlier write-through
    /// tickets still in flight count as coverage, which is what lets a
    /// pure-`always` cell pipeline K deep. Call before the seal.
    #[must_use]
    pub fn write_through_due(&self) -> bool {
        self.sync_due && self.coverage_tail().1 == self.queued_bytes
    }

    /// The barrier the next frame may carry, or `Wait` (ADR-0087 D3),
    /// given whether the rotor allows write-through for it and whether a
    /// rotation's seal fdatasync will be registered ahead of it
    /// (`seal_ahead`: the seal covers every queued byte, so the frame
    /// extends the durable prefix by construction — the LOG step decides
    /// before rotating, so the ledger cannot show the entry yet). The
    /// `Wait` arm is the rule that keeps a due from starving behind
    /// barrier-less frames: sealing with no barrier while writes are in
    /// flight below would let every later frame find the same shape. The
    /// FLUSH-slot arm keeps §8.2 batching byte-for-byte (the due
    /// accumulates while the pipeline is drained but the slot is busy).
    #[must_use]
    pub fn frame_plan(&self, write_through_ok: bool, seal_ahead: bool) -> FramePlan {
        // The reorder window (ADR-0087 D2 as amended) bounds the queue
        // before any barrier question: a frame sealed now could not be
        // queued, whatever barrier it carried.
        if self.reorder_window_full() {
            return FramePlan::Wait;
        }
        if !self.sync_due {
            return FramePlan::Plain;
        }
        if write_through_ok && (seal_ahead || self.write_through_due()) {
            return FramePlan::WriteThrough;
        }
        if !self.drained() {
            return FramePlan::Wait;
        }
        if self.syncs_in_flight() < self.flush_bound {
            return FramePlan::LinkedFsync;
        }
        FramePlan::Plain
    }

    /// Is a standalone fdatasync due (sync owed, FLUSH slot free, no
    /// frame to chain to, dirty *written* bytes not already covered by a
    /// pending sync)?
    #[must_use]
    pub fn standalone_fsync_due(&self) -> bool {
        self.sync_due
            && self.syncs_in_flight() < self.flush_bound
            && self.written_bytes > self.durable_bytes
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
    }

    /// May an fsync-completion CQE issue the deferred sync right now
    /// (M2.5-S07)? Fires only under the two-in-flight pipeline and only
    /// while a slot stays reserved for the LOG step's linked sync — at
    /// bound 1 it is never true, preserving the ADR-0022 D3 discipline
    /// byte-for-byte. Requires full discharge: every owed `always` record
    /// must already sit in a written frame the sync will cover, or the
    /// due stays for the LOG step's linked sync.
    #[must_use]
    pub fn completion_fsync_due(&self) -> bool {
        self.sync_due
            && self.flush_bound > 1
            && self.syncs_in_flight() < self.flush_bound - 1
            && self.written_bytes > self.durable_bytes
            && self.always_discharged_at_written()
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
    }

    /// A sync covering `written_up_to` is being registered: settle what
    /// it discharges. The `always` half survives unless every owed record
    /// is inside the covered range (M2.5-S07). The `everysec` half
    /// survives while a queued frame is still in flight **outside** the
    /// coverage (ADR-0013 D3 as amended, 2026-08-22): the tick's promise
    /// is every record staged before it, and a frame landing after this
    /// sync would otherwise wait for the *next* tick — a ~2 s loss window
    /// the `m2-reorder-window` sweep found (seed `0x2e0d0179`: one plain
    /// write in flight at the tick, its frame acked 1.11 s before the
    /// cut, lost). Kept due, the next LOG step drains and covers it: a
    /// linked sync on the next frame or another standalone once the
    /// write lands — one write latency, once per tick, never a spin
    /// (`standalone_fsync_due` needs new written bytes).
    fn settle_due_at_written(&mut self) {
        let always_owed = self.always_pending && !self.always_discharged_at_written();
        let in_flight_uncovered = self.written_bytes < self.queued_bytes;
        self.sync_due = always_owed || in_flight_uncovered;
        self.always_pending = always_owed;
    }

    /// Every owed `always` record sits in a written frame — a standalone
    /// covering `written_up_to` fully discharges the due.
    fn always_discharged_at_written(&self) -> bool {
        if !self.always_pending {
            return true;
        }
        !self.always_unqueued
            && self
                .always_queued_up_to
                .is_some_and(|end| self.written_up_to.is_some_and(|written| end <= written))
    }

    // ---- LOG-step registrations (submission order = ledger order) ----------

    /// A deferred seal leaves the rotor: register its fdatasync. Covers the
    /// sealed segment's exclusive end — sound because rotation is a
    /// pipeline drain point (ADR-0087 D4; asserted: everything queued has
    /// completed, so the sync runs with every write into the segment in
    /// the file).
    pub fn register_seal_fsync(&mut self, handoff: SealHandoff<File>, now: Nanos) -> FsyncTicket {
        assert!(self.drained(), "deferred seal with a frame write in flight — rotation drains");
        let covers_up_to = Lsn::new(handoff.segment(), handoff.end_offset());
        self.stats.fsyncs_seal += 1;
        self.push_pending(
            covers_up_to,
            self.queued_bytes,
            now,
            SyncReason::Seal,
            HeldHandle::Seal(handoff),
        )
    }

    /// Register one boot-metadata barrier (M2.5-S01): a driver-ridden
    /// fdatasync on a boot directory handle (`held = Some`) or the active
    /// segment fd (`held = None`, the rotor owns it). `floor` is the
    /// append cursor at enable time — at or below everything a future
    /// frame or sync will cover, so the done-prefix rule fences every
    /// durable ack behind the barriers. Covers zero log bytes: nothing
    /// was promised yet.
    ///
    /// # Panics
    /// If any frame was queued or any fsync completed first — barriers
    /// belong at the head of the ledger, before durable traffic exists.
    pub fn register_boot_barrier(
        &mut self,
        floor: Lsn,
        held: Option<File>,
        now: Nanos,
    ) -> FsyncTicket {
        assert_eq!(self.stats.frames_queued, 0, "boot barriers register before any frame");
        assert!(self.durable_up_to.is_none(), "boot barriers register before any completion");
        self.stats.fsyncs_boot_barrier += 1;
        let held = held.map_or(HeldHandle::None, HeldHandle::Dir);
        self.push_pending(floor, 0, now, SyncReason::BootBarrier, held)
    }

    /// Register one deferred-prealloc metadata barrier (M2.5-S01): a
    /// driver-ridden fdatasync on the log-dir handle making the new
    /// segment's directory entry durable. **Coverage-neutral**: it enters
    /// the ledger at the current coverage tail (a dir sync promises no
    /// log-data coverage), so it fences later acks by ledger order
    /// without ever advancing the watermark past real data syncs.
    pub fn register_prealloc_barrier(&mut self, dir: File, now: Nanos) -> FsyncTicket {
        let (covers_up_to, covers_bytes) = self.coverage_tail();
        self.stats.fsyncs_prealloc_barrier += 1;
        self.push_pending(
            covers_up_to,
            covers_bytes,
            now,
            SyncReason::PreallocBarrier,
            HeldHandle::Dir(dir),
        )
    }

    /// Registers a sealed blob extent's fdatasync as a coverage-neutral
    /// ledger barrier (M4-S26 realizing ADR-0061 D3): call **before**
    /// staging the referencing record, so this barrier's ledger position
    /// precedes the frame's linked fsync and the done-prefix rule fences
    /// the referencing ack behind extent durability. The handle stays
    /// held until the `Synced` completion.
    pub fn register_extent_barrier(&mut self, extent: File, now: Nanos) -> FsyncTicket {
        let (covers_up_to, covers_bytes) = self.coverage_tail();
        self.push_pending(
            covers_up_to,
            covers_bytes,
            now,
            SyncReason::ExtentSeal,
            HeldHandle::Extent(extent),
        )
    }

    /// Register one zero-fill barrier (ADR-0086 D4): the fdatasync on a
    /// pre-zeroed next segment's fd that commits its extent metadata.
    /// **Coverage-neutral** like the prealloc dir barrier — it enters at
    /// the current coverage tail and can never advance the watermark past
    /// a real data sync. The rotor keeps the fd open (the segment becomes
    /// active later), so nothing is held here.
    pub fn register_zero_fill_barrier(&mut self, now: Nanos) -> FsyncTicket {
        let (covers_up_to, covers_bytes) = self.coverage_tail();
        self.stats.fsyncs_zero_fill += 1;
        self.push_pending(covers_up_to, covers_bytes, now, SyncReason::ZeroFill, HeldHandle::None)
    }

    /// The ledger's current coverage tail — what a coverage-neutral
    /// barrier registers at.
    fn coverage_tail(&self) -> (Lsn, u64) {
        self.pending.back().map_or_else(
            || {
                (
                    self.durable_up_to.unwrap_or(Lsn::new(crate::lsn::SegmentId(0), 0)),
                    self.durable_bytes,
                )
            },
            |tail| (tail.covers_up_to, tail.covers_bytes),
        )
    }

    /// This iteration's frame was handed to the driver (`LogWrite` at the
    /// slot's base). `end` is the frame's exclusive end LSN (the
    /// successor's base — padding included on aligned segments). Returns
    /// the frame's id for [`note_frame_written`](Self::note_frame_written).
    ///
    /// # Panics
    /// If more than `MAX_FRAMES_IN_FLIGHT` frames are queued unwritten
    /// (the staging ring bounds this) or the reorder window is full
    /// (`frame_plan` answered `Wait`) — either is a plane bug.
    pub fn note_frame_queued(&mut self, end: Lsn, frame_len: u32) -> FrameId {
        // Release assert (M2.5-S13): out-of-order queue breaks the LSN↔seq
        // FIFO the ack gate and reader rely on. Per-batch, free.
        assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
        // Two bounds, both release-asserted, per frame, free. Unwritten
        // frames: what the staging ring holds in flight. The whole queue:
        // the reorder window (ADR-0087 D2 as amended) — frames that
        // landed ahead of an earlier one still in flight stay queued
        // until the prefix reaches them (the `m2-mode-transition` sweep
        // found the first bound mis-scoped to the queue: seven `everysec`
        // frames behind one late write crashed the cell; the review of
        // that fix found the queue then unbounded).
        assert!(
            self.unwritten < MAX_FRAMES_IN_FLIGHT,
            "more frames in flight than the ring can hold"
        );
        assert!(!self.reorder_window_full(), "frame queued into a full reorder window");
        self.queued_up_to = Some(end);
        self.queued_bytes += u64::from(frame_len);
        self.next_frame_id += 1;
        let id = FrameId(self.next_frame_id);
        self.unwritten += 1;
        self.queued.push_back(QueuedFrame {
            id,
            end,
            end_bytes: self.queued_bytes,
            written: false,
        });
        self.stats.frames_queued += 1;
        self.stats.frame_bytes_queued += u64::from(frame_len);
        // The seal drains the whole staging builder, so every record
        // staged before this LOG step now rides a queued frame.
        if self.always_unqueued {
            self.always_queued_up_to = Some(end);
            self.always_unqueued = false;
        }
        id
    }

    /// The queued frame chains an fdatasync (`always` traffic present or an
    /// everysec tick owed one). One linked sync covers every class due this
    /// iteration — group commit (§8.2).
    ///
    /// # Panics
    /// If no frame was queued first — a linked sync without its write is a
    /// LOG-step sequencing bug.
    pub fn register_linked_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.queued_up_to.expect("linked fsync before any frame was queued");
        // The frame just queued absorbed every staged record, so the
        // linked sync's coverage (queued_up_to) discharges the whole due.
        // Release assert (M2.5-S13): discharging with an `always` record
        // still unqueued gates its ack on a sync that does not cover it —
        // ack before durable. Per-batch, free.
        assert!(!self.always_unqueued, "linked sync with an unqueued always record");
        // Release assert (ADR-0087 D3): the chain's fdatasync is ordered
        // after this frame's write only; a frame still in flight below it
        // would sit outside the sync's coverage. The one frame queued and
        // unwritten is this one.
        assert!(
            self.queued.len() == 1 && !self.queued[0].written,
            "linked fsync with earlier frame writes still in flight"
        );
        self.sync_due = false;
        self.always_pending = false;
        self.stats.fsyncs_linked += 1;
        self.push_pending(
            covers_up_to,
            self.queued_bytes,
            now,
            SyncReason::Linked,
            HeldHandle::None,
        )
    }

    /// The queued frame is written write-through (ADR-0086 D5): the write
    /// itself is the barrier, completed from `LogWritten`. Covers the
    /// frame's exclusive end and discharges the whole due exactly like a
    /// linked sync.
    ///
    /// # Panics
    /// If no frame was queued first, or an `always` record is still
    /// unqueued (the same sequencing invariants as the linked sync).
    pub fn register_write_through(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.queued_up_to.expect("write-through before any frame was queued");
        assert!(!self.always_unqueued, "write-through with an unqueued always record");
        // Release assert: the prefix rule (module docs). A FUA ticket
        // claiming bytes it did not write is the ack-before-durable bug
        // the sweep caught; per-frame, free. The frame just queued is the
        // back of the FIFO; its base is the previous entry's end.
        let base = self.queued.iter().rev().nth(1).map_or(self.written_bytes, |f| f.end_bytes);
        assert_eq!(
            self.coverage_tail().1,
            base,
            "write-through frame must extend the durable prefix"
        );
        self.sync_due = false;
        self.always_pending = false;
        self.stats.fsyncs_write_through += 1;
        self.push_pending(
            covers_up_to,
            self.queued_bytes,
            now,
            SyncReason::WriteThrough,
            HeldHandle::None,
        )
    }

    /// An everysec-owed fdatasync with no frame to chain to. Covers what
    /// has *completed writing* — an fdatasync races in-flight writes.
    ///
    /// # Panics
    /// If nothing was ever written — callers check
    /// [`standalone_fsync_due`](Self::standalone_fsync_due).
    pub fn register_standalone_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.written_up_to.expect("standalone fsync with nothing written");
        // The always due survives unless everything owed is inside the
        // covered range (M2.5-S07 made this exact; the pre-S07 shape kept
        // the due whenever always traffic was pending).
        self.settle_due_at_written();
        self.stats.fsyncs_standalone += 1;
        self.push_pending(
            covers_up_to,
            self.written_bytes,
            now,
            SyncReason::Standalone,
            HeldHandle::None,
        )
    }

    /// The completion-CQE-issued sync (M2.5-S07): covers what has
    /// completed writing, exactly like a standalone — callers check
    /// [`completion_fsync_due`](Self::completion_fsync_due), which
    /// guarantees full discharge of the always due.
    ///
    /// # Panics
    /// If nothing was ever written.
    pub fn register_completion_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.written_up_to.expect("completion fsync with nothing written");
        debug_assert!(self.always_discharged_at_written());
        self.settle_due_at_written();
        self.stats.fsyncs_completion += 1;
        self.push_pending(
            covers_up_to,
            self.written_bytes,
            now,
            SyncReason::Completion,
            HeldHandle::None,
        )
    }

    fn push_pending(
        &mut self,
        covers_up_to: Lsn,
        covers_bytes: u64,
        now: Nanos,
        reason: SyncReason,
        held: HeldHandle<File>,
    ) -> FsyncTicket {
        // Release assert (M2.5-S13): *the* watermark-honesty invariant — a
        // non-monotone entry lets the done-prefix advance the watermark
        // past unfsynced data. Per-batch, free.
        assert!(
            self.pending.back().is_none_or(|p| p.covers_up_to <= covers_up_to),
            "fsync coverage must be monotone in submission order"
        );
        self.next_ticket += 1;
        let ticket = self.next_ticket;
        self.pending.push_back(PendingFsync {
            ticket,
            covers_up_to,
            covers_bytes,
            submitted_at: now,
            reason,
            done: false,
            failed: false,
            held,
        });
        FsyncTicket(ticket)
    }

    // ---- REAP completions --------------------------------------------------

    /// Frame `id`'s `LogWritten` arrived: its bytes reached the file (NOT
    /// durable unless the frame was write-through — release the staging
    /// lease, never ack from here). Advances the written prefix over
    /// every leading frame now written (ADR-0087 D2); a later frame
    /// completing first advances nothing.
    ///
    /// # Panics
    /// On an unknown or already-written id — completions are exactly-once.
    pub fn note_frame_written(&mut self, id: FrameId) {
        // Ids are consecutive ordinals and the queue pops only at the
        // front, so the frame's index is its distance from the front —
        // O(1) whatever the window holds (ADR-0087 D2 as amended).
        let front = self.queued.front().expect("LogWritten with nothing queued").id;
        let index = id.0.checked_sub(front.0).expect("LogWritten for a frame already written");
        let frame = usize::try_from(index)
            .ok()
            .and_then(|index| self.queued.get_mut(index))
            .expect("LogWritten for a frame that was not queued");
        debug_assert_eq!(frame.id, id, "queue ordinals are consecutive");
        assert!(!frame.written, "frame written twice");
        frame.written = true;
        self.unwritten -= 1;
        while let Some(front) = self.queued.front() {
            if !front.written {
                break;
            }
            self.written_up_to = Some(front.end);
            self.written_bytes = front.end_bytes;
            self.queued.pop_front();
        }
    }

    /// Frames queued and not yet known written (`0..=frames_in_flight`) —
    /// the in-flight count, not the queue's length (the queue also holds
    /// written frames waiting behind an unwritten earlier one).
    #[must_use]
    pub fn frames_unwritten(&self) -> usize {
        usize::from(self.unwritten)
    }

    /// Frames queued and not yet part of the written prefix: the
    /// unwritten ones plus those that landed ahead of an earlier one.
    #[must_use]
    pub fn frames_behind_prefix(&self) -> usize {
        self.queued.len()
    }

    /// Rebase a pending linked fsync's latency clock to `now` — its
    /// covering write just completed (M4.5-S27, ADR-0083 D4). A linked
    /// fdatasync is an `IO_LINK` chain: the sync SQE starts only after
    /// the write, but the ticket registered at the LOG step, so without
    /// this the histogram absorbs the write's full duration (under
    /// writeback throttling, seconds — the ADR-0081 D6 artefact).
    /// No-ops on a ticket already completed or failed (a short-write
    /// resubmission chain may complete its sync out of band).
    pub fn rebase_clock(&mut self, ticket: FsyncTicket, now: Nanos) {
        if let Some(entry) = self.pending.iter_mut().find(|p| p.ticket == ticket.0)
            && !entry.done
            && !entry.failed
        {
            debug_assert_eq!(entry.reason, SyncReason::Linked, "only linked syncs rebase");
            entry.submitted_at = now;
        }
    }

    /// An fsync's `Synced` arrived. Returns the new watermark — the
    /// exclusive end of the durable range — when the done-prefix advanced;
    /// the plane feeds it to `WatermarkGate::advance(end.to_u64())` (S06).
    ///
    /// # Panics
    /// On an unknown or already-completed ticket — completions are
    /// exactly-once by the driver contract.
    pub fn on_fsync_complete(&mut self, ticket: FsyncTicket, now: Nanos) -> Option<Lsn> {
        let entry = self
            .pending
            .iter_mut()
            .find(|p| p.ticket == ticket.0)
            .expect("fsync completion for an unknown ticket");
        assert!(!entry.done && !entry.failed, "fsync ticket completed twice");
        entry.done = true;
        // Seal durability and write-handle drop coincide (ADR-0013 D4);
        // boot-barrier dir handles close the same way (M2.5-S01).
        entry.held = HeldHandle::None;
        let elapsed = now.saturating_sub(entry.submitted_at);
        self.fsync_hist_us.record(elapsed.as_micros());
        if entry.reason == SyncReason::WriteThrough {
            self.write_through_hist_us.record(elapsed.as_micros());
        }
        self.stats.fsyncs_completed += 1;

        let before = self.durable_up_to;
        while let Some(front) = self.pending.front() {
            if !front.done || front.failed {
                break;
            }
            self.durable_up_to = Some(front.covers_up_to);
            self.durable_bytes = front.covers_bytes;
            self.pending.pop_front();
        }
        (self.durable_up_to != before).then(|| self.durable_up_to.expect("advanced past None"))
    }

    /// An fsync failed (`Error` on its token — including `ECANCELED` from a
    /// failed linked write). The watermark freezes at the previous entry
    /// forever; the caller fail-stops the cell (§8.4 fsyncgate rule — this
    /// method exists so the freeze is observable in tests and the error
    /// path can name what was lost, never so the caller can continue).
    pub fn on_fsync_error(&mut self, ticket: FsyncTicket) -> SyncReason {
        let entry = self
            .pending
            .iter_mut()
            .find(|p| p.ticket == ticket.0)
            .expect("fsync error for an unknown ticket");
        assert!(!entry.done, "fsync ticket errored after completing");
        entry.failed = true;
        entry.reason
    }

    // ---- Observability (S21 counter-set vocabulary) -------------------------

    /// The durability watermark: exclusive end of the fsync-covered range.
    /// `None` until the first fsync completes.
    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        self.durable_up_to
    }

    /// Exclusive end of everything handed to the driver — with
    /// [`watermark`](Self::watermark), the `watermark_lag_lsn` input.
    #[must_use]
    pub fn queued_up_to(&self) -> Option<Lsn> {
        self.queued_up_to
    }

    /// True while any extent-seal barrier (ADR-0061 D3) still awaits its
    /// `Synced`. `seal_log` holds the next frame behind it: that frame
    /// may carry the referencing record, and the frame's durability must
    /// never precede the extent's — record durable ⇒ extent durable, the
    /// D9 "reverse assert" at reactor tier. The ledger barrier alone
    /// fences only the *ack* (done-prefix); the device is free to
    /// complete the frame's linked fsync before the extent's fdatasync,
    /// and a cut in that window replays a dangling reference (caught by
    /// the `m4-tiered` DST audit, seeds 0x5eed000c/0x5eed003a).
    #[must_use]
    pub fn extent_barrier_pending(&self) -> bool {
        self.pending.iter().any(|p| p.reason == SyncReason::ExtentSeal && !p.done)
    }

    /// Frame bytes queued but not yet fsync-covered (`pending_log_bytes`).
    #[must_use]
    pub fn pending_log_bytes(&self) -> u64 {
        self.queued_bytes - self.durable_bytes
    }

    /// Submitted fsyncs whose completion has not arrived.
    #[must_use]
    pub fn pending_fsyncs(&self) -> usize {
        self.pending.iter().filter(|p| !p.done).count()
    }

    /// Durability-barrier completion latency, microseconds, all classes
    /// (`fsync_latency_hist`).
    #[must_use]
    pub fn fsync_latency_hist(&self) -> &LogHistogram {
        &self.fsync_hist_us
    }

    /// Write-through (FUA-class) frame latency, microseconds (ADR-0086
    /// D5) — submission → `LogWritten`, the barrier the client waits on.
    #[must_use]
    pub fn write_through_latency_hist(&self) -> &LogHistogram {
        &self.write_through_hist_us
    }

    /// Write-through tickets whose completion has not arrived
    /// (`0..=frames_in_flight`).
    #[must_use]
    pub fn write_through_in_flight(&self) -> usize {
        self.pending.iter().filter(|p| !p.done && p.reason == SyncReason::WriteThrough).count()
    }

    #[must_use]
    pub fn stats(&self) -> CommitStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SegmentFs;
    use crate::fs::mem::MemFile;
    use crate::lsn::SegmentId;

    fn lsn(seg: u32, off: u32) -> Lsn {
        Lsn::new(SegmentId(seg), off)
    }

    fn commit() -> GroupCommit<MemFile> {
        GroupCommit::new()
    }

    /// One late plain write at the front with barrier-less frames landing
    /// behind it (the `m2-mode-transition` seed `0x7a4e01cb` shape): the
    /// queue grows past `MAX_FRAMES_IN_FLIGHT` while the in-flight count
    /// never exceeds the ring's K — the ledger keeps them — **up to the
    /// reorder window**, where the plan holds the next frame (ADR-0087
    /// D2 as amended) whether or not a sync is due. The front landing
    /// advances the prefix past all of them at once and reopens the
    /// window.
    #[test]
    fn late_front_write_fills_the_reorder_window_then_the_plan_waits() {
        let mut gc = commit();
        let window = u32::try_from(REORDER_WINDOW_FRAMES).expect("small constant");
        assert!(window > u32::from(MAX_FRAMES_IN_FLIGHT), "the window admits a full pipeline");
        let mut ids = Vec::new();
        for i in 1..=window {
            gc.note_staged(FsyncClass::Everysec);
            assert!(!gc.reorder_window_full(), "frame {i}");
            assert_eq!(gc.frame_plan(false, false), FramePlan::Plain, "frame {i}");
            ids.push(gc.note_frame_queued(lsn(0, i * 64), 64));
            // Every frame but the first lands at once: ≤ 2 in flight.
            if i > 1 {
                gc.note_frame_written(ids[i as usize - 1]);
            }
            assert!(gc.frames_unwritten() <= 1);
            assert_eq!(gc.frames_behind_prefix(), i as usize);
            assert_eq!(gc.written_up_to, None, "the prefix waits for the front");
        }
        // The window is full: no barrier question is asked — plain,
        // write-through-eligible, or sync-due frames all wait.
        assert!(gc.reorder_window_full());
        assert_eq!(gc.frame_plan(false, false), FramePlan::Wait);
        gc.note_staged(FsyncClass::Always);
        assert_eq!(gc.frame_plan(true, false), FramePlan::Wait);
        assert_eq!(gc.frame_plan(true, true), FramePlan::Wait, "even ahead of a seal");
        // The front lands: the prefix jumps past every frame, the window
        // reopens, and the due barrier is planned as before.
        gc.note_frame_written(ids[0]);
        assert_eq!(gc.frames_behind_prefix(), 0);
        assert_eq!(gc.frames_unwritten(), 0);
        assert_eq!(gc.written_up_to, Some(lsn(0, window * 64)));
        assert!(!gc.reorder_window_full());
        assert_eq!(gc.frame_plan(false, false), FramePlan::LinkedFsync);
    }

    /// ADR-0013 D3 as amended: a standalone issued while a barrier-less
    /// frame is still in flight covers the written prefix only — and the
    /// due **survives** it, so the in-flight frame is covered within the
    /// tick (a linked sync on the next frame, or another standalone once
    /// it lands) instead of waiting for the next tick.
    #[test]
    fn everysec_due_survives_a_standalone_that_leaves_a_frame_in_flight() {
        let mut gc = commit();
        gc.note_staged(FsyncClass::Everysec);
        let a = gc.note_frame_queued(lsn(0, 64), 64);
        gc.note_staged(FsyncClass::Everysec);
        let b = gc.note_frame_queued(lsn(0, 128), 64);
        gc.note_frame_written(a);
        // The tick fires with B in flight: the standalone covers A only.
        gc.note_everysec_tick();
        assert!(gc.standalone_fsync_due());
        let t1 = gc.register_standalone_fsync(Nanos::ZERO);
        assert!(gc.sync_due(), "B's bytes are queued outside the coverage — the due stays");
        assert!(!gc.standalone_fsync_due(), "nothing new written: no second standalone yet");
        assert_eq!(gc.frame_plan(false, false), FramePlan::Wait, "a next frame drains first");
        assert_eq!(gc.on_fsync_complete(t1, Nanos::ZERO), Some(lsn(0, 64)));
        // B lands: the due is still owed and now coverable.
        gc.note_frame_written(b);
        assert!(gc.standalone_fsync_due());
        let t2 = gc.register_standalone_fsync(Nanos::ZERO);
        assert!(!gc.sync_due(), "everything queued is inside the coverage");
        assert_eq!(gc.on_fsync_complete(t2, Nanos::ZERO), Some(lsn(0, 128)));
        assert_eq!(gc.frame_plan(false, false), FramePlan::Plain);
    }

    /// Queueing into a full window is a plane bug, release-asserted.
    #[test]
    #[should_panic(expected = "full reorder window")]
    fn queueing_into_a_full_reorder_window_panics() {
        let mut gc = commit();
        let window = u32::try_from(REORDER_WINDOW_FRAMES).expect("small constant");
        let mut ids = Vec::new();
        for i in 1..=window {
            ids.push(gc.note_frame_queued(lsn(0, i * 64), 64));
            if i > 1 {
                gc.note_frame_written(ids[i as usize - 1]);
            }
        }
        gc.note_frame_queued(lsn(0, (window + 1) * 64), 64);
    }

    /// Completion routing is ordinal arithmetic: the window's last frame
    /// is found without a scan, an already-popped id is refused.
    #[test]
    #[should_panic(expected = "already written")]
    fn written_for_a_popped_frame_panics() {
        let mut gc = commit();
        let a = gc.note_frame_queued(lsn(0, 64), 64);
        let _b = gc.note_frame_queued(lsn(0, 128), 64);
        gc.note_frame_written(a);
        gc.note_frame_written(a);
    }

    /// `LogWritten` for the oldest frame still unwritten — the K = 1
    /// completion order every pre-S35 test assumed. Ids are ordinals
    /// from 1, so the oldest unwritten one is derivable from the counters.
    fn write_oldest(gc: &mut GroupCommit<MemFile>) {
        let oldest = gc.stats().frames_queued - gc.frames_unwritten() as u64 + 1;
        gc.note_frame_written(FrameId(oldest));
    }

    #[test]
    fn always_traffic_makes_the_frame_sync() {
        let mut gc = commit();
        assert!(!gc.frame_fsync_due());
        gc.note_staged(FsyncClass::Everysec);
        assert!(!gc.frame_fsync_due(), "everysec records ride the timer, not per-frame syncs");
        gc.note_staged(FsyncClass::Always);
        assert!(gc.frame_fsync_due());
        gc.note_frame_queued(lsn(0, 100), 100);
        gc.register_linked_fsync(Nanos::ZERO);
        assert!(!gc.frame_fsync_due(), "one linked sync covers everything due this iteration");
    }

    #[test]
    fn watermark_advances_only_on_completion_and_by_done_prefix() {
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.watermark(), None);
        // Out-of-order completion: t2 first — no advance (t1 still pending).
        assert_eq!(gc.on_fsync_complete(t2, Nanos::from_micros(50)), None);
        assert_eq!(gc.watermark(), None);
        // t1 completes: the prefix drains through both entries.
        assert_eq!(gc.on_fsync_complete(t1, Nanos::from_micros(80)), Some(lsn(0, 200)));
        assert_eq!(gc.watermark(), Some(lsn(0, 200)));
        assert_eq!(gc.pending_log_bytes(), 0);
        assert_eq!(gc.fsync_latency_hist().count(), 2);
    }

    #[test]
    fn linked_fsync_latency_rebases_at_write_completion() {
        // ADR-0083 D4 (resolves ADR-0081 D6): a linked fdatasync's SQE
        // starts only after its covering write completes (IO_LINK), so
        // its latency clock must start at `LogWritten` — measured from
        // registration it absorbs the write's full duration, which is
        // exactly how the finding's 8.39 s "fsync p99" sample was made
        // (a multi-second throttled write ahead of a millisecond sync).
        let mut gc = commit();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 100), 100);
        let t = gc.register_linked_fsync(Nanos::ZERO);
        // The covering write stalls 5 s in writeback throttling, then
        // completes; the device services the sync itself in 2 ms.
        gc.rebase_clock(t, Nanos::from_secs(5));
        write_oldest(&mut gc);
        gc.on_fsync_complete(t, Nanos::from_secs(5) + Nanos::from_millis(2));
        let p50 = gc.fsync_latency_hist().percentile(50.0);
        assert!(p50 >= 1_000, "the 2 ms sync is in the histogram: p50={p50}µs");
        assert!(p50 < 100_000, "sync service time recorded, never the 5 s chain: p50={p50}µs");
    }

    #[test]
    fn rebase_clock_ignores_completed_and_failed_tickets() {
        // A short-write resubmission chain can complete (or fail) its
        // sync before the rebasing call lands — the rebase must no-op.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 100), 100);
        let t = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        gc.on_fsync_complete(t, Nanos::from_millis(3));
        gc.rebase_clock(t, Nanos::from_secs(9));
        assert_eq!(gc.fsync_latency_hist().count(), 1, "completed ticket stays completed");
    }

    #[test]
    fn everysec_tick_is_free_when_clean_and_dedupes_against_pending() {
        let mut gc = commit();
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "nothing dirty: idle tick");
        assert_eq!(gc.stats().idle_ticks, 1);

        gc.note_frame_queued(lsn(0, 100), 100);
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        assert!(gc.standalone_fsync_due());
        let t = gc.register_standalone_fsync(Nanos::ZERO);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "pending sync already covers written bytes");
        gc.on_fsync_complete(t, Nanos::ZERO);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "clean again");
        assert_eq!(gc.stats().idle_ticks, 2);
    }

    #[test]
    fn standalone_covers_written_not_queued() {
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        write_oldest(&mut gc);
        // A second frame is queued but its write has not completed.
        gc.note_frame_queued(lsn(0, 220), 120);
        gc.note_everysec_tick();
        assert!(gc.standalone_fsync_due());
        let t = gc.register_standalone_fsync(Nanos::ZERO);
        // The sync may not include the in-flight write: covers stops there.
        assert_eq!(gc.on_fsync_complete(t, Nanos::ZERO), Some(lsn(0, 100)));
        assert_eq!(gc.pending_log_bytes(), 120);
    }

    #[test]
    fn pipeline_bound_two_allows_a_second_linked_sync_and_no_more() {
        // M2.5-S07: at bound 2 the LOG step may link a second sync while
        // the first flushes; a third due accumulates (bounded, never a
        // queue). Completions drain in done-prefix order as ever.
        let mut gc: GroupCommit<MemFile> = GroupCommit::with_flush_bound(2);
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        gc.note_staged(FsyncClass::Always);
        assert!(gc.frame_fsync_due(), "second slot free at bound 2");
        gc.note_frame_queued(lsn(0, 200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        gc.note_staged(FsyncClass::Always);
        assert!(!gc.frame_fsync_due(), "third due accumulates — bounded at 2");
        gc.note_frame_queued(lsn(0, 300), 100);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(t1, Nanos::from_micros(10)), Some(lsn(0, 100)));
        assert!(gc.frame_fsync_due(), "slot freed at completion");
        assert_eq!(gc.on_fsync_complete(t2, Nanos::from_micros(20)), Some(lsn(0, 200)));
    }

    #[test]
    fn completion_issue_fires_only_with_a_reserved_linked_slot() {
        // M2.5-S07 sync-at-CQE: at bound 1 the completion path never
        // issues (the ADR-0022 D3 discipline byte-for-byte); at bound 2
        // it fires when the pipeline drained AND the covering standalone
        // fully discharges the always due (every owed record written).
        let mut gc1 = commit();
        gc1.note_staged(FsyncClass::Always);
        gc1.note_frame_queued(lsn(0, 100), 100);
        let t = gc1.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc1);
        gc1.note_staged(FsyncClass::Always);
        gc1.note_frame_queued(lsn(0, 200), 100);
        write_oldest(&mut gc1);
        gc1.on_fsync_complete(t, Nanos::from_micros(10));
        assert!(!gc1.completion_fsync_due(), "bound 1 never completion-issues");

        let mut gc2: GroupCommit<MemFile> = GroupCommit::with_flush_bound(2);
        gc2.note_staged(FsyncClass::Always);
        gc2.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc2.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc2);
        // Dues accumulate while the pipeline is at 1-in-flight; the frame
        // carrying them is written, so the due is fully dischargeable.
        gc2.note_staged(FsyncClass::Always);
        gc2.note_frame_queued(lsn(0, 200), 100);
        write_oldest(&mut gc2);
        assert!(!gc2.completion_fsync_due(), "linked slot reserved while one is in flight");
        gc2.on_fsync_complete(t1, Nanos::from_micros(10));
        assert!(gc2.completion_fsync_due(), "pipeline drained: issue at the CQE");
        let c = gc2.register_completion_fsync(Nanos::from_micros(11));
        assert_eq!(gc2.stats().fsyncs_completion, 1);
        assert!(!gc2.frame_fsync_due(), "due discharged — no spurious extra sync");
        assert_eq!(gc2.on_fsync_complete(c, Nanos::from_micros(30)), Some(lsn(0, 200)));

        // An always record still in the staging ring (no frame queued
        // yet) blocks completion-issue: the standalone could not cover it.
        gc2.note_staged(FsyncClass::Always);
        assert!(!gc2.completion_fsync_due(), "unqueued always record: wait for the LOG link");
    }

    #[test]
    fn boot_barriers_fence_acks_by_ledger_order() {
        // M2.5-S01: boot-metadata barriers sit at the head of the ledger,
        // so the done-prefix rule keeps the watermark (and with it every
        // gated ack) behind them even when a data sync completes first —
        // durable honesty without a blocking boot fsync.
        let mut gc = commit();
        let b1 = gc.register_boot_barrier(lsn(0, 0), None, Nanos::ZERO);
        let b2 = gc.register_boot_barrier(lsn(0, 0), None, Nanos::ZERO);
        assert_eq!(gc.stats().fsyncs_boot_barrier, 2);
        assert!(!gc.frame_fsync_due(), "barriers count as in-flight syncs");

        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 100), 100);
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "dues accumulate behind the barriers");

        // Barriers drain: the deferred data sync may now issue.
        assert_eq!(gc.on_fsync_complete(b1, Nanos::from_micros(10)), Some(lsn(0, 0)));
        gc.on_fsync_complete(b2, Nanos::from_micros(12));
        assert!(gc.standalone_fsync_due(), "deferred due issues once barriers complete");
        let t = gc.register_standalone_fsync(Nanos::ZERO);
        assert_eq!(gc.on_fsync_complete(t, Nanos::from_micros(40)), Some(lsn(0, 100)));
        assert_eq!(gc.watermark(), Some(lsn(0, 100)));
    }

    #[test]
    fn boot_barrier_completion_never_covers_data() {
        // A barrier completing out of order must not advance the
        // watermark past unfsynced frames — its covers is the boot floor.
        let mut gc = commit();
        let b = gc.register_boot_barrier(lsn(0, 0), None, Nanos::ZERO);
        gc.note_frame_queued(lsn(0, 100), 100);
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "barrier in flight: due accumulates");
        assert_eq!(gc.on_fsync_complete(b, Nanos::from_micros(5)), Some(lsn(0, 0)));
        assert_eq!(gc.watermark(), Some(lsn(0, 0)), "frame bytes stay uncovered");
        assert_eq!(gc.pending_log_bytes(), 100);
    }

    #[test]
    fn prealloc_barrier_is_coverage_neutral() {
        // M2.5-S01: the deferred-prealloc dir barrier enters at the
        // coverage tail — completing it advances nothing on its own, and
        // a data sync registered after it is fenced behind it.
        let fs = crate::fs::mem::MemFs::new();
        fs.create_dir_all(std::path::Path::new("log")).expect("mem dir");
        let mut gc = commit();
        let dir = fs.open_dir(std::path::Path::new("log")).expect("mem dir handle");
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        let p = gc.register_prealloc_barrier(dir, Nanos::ZERO);
        assert_eq!(gc.stats().fsyncs_prealloc_barrier, 1);

        assert_eq!(gc.on_fsync_complete(t1, Nanos::from_micros(20)), Some(lsn(0, 100)));
        // The barrier completing adds no coverage (same covers value).
        assert_eq!(gc.on_fsync_complete(p, Nanos::from_micros(30)), None);
        assert_eq!(gc.watermark(), Some(lsn(0, 100)));

        // A later data sync behind an incomplete barrier stays fenced.
        let dir2 = fs.open_dir(std::path::Path::new("log")).expect("mem dir handle");
        let p2 = gc.register_prealloc_barrier(dir2, Nanos::ZERO);
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(1, 80), 80);
        assert!(!gc.frame_fsync_due(), "barrier counts as in-flight");
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due());
        gc.on_fsync_complete(p2, Nanos::from_micros(40));
        assert!(gc.standalone_fsync_due());
        let t2 = gc.register_standalone_fsync(Nanos::ZERO);
        assert_eq!(gc.on_fsync_complete(t2, Nanos::from_micros(60)), Some(lsn(1, 80)));
    }

    #[test]
    fn dues_accumulate_behind_the_in_flight_sync() {
        // §8.2 group commit, one durability fsync in flight per cell
        // (M2-S22): always traffic arriving while a sync is outstanding
        // must NOT issue another — it batches behind it, and the sync
        // issued at completion covers everything queued since. The
        // per-iteration-fsync shape measured ratio 16.7 / 106k w/s on a
        // 5 ms-fsync device; this test pins the discipline that fixes it.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Always);
        assert!(gc.frame_fsync_due());
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);

        // Ten more always-frames arrive while t1 is still in flight:
        // due stays owed but never issues.
        for i in 1..=10u32 {
            gc.note_staged(FsyncClass::Always);
            assert!(!gc.frame_fsync_due(), "no second sync while one is in flight");
            assert_eq!(gc.frame_plan(false, false), FramePlan::Plain, "the frame still flows");
            gc.note_frame_queued(lsn(0, 100 + i * 100), 100);
            assert!(!gc.standalone_fsync_due(), "no standalone either");
            write_oldest(&mut gc);
        }

        // t1 completes: the owed sync issues NOW and covers all ten
        // accumulated frames in one fdatasync — the batch.
        gc.on_fsync_complete(t1, Nanos::from_micros(5_000));
        assert!(gc.frame_fsync_due(), "deferred due issues at completion");
        gc.note_frame_queued(lsn(0, 1200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(
            gc.on_fsync_complete(t2, Nanos::from_micros(10_000)),
            Some(lsn(0, 1200)),
            "one sync covers the whole accumulated batch"
        );
        assert_eq!(gc.stats().fsyncs_linked, 2, "12 frames, 2 fsyncs — not 12");
    }

    #[test]
    fn write_through_completes_at_log_written_and_skips_the_pipeline_bound() {
        // ADR-0086 D5: a write-through ticket is a ledger entry like any
        // other (done-prefix, monotone coverage) but does not occupy a
        // sync-pipeline slot — a seal FLUSH in flight never defers it.
        let mut gc = commit();
        let fs = crate::fs::mem::MemFs::new();
        fs.create_dir_all(std::path::Path::new("log")).expect("mem dir");
        let dir = fs.open_dir(std::path::Path::new("log")).expect("mem dir handle");
        let barrier = gc.register_prealloc_barrier(dir, Nanos::ZERO);
        gc.note_staged(FsyncClass::Always);
        assert!(!gc.frame_fsync_due(), "the FLUSH class is bounded behind the barrier");
        assert!(gc.write_through_due(), "the write-through class is not");
        assert_eq!(gc.frame_plan(true, false), FramePlan::WriteThrough);
        gc.note_frame_queued(lsn(0, 4096), 4096);
        let t = gc.register_write_through(Nanos::ZERO);
        assert_eq!(gc.stats().fsyncs_write_through, 1);
        assert!(!gc.frame_fsync_due(), "the due is discharged");
        write_oldest(&mut gc);
        // The FUA frame lands first: nothing advances past the barrier.
        assert_eq!(gc.on_fsync_complete(t, Nanos::from_micros(300)), None);
        assert_eq!(gc.watermark(), None);
        assert_eq!(gc.on_fsync_complete(barrier, Nanos::from_micros(900)), Some(lsn(0, 4096)));
        assert_eq!(gc.write_through_latency_hist().count(), 1);
        assert_eq!(gc.fsync_latency_hist().count(), 2, "all-class histogram sees both");
    }

    #[test]
    fn recycled_segment_frames_are_fenced_behind_the_rename_barrier() {
        // ADR-0090 D3 as amended: the rename's dir barrier registers in
        // the MAINTAIN slice that renamed (coverage-neutral, at the
        // ledger's tail); every write-through ticket of the renamed
        // segment's frames enters behind it, so a FUA frame completing
        // first advances nothing — the ack waits for the directory entry
        // that makes the segment findable after a power cut.
        let mut gc = commit();
        let fs = crate::fs::mem::MemFs::new();
        fs.create_dir_all(std::path::Path::new("log")).expect("mem dir");
        // The active segment's last frame, durable.
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(2, 4096), 4096);
        let t0 = gc.register_write_through(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(t0, Nanos::from_micros(300)), Some(lsn(2, 4096)));
        // MAINTAIN: seg-1 renamed to seg-3, barrier registered.
        let dir = fs.open_dir(std::path::Path::new("log")).expect("mem dir handle");
        let rename_barrier = gc.register_prealloc_barrier(dir, Nanos::ZERO);
        // LOG: rotation onto seg-3, its first frame write-through.
        gc.note_staged(FsyncClass::Always);
        assert_eq!(gc.frame_plan(true, false), FramePlan::WriteThrough);
        gc.note_frame_queued(lsn(3, 4096), 4096);
        let t1 = gc.register_write_through(Nanos::ZERO);
        write_oldest(&mut gc);
        // The FUA frame lands before the directory barrier: no ack.
        assert_eq!(gc.on_fsync_complete(t1, Nanos::from_micros(600)), None);
        assert_eq!(gc.watermark(), Some(lsn(2, 4096)), "nothing of seg-3 is acknowledged");
        // The barrier lands: the prefix reaches the frame, the ack may go.
        assert_eq!(
            gc.on_fsync_complete(rename_barrier, Nanos::from_micros(900)),
            Some(lsn(3, 4096))
        );
        assert_eq!(gc.watermark(), Some(lsn(3, 4096)));
    }

    #[test]
    fn write_through_in_flight_does_not_block_a_flush_class_sync() {
        // The bound counts FLUSH-class entries only: with a write-through
        // ticket outstanding, a standalone everysec sync may still issue.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 4096), 4096);
        let t = gc.register_write_through(Nanos::ZERO);
        assert_eq!(gc.write_through_in_flight(), 1);
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        // Everything written is already promised by the write-through
        // ticket (covers == written): the tick is deduped, not blocked.
        assert!(!gc.standalone_fsync_due());
        gc.on_fsync_complete(t, Nanos::from_micros(300));
        assert_eq!(gc.write_through_in_flight(), 0);
    }

    #[test]
    fn write_through_requires_the_durable_prefix() {
        // The prefix rule (ADR-0086 D5, found by the sweep): an everysec-
        // only frame written with no barrier sits un-covered; the next
        // always frame must take the FLUSH-class linked fsync (which
        // covers the gap), never a write-through that would claim it.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Everysec);
        assert!(!gc.write_through_due(), "no sync due");
        gc.note_frame_queued(lsn(0, 4096), 4096);
        write_oldest(&mut gc);
        gc.note_staged(FsyncClass::Always);
        assert!(!gc.write_through_due(), "un-covered bytes below the frame: FLUSH class");
        assert!(gc.frame_fsync_due());
        assert_eq!(gc.frame_plan(true, false), FramePlan::LinkedFsync);
        gc.note_frame_queued(lsn(0, 8192), 4096);
        let t = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(t, Nanos::from_micros(900)), Some(lsn(0, 8192)));
        // Durable prefix restored: the next always frame is write-through.
        gc.note_staged(FsyncClass::Always);
        assert!(gc.write_through_due());
        gc.note_frame_queued(lsn(0, 12288), 4096);
        let w = gc.register_write_through(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(w, Nanos::from_micros(300)), Some(lsn(0, 12288)));
        // A pending FLUSH entry covering the gap also satisfies the rule.
        gc.note_staged(FsyncClass::Everysec);
        gc.note_frame_queued(lsn(0, 16384), 4096);
        write_oldest(&mut gc);
        gc.note_everysec_tick();
        assert!(gc.standalone_fsync_due());
        let s = gc.register_standalone_fsync(Nanos::ZERO);
        gc.note_staged(FsyncClass::Always);
        assert!(gc.write_through_due(), "the standalone in flight covers the gap");
        gc.note_frame_queued(lsn(0, 20480), 4096);
        let w2 = gc.register_write_through(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(w2, Nanos::from_micros(300)), None, "prefix holds");
        assert_eq!(gc.on_fsync_complete(s, Nanos::from_micros(900)), Some(lsn(0, 20480)));
    }

    #[test]
    fn k_write_through_frames_in_flight_ack_only_on_the_prefix() {
        // ADR-0087 D3/D7: every frame of a pure-always run on a pre-zeroed
        // Direct segment goes write-through while its predecessors are
        // still in flight (their pending tickets are the coverage the
        // prefix rule asks for); a later frame landing first advances
        // nothing; the prefix drains as its predecessors land.
        let mut gc = commit();
        let mut tickets = Vec::new();
        let mut ids = Vec::new();
        for i in 1..=3u32 {
            gc.note_staged(FsyncClass::Always);
            assert_eq!(gc.frame_plan(true, false), FramePlan::WriteThrough, "frame {i}");
            ids.push(gc.note_frame_queued(lsn(0, i * 4096), 4096));
            tickets.push(gc.register_write_through(Nanos::ZERO));
        }
        assert_eq!(gc.write_through_in_flight(), 3);
        assert_eq!(gc.frames_unwritten(), 3);
        // Frame 3 lands first: written prefix and watermark stay put.
        gc.note_frame_written(ids[2]);
        assert_eq!(gc.on_fsync_complete(tickets[2], Nanos::from_micros(300)), None);
        assert_eq!(gc.watermark(), None);
        assert!(!gc.drained());
        // Frame 1 lands: the prefix advances to frame 1's end only.
        gc.note_frame_written(ids[0]);
        assert_eq!(gc.on_fsync_complete(tickets[0], Nanos::from_micros(310)), Some(lsn(0, 4096)));
        // Frame 2 lands: the done prefix runs through frame 3.
        gc.note_frame_written(ids[1]);
        assert_eq!(gc.on_fsync_complete(tickets[1], Nanos::from_micros(320)), Some(lsn(0, 12288)));
        assert!(gc.drained());
        assert_eq!(gc.stats().fsyncs_write_through, 3, "K frames, K barriers");
    }

    #[test]
    fn written_prefix_is_completion_ordered() {
        // ADR-0087 D2: `written_up_to` is what an fdatasync can cover —
        // the longest run of queued frames whose writes have completed.
        let mut gc = commit();
        let a = gc.note_frame_queued(lsn(0, 100), 100);
        let b = gc.note_frame_queued(lsn(0, 250), 150);
        let c = gc.note_frame_queued(lsn(0, 300), 50);
        gc.note_frame_written(c);
        gc.note_frame_written(b);
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "nothing is written as a prefix yet");
        gc.note_frame_written(a);
        assert!(gc.standalone_fsync_due());
        let t = gc.register_standalone_fsync(Nanos::ZERO);
        assert_eq!(gc.on_fsync_complete(t, Nanos::from_micros(900)), Some(lsn(0, 300)));
        assert_eq!(gc.pending_log_bytes(), 0);
    }

    #[test]
    fn a_due_frame_waits_behind_in_flight_plain_frames_then_links() {
        // ADR-0087 D3: a linked fdatasync is ordered after its own write
        // only, so while earlier barrier-less frames are in flight the due
        // frame is held (`Wait`), never sealed with no barrier (that would
        // starve the due under load). Once the pipeline drains it links.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Everysec);
        assert_eq!(gc.frame_plan(false, false), FramePlan::Plain);
        let plain = gc.note_frame_queued(lsn(0, 4096), 4096);
        gc.note_staged(FsyncClass::Always);
        assert_eq!(gc.frame_plan(false, false), FramePlan::Wait, "plain frame in flight below");
        assert!(!gc.frame_fsync_due());
        gc.note_frame_written(plain);
        assert_eq!(gc.frame_plan(false, false), FramePlan::LinkedFsync, "drained: link");
        gc.note_frame_queued(lsn(0, 8192), 4096);
        let t = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_complete(t, Nanos::from_micros(900)), Some(lsn(0, 8192)));
    }

    #[test]
    fn write_through_is_never_held_and_a_seal_ahead_restores_the_prefix() {
        // A write-through-capable frame is not held behind in-flight
        // plain frames when the prefix holds; and the first frame after a
        // rotation that follows plain frames may go write-through because
        // the seal fdatasync registered ahead of it covers them.
        let mut gc = commit();
        gc.note_staged(FsyncClass::Everysec);
        let plain = gc.note_frame_queued(lsn(0, 4096), 4096);
        gc.note_staged(FsyncClass::Always);
        assert_eq!(gc.frame_plan(true, false), FramePlan::Wait, "prefix broken, not drained");
        assert_eq!(gc.frame_plan(true, true), FramePlan::WriteThrough, "a seal ahead covers it");
        gc.note_frame_written(plain);
        assert_eq!(gc.frame_plan(true, false), FramePlan::LinkedFsync, "drained, prefix broken");
    }

    #[test]
    fn zero_fill_barrier_is_coverage_neutral() {
        // ADR-0086 D4: the zero-fill fdatasync enters at the coverage tail
        // and fences later data syncs without advancing anything itself.
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        let z = gc.register_zero_fill_barrier(Nanos::ZERO);
        assert_eq!(gc.stats().fsyncs_zero_fill, 1);
        assert_eq!(gc.on_fsync_complete(t1, Nanos::from_micros(20)), Some(lsn(0, 100)));
        assert_eq!(gc.on_fsync_complete(z, Nanos::from_micros(30)), None);
        assert_eq!(gc.watermark(), Some(lsn(0, 100)));
    }

    #[test]
    fn failed_fsync_freezes_the_watermark() {
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        write_oldest(&mut gc);
        assert_eq!(gc.on_fsync_error(t1), SyncReason::Linked);
        // t2 completing can never advance past the failed t1.
        assert_eq!(gc.on_fsync_complete(t2, Nanos::ZERO), None);
        assert_eq!(gc.watermark(), None);
    }
}
