//! macOS development backend (M0-S05): a readiness→completion adapter over
//! kqueue, so the whole stack develops and tests on a laptop without forking
//! code paths.
//!
//! **Correctness tier only.** This backend never appears in a performance
//! gate (`Capabilities::performance_tier == false`): it performs the actual
//! read/write/accept syscalls at readiness, one bounded slice per fd per
//! reap, and makes no batching claims. The contract it does honor exactly:
//! one `kevent` wait per `submit_and_reap`, completion-shaped delivery, and
//! the buffer-lifecycle guarantees (every leased buffer provably returns).
//!
//! Mechanics: level-triggered filters; filter-state changes are queued and
//! applied by the next `kevent` call (before its wait), so a disable/delete
//! costs no extra syscall — at worst one spurious wakeup. A write filter
//! lives exactly as long as the blocked queue that armed it (F-L11-08), a
//! refused arm fails its queue whole with every buffer (F-L11-09), and a
//! closed number takes its queued changes with it. A `Close` discards the
//! socket's unread input first (`shutdown(SHUT_RD)`): XNU answers a close
//! over unread bytes with RST, and the consumer that stopped reading owes
//! its peer a FIN (lane L11 N19, ADR-0124 D2).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use inf_alloc::{BufferId, BufferPool, LeaseKind};

use crate::driver::{
    AcceptFailure, BackendDriver, Capabilities, Completion, CompletionResult, IoOp, RawFd,
    StableBytes, StableBytesMut, SubmitStats, Wait, WriteBarrier, classify_accept_errno,
};
use crate::token::CompletionToken;

const ACCEPT_BATCH: usize = 32;
const SEND_RETRY_LIMIT: usize = 8;

struct RecvState {
    token: CompletionToken,
    /// Consumer-requested disarm (`RecvDisarm`): stay quiet until re-armed.
    disarmed: bool,
    /// Pool-dry pause: auto-resumes once buffers return.
    paused: bool,
}

struct PendingSend {
    token: CompletionToken,
    buf: BufferId,
    len: u32,
    written: u32,
}

/// One fd's blocked sends and whether `EVFILT_WRITE` is registered for
/// them (or its `EV_ADD` queued). The entry lives exactly as long as the
/// filter: a drained queue removes both.
#[derive(Default)]
struct SendQueue {
    pending: VecDeque<PendingSend>,
    armed: bool,
}

/// One listener's accept arm; `parked` = the read filter is disabled after
/// an exhaustion/broken failure (the shared table in `driver.rs`,
/// F-L11-02/F-L11-06) until `AcceptArm` or (exhaustion only) a `Close`.
#[derive(Copy, Clone, Debug)]
struct AcceptState {
    token: CompletionToken,
    parked: Option<AcceptFailure>,
}

/// kqueue-backed [`BackendDriver`]. See module docs for tier caveats.
pub struct KqueueDriver {
    kq: RawFd,
    pending_ops: Vec<IoOp>,
    /// Filter changes to apply on the next `kevent` (before its wait).
    changes: Vec<libc::kevent>,
    accepts: HashMap<RawFd, AcceptState>,
    recvs: HashMap<RawFd, RecvState>,
    sends: HashMap<RawFd, SendQueue>,
    events: Vec<libc::kevent>,
    stats: SubmitStats,
}

