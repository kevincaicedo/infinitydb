//! Tier-file retirement on the control thread (ADR-0156 D2–D3, ADR-0157):
//! the cell's job positions, the jobs and results that cross between a
//! cell and the control thread's worker, the worker's pass and wait, the
//! boot reservation of the transport, and the final close of a retired
//! tier file's handle.
//!
//! A cell offers a retired tier file's owner, its meta and at most one
//! handle, as a job into its request ring, holding one of its eight job
//! positions; the worker closes the handle once, through [`FinalClose`],
//! unlinks the path through [`SegmentFs::unlink_tier`] and returns the same
//! owner in a result; only the cell's consumption of that result frees the
//! position. A job and a result are values, never `Clone` and built only
//! here, so the value itself is the identity that crosses and returns: a
//! position is reused only after the one value naming it was consumed.
//!
//! | Job position | Event | Next |
//! |---|---|---|
//! | Free | an offer, a ring slot reserved first | Out |
//! | Out | the worker takes the request | Out (a dequeue frees nothing) |
//! | Out | its `Unlinked` or `AlreadyAbsent` consumed | Free, the owner dropped |
//! | Out | its `Retry` consumed | Free, the owner back on its position |
//!
//! Every value is built here, so per cell `Q + W + R ≤ 8` (requests
//! queued, in the worker's hand, results unconsumed): a result slot is free
//! whenever a job was taken, and the worker reserves it before it takes.
//!
//! The transport's stop (ADR-0156 D3, ADR-0157 D3–D4):
//!
//! | Transport | Event | Next |
//! |---|---|---|
//! | Open | the cell's stop | Closing: no offer from now, results still consumed |
//! | Closing | every position Free | Released: `closing` stored, both endpoints out, the bell |
//! | a lane | the worker reads `closing` | released: its two endpoints, then the cell's wake |
//! | Released | both endpoint counts read zero | Joined |
//! | Joined | each step | one block a ring, then Retired |
//!
//! A backing dropped before Retired fails stop (the ring's class-I `Drop`);
//! only the stop's rows, a process exit that runs no destructor, a panic's
//! unwind (which frees nothing) or a test host's cut (`test-support`) end a
//! part.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, fence};

use inf_fabric::{
    RETAINED_RING_SLOTS, Reservation, RetainedConsumer, RetainedProducer, RetainedRing, RingBlock,
    RingPhase, RingSteps,
};
use inf_foundation::grants::{
    BellWake, ClosingPayload, DirectoryPayload, DirectoryWriter, GrantDirectory, GrantPos,
    GrantsPayload, PayloadRefused, ReceiptBoard, ReceiptPayload, RetireBoard,
};
use inf_foundation::limits::TIER_RETIRE_JOBS_PER_CELL;
use inf_log::fs::mem::MemFile;
use inf_log::fs::sim::SimFile;
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFile};
use inf_log::{NsId, TierFileMeta};

use crate::limits::{RETIRE_WORKER_JOBS_PER_PASS, RETIRE_WORKER_VISITS_PER_PASS};

const _: () = assert!(RETAINED_RING_SLOTS == TIER_RETIRE_JOBS_PER_CELL, "a slot a job position");
const _: () = assert!(TIER_RETIRE_JOBS_PER_CELL <= u8::MAX as usize, "a job position fits a u8");

/// A tier file's handle closed once, by its final owner (ADR-0156 D3): the
/// handle is consumed on every outcome, so nothing can close it again.
pub trait FinalClose {
    /// Closes the handle and reports the close's own error.
    ///
    /// # Errors
    /// The close's error: an errno `close(2)` reported, or the simulated
    /// disk's `EIO` from `tier_close_fail`. The handle is gone either way.
    fn final_close(self) -> io::Result<()>;
}

impl FinalClose for StdSegmentFile {
    fn final_close(self) -> io::Result<()> {
        inf_runtime::net::close_reporting(self.into_owned_fd())
    }
}

impl FinalClose for SimFile {
    fn final_close(self) -> io::Result<()> {
        self.close_reporting()
    }
}

impl FinalClose for MemFile {
    fn final_close(self) -> io::Result<()> {
        drop(self);
        Ok(())
    }
}

// ---- the jobs and their results ----------------------------------------

/// One of a cell's job positions, below [`TIER_RETIRE_JOBS_PER_CELL`]: made
/// only by the cell's job table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct JobPos(u8);

/// What an Out position names: the owner's grant position, its namespace
/// and its file id, which its result must match at consumption.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct JobKey {
    pos: GrantPos,
    ns: NsId,
    file: u32,
}

/// A cell's job positions (ADR-0156 D2): each Free or Out from its offer to
/// its result's consumption, the only event that frees it.
#[derive(Debug)]
pub(crate) struct RetireJobs {
    slots: [Option<JobKey>; TIER_RETIRE_JOBS_PER_CELL],
    out: u8,
}

impl RetireJobs {
    fn new() -> RetireJobs {
        RetireJobs { slots: [None; TIER_RETIRE_JOBS_PER_CELL], out: 0 }
    }

    /// Positions Out: jobs queued, in the worker's hand or with their
    /// results unconsumed.
    #[must_use]
    pub fn out(&self) -> usize {
        usize::from(self.out)
    }

