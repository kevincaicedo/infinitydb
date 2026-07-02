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
    pub fsyncs_completed: u64,
    /// everysec ticks that found nothing dirty (proves idle ticks are free).
    pub idle_ticks: u64,
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
    /// Deferred-seal write handle: dropped when the covering sync
    /// completes — seal durability and handle drop coincide (ADR-0013 D4).
    held_seal: Option<SealHandoff<File>>,
}

/// The group-commit engine of one cell. Single-threaded by design (L1);
/// time is injected (`now` parameters — L7); generic over the segment-file
/// tier only to hold [`SealHandoff`]s until their sync completes.
pub struct GroupCommit<File> {
    /// An fsync is due this iteration (everysec tick fired, or `always`
    /// traffic staged). Cleared by the registration that covers it.
    sync_due: bool,
    always_pending: bool,
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
        GroupCommit {
            sync_due: false,
            always_pending: false,
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

    /// Should this iteration's frame write chain an fdatasync?
    #[must_use]
    pub fn frame_fsync_due(&self) -> bool {
        self.sync_due
    }

    /// Is a standalone fdatasync due (sync owed, no frame to chain it to,
    /// dirty *written* bytes not already covered by a pending sync)?
    #[must_use]
    pub fn standalone_fsync_due(&self) -> bool {
        self.sync_due
            && self.written_bytes > self.durable_bytes
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
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
        self.push_pending(covers_up_to, self.queued_bytes, now, SyncReason::Seal, Some(handoff))
    }

    /// This iteration's frame was handed to the driver (`LogWrite` at the
    /// slot's base). `end` is the frame's exclusive end LSN.
    pub fn note_frame_queued(&mut self, end: Lsn, frame_len: u32) {
        debug_assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
        self.queued_up_to = Some(end);
        self.queued_bytes += u64::from(frame_len);
        self.stats.frames_queued += 1;
        self.stats.frame_bytes_queued += u64::from(frame_len);
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
        self.sync_due = false;
        self.always_pending = false;
        self.stats.fsyncs_linked += 1;
        self.push_pending(covers_up_to, self.queued_bytes, now, SyncReason::Linked, None)
    }

    /// An everysec-owed fdatasync with no frame to chain to. Covers what
    /// has *completed writing* — an fdatasync races in-flight writes.
    ///
    /// # Panics
    /// If nothing was ever written — callers check
    /// [`standalone_fsync_due`](Self::standalone_fsync_due).
    pub fn register_standalone_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.written_up_to.expect("standalone fsync with nothing written");
        self.sync_due = self.always_pending;
        self.stats.fsyncs_standalone += 1;
        self.push_pending(covers_up_to, self.written_bytes, now, SyncReason::Standalone, None)
    }

    fn push_pending(
        &mut self,
        covers_up_to: Lsn,
        covers_bytes: u64,
        now: Nanos,
        reason: SyncReason,
        held_seal: Option<SealHandoff<File>>,
    ) -> FsyncTicket {
        debug_assert!(
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
            held_seal,
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
        // Seal durability and write-handle drop coincide (ADR-0013 D4).
        entry.held_seal = None;
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
        gc.note_frame_queued(lsn(0, 200), 100);
        gc.note_staged(FsyncClass::Always);
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
    fn failed_fsync_freezes_the_watermark() {
        let mut gc = commit();
        gc.note_frame_queued(lsn(0, 100), 100);
        let t1 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        gc.note_frame_queued(lsn(0, 200), 100);
        gc.note_staged(FsyncClass::Always);
        let t2 = gc.register_linked_fsync(Nanos::ZERO);
        gc.note_frame_written();
        assert_eq!(gc.on_fsync_error(t1), SyncReason::Linked);
        // t2 completing can never advance past the failed t1.
        assert_eq!(gc.on_fsync_complete(t2, Nanos::ZERO), None);
        assert_eq!(gc.watermark(), None);
    }
}