impl KqueueDriver {
    /// # Errors
    /// Fails only if the kernel refuses a kqueue (fd exhaustion).
    pub fn new() -> io::Result<KqueueDriver> {
        // SAFETY: plain syscall, no pointers.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(KqueueDriver {
            kq,
            pending_ops: Vec::with_capacity(64),
            changes: Vec::with_capacity(64),
            accepts: HashMap::new(),
            recvs: HashMap::new(),
            sends: HashMap::new(),
            events: vec![zero_event(); 256],
            stats: SubmitStats::default(),
        })
    }

    fn push_change(&mut self, fd: RawFd, filter: i16, flags: u16) {
        let mut ev = zero_event();
        ev.ident = fd as usize;
        ev.filter = filter;
        ev.flags = flags;
        self.changes.push(ev);
    }

    /// Process queued ops into state + filter changes; immediate completions
    /// (sync send success, close) go straight to `out`.
    fn apply_ops(&mut self, pool: &mut BufferPool, out: &mut Vec<Completion>) {
        let ops = core::mem::take(&mut self.pending_ops);
        for op in ops {
            match op {
                IoOp::AcceptArm { listener, token } => {
                    // Idempotent while armed; a parked arm resumes.
                    set_nonblocking(listener);
                    self.accepts.insert(listener, AcceptState { token, parked: None });
                    self.push_change(listener, libc::EVFILT_READ, libc::EV_ADD | libc::EV_ENABLE);
                }
                IoOp::RecvArm { fd, token } => {
                    set_nonblocking(fd);
                    self.recvs.insert(fd, RecvState { token, disarmed: false, paused: false });
                    self.push_change(fd, libc::EVFILT_READ, libc::EV_ADD | libc::EV_ENABLE);
                }
                IoOp::RecvDisarm { fd } => {
                    if let Some(state) = self.recvs.get_mut(&fd) {
                        state.disarmed = true;
                        self.push_change(fd, libc::EVFILT_READ, libc::EV_DISABLE);
                    }
                }
                IoOp::Send { fd, buf, len, token } => {
                    self.queue_send(fd, buf, len, token, pool, out);
                }
                IoOp::Close { fd, token } => self.close_fd(fd, token, out),
                // File ops (M2-S05, ADR-0013): regular files are always
                // "ready" — the readiness tier executes them synchronously
                // at submit, delivering the same completion contract as the
                // uring tier (fsync only after the full write; a failed
                // write cancels the chained sync). `WriteThrough` is write
                // + fsync here — the same durability promise at FLUSH-class
                // cost: the correctness tier, never a gate artifact
                // (ADR-0086 D1).
                IoOp::LogWrite { fd, offset, data, token, barrier } => {
                    self.log_write(fd, offset, data, token, barrier, out);
                }
                IoOp::Fdatasync { fd, token } => {
                    out.push(sync_file(fd, token, &mut self.stats));
                }
                IoOp::TierRead { fd, offset, buf, token } => {
                    out.push(Completion {
                        token,
                        result: match tier_pread_all(fd, offset, buf, &mut self.stats) {
                            Ok(()) => CompletionResult::TierRead,
                            Err(errno) => CompletionResult::Error { errno, buf: None },
                        },
                    });
                }
            }
        }
    }

    /// One `Send`: drain synchronously — the common case on a writable
    /// socket — and arm `EVFILT_WRITE` only on `EAGAIN`.
    fn queue_send(
        &mut self,
        fd: RawFd,
        buf: BufferId,
        len: u32,
        token: CompletionToken,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        if len as usize > pool.buf_size() {
            out.push(Completion {
                token,
                result: CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) },
            });
            return;
        }
        let state = self.sends.entry(fd).or_default();
        state.pending.push_back(PendingSend { token, buf, len, written: 0 });
        let drained = drain_sends(fd, &mut state.pending, pool, out, &mut self.stats);
        self.settle_write_filter(fd, drained);
    }

    /// After a drain attempt on `fd`'s queue. Drained: the entry goes, and
    /// with it the write filter it armed — level-triggered, an orphan fires
    /// on every wait (F-L11-08). Blocked: the filter is armed once.
    fn settle_write_filter(&mut self, fd: RawFd, drained: bool) {
        let Some(state) = self.sends.get_mut(&fd) else { return };
        let change = if drained {
            let armed = state.armed;
            self.sends.remove(&fd);
            armed.then_some(libc::EV_DELETE)
        } else if state.armed {
            None
        } else {
            state.armed = true;
            Some(libc::EV_ADD | libc::EV_ENABLE)
        };
        if let Some(flags) = change {
            self.push_change(fd, libc::EVFILT_WRITE, flags);
        }
    }

    /// Pending sends cancel (buffers returned) before `Closed`. Unread
    /// input is discarded so the peer sees a FIN (module doc). close(2)
    /// drops the fd's filters, and its queued changes go too: the number
    /// may be another thread's fd before the next `kevent` applies them.
    fn close_fd(&mut self, fd: RawFd, token: CompletionToken, out: &mut Vec<Completion>) {
        self.accepts.remove(&fd);
        self.recvs.remove(&fd);
        if let Some(state) = self.sends.remove(&fd) {
            fail_queue(state.pending, libc::ECANCELED, out);
        }
        self.changes.retain(|change| change.ident != fd as usize);
        // SAFETY: shutdown on an fd we were handed; a non-socket or an
        // unconnected socket refuses (ENOTSOCK/ENOTCONN) and nothing else
        // happens — the close below is the one that matters.
        unsafe { libc::shutdown(fd, libc::SHUT_RD) };
        // SAFETY: closing an fd we were handed; kqueue drops its filters
        // automatically.
        let rc = unsafe { libc::close(fd) };
        self.stats.syscalls += 1;
        out.push(Completion {
            token,
            result: if rc == 0 {
                CompletionResult::Closed
            } else {
                CompletionResult::Error {
                    errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
                    buf: None,
                }
            },
        });
        // close(2) released the descriptor either way: every
        // exhaustion-parked accept arm may try again.
        self.resume_exhausted_accepts();
    }

    /// One `LogWrite` on the readiness tier (M2-S05, ADR-0013): regular
    /// files are always "ready", so it executes synchronously at submit
    /// with the uring tier's completion contract (fsync only after the
    /// full write; a failed write cancels the chained sync).
    /// `WriteThrough` is write + fsync here — the same durability promise
    /// at FLUSH-class cost: the correctness tier, never a gate artifact
    /// (ADR-0086 D1).
    fn log_write(
        &mut self,
        fd: RawFd,
        offset: u64,
        data: StableBytes,
        token: CompletionToken,
        barrier: WriteBarrier,
        out: &mut Vec<Completion>,
    ) {
        let fsync_token = barrier.fsync_token();
        let write_through = matches!(barrier, WriteBarrier::WriteThrough);
        match log_pwrite_all(fd, offset, data, &mut self.stats) {
            Ok(()) => {
                let through = if write_through {
                    // The write's own token is the durability fact: a
                    // failed sync is the write's error.
                    match sync_file(fd, token, &mut self.stats).result {
                        CompletionResult::Synced => CompletionResult::LogWritten,
                        failed => failed,
                    }
                } else {
                    CompletionResult::LogWritten
                };
                out.push(Completion { token, result: through });
                if let Some(ft) = fsync_token {
                    out.push(sync_file(fd, ft, &mut self.stats));
                }
            }
            Err(errno) => {
                out.push(Completion {
                    token,
                    result: CompletionResult::Error { errno, buf: None },
                });
                if let Some(ft) = fsync_token {
                    out.push(Completion {
                        token: ft,
                        result: CompletionResult::Error { errno: libc::ECANCELED, buf: None },
                    });
                }
            }
        }
    }

    /// Resume recvs paused on pool exhaustion once buffers are available.
    fn resume_paused(&mut self, pool: &BufferPool) {
        if pool.leased() >= pool.capacity() {
            return;
        }
        let resumable: Vec<RawFd> =
            self.recvs.iter().filter(|(_, s)| s.paused && !s.disarmed).map(|(fd, _)| *fd).collect();
        for fd in resumable {
            self.recvs.get_mut(&fd).expect("collected above").paused = false;
            self.push_change(fd, libc::EVFILT_READ, libc::EV_ENABLE);
        }
    }

    fn wait_kevent(&mut self, wait: Wait) -> io::Result<usize> {
        let timeout_storage;
        let timeout: *const libc::timespec = match wait {
            Wait::Poll => {
                timeout_storage = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                &timeout_storage
            }
            Wait::Park { timeout: Some(d) } => {
                timeout_storage = duration_to_timespec(d);
                &timeout_storage
            }
            Wait::Park { timeout: None } => core::ptr::null(),
        };
        // SAFETY: changes/events point at live Vec storage with correct
        // lengths; timeout is null or a live timespec on this frame.
        let n = unsafe {
            libc::kevent(
                self.kq,
                self.changes.as_ptr(),
                i32::try_from(self.changes.len()).expect("changelist fits i32"),
                self.events.as_mut_ptr(),
                i32::try_from(self.events.len()).expect("eventlist fits i32"),
                timeout,
            )
        };
        self.stats.syscalls += 1;
        self.changes.clear();
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(0); // spurious wake; next iteration retries
            }
            return Err(err);
        }
        Ok(n as usize)
    }

    fn dispatch_event(
        &mut self,
        ev: libc::kevent,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        let fd = ev.ident as RawFd;
        if ev.flags & libc::EV_ERROR != 0 {
            self.change_refused(ev, out);
            return;
        }
        match ev.filter {
            libc::EVFILT_READ if self.accepts.contains_key(&fd) => {
                self.accept_slice(fd, out);
            }
            libc::EVFILT_READ => self.recv_one(fd, pool, out),
            libc::EVFILT_WRITE => self.write_ready(fd, pool, out),
            _ => {}
        }
    }

    /// A changelist entry the kernel refused. A refused `EV_DELETE` is
    /// nobody's error — the filter is gone either way (`ENOENT` once the
    /// fd closed underneath it). A refused arm surfaces on its owner: the
    /// write queue fails whole, every buffer returned (F-L11-09); a recv
    /// or accept arm reports on its token.
    fn change_refused(&mut self, ev: libc::kevent, out: &mut Vec<Completion>) {
        let fd = ev.ident as RawFd;
        let errno = ev.data as i32;
        if errno == 0 || ev.flags & libc::EV_DELETE != 0 {
            return;
        }
        if ev.filter == libc::EVFILT_WRITE {
            if let Some(state) = self.sends.remove(&fd) {
                fail_queue(state.pending, errno, out);
            }
            return;
        }
        let token =
            self.accepts.get(&fd).map(|a| a.token).or_else(|| self.recvs.get(&fd).map(|s| s.token));
        if let Some(token) = token {
            out.push(Completion { token, result: CompletionResult::Error { errno, buf: None } });
        }
    }

    /// Writable: drain a bounded slice of the fd's queue. A filter no
    /// queue owns is deleted here — the one spurious wakeup the module
    /// doc allows.
    fn write_ready(&mut self, fd: RawFd, pool: &mut BufferPool, out: &mut Vec<Completion>) {
        let Some(state) = self.sends.get_mut(&fd) else {
            self.push_change(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
            return;
        };
        let drained = drain_sends(fd, &mut state.pending, pool, out, &mut self.stats);
        self.settle_write_filter(fd, drained);
    }

    /// Multishot-accept emulation: drain a bounded slice of the backlog.
    fn accept_slice(&mut self, listener: RawFd, out: &mut Vec<Completion>) {
        let token = self.accepts[&listener].token;
        for _ in 0..ACCEPT_BATCH {
            // SAFETY: plain accept; we pass no out-pointers for the peer.
            let fd =
                unsafe { libc::accept(listener, core::ptr::null_mut(), core::ptr::null_mut()) };
            self.stats.syscalls += 1;
            if fd >= 0 {
                set_nonblocking(fd);
                out.push(Completion { token, result: CompletionResult::Accepted { fd } });
                continue;
            }
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // The shared table (driver.rs): transient ⇒ wait for the next
            // readiness edge; exhaustion/broken ⇒ one `Error` and the
            // read filter is disabled (parked) — a level-triggered listener
            // would otherwise refire every kevent (the F-L11-02 spin).
            let class = classify_accept_errno(errno);
            if class != AcceptFailure::Transient {
                if let Some(arm) = self.accepts.get_mut(&listener) {
                    arm.parked = Some(class);
                }
                self.push_change(listener, libc::EVFILT_READ, libc::EV_DISABLE);
                out.push(Completion {
                    token,
                    result: CompletionResult::Error { errno, buf: None },
                });
            }
            break;
        }
    }

    /// An fd returned to the process: every exhaustion-parked accept arm
    /// re-enables its read filter (the consumer's timed `AcceptArm` covers
    /// fds freed outside this driver).
    fn resume_exhausted_accepts(&mut self) {
        let parked: Vec<RawFd> = self
            .accepts
            .iter()
            .filter(|(_, arm)| arm.parked == Some(AcceptFailure::Exhausted))
            .map(|(fd, _)| *fd)
            .collect();
        for listener in parked {
            if let Some(arm) = self.accepts.get_mut(&listener) {
                arm.parked = None;
            }
            self.push_change(listener, libc::EVFILT_READ, libc::EV_ENABLE);
        }
    }

    /// One bounded read at readiness; level-triggering refires while data
    /// remains (per-connection fairness without per-fd loops).
    fn recv_one(&mut self, fd: RawFd, pool: &mut BufferPool, out: &mut Vec<Completion>) {
        let Some(state) = self.recvs.get_mut(&fd) else { return };
        if state.disarmed || state.paused {
            return;
        }
        let token = state.token;
        let Some(buf) = pool.try_lease(LeaseKind::Recv) else {
            // Pool dry: pause this fd (disable applies before the next wait)
            // and tell the consumer once. Resumes via `resume_paused`.
            state.paused = true;
            self.push_change(fd, libc::EVFILT_READ, libc::EV_DISABLE);
            out.push(Completion { token, result: CompletionResult::RecvDropped });
            return;
        };
        let capacity = pool.buf_size();
        let target = pool.bytes_mut(buf);
        // SAFETY: target is a live unique borrow of the leased buffer, valid
        // for `capacity` bytes.
        let n = unsafe { libc::read(fd, target.as_mut_ptr().cast(), capacity) };
        self.stats.syscalls += 1;
        if n >= 0 {
            // n == 0 ⇒ EOF, delivered with the buffer per the contract.
            out.push(Completion { token, result: CompletionResult::Recv { buf, len: n as u32 } });
            return;
        }
        pool.release(buf);
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EAGAIN && errno != libc::EINTR {
            out.push(Completion { token, result: CompletionResult::Error { errno, buf: None } });
        }
    }
}