    /// The lowest Free position.
    fn free(&self) -> Option<JobPos> {
        let at = self.slots.iter().position(Option::is_none)?;
        // `at < TIER_RETIRE_JOBS_PER_CELL ≤ u8::MAX` (the const assertion).
        Some(JobPos(at as u8))
    }

    fn take(&mut self, at: JobPos, key: JobKey) {
        let slot = &mut self.slots[usize::from(at.0)];
        debug_assert!(slot.is_none(), "an offer takes a Free position");
        *slot = Some(key);
        self.out += 1;
    }

    /// Frees `at` when it names `key`: a result's consumption. `false` for a
    /// result whose identity the position does not hold, which custody
    /// makes unreachable and which changes nothing.
    fn release(&mut self, at: JobPos, key: JobKey) -> bool {
        let slot = &mut self.slots[usize::from(at.0)];
        if *slot != Some(key) {
            return false;
        }
        *slot = None;
        self.out -= 1;
        true
    }
}

/// A retired tier file's owner on its way to the worker: its meta, at most
/// one handle, and the job position it holds. Not `Clone`; built and opened
/// only in this module.
///
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<inf_server::RetireJob<inf_log::fs::mem::MemFile>>();
/// ```
pub struct RetireJob<J> {
    job: JobPos,
    key: JobKey,
    meta: TierFileMeta,
    handle: Option<J>,
}

/// The worker's answer for a job: the same owner, its handle closed, and the
/// outcome. Not `Clone`; built only by the worker's run, opened only by the
/// cell's consumption, so a result is consumed once.
///
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<inf_server::RetireResult>();
/// ```
///
/// ```compile_fail,E0451
/// fn forge(job: inf_server::JobPos, meta: inf_log::TierFileMeta) -> inf_server::RetireResult {
///     inf_server::RetireResult { job, meta, outcome: inf_server::RetireOutcome::Unlinked }
/// }
/// ```
pub struct RetireResult {
    job: JobPos,
    key: JobKey,
    meta: TierFileMeta,
    outcome: RetireOutcome,
}

/// How a job ended (ADR-0156 D3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RetireOutcome {
    /// The file was unlinked: terminal.
    Unlinked,
    /// No file had the name (`NotFound`): terminal under F11's non-reuse.
    AlreadyAbsent,
    /// The close or the unlink failed: the owner returns, its handle closed.
    Retry(RetryCause),
}

/// The failed step of a `Retry`, with its error's kind and OS errno (none
/// for an injected point).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RetryCause {
    Close { kind: io::ErrorKind, errno: Option<i32> },
    Unlink { kind: io::ErrorKind, errno: Option<i32> },
}

impl RetryCause {
    fn of(error: &io::Error, close: bool) -> RetryCause {
        let (kind, errno) = (error.kind(), error.raw_os_error());
        if close { RetryCause::Close { kind, errno } } else { RetryCause::Unlink { kind, errno } }
    }

    /// A transient class (`EINTR`, `EAGAIN`, `EBUSY`, `ENOMEM`); every other
    /// error, one with no OS errno included, is the operator's class.
    #[must_use]
    pub fn transient(self) -> bool {
        let (RetryCause::Close { kind, .. } | RetryCause::Unlink { kind, .. }) = self;
        matches!(
            kind,
            io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::ResourceBusy
                | io::ErrorKind::OutOfMemory
        )
    }
}

/// A consumed result, as the cell's position sees it.
pub(crate) struct Consumed {
    pub(crate) pos: GrantPos,
    pub(crate) ns: NsId,
    pub(crate) meta: TierFileMeta,
    pub(crate) outcome: RetireOutcome,
}

// ---- the cell's part ----------------------------------------------------

/// Where a cell's transport stands in its stop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransportPhase {
    /// Serving: offers and results.
    Open,
    /// Stopping: no offer; results consumed until every position is Free.
    Closing,
    /// Both cell endpoints released: waiting for the worker's lane.
    Released,
    /// Every endpoint released and joined: the backings retire a block a
    /// step.
    Joined,
    /// Both backings retired: the cell may drop its part.
    Retired,
}

/// One cell's half of the transport (ADR-0156 D2, ADR-0157): its job
/// positions, the request producer, the result consumer and both rings'
/// backing owners. Built only by the boot reservation; held in the plane,
/// then in the cell's `CellGrants`, until its stop retires it.
pub struct CellPart<J> {
    cell: u16,
    jobs: RetireJobs,
    requests: Option<RetainedProducer<RetireJob<J>>>,
    results: Option<RetainedConsumer<RetireResult>>,
    request_ring: RetainedRing<RetireJob<J>>,
    result_ring: RetainedRing<RetireResult>,
    phase: TransportPhase,
}

/// A turn's offers on one cell (ADR-0157 D1): ring slots reserved before
/// any owner moves, one Free position an offer; dropping it publishes what
/// it wrote with one `Release` store.
pub(crate) struct OfferBatch<'a, J> {
    reservation: Option<Reservation<'a, RetireJob<J>>>,
    jobs: &'a mut RetireJobs,
    offered: usize,
}

