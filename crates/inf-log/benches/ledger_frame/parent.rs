//! The A leg of the ledger row: the fsync ledger as it was before its
//! classes, its frame queue and its ack map were capped — an unbounded
//! queue and ledger, the reorder window a compare beside them. Code only,
//! its rules stated in `inf_log::commit`; deleted once the row's verdict
//! is recorded.
#![allow(dead_code, reason = "the A leg runs one frame's path of the ledger")]

use core::fmt;
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::rc::Rc;

use inf_foundation::LogHistogram;
use inf_foundation::bounded::{CapCensus, CappedDeque, Reserve, Reserved};
use inf_foundation::limits::DEQUE_BACKING_BYTES_MAX;
use inf_foundation::time::Nanos;

use inf_log::Lsn;
use inf_log::MAX_FRAMES_IN_FLIGHT;
use inf_log::SealHandoff;
use inf_log::fs::SegmentFile;
use inf_log::limits::{EXTENT_SEAL_WINDOW, EXTENT_SEAL_WINDOW_ENTRIES};

pub const REORDER_WINDOW_FRAMES: usize = 2 * MAX_FRAMES_IN_FLIGHT as usize;

pub const WRITE_THROUGH_WINDOW_ENTRIES: usize = REORDER_WINDOW_FRAMES;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FsyncClass {
    Everysec,
    Always,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SyncReason {
    Linked,
    Seal,
    Standalone,
    Completion,
    BootBarrier,
    PreallocBarrier,
    ExtentSeal,
    WriteThrough,
    ZeroFill,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FsyncTicket(u64);

impl FsyncTicket {
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn from_u64(raw: u64) -> FsyncTicket {
        FsyncTicket(raw)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ExtentSync {
    pub ticket: FsyncTicket,
    pub fd: Option<RawFd>,
}

const _: () = assert!(
    EXTENT_SEAL_WINDOW_ENTRIES as usize * core::mem::size_of::<ExtentSync>()
        <= DEQUE_BACKING_BYTES_MAX,
    "the extent-seal window fits one deque allocation"
);

#[must_use = "a registration's effect ran, or no effect ran: the caller says which reply follows"]
pub enum ExtentRegistration<T, E> {
    Registered(FsyncTicket, T),
    Full,
    Refused(E),
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FrameId(pub u64);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FramePlan {
    Plain,
    WriteThrough,
    LinkedFsync,
    Wait,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    pub frames_queued: u64,
    pub frame_bytes_queued: u64,
    pub fsyncs_linked: u64,
    pub fsyncs_seal: u64,
    pub fsyncs_standalone: u64,
    pub fsyncs_completion: u64,
    pub fsyncs_boot_barrier: u64,
    pub fsyncs_prealloc_barrier: u64,
    pub fsyncs_write_through: u64,
    pub fsyncs_zero_fill: u64,
    pub fsyncs_completed: u64,
    pub idle_ticks: u64,
}

enum HeldHandle<File> {
    None,
    Seal(SealHandoff<File>),
    Dir(File),
    Extent(File),
}

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
    failed: bool,
    held: HeldHandle<File>,
}

pub struct GroupCommit<File> {
    sync_due: bool,
    always_pending: bool,
    always_unqueued: bool,
    everysec_unqueued: bool,
    always_queued_up_to: Option<Lsn>,
    flush_bound: usize,
    flush_in_flight: usize,
    write_through_pending: usize,
    queued_up_to: Option<Lsn>,
    queued_bytes: u64,
    queued: VecDeque<QueuedFrame>,
    unwritten: u8,
    next_frame_id: u64,
    written_up_to: Option<Lsn>,
    written_bytes: u64,
    durable_up_to: Option<Lsn>,
    durable_bytes: u64,
    pending: VecDeque<PendingFsync<File>>,
    next_ticket: u64,
    extent_window: CappedDeque<ExtentSync, Reserve>,
    extent_unsent: usize,
    fsync_hist_us: LogHistogram,
    write_through_hist_us: LogHistogram,
    stats: CommitStats,
}

impl<File> fmt::Debug for GroupCommit<File> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupCommit")
            .field("sync_due", &self.sync_due)
            .field("always_pending", &self.always_pending)
            .field("always_unqueued", &self.always_unqueued)
            .field("everysec_unqueued", &self.everysec_unqueued)
            .field("queued_up_to", &self.queued_up_to)
            .field("written_up_to", &self.written_up_to)
            .field("durable_up_to", &self.durable_up_to)
            .field("pending_fsyncs", &self.pending.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl<File: SegmentFile> GroupCommit<File> {
    #[must_use]
    pub fn new(census: &Rc<CapCensus>) -> GroupCommit<File> {
        GroupCommit::with_flush_bound(1, census)
    }

    #[must_use]
    pub fn with_flush_bound(bound: usize, census: &Rc<CapCensus>) -> GroupCommit<File> {
        assert!((1..=2).contains(&bound), "FLUSH-class bound is 1 or 2, never a queue");
        let extent_window = CappedDeque::new(EXTENT_SEAL_WINDOW, census)
            .unwrap_or_else(|refused| panic!("the extent-seal window refused: {refused}"));
        GroupCommit {
            sync_due: false,
            always_pending: false,
            always_unqueued: false,
            everysec_unqueued: false,
            always_queued_up_to: None,
            flush_bound: bound,
            flush_in_flight: 0,
            write_through_pending: 0,
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
            extent_window,
            extent_unsent: 0,
            fsync_hist_us: LogHistogram::new(),
            write_through_hist_us: LogHistogram::new(),
            stats: CommitStats::default(),
        }
    }

    pub fn note_staged(&mut self, class: FsyncClass) {
        match class {
            FsyncClass::Always => {
                self.always_pending = true;
                self.always_unqueued = true;
                self.sync_due = true;
            }
            FsyncClass::Everysec => self.everysec_unqueued = true,
        }
    }

    pub fn note_everysec_tick(&mut self) {
        if self.queued_bytes == self.durable_bytes
            && !self.always_pending
            && !self.everysec_unqueued
        {
            self.stats.idle_ticks += 1;
            return;
        }
        self.sync_due = true;
    }

    fn syncs_in_flight(&self) -> usize {
        debug_assert_eq!(
            self.flush_in_flight,
            self.pending
                .iter()
                .filter(|p| !p.done && !p.failed && p.reason != SyncReason::WriteThrough)
                .count(),
            "flush_in_flight tracks the ledger"
        );
        self.flush_in_flight
    }

    #[must_use]
    pub fn sync_due(&self) -> bool {
        self.sync_due
    }

    #[must_use]
    pub fn drained(&self) -> bool {
        self.written_bytes == self.queued_bytes
    }

    #[must_use]
    pub fn reorder_window_full(&self) -> bool {
        self.queued.len() >= REORDER_WINDOW_FRAMES
    }

    #[must_use]
    pub fn write_through_window_full(&self) -> bool {
        self.write_through_pending >= WRITE_THROUGH_WINDOW_ENTRIES
    }

    #[must_use]
    pub fn write_through_entries(&self) -> usize {
        self.write_through_pending
    }

    #[must_use]
    pub fn frame_fsync_due(&self) -> bool {
        self.sync_due && self.drained() && self.syncs_in_flight() < self.flush_bound
    }

    #[must_use]
    pub fn write_through_due(&self) -> bool {
        self.sync_due && self.coverage_tail().1 == self.queued_bytes
    }

    #[must_use]
    pub fn frame_plan(&self, write_through_ok: bool, seal_ahead: bool) -> FramePlan {
        if self.reorder_window_full() {
            return FramePlan::Wait;
        }
        if !self.sync_due {
            return FramePlan::Plain;
        }
        if write_through_ok
            && (seal_ahead || self.write_through_due())
            && !self.write_through_window_full()
        {
            return FramePlan::WriteThrough;
        }
        if !self.drained() {
            return FramePlan::Wait;
        }
        if self.syncs_in_flight() + usize::from(seal_ahead) < self.flush_bound {
            return FramePlan::LinkedFsync;
        }
        FramePlan::Plain
    }

    #[must_use]
    pub fn standalone_fsync_due(&self) -> bool {
        self.sync_due
            && self.syncs_in_flight() < self.flush_bound
            && self.written_bytes > self.durable_bytes
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
    }

    #[must_use]
    pub fn completion_fsync_due(&self) -> bool {
        self.sync_due
            && self.flush_bound > 1
            && self.syncs_in_flight() < self.flush_bound - 1
            && self.written_bytes > self.durable_bytes
            && self.always_discharged_at_written()
            && self.pending.back().is_none_or(|p| p.covers_bytes < self.written_bytes)
    }

    fn settle_due_at_written(&mut self) {
        let always_owed = self.always_pending && !self.always_discharged_at_written();
        let in_flight_uncovered = self.written_bytes < self.queued_bytes;
        self.sync_due = always_owed || in_flight_uncovered || self.everysec_unqueued;
        self.always_pending = always_owed;
    }

    fn always_discharged_at_written(&self) -> bool {
        if !self.always_pending {
            return true;
        }
        !self.always_unqueued
            && self
                .always_queued_up_to
                .is_some_and(|end| self.written_up_to.is_some_and(|written| end <= written))
    }

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

    pub fn register_extent<T, E>(
        &mut self,
        now: Nanos,
        effect: impl FnOnce() -> Result<(File, T), E>,
    ) -> ExtentRegistration<T, E> {
        let (covers_up_to, covers_bytes) = self.coverage_tail();
        let ticket = self.next_ticket();
        let Reserved::Slot(unit) = self.extent_window.reserve() else {
            return ExtentRegistration::Full;
        };
        let (extent, made) = match effect() {
            Ok(made) => made,
            Err(refused) => return ExtentRegistration::Refused(refused),
        };
        unit.publish(ExtentSync { ticket, fd: extent.raw_fd() });
        self.extent_unsent += 1;
        let minted = self.push_pending(
            covers_up_to,
            covers_bytes,
            now,
            SyncReason::ExtentSeal,
            HeldHandle::Extent(extent),
        );
        debug_assert_eq!(minted, ticket, "the window entry names the ticket the ledger minted");
        ExtentRegistration::Registered(ticket, made)
    }

    pub fn issue_extent_syncs(&mut self, mut issue: impl FnMut(ExtentSync)) {
        debug_assert!(self.extent_unsent <= self.extent_window.len(), "unsent entries are held");
        let issued = self.extent_window.len().saturating_sub(self.extent_unsent);
        for sync in self.extent_window.iter().skip(issued) {
            issue(*sync);
        }
        self.extent_unsent = 0;
    }

    #[must_use]
    pub fn extent_window_len(&self) -> usize {
        self.extent_window.len()
    }

    fn next_ticket(&self) -> FsyncTicket {
        FsyncTicket(self.next_ticket + 1)
    }

    fn fold_extent(&mut self, ticket: FsyncTicket) {
        let front = self.extent_window.pop_front();
        debug_assert_eq!(
            front.map(|sync| sync.ticket),
            Some(ticket),
            "the window mirrors the ledger's extent barriers in order"
        );
        debug_assert!(
            self.extent_unsent <= self.extent_window.len(),
            "a folded barrier was issued"
        );
    }

    pub fn register_zero_fill_barrier(&mut self, now: Nanos) -> FsyncTicket {
        let (covers_up_to, covers_bytes) = self.coverage_tail();
        self.stats.fsyncs_zero_fill += 1;
        self.push_pending(covers_up_to, covers_bytes, now, SyncReason::ZeroFill, HeldHandle::None)
    }

    fn coverage_tail(&self) -> (Lsn, u64) {
        self.pending.back().map_or_else(
            || {
                (
                    self.durable_up_to.unwrap_or(Lsn::new(inf_log::SegmentId(0), 0)),
                    self.durable_bytes,
                )
            },
            |tail| (tail.covers_up_to, tail.covers_bytes),
        )
    }

    pub fn note_frame_queued(&mut self, end: Lsn, frame_len: u32) -> FrameId {
        assert!(self.queued_up_to.is_none_or(|q| q < end), "frames queue in append order");
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
        if self.always_unqueued {
            self.always_queued_up_to = Some(end);
            self.always_unqueued = false;
        }
        self.everysec_unqueued = false;
        id
    }

    pub fn register_linked_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.queued_up_to.expect("linked fsync before any frame was queued");
        assert!(!self.always_unqueued, "linked sync with an unqueued always record");
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

    pub fn register_write_through(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.queued_up_to.expect("write-through before any frame was queued");
        assert!(!self.always_unqueued, "write-through with an unqueued always record");
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

    pub fn register_standalone_fsync(&mut self, now: Nanos) -> FsyncTicket {
        let covers_up_to = self.written_up_to.expect("standalone fsync with nothing written");
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
        assert!(
            self.pending.back().is_none_or(|p| p.covers_up_to <= covers_up_to),
            "fsync coverage must be monotone in submission order"
        );
        if matches!(reason, SyncReason::Linked | SyncReason::Standalone | SyncReason::Completion) {
            assert!(
                self.flush_in_flight < self.flush_bound,
                "FLUSH-class barrier into a full pipeline"
            );
        }
        if reason == SyncReason::WriteThrough {
            assert!(
                self.write_through_pending < WRITE_THROUGH_WINDOW_ENTRIES,
                "write-through ticket into a full window"
            );
            self.write_through_pending += 1;
        } else {
            self.flush_in_flight += 1;
        }
        let ticket = self.next_ticket();
        self.next_ticket = ticket.0;
        self.pending.push_back(PendingFsync {
            ticket: ticket.0,
            covers_up_to,
            covers_bytes,
            submitted_at: now,
            reason,
            done: false,
            failed: false,
            held,
        });
        debug_assert_eq!(
            self.write_through_pending,
            self.pending.iter().filter(|p| p.reason == SyncReason::WriteThrough).count(),
            "write-through count tracks the FIFO"
        );
        ticket
    }

    pub fn note_frame_written(&mut self, id: FrameId) {
        let front = self.queued.front().expect("LogWritten with nothing queued").id;
        let index = id.0.checked_sub(front.0).expect("LogWritten for a frame already written");
        let frame = usize::try_from(index)
            .ok()
            .and_then(|index| self.queued.get_mut(index))
            .expect("LogWritten for a frame that was not queued");
        debug_assert_eq!(frame.id, id, "queue ordinals are consecutive");
        assert!(!frame.written, "frame written twice");
        frame.written = true;
        self.unwritten = self.unwritten.checked_sub(1).expect("LogWritten with no unwritten frame");
        while let Some(front) = self.queued.front() {
            if !front.written {
                break;
            }
            self.written_up_to = Some(front.end);
            self.written_bytes = front.end_bytes;
            self.queued.pop_front();
        }
    }

    #[must_use]
    pub fn frames_unwritten(&self) -> usize {
        usize::from(self.unwritten)
    }

    #[must_use]
    pub fn frames_behind_prefix(&self) -> usize {
        self.queued.len()
    }

    fn pending_mut(&mut self, ticket: FsyncTicket) -> Option<&mut PendingFsync<File>> {
        let front = self.pending.front()?.ticket;
        let index = usize::try_from(ticket.0.checked_sub(front)?).ok()?;
        let entry = self.pending.get_mut(index)?;
        debug_assert_eq!(entry.ticket, ticket.0, "ticket ordinals are consecutive");
        Some(entry)
    }

    pub fn rebase_clock(&mut self, ticket: FsyncTicket, now: Nanos) {
        if let Some(entry) = self.pending_mut(ticket)
            && !entry.done
            && !entry.failed
        {
            debug_assert_eq!(entry.reason, SyncReason::Linked, "only linked syncs rebase");
            entry.submitted_at = now;
        }
    }

    pub fn on_fsync_complete(&mut self, ticket: FsyncTicket, now: Nanos) -> Option<Lsn> {
        let entry = self.pending_mut(ticket).expect("fsync completion for an unknown ticket");
        assert!(!entry.done, "fsync ticket completed twice");
        assert!(!entry.failed, "fsync ticket completed after erroring");
        entry.done = true;
        let reason = entry.reason;
        entry.held = HeldHandle::None;
        let elapsed = now.saturating_sub(entry.submitted_at);
        self.fsync_hist_us.record(elapsed.as_micros());
        if reason == SyncReason::WriteThrough {
            self.write_through_hist_us.record(elapsed.as_micros());
        } else {
            self.flush_in_flight -= 1;
        }
        self.stats.fsyncs_completed += 1;

        let before = self.durable_up_to;
        while let Some(front) = self.pending.front() {
            if !front.done || front.failed {
                break;
            }
            self.durable_up_to = Some(front.covers_up_to);
            self.durable_bytes = front.covers_bytes;
            let (reason, folded) = (front.reason, FsyncTicket(front.ticket));
            if reason == SyncReason::WriteThrough {
                self.write_through_pending -= 1;
            }
            self.pending.pop_front();
            if reason == SyncReason::ExtentSeal {
                self.fold_extent(folded);
            }
        }
        (self.durable_up_to != before).then(|| self.durable_up_to.expect("advanced past None"))
    }

    pub fn on_fsync_error(&mut self, ticket: FsyncTicket) -> SyncReason {
        let entry = self.pending_mut(ticket).expect("fsync error for an unknown ticket");
        assert!(!entry.done, "fsync ticket errored after completing");
        assert!(!entry.failed, "fsync ticket errored twice");
        entry.failed = true;
        let reason = entry.reason;
        if reason != SyncReason::WriteThrough {
            self.flush_in_flight -= 1;
        }
        reason
    }

    #[must_use]
    pub fn watermark(&self) -> Option<Lsn> {
        self.durable_up_to
    }

    #[must_use]
    pub fn queued_up_to(&self) -> Option<Lsn> {
        self.queued_up_to
    }

    #[must_use]
    pub fn extent_barrier_pending(&self) -> bool {
        self.pending.iter().any(|p| p.reason == SyncReason::ExtentSeal && !p.done)
    }

    #[must_use]
    pub fn pending_log_bytes(&self) -> u64 {
        self.queued_bytes - self.durable_bytes
    }

    #[must_use]
    pub fn pending_fsyncs(&self) -> usize {
        self.pending.iter().filter(|p| !p.done).count()
    }

    #[must_use]
    pub fn pending_entries(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn pending_covers_within(&self, lo: Lsn, hi: Lsn) -> bool {
        self.pending.iter().any(|p| lo <= p.covers_up_to && p.covers_up_to < hi)
    }

    #[must_use]
    pub fn written_up_to(&self) -> Option<Lsn> {
        self.written_up_to
    }

    #[must_use]
    pub fn fsync_latency_hist(&self) -> &LogHistogram {
        &self.fsync_hist_us
    }

    #[must_use]
    pub fn write_through_latency_hist(&self) -> &LogHistogram {
        &self.write_through_hist_us
    }

    #[must_use]
    pub fn write_through_in_flight(&self) -> usize {
        self.pending.iter().filter(|p| !p.done && p.reason == SyncReason::WriteThrough).count()
    }

    #[must_use]
    pub fn stats(&self) -> CommitStats {
        self.stats
    }
}