impl BackendDriver for KqueueDriver {
    fn push(&mut self, op: IoOp) {
        self.pending_ops.push(op);
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let before = out.len();
        self.stats = SubmitStats { sqes: self.pending_ops.len() as u64, ..SubmitStats::default() };
        self.apply_ops(pool, out);
        self.resume_paused(pool);
        // Sync completions (send fast path, close) may already satisfy the
        // caller; still poll the queue so filter changes land. Never park
        // while holding deliverable completions.
        let wait = if out.len() > before { Wait::Poll } else { wait };
        let n = self.wait_kevent(wait)?;
        for i in 0..n {
            let ev = self.events[i];
            self.dispatch_event(ev, pool, out);
        }
        let produced = out.len() - before;
        self.stats.cqes = produced as u64;
        Ok(produced)
    }

    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(()) // no kernel-side buffer registration on kqueue
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "kqueue",
            // Arm-once semantics hold (the adapter re-fires internally)…
            multishot_accept: true,
            multishot_recv: true,
            // …but nothing kernel-side is provided/fixed, and nothing here
            // is a performance statement.
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: false,
            defer_taskrun: false,
            performance_tier: false,
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}

impl Drop for KqueueDriver {
    fn drop(&mut self) {
        // SAFETY: kq came from kqueue(); closing it releases all filters.
        unsafe { libc::close(self.kq) };
    }
}