impl<J> OfferBatch<'_, J> {
    /// Whether an offer has a position and a slot: checked before the
    /// owner leaves its list, so a deferred offer moves nothing.
    pub(crate) fn room(&self) -> bool {
        let slot = self.reservation.as_ref().is_some_and(|r| r.remaining() > 0);
        // Canary `inf_canary_retire_credit_at_dequeue`: a position counts as
        // Free once the worker took its request (a free ring slot), so a
        // ninth owner is admitted while eight results are unconsumed.
        if cfg!(inf_canary_retire_credit_at_dequeue) {
            return slot;
        }
        slot && self.jobs.free().is_some()
    }

    /// Offers one owner into the batch; [`room`](Self::room) first. With no
    /// room the owner comes back whole.
    pub(crate) fn offer(
        &mut self,
        pos: GrantPos,
        ns: NsId,
        meta: TierFileMeta,
        handle: Option<J>,
    ) -> Result<(), (TierFileMeta, Option<J>)> {
        if !self.room() {
            return Err((meta, handle));
        }
        let Some(reservation) = self.reservation.as_mut() else { return Err((meta, handle)) };
        let Some(permit) = reservation.permit() else { return Err((meta, handle)) };
        let job = self.jobs.free().unwrap_or(JobPos(0));
        let key = JobKey { pos, ns, file: meta.id };
        if self.jobs.slots[usize::from(job.0)].is_none() {
            self.jobs.take(job, key);
        }
        permit.write(RetireJob { job, key, meta, handle });
        self.offered += 1;
        Ok(())
    }

    /// Publishes the batch (one `Release` store); returns the jobs offered.
    pub(crate) fn publish(self) -> usize {
        self.offered
    }
}

impl<J> CellPart<J> {
    /// The cell this part is.
    #[must_use]
    pub fn cell(&self) -> u16 {
        self.cell
    }

    /// Job positions Out (`tier_retire_jobs_out`).
    #[must_use]
    pub fn jobs_out(&self) -> usize {
        self.jobs.out()
    }

    /// Where the transport stands.
    #[must_use]
    pub fn phase(&self) -> TransportPhase {
        self.phase
    }

    /// A turn's offers: as many ring slots as positions are Free, none once
    /// the stop began.
    pub(crate) fn offers(&mut self) -> OfferBatch<'_, J> {
        // The planted rule counts ring slots, not positions (canary
        // `inf_canary_retire_credit_at_dequeue`, below).
        let free = if cfg!(inf_canary_retire_credit_at_dequeue) {
            TIER_RETIRE_JOBS_PER_CELL
        } else {
            TIER_RETIRE_JOBS_PER_CELL - self.jobs.out()
        };
        let open = self.phase == TransportPhase::Open;
        let CellPart { requests, jobs, .. } = self;
        let reservation = requests.as_mut().filter(|_| open).map(|r| r.reserve(free));
        OfferBatch { reservation, jobs, offered: 0 }
    }

    /// Consumes the ready results, at most one a position, in order: each
    /// frees its position and goes to `consume`. A result whose identity
    /// its position does not hold is counted in the return's second half
    /// and changes nothing (unreachable while only this module builds
    /// them).
    pub(crate) fn consume(&mut self, mut consume: impl FnMut(Consumed)) -> (usize, usize) {
        let Some(results) = self.results.as_mut() else { return (0, 0) };
        let jobs = &mut self.jobs;
        let mut stale = 0;
        let taken = results.consume_batch(TIER_RETIRE_JOBS_PER_CELL, |result| {
            let RetireResult { job, key, meta, outcome } = result;
            if !jobs.release(job, key) {
                stale += 1;
                return;
            }
            consume(Consumed { pos: key.pos, ns: key.ns, meta, outcome });
        });
        (taken, stale)
    }

    /// Whether a result waits while a position is Out: the parking cell's
    /// final check after it announces its park (the worker's result wake
    /// reads that announcement after its publish).
    pub(crate) fn results_ready(&mut self) -> bool {
        self.jobs.out() > 0 && self.results.as_mut().is_some_and(RetainedConsumer::ready)
    }

    /// The stop's rows, one step a call, never waiting: Open enters
    /// Closing; Closing, once every position is Free, stores the cell's
    /// `closing`, releases both cell endpoints and rings the bell;
    /// Released joins once both endpoint counts read zero; Joined retires
    /// one block a ring. Results are consumed by the caller's turn first.
    pub(crate) fn stop_step(
        &mut self,
        board: &RetireBoard,
        wake: &impl BellWake,
    ) -> TransportPhase {
        match self.phase {
            TransportPhase::Open => self.phase = TransportPhase::Closing,
            TransportPhase::Closing => self.release_endpoints(board, wake),
            TransportPhase::Released => self.join_step(),
            TransportPhase::Joined => self.retire_step(),
            TransportPhase::Retired => {}
        }
        self.phase
    }

    fn release_endpoints(&mut self, board: &RetireBoard, wake: &impl BellWake) {
        // Canary `inf_canary_retire_join_early`: the endpoints released with
        // a result unconsumed, which then strands in its ring.
        if self.jobs.out() > 0 && !cfg!(inf_canary_retire_join_early) {
            return;
        }
        board.close(usize::from(self.cell));
        self.requests = None;
        self.results = None;
        board.bell().ring(wake);
        self.phase = TransportPhase::Released;
    }

    fn join_step(&mut self) {
        // Canary `inf_canary_stop_waits_worker`: the stop waits for the
        // worker's release in a loop instead of one load a turn, so a held
        // worker holds the cell past its stop deadline.
        while cfg!(inf_canary_stop_waits_worker)
            && (matches!(self.request_ring.join(), RingPhase::Live)
                || matches!(self.result_ring.join(), RingPhase::Live))
        {
            std::hint::spin_loop();
        }
        // One `Acquire` load each: a nonzero count changes nothing.
        let requests = self.request_ring.join();
        let results = self.result_ring.join();
        if !matches!(requests, RingPhase::Live) && !matches!(results, RingPhase::Live) {
            self.phase = TransportPhase::Joined;
        }
    }

    fn retire_step(&mut self) {
        let mut steps = RingSteps::new(2);
        let requests = self.request_ring.retire_step(&mut steps, strand);
        let results = self.result_ring.retire_step(&mut steps, strand);
        if matches!(requests, RingPhase::Terminal(_)) && matches!(results, RingPhase::Terminal(_)) {
            self.phase = TransportPhase::Retired;
        }
    }
}

