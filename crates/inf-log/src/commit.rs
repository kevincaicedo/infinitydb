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
//! REAP      LogWritten → note_frame_written(); staging.release(lease)
//!           Synced     → on_fsync_complete → Some(end) ⇒ gate.advance(end)
//! ```
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
//! The pending set is bounded by construction: at most one write (and so
//! one linked sync) is in flight, standalone syncs dedupe against the
//! ledger tail, and seal syncs arrive at segment cadence.

use core::fmt;
use std::collections::VecDeque;

use inf_foundation::LogHistogram;
use inf_foundation::time::Nanos;

use crate::fs::SegmentFile;
use crate::lsn::Lsn;
use crate::segment::SealHandoff;

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
    /// linked sync (`sync_pipeline_bound ≥ 2`).
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
    /// Durability syncs allowed in flight at once (M2.5-S07): 1 = the
    /// ADR-0022 D3 discipline; 2 = the bounded two-in-flight pipeline
    /// (never more — a queue is the batch=1.0 disease reborn).
    sync_pipeline_bound: usize,
    queued_up_to: Option<Lsn>,
    queued_bytes: u64,
    written_up_to: Option<Lsn>,
    written_bytes: u64,
    durable_up_to: Option<Lsn>,
    durable_bytes: u64,
    pending: VecDeque<PendingFsync<File>>,
    next_ticket: u64,
    fsync_hist_us: LogHistogram,
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
        GroupCommit::with_sync_pipeline(1)
    }

    /// `bound` durability syncs may be in flight at once (M2.5-S07).
    ///
    /// # Panics
    /// Outside `1..=2` — the pipeline is bounded by construction, never a
    /// queue.
    #[must_use]
    pub fn with_sync_pipeline(bound: usize) -> GroupCommit<File> {
        assert!((1..=2).contains(&bound), "sync pipeline is 1 or 2, never a queue");
        GroupCommit {
            sync_due: false,
            always_pending: false,
            always_unqueued: false,
            always_queued_up_to: None,
            sync_pipeline_bound: bound,
            queued_up_to: None,
            queued_bytes: 0,
            written_up_to: None,
            written_bytes: 0,
            durable_up_to: None,
            durable_bytes: 0,
            pending: VecDeque::new(),
            next_ticket: 0,
            fsync_hist_us: LogHistogram::new(),
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

    /// Registered fsyncs not yet completed. While the pipeline is full,
    /// new dues accumulate instead of issuing — **group commit is a
    /// bounded number of durability fsyncs in flight per cell** (§8.2:
    /// batch = what arrives during the sync window; the S22 campaign
    /// measured the per-iteration-fsync shape at ratio 16.7 / 106k w/s on
    /// a 5 ms-fsync device vs ratio ≥ 500 expected — the batch=1.0
    /// disease expressed at the device tier; ADR-0022 D3 fixed the bound
    /// at 1, M2.5-S07 evaluates 2).
    fn syncs_in_flight(&self) -> usize {
        self.pending.iter().filter(|p| !p.done && !p.failed).count()
    }

    /// Should this iteration's frame write chain an fdatasync?
    #[must_use]
    pub fn frame_fsync_due(&self) -> bool {
        self.sync_due && self.syncs_in_flight() < self.sync_pipeline_bound
    }

    /// Is a standalone fdatasync due (sync owed, pipeline slot free, no
    /// frame to chain to, dirty *written* bytes not already covered by a
    /// pending sync)?
    #[must_use]
    pub fn standalone_fsync_due(&self) -> bool {
        self.sync_due
            && self.syncs_in_flight() < self.sync_pipeline_bound
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
            && self.sync_pipeline_bound > 1
            && self.syncs_in_flight() < self.sync_pipeline_bound - 1
            && self.written_bytes > self.durable_bytes
            && self.always_discharged_at_written()
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
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
    /// sealed segment's exclusive end — sound because rotation happens only
    /// with no write in flight (asserted: everything queued has completed).
    pub fn register_seal_fsync(&mut self, handoff: SealHandoff<File>, now: Nanos) -> FsyncTicket {
        assert_eq!(
            self.queued_bytes, self.written_bytes,
            "deferred seal with a frame write in flight — the lease serializes writes"
        );
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
        let (covers_up_to, covers_bytes) = self.pending.back().map_or_else(
            || {
                (
                    self.durable_up_to.unwrap_or(Lsn::new(crate::lsn::SegmentId(0), 0)),
                    self.durable_bytes,
                )
            },
            |tail| (tail.covers_up_to, tail.covers_bytes),
        );
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
        let (covers_up_to, covers_bytes) = self.pending.back().map_or_else(
            || {
                (
                    self.durable_up_to.unwrap_or(Lsn::new(crate::lsn::SegmentId(0), 0)),
                    self.durable_bytes,
                )
            },
            |tail| (tail.covers_up_to, tail.covers_bytes),
        );
        self.push_pending(
            covers_up_to,
            covers_bytes,
            now,
            SyncReason::ExtentSeal,
            HeldHandle::Extent(extent),
        )
    }

    /// This iteration's frame was handed to the driver (`LogWrite` at the
    /// slot's base). `end` is the frame's exclusive end LSN.
    pub fn note_frame_queued(&mut self, end: Lsn, frame_len: u32) {
        // Release assert (M2.5-S13): out-of-order queue breaks the LSN↔seq
        // FIFO the ack gate and reader rely on. Per-batch, free.
        assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
        self.queued_up_to = Some(end);
        self.queued_bytes += u64::from(frame_len);
        self.stats.frames_queued += 1;
        self.stats.frame_bytes_queued += u64::from(frame_len);
        // The seal drains the whole staging builder, so every record
        // staged before this LOG step now rides a queued frame.
        if self.always_unqueued {
            self.always_queued_up_to = Some(end);
            self.always_unqueued = false;
        }
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
        self.sync_due = self.always_pending && !self.always_discharged_at_written();
        self.always_pending = self.sync_due;
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
        self.sync_due = false;
        self.always_pending = false;
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

    /// The in-flight frame's `LogWritten` arrived: its bytes are in the
    /// page cache (NOT durable — release the staging lease, never ack).
    pub fn note_frame_written(&mut self) {
        self.written_up_to = self.queued_up_to;
        self.written_bytes = self.queued_bytes;
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

    /// fdatasync completion latency, microseconds (`fsync_latency_hist`).
    #[must_use]
    pub fn fsync_latency_hist(&self) -> &LogHistogram {
        &self.fsync_hist_us
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
        gc.note_frame_written();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
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
    fn everysec_tick_is_free_when_clean_and_dedupes_against_pending() {
        let mut gc = commit();
        gc.note_everysec_tick();
        assert!(!gc.standalone_fsync_due(), "nothing dirty: idle tick");
        assert_eq!(gc.stats().idle_ticks, 1);

        gc.note_frame_queued(lsn(0, 100), 100);
        gc.note_frame_written();
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
        gc.note_frame_written();
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
        let mut gc: GroupCommit<MemFile> = GroupCommit::with_sync_pipeline(2);
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 200), 100);
        assert!(gc.frame_fsync_due(), "second slot free at bound 2");
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 300), 100);
        assert!(!gc.frame_fsync_due(), "third due accumulates — bounded at 2");
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
        gc1.note_frame_written();
        gc1.note_staged(FsyncClass::Always);
        gc1.note_frame_queued(lsn(0, 200), 100);
        gc1.note_frame_written();
        gc1.on_fsync_complete(t, Nanos::from_micros(10));
        assert!(!gc1.completion_fsync_due(), "bound 1 never completion-issues");

        let mut gc2: GroupCommit<MemFile> = GroupCommit::with_sync_pipeline(2);
        gc2.note_staged(FsyncClass::Always);
        gc2.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc2.register_linked_fsync(Nanos::ZERO);
        gc2.note_frame_written();
        // Dues accumulate while the pipeline is at 1-in-flight; the frame
        // carrying them is written, so the due is fully dischargeable.
        gc2.note_staged(FsyncClass::Always);
        gc2.note_frame_queued(lsn(0, 200), 100);
        gc2.note_frame_written();
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
        gc.note_frame_written();
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
        gc.note_frame_written();
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
        gc.note_frame_written();
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
        gc.note_frame_written();
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
        gc.note_frame_queued(lsn(0, 100), 100);
        assert!(gc.frame_fsync_due());
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();

        // Ten more always-frames arrive while t1 is still in flight:
        // due stays owed but never issues.
        for i in 1..=10u32 {
            gc.note_staged(FsyncClass::Always);
            gc.note_frame_queued(lsn(0, 100 + i * 100), 100);
            assert!(!gc.frame_fsync_due(), "no second sync while one is in flight");
            assert!(!gc.standalone_fsync_due(), "no standalone either");
            gc.note_frame_written();
        }

        // t1 completes: the owed sync issues NOW and covers all ten
        // accumulated frames in one fdatasync — the batch.
        gc.on_fsync_complete(t1, Nanos::from_micros(5_000));
        assert!(gc.frame_fsync_due(), "deferred due issues at completion");
        gc.note_frame_queued(lsn(0, 1200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        assert_eq!(
            gc.on_fsync_complete(t2, Nanos::from_micros(10_000)),
            Some(lsn(0, 1200)),
            "one sync covers the whole accumulated batch"
        );
        assert_eq!(gc.stats().fsyncs_linked, 2, "12 frames, 2 fsyncs — not 12");
    }

    #[test]
    fn failed_fsync_freezes_the_watermark() {
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        gc.note_staged(FsyncClass::Always);
        gc.note_frame_queued(lsn(0, 200), 100);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        assert_eq!(gc.on_fsync_error(t1), SyncReason::Linked);
        // t2 completing can never advance past the failed t1.
        assert_eq!(gc.on_fsync_complete(t2, Nanos::ZERO), None);
        assert_eq!(gc.watermark(), None);
    }
}