impl core::fmt::Debug for KqueueDriver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "KqueueDriver {{ accepts: {}, recvs: {}, sends: {} }}",
            self.accepts.len(),
            self.recvs.len(),
            self.sends.len()
        )
    }
}

/// Write the queue head(s) until drained, blocked, or errored. Returns
/// whether the queue is now empty. A hard error fails the queue whole (a
/// broken stream cannot carry later sends).
fn drain_sends(
    fd: RawFd,
    queue: &mut VecDeque<PendingSend>,
    pool: &BufferPool,
    out: &mut Vec<Completion>,
    stats: &mut SubmitStats,
) -> bool {
    let mut spins = 0;
    while let Some(head) = queue.front_mut() {
        let bytes = pool.bytes(head.buf);
        let remaining = &bytes[head.written as usize..head.len as usize];
        // SAFETY: remaining is a live borrow of the leased buffer.
        let n = unsafe { libc::write(fd, remaining.as_ptr().cast(), remaining.len()) };
        stats.syscalls += 1;
        if n > 0 {
            head.written += n as u32;
            if head.written == head.len {
                let done = queue.pop_front().expect("head exists");
                out.push(Completion {
                    token: done.token,
                    result: CompletionResult::Sent { buf: done.buf },
                });
            }
            spins += 1;
            if spins >= SEND_RETRY_LIMIT {
                return queue.is_empty(); // bounded work per slice
            }
            continue;
        }
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EAGAIN || errno == libc::EINTR {
            return false;
        }
        fail_queue(core::mem::take(queue), errno, out);
        return true;
    }
    true
}