/// A value found queued at a stop's join: none on a correct build, since
/// every position was Free before the release. Dropped, its handle closed
/// with it.
fn strand<T>(value: T) {
    debug_assert!(false, "STRANDED: a value queued at the stop's join");
    drop(value);
}

// ---- the worker ----------------------------------------------------------

/// The worker's half of one cell's transport: the request consumer and the
/// result producer, until the cell's `closing` releases them.
pub struct RetireLane<J> {
    cell: u16,
    ends: Option<LaneEnds<J>>,
    /// The canary's remembered number of a close that failed.
    #[cfg(inf_canary_retire_close_twice)]
    failed_close: Option<std::os::fd::RawFd>,
}

struct LaneEnds<J> {
    requests: RetainedConsumer<RetireJob<J>>,
    results: RetainedProducer<RetireResult>,
}

/// How the worker ends a parked cell's park after a publish to it: the
/// park board's flags and each cell's loop waker, as the fabric's doorbell
/// does (M0-R1).
pub struct CellWake {
    flags: Arc<Vec<AtomicBool>>,
    wake: Box<dyn Fn(u16) + Send>,
}

impl CellWake {
    /// The park board's `flags` and the call that wakes cell `n`.
    pub fn new(flags: Arc<Vec<AtomicBool>>, wake: impl Fn(u16) + Send + 'static) -> CellWake {
        CellWake { flags, wake: Box::new(wake) }
    }
}

/// The control thread's receive side as the worker's turn sees it: one
/// message without waiting, and the wait on the one-slot wake.
pub(crate) trait WorkerInbox {
    type Msg;
    /// One queued message, if any; `None` too once every sender is gone,
    /// which [`disconnected`](Self::disconnected) then reports.
    fn try_recv(&mut self) -> Option<Self::Msg>;
    /// Waits for the wake (or its timeout, while the boot narrates).
    fn wait(&mut self);
    /// Every sender is gone: the node ended.
    fn disconnected(&self) -> bool;
}

/// The control thread's retirement worker (ADR-0156 D3): one lane a cell,
/// visited cyclically from `cursor`, `idle` the consecutive visits that took
/// nothing, carried across passes.
pub struct RetireWorker<J> {
    lanes: Vec<RetireLane<J>>,
    board: Arc<RetireBoard>,
    cursor: usize,
    idle: usize,
    cell_wake: Option<CellWake>,
    /// The planted rule's last pass took a job.
    #[cfg(inf_canary_retire_block_after_empty_pass)]
    last_took: bool,
}

impl<J: SegmentFile + FinalClose> RetireWorker<J> {
    fn new(lanes: Vec<RetireLane<J>>, board: Arc<RetireBoard>) -> RetireWorker<J> {
        RetireWorker {
            lanes,
            board,
            cursor: 0,
            idle: 0,
            cell_wake: None,
            #[cfg(inf_canary_retire_block_after_empty_pass)]
            last_took: true,
        }
    }

    /// Wires the result wake: after each publish to a cell, and after a
    /// lane's release, a `SeqCst` fence, then the cell's park flag; set, its
    /// wake (the fabric's doorbell handshake).
    pub fn set_cell_wake(&mut self, wake: CellWake) {
        self.cell_wake = Some(wake);
    }

    /// The lanes, one a cell.
    #[must_use]
    pub fn lanes(&self) -> usize {
        self.lanes.len()
    }

    /// One iteration of the control loop: at most one message, handed to
    /// `handle`, the wait only after a whole circuit of empty visits since
    /// the last job and the last set bell
    /// ([`inf_foundation::grants::Bell::worker_turn`]), then a pass unless
    /// that circuit is still empty. Returns whether every sender is gone.
    pub(crate) fn iterate<I: WorkerInbox, F: SegmentFs<File = J>>(
        &mut self,
        inbox: &mut I,
        fs: &F,
        handle: impl FnOnce(I::Msg),
    ) -> bool {
        let circuit_empty = self.circuit_empty();
        let board = Arc::clone(&self.board);
        let turn = {
            let inbox = &mut *inbox;
            let inbox = core::cell::RefCell::new(inbox);
            board.bell().worker_turn(
                circuit_empty,
                || inbox.borrow_mut().try_recv(),
                || inbox.borrow_mut().wait(),
            )
        };
        if turn.rung {
            self.idle = 0;
        }
        if let Some(message) = turn.message {
            handle(message);
        }
        if !self.circuit_empty() {
            self.pass(fs);
        }
        inbox.disconnected()
    }

    /// Whether the worker may wait: a whole circuit of empty visits.
    fn circuit_empty(&self) -> bool {
        // Canary `inf_canary_retire_worker_never_waits`: the worker never
        // waits, a busy loop on an idle node.
        if cfg!(inf_canary_retire_worker_never_waits) {
            return false;
        }
        // Canary `inf_canary_retire_block_after_empty_pass`: the loop waits
        // after any pass that took no job, before a whole circuit.
        #[cfg(inf_canary_retire_block_after_empty_pass)]
        {
            !self.last_took
        }
        #[cfg(not(inf_canary_retire_block_after_empty_pass))]
        {
            self.idle >= self.lanes.len()
        }
    }

    /// One pass: lanes visited cyclically from `cursor` until 64 visits, 8
    /// jobs or a whole circuit of empty visits; each visit reserves a result
    /// slot first, then takes the lane's front request and runs it. Returns
    /// the jobs it ran.
    pub(crate) fn pass<F: SegmentFs<File = J>>(&mut self, fs: &F) -> usize {
        let lanes = self.lanes.len();
        // Canary `inf_canary_retire_cursor_reset`: each pass starts at lane
        // 0, so a later lane waits behind every earlier lane's work.
        if cfg!(inf_canary_retire_cursor_reset) {
            self.cursor = 0;
        }
        let (mut visits, mut jobs) = (0, 0);
        while lanes > 0
            && visits < RETIRE_WORKER_VISITS_PER_PASS
            && jobs < RETIRE_WORKER_JOBS_PER_PASS
            && self.idle < lanes
        {
            let at = self.cursor;
            self.cursor = (at + 1) % lanes;
            visits += 1;
            if self.visit(at, fs) {
                jobs += 1;
                self.idle = 0;
            } else {
                self.idle += 1;
            }
        }
        #[cfg(inf_canary_retire_block_after_empty_pass)]
        {
            self.last_took = jobs > 0;
        }
        self.board.note_pass(jobs as u64);
        jobs
    }

    /// One lane's visit: `true` when it ran a job.
    fn visit<F: SegmentFs<File = J>>(&mut self, at: usize, fs: &F) -> bool {
        let lane = &mut self.lanes[at];
        let cell = lane.cell;
        let Some(ends) = lane.ends.as_mut() else { return false };
        if self.board.closing(usize::from(cell)) {
            // Every position of the cell is Free before its `closing`, so
            // the lane holds no value: its release frees nothing.
            lane.ends = None;
            wake_cell(self.cell_wake.as_ref(), cell);
            return false;
        }
        let mut reservation = ends.results.reserve(1);
        let Some(permit) = reservation.permit() else { return false };
        let mut permit = Some(permit);
        #[cfg(inf_canary_retire_close_twice)]
        let failed_close = &mut lane.failed_close;
        let taken = ends.requests.consume_batch(1, |job| {
            #[cfg(inf_canary_retire_close_twice)]
            let number = close_again_planted(&job, failed_close);
            let result = run(job, fs);
            // Canary `inf_canary_retire_result_dropped`: a `Retry` result is
            // dropped, its owner with it, never consumed.
            if cfg!(inf_canary_retire_result_dropped)
                && matches!(result.outcome, RetireOutcome::Retry(_))
            {
                return;
            }
            #[cfg(inf_canary_retire_close_twice)]
            if let RetireOutcome::Retry(RetryCause::Close { .. }) = result.outcome {
                *failed_close = number;
            }
            if let Some(permit) = permit.take() {
                permit.write(result);
            }
        });
        drop(reservation);
        if taken == 0 {
            return false;
        }
        // Canary `inf_canary_retire_result_wake_silent`: the result is
        // published with no wake, so a parked cell sleeps past it.
        if !cfg!(inf_canary_retire_result_wake_silent) {
            wake_cell(self.cell_wake.as_ref(), cell);
        }
        true
    }
}

/// The cell's wake after a publish to it or a lane's release: the fence
/// pairs with the cell's park handshake (flag store, fence, its final
/// check), so either the worker sees the flag or the cell sees the result.
fn wake_cell(wake: Option<&CellWake>, cell: u16) {
    let Some(wake) = wake else { return };
    fence(Ordering::SeqCst);
    if wake.flags.get(usize::from(cell)).is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        (wake.wake)(cell);
    }
}

/// One job: its handle closed once, through its final close, consumed on
/// every outcome (an error is `Retry(Close)`, no unlink this attempt); then
/// its path unlinked: `Ok` is `Unlinked`, `NotFound` `AlreadyAbsent`, any
/// other error `Retry(Unlink)`.
fn run<J: FinalClose, F: SegmentFs<File = J>>(job: RetireJob<J>, fs: &F) -> RetireResult {
    let RetireJob { job, key, meta, handle } = job;
    let outcome = match handle.map(FinalClose::final_close) {
        Some(Err(error)) => RetireOutcome::Retry(RetryCause::of(&error, true)),
        Some(Ok(())) | None => match fs.unlink_tier(&meta.path) {
            Ok(()) => RetireOutcome::Unlinked,
            Err(error) if error.kind() == io::ErrorKind::NotFound => RetireOutcome::AlreadyAbsent,
            Err(error) => RetireOutcome::Retry(RetryCause::of(&error, false)),
        },
    };
    RetireResult { job, key, meta, outcome }
}