/// Terminal failure of a whole queue: the head with `errno`, the rest
/// `ECANCELED` — every buffer returned (the contract's "ALWAYS").
fn fail_queue(queue: VecDeque<PendingSend>, errno: i32, out: &mut Vec<Completion>) {
    let mut errno = errno;
    for p in queue {
        out.push(Completion {
            token: p.token,
            result: CompletionResult::Error { errno, buf: Some(p.buf) },
        });
        errno = libc::ECANCELED;
    }
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl on an fd we own; failure leaves the fd blocking, which
    // surfaces as a hung test, never UB.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    // Sub-MSS replies + Nagle + delayed ACK = ~40 ms pipelined stalls;
    // accepted sockets are TCP at M0 (failure on non-TCP fds is ignored).
    let one: libc::c_int = 1;
    // SAFETY: setsockopt with a valid int pointer on a live fd.
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&raw const one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Synchronous positional write of the whole range (short writes retried in
/// place — the readiness tier has no async file I/O). Returns errno on
/// terminal failure; zero-progress writes surface as `EIO`.
fn log_pwrite_all(
    fd: RawFd,
    offset: u64,
    data: StableBytes,
    stats: &mut SubmitStats,
) -> Result<(), i32> {
    let mut written: u32 = 0;
    while written < data.len() {
        // SAFETY: `data` upholds the StableBytes contract (live and stable
        // for the duration of the op); `written` never exceeds `data.len()`.
        let n = unsafe {
            libc::pwrite(
                fd,
                data.as_ptr().add(written as usize).cast(),
                (data.len() - written) as usize,
                (offset + u64::from(written)) as libc::off_t,
            )
        };
        stats.syscalls += 1;
        if n > 0 {
            written += n as u32;
            continue;
        }
        if n == 0 {
            return Err(libc::EIO);
        }
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        if errno == libc::EINTR {
            continue;
        }
        return Err(errno);
    }
    Ok(())
}

/// Synchronous positional read of the whole range (M4-S04; short reads
/// retried in place — the readiness tier has no async file I/O). EOF
/// before the buffer fills is `EIO`: tier reads are always within the
/// flushed range, so a short file is corruption, not a condition.
fn tier_pread_all(
    fd: RawFd,
    offset: u64,
    buf: StableBytesMut,
    stats: &mut SubmitStats,
) -> Result<(), i32> {
    let mut got: u32 = 0;
    while got < buf.len() {
        // SAFETY: `buf` upholds the StableBytesMut contract (live, stable,
        // unaliased for the duration of the op); `got` never exceeds
        // `buf.len()`.
        let n = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr().add(got as usize).cast(),
                (buf.len() - got) as usize,
                (offset + u64::from(got)) as libc::off_t,
            )
        };
        stats.syscalls += 1;
        if n > 0 {
            got += n as u32;
            continue;
        }
        if n == 0 {
            return Err(libc::EIO);
        }
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        if errno == libc::EINTR {
            continue;
        }
        return Err(errno);
    }
    Ok(())
}