/// The canary's double close: a retry of a file whose close failed (it has
/// no handle now) closes the number that close had again, which a later
/// open may have reused. Returns the job's own number, read before its
/// close, for the lane to remember should that close fail.
#[cfg(inf_canary_retire_close_twice)]
fn close_again_planted<J: SegmentFile>(
    job: &RetireJob<J>,
    failed: &mut Option<std::os::fd::RawFd>,
) -> Option<std::os::fd::RawFd> {
    if job.handle.is_none()
        && let Some(number) = failed.take()
    {
        inf_runtime::net::close_number_planted(number);
    }
    job.handle.as_ref().and_then(SegmentFile::raw_fd)
}

// ---- the boot reservation --------------------------------------------

/// What the boot reservation's node and cells share with no ring
/// (ADR-0148 D4, ADR-0156 D3): the grant directory, the receipts and the
/// retirement board, three `Arc`s made after every payload.
#[derive(Clone)]
pub struct TierShared {
    pub(crate) directory: Arc<GrantDirectory>,
    pub(crate) receipts: Arc<ReceiptBoard>,
    pub(crate) board: Arc<RetireBoard>,
}

/// The term of the boot reservation a refusal names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BootTerm {
    /// A payload of the grants or the board.
    Payload(GrantsPayload),
    /// The worker's lanes, one a cell.
    Lanes,
    /// The cells' parts, one a cell.
    Parts,
    /// A cell's request ring, one of its blocks.
    RequestRing(RingBlock),
    /// A cell's result ring, one of its blocks.
    ResultRing(RingBlock),
}

/// The boot reservation was refused (ADR-0148 D4): its term and the bytes
/// it asked (`None` when the size does not fit a `usize`). What was built
/// before it was retired; no cell exists and nothing listens.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BootReservation {
    pub term: BootTerm,
    pub bytes: Option<usize>,
}

impl core::fmt::Display for BootReservation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bytes = self.bytes.map_or_else(|| "more than usize::MAX".to_owned(), |b| b.to_string());
        write!(
            f,
            "the tiered namespaces' boot storage could not be reserved: {:?} of {bytes} bytes \
             (ADR-0148 D4); nothing is served",
            self.term
        )
    }
}

impl From<PayloadRefused> for BootReservation {
    fn from(refused: PayloadRefused) -> BootReservation {
        BootReservation { term: BootTerm::Payload(refused.payload), bytes: refused.bytes }
    }
}

/// What the boot reservation hands the control plane: the shared half, the
/// directory's one writer, the worker with every lane, and every cell's
/// part, in cell order.
pub struct TierBoot<J> {
    pub(crate) shared: TierShared,
    pub(crate) writer: DirectoryWriter,
    pub(crate) worker: RetireWorker<J>,
    pub(crate) parts: Vec<CellPart<J>>,
}

impl<J: SegmentFile + FinalClose> TierBoot<J> {
    /// The worker and the parts as a detached host holds them, whose drop
    /// (under `test-support`) cuts every part: the boot test's end.
    #[must_use]
    pub fn into_detached(self) -> DetachedTier<J> {
        DetachedTier::new(self.worker, self.parts)
    }
}

/// The node's tier-lifetime storage at boot (ADR-0148 D4), before any cell.
pub struct TierLifetimeBoot;

impl TierLifetimeBoot {
    /// Reserves every payload and each cell's two rings by checked
    /// arithmetic and fallible allocation, in this order: the directory's
    /// slots, the receipts and their writers' flags, the cells' `closing`
    /// words, the lanes and the parts (one `Vec` each, exact), then each
    /// cell's request and result rings (a slot block and a header each,
    /// ADR-0157 D2's steps); then the three `Arc` headers, its only
    /// infallible allocations. A refusal at any step retires every ring
    /// built so far and returns its term; nothing else of it survives.
    ///
    /// # Errors
    /// [`BootReservation`], the refused term and its bytes.
    pub fn reserve<J: SegmentFile + FinalClose>(
        cells: u16,
    ) -> Result<TierBoot<J>, BootReservation> {
        let directory = DirectoryPayload::try_new()?;
        let receipts = ReceiptPayload::try_new(cells)?;
        let closing = ClosingPayload::try_new(cells)?;
        let count = usize::from(cells);
        let lanes = reserved_vec::<RetireLane<J>>(count, BootTerm::Lanes)?;
        let mut parts = reserved_vec::<CellPart<J>>(count, BootTerm::Parts)?;
        for cell in 0..cells {
            match build_part(cell) {
                Ok(part) => parts.push(part),
                Err(refused) => {
                    retire_built(&mut parts);
                    return Err(refused);
                }
            }
        }
        let mut lanes = lanes;
        for part in &mut parts {
            lanes.push(part.split_lane());
        }
        let (directory, writer) = GrantDirectory::new(directory);
        let shared = TierShared {
            directory,
            receipts: ReceiptBoard::new(receipts),
            board: RetireBoard::new(closing),
        };
        let worker = RetireWorker::new(lanes, Arc::clone(&shared.board));
        Ok(TierBoot { shared, writer, worker, parts })
    }
}

/// An empty vector of exactly `len` reserved, fallibly, its bytes checked.
fn reserved_vec<T>(len: usize, term: BootTerm) -> Result<Vec<T>, BootReservation> {
    let refused = BootReservation { term, bytes: len.checked_mul(size_of::<T>()) };
    if refused.bytes.is_none() {
        return Err(refused);
    }
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|_| refused)?;
    Ok(values)
}

/// A cell's part with both rings built to Ready, not yet split.
fn build_part<J>(cell: u16) -> Result<CellPart<J>, BootReservation> {
    let mut request_ring = admit_ring(BootTerm::RequestRing)?;
    if let Err(refused) = build_ring(&mut request_ring, BootTerm::RequestRing) {
        retire_ring(&mut request_ring);
        return Err(refused);
    }
    let mut result_ring = match admit_ring(BootTerm::ResultRing) {
        Ok(ring) => ring,
        Err(refused) => {
            retire_ring(&mut request_ring);
            return Err(refused);
        }
    };
    if let Err(refused) = build_ring(&mut result_ring, BootTerm::ResultRing) {
        retire_ring(&mut result_ring);
        retire_ring(&mut request_ring);
        return Err(refused);
    }
    Ok(CellPart {
        cell,
        jobs: RetireJobs::new(),
        requests: None,
        results: None,
        request_ring,
        result_ring,
        phase: TransportPhase::Open,
    })
}

fn admit_ring<T>(term: fn(RingBlock) -> BootTerm) -> Result<RetainedRing<T>, BootReservation> {
    RetainedRing::admit()
        .map_err(|refused| BootReservation { term: term(refused.block), bytes: refused.bytes })
}

/// Builds `ring` to Ready in its three steps: the slot block, its slots,
/// the header. A refused block's term and bytes on refusal.
fn build_ring<T>(
    ring: &mut RetainedRing<T>,
    term: fn(RingBlock) -> BootTerm,
) -> Result<(), BootReservation> {
    let slots = RETAINED_RING_SLOTS * size_of::<T>();
    for step in 0..3 {
        match ring.build_step(&mut RingSteps::new(1)) {
            RingPhase::Building | RingPhase::Ready => {}
            RingPhase::Live | RingPhase::Retiring(_) | RingPhase::Terminal(_) => {
                let (block, bytes) = if step == 0 && slots > 0 {
                    (RingBlock::Slots, slots)
                } else {
                    (RingBlock::Header, ring.requested_bytes() - slots)
                };
                return Err(BootReservation { term: term(block), bytes: Some(bytes) });
            }
        }
    }
    debug_assert_eq!(ring.phase(), RingPhase::Ready, "three steps build a ring");
    Ok(())
}

/// Retires an unsplit ring to Terminal: cancelled if it was not refused,
/// then its blocks freed a step at a time (it holds no value).
fn retire_ring<T>(ring: &mut RetainedRing<T>) {
    ring.cancel();
    for _ in 0..4 {
        if matches!(ring.retire_step(&mut RingSteps::new(1), strand), RingPhase::Terminal(_)) {
            return;
        }
    }
    debug_assert!(false, "an unsplit ring retires in at most two steps");
}

/// Every part built before a refusal, retired: their rings were never split.
fn retire_built<J>(parts: &mut Vec<CellPart<J>>) {
    for part in parts.iter_mut() {
        retire_ring(&mut part.request_ring);
        retire_ring(&mut part.result_ring);
        part.phase = TransportPhase::Retired;
    }
    parts.clear();
}

impl<J> CellPart<J> {
    /// Splits both Ready rings: the cell keeps the request producer and the
    /// result consumer, the worker's lane takes the other two.
    fn split_lane(&mut self) -> RetireLane<J> {
        let requests = self.request_ring.split();
        let results = self.result_ring.split();
        let ends = match (requests, results) {
            (Some((request_tx, request_rx)), Some((result_tx, result_rx))) => {
                self.requests = Some(request_tx);
                self.results = Some(result_rx);
                Some(LaneEnds { requests: request_rx, results: result_tx })
            }
            // Unreachable: both rings were built Ready and split once.
            (requests, results) => {
                debug_assert!(false, "a reserved part's rings split once");
                drop((requests, results));
                None
            }
        };
        RetireLane {
            cell: self.cell,
            ends,
            #[cfg(inf_canary_retire_close_twice)]
            failed_close: None,
        }
    }
}

// ---- test hosts ------------------------------------------------------

/// One step of a test host's retirement of a cell's transport.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RetireStep {
    /// A job is out, the worker's lane is not yet released, or a block is
    /// left.
    Pending,
    /// Both backings retired.
    Retired,
}