/// Durably flush file data. macOS has no fdatasync in the stable syscall
/// surface; plain `fsync` is the stronger stand-in on this correctness-only
/// dev tier (never a performance claim — `performance_tier == false`).
fn sync_file(fd: RawFd, token: CompletionToken, stats: &mut SubmitStats) -> Completion {
    // SAFETY: plain syscall on a live fd, no pointers.
    let rc = unsafe { libc::fsync(fd) };
    stats.syscalls += 1;
    Completion {
        token,
        result: if rc == 0 {
            CompletionResult::Synced
        } else {
            CompletionResult::Error {
                errno: io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO),
                buf: None,
            }
        },
    }
}

fn duration_to_timespec(d: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}

fn zero_event() -> libc::kevent {
    libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: core::ptr::null_mut() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::TokenClass;

    /// A connected non-blocking pair whose driver side has a 4 KiB send
    /// buffer; the peer never reads, so a few sends block in the driver.
    fn blocked_pair() -> (RawFd, RawFd) {
        let mut sv = [0 as RawFd; 2];
        // SAFETY: socketpair writes two fds into the live array.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair");
        let size: libc::c_int = 4096;
        // SAFETY: setsockopt with a valid int pointer on a live socket.
        unsafe {
            libc::setsockopt(
                sv[0],
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const size).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        set_nonblocking(sv[0]);
        (sv[0], sv[1])
    }

    fn send_token(slot: u32) -> CompletionToken {
        CompletionToken::new(TokenClass::Send, slot, 0)
    }

    /// Pushes full-buffer sends until one blocks, then one more behind it;
    /// returns the two queued tokens (head, tail).
    fn block_two_sends(
        driver: &mut KqueueDriver,
        pool: &mut BufferPool,
        fd: RawFd,
    ) -> [CompletionToken; 2] {
        let mut out = Vec::new();
        let mut head = None;
        for slot in 0..64u32 {
            let buf = pool.try_lease(LeaseKind::Send).expect("lease");
            pool.bytes_mut(buf).fill(0xAB);
            let len = pool.buf_size() as u32;
            driver.push(IoOp::Send { fd, buf, len, token: send_token(slot) });
            out.clear();
            driver.submit_and_reap(pool, Wait::Poll, &mut out).expect("submit");
            match out.pop().map(|c| c.result) {
                Some(CompletionResult::Sent { buf }) => pool.release(buf),
                Some(other) => panic!("unexpected {other:?}"),
                None => {
                    head = Some(slot);
                    break;
                }
            }
        }
        let head = head.expect("could not block a send");
        let buf = pool.try_lease(LeaseKind::Send).expect("lease");
        let len = pool.buf_size() as u32;
        driver.push(IoOp::Send { fd, buf, len, token: send_token(head + 1) });
        out.clear();
        driver.submit_and_reap(pool, Wait::Poll, &mut out).expect("submit");
        assert!(out.is_empty(), "the second send queued behind the blocked head");
        [send_token(head), send_token(head + 1)]
    }

    /// The record kevent returns for a changelist entry it refused.
    fn refused_change(fd: RawFd, flags: u16, errno: i32) -> libc::kevent {
        libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_WRITE,
            flags: flags | libc::EV_ERROR,
            fflags: 0,
            data: errno as isize,
            udata: core::ptr::null_mut(),
        }
    }

    /// F-L11-09: a refused `EV_ADD` on a live write queue is the queue's
    /// terminal failure — every buffer comes back, and the later `Close`
    /// names no token twice.
    #[test]
    fn a_refused_write_filter_fails_the_queue_with_its_buffers() {
        let (fd, peer) = blocked_pair();
        let mut driver = KqueueDriver::new().expect("kqueue");
        let mut pool = BufferPool::new(8, 4096);
        let [head, tail] = block_two_sends(&mut driver, &mut pool, fd);

        let mut out = Vec::new();
        let report = refused_change(fd, libc::EV_ADD | libc::EV_ENABLE, libc::EBADF);
        driver.dispatch_event(report, &mut pool, &mut out);
        let mut failed = Vec::new();
        for c in out.drain(..) {
            match c.result {
                CompletionResult::Error { errno, buf: Some(buf) } => {
                    pool.release(buf);
                    failed.push((c.token, errno));
                }
                other => panic!("a terminal without its buffer: {other:?}"),
            }
        }
        assert_eq!(failed, vec![(head, libc::EBADF), (tail, libc::ECANCELED)]);
        assert!(!driver.sends.contains_key(&fd), "the failed queue is gone");
        assert_eq!(pool.reconcile(), Ok(()));

        let close = CompletionToken::new(TokenClass::Close, 7, 0);
        driver.push(IoOp::Close { fd, token: close });
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("close");
        let tokens: Vec<CompletionToken> = out.iter().map(|c| c.token).collect();
        assert_eq!(tokens, vec![close], "exactly one terminal per token");
        // SAFETY: the peer fd is ours and unused after this.
        unsafe { libc::close(peer) };
    }

    /// A refused `EV_DELETE` (`ENOENT` once the fd closed underneath it) is
    /// never the queue's error: nothing is delivered, the queue stands.
    #[test]
    fn a_refused_delete_is_not_the_queues_error() {
        let (fd, peer) = blocked_pair();
        let mut driver = KqueueDriver::new().expect("kqueue");
        let mut pool = BufferPool::new(8, 4096);
        let [head, tail] = block_two_sends(&mut driver, &mut pool, fd);

        let mut out = Vec::new();
        driver.dispatch_event(
            refused_change(fd, libc::EV_DELETE, libc::ENOENT),
            &mut pool,
            &mut out,
        );
        assert!(out.is_empty(), "a failed delete delivered {out:?}");
        assert!(driver.sends.contains_key(&fd), "the queue stands");

        let close = CompletionToken::new(TokenClass::Close, 7, 0);
        driver.push(IoOp::Close { fd, token: close });
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("close");
        let mut cancelled = Vec::new();
        for c in out.drain(..) {
            if let CompletionResult::Error { errno, buf: Some(buf) } = c.result {
                pool.release(buf);
                cancelled.push((c.token, errno));
            }
        }
        assert_eq!(cancelled, vec![(head, libc::ECANCELED), (tail, libc::ECANCELED)]);
        assert_eq!(pool.reconcile(), Ok(()));
        // SAFETY: the peer fd is ours and unused after this.
        unsafe { libc::close(peer) };
    }
}