/// Retires a threaded test fixture's cell transport at its hard end, on
/// the cell's own thread before its closure returns: `step` until
/// `Retired`, and after each `Pending` the caller's `wait(attempt)`, its own
/// clock (about a millisecond in every fixture). The worker keeps running
/// meanwhile and releases the lane. Panics naming `cell` when `wait`
/// answers `false` or the attempts reach `RETIRE_ATTEMPTS_MAX`, its
/// backings kept.
#[cfg(any(test, feature = "test-support"))]
pub fn retire_with(
    cell: u16,
    mut step: impl FnMut() -> RetireStep,
    mut wait: impl FnMut(u32) -> bool,
) {
    use crate::limits::RETIRE_ATTEMPTS_MAX;
    for attempt in 0..RETIRE_ATTEMPTS_MAX {
        if step() == RetireStep::Retired {
            return;
        }
        if !wait(attempt) {
            break;
        }
    }
    // A job out, or the worker's lane not released.
    panic!(
        "cell {cell}: its tier-file transport was not retired after {RETIRE_ATTEMPTS_MAX} waits"
    );
}

/// The custody of a cut: every value found queued, each dropped as the
/// dead process's descriptors close.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct CutCustody {
    jobs: usize,
    results: usize,
}

/// Cuts one cell's transport as a process exit would, on one thread that
/// holds all four endpoints: releases them, joins both rings and retires
/// them, every queued value dropped (its handle closed). Nothing on a
/// Retired part. The simulator's node runs it at its end, and the test
/// hosts' `Drop` (ADR-0157 D3: a host that holds a part retires it).
#[cfg(any(test, feature = "test-support"))]
pub fn cut_for_sim<J>(part: &mut CellPart<J>, lane: &mut RetireLane<J>) {
    if part.phase == TransportPhase::Retired {
        return;
    }
    debug_assert_eq!(part.cell, lane.cell, "a part is cut with its own lane");
    part.requests = None;
    part.results = None;
    lane.ends = None;
    let mut custody = CutCustody::default();
    let request = part.request_ring.join();
    let result = part.result_ring.join();
    debug_assert!(
        !matches!(request, RingPhase::Live) && !matches!(result, RingPhase::Live),
        "every endpoint is released"
    );
    for _ in 0..(2 * RETAINED_RING_SLOTS + 4) {
        let mut steps = RingSteps::new(1);
        let requests = part.request_ring.retire_step(&mut steps, |job| {
            custody.jobs += 1;
            drop(job);
        });
        let mut steps = RingSteps::new(1);
        let results = part.result_ring.retire_step(&mut steps, |result| {
            custody.results += 1;
            drop(result);
        });
        if matches!(requests, RingPhase::Terminal(_)) && matches!(results, RingPhase::Terminal(_)) {
            part.phase = TransportPhase::Retired;
            return;
        }
    }
    debug_assert!(false, "a cut retires both rings in bounded steps");
}

impl<J> RetireWorker<J> {
    /// The lane of cell `cell`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn lane_mut(&mut self, cell: u16) -> Option<&mut RetireLane<J>> {
        self.lanes.iter_mut().find(|lane| lane.cell == cell)
    }
}

/// A detached control plane's transport (a test host or the simulator):
/// the worker with every lane and the parts not handed out. Its `Drop`,
/// under `test-support` or `cfg(test)` only, cuts every part it still
/// holds; a part handed to a plane is that plane's host's to retire.
pub struct DetachedTier<J: SegmentFile + FinalClose> {
    worker: RetireWorker<J>,
    parts: Vec<CellPart<J>>,
}

impl<J: SegmentFile + FinalClose> DetachedTier<J> {
    pub(crate) fn new(worker: RetireWorker<J>, parts: Vec<CellPart<J>>) -> DetachedTier<J> {
        DetachedTier { worker, parts }
    }

    /// Cell `cell`'s part, the first time it is asked for.
    pub fn take_part(&mut self, cell: u16) -> Option<CellPart<J>> {
        let at = self.parts.iter().position(|part| part.cell == cell)?;
        Some(self.parts.swap_remove(at))
    }

    /// The worker.
    pub fn worker(&mut self) -> &mut RetireWorker<J> {
        &mut self.worker
    }

    /// The worker's one iteration with no message and no wait: the
    /// simulator's step and the test hosts' pump (ADR-0156 D3's planeless
    /// adapter, whose receive is the inbox's drain).
    pub fn iterate<F: SegmentFs<File = J>>(&mut self, fs: &F) {
        let _ = self.worker.iterate(&mut NeverWait, fs, drop);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<J: SegmentFile + FinalClose> DetachedTier<J> {
    /// Cuts `part` with its lane: its host's end.
    pub fn cut(&mut self, part: &mut CellPart<J>) {
        if let Some(lane) = self.worker.lane_mut(part.cell) {
            cut_for_sim(part, lane);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<J: SegmentFile + FinalClose> Drop for DetachedTier<J> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let mut parts = core::mem::take(&mut self.parts);
        for part in &mut parts {
            self.cut(part);
        }
    }
}

/// A receive that never waits and never holds a message: the simulator's
/// and the test hosts' worker, whose messages the inbox's drain handles.
pub(crate) struct NeverWait;

impl WorkerInbox for NeverWait {
    type Msg = ();

    fn try_recv(&mut self) -> Option<()> {
        None
    }

    fn wait(&mut self) {}

    fn disconnected(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "test-only: real files and descriptors for T1")]
#[path = "tier_retire_tests.rs"]
mod tier_retire_tests;
