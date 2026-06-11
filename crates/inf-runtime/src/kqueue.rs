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
//! costs no extra syscall — at worst one spurious wakeup.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use inf_alloc::{BufferId, BufferPool, LeaseKind};

use crate::driver::{
    BackendDriver, Capabilities, Completion, CompletionResult, IoOp, RawFd, SubmitStats, Wait,
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

/// kqueue-backed [`BackendDriver`]. See module docs for tier caveats.
pub struct KqueueDriver {
    kq: RawFd,
    pending_ops: Vec<IoOp>,
    /// Filter changes to apply on the next `kevent` (before its wait).
    changes: Vec<libc::kevent>,
    accepts: HashMap<RawFd, CompletionToken>,
    recvs: HashMap<RawFd, RecvState>,
    sends: HashMap<RawFd, VecDeque<PendingSend>>,
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
                    set_nonblocking(listener);
                    self.accepts.insert(listener, token);
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
                    if len as usize > pool.buf_size() {
                        out.push(Completion {
                            token,
                            result: CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) },
                        });
                        continue;
                    }
                    let queue = self.sends.entry(fd).or_default();
                    queue.push_back(PendingSend { token, buf, len, written: 0 });
                    // Try to drain synchronously — the common case on a
                    // writable socket — and only arm EVFILT_WRITE on EAGAIN.
                    let drained = drain_sends(fd, queue, pool, out, &mut self.stats);
                    if drained {
                        self.sends.remove(&fd);
                    } else {
                        self.push_change(fd, libc::EVFILT_WRITE, libc::EV_ADD | libc::EV_ENABLE);
                    }
                }
                IoOp::Close { fd, token } => {
                    self.accepts.remove(&fd);
                    self.recvs.remove(&fd);
                    if let Some(queue) = self.sends.remove(&fd) {
                        for p in queue {
                            out.push(Completion {
                                token: p.token,
                                result: CompletionResult::Error {
                                    errno: libc::ECANCELED,
                                    buf: Some(p.buf),
                                },
                            });
                        }
                    }
                    // SAFETY: closing an fd we were handed; kqueue drops its
                    // filters automatically.
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
            // Stale change on an already-closed fd is routine; a real error
            // on a live fd surfaces on its owner token.
            let errno = ev.data as i32;
            if errno == 0 {
                return;
            }
            let token = if ev.filter == libc::EVFILT_WRITE {
                self.sends.get(&fd).and_then(|q| q.front()).map(|p| p.token)
            } else {
                self.accepts.get(&fd).copied().or_else(|| self.recvs.get(&fd).map(|s| s.token))
            };
            if let Some(token) = token {
                out.push(Completion {
                    token,
                    result: CompletionResult::Error { errno, buf: None },
                });
            }
            return;
        }
        match ev.filter {
            libc::EVFILT_READ if self.accepts.contains_key(&fd) => {
                self.accept_slice(fd, out);
            }
            libc::EVFILT_READ => self.recv_one(fd, pool, out),
            libc::EVFILT_WRITE => {
                if let Some(mut queue) = self.sends.remove(&fd) {
                    let drained = drain_sends(fd, &mut queue, pool, out, &mut self.stats);
                    if drained {
                        self.push_change(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
                    } else {
                        self.sends.insert(fd, queue);
                    }
                }
            }
            _ => {}
        }
    }

    /// Multishot-accept emulation: drain a bounded slice of the backlog.
    fn accept_slice(&mut self, listener: RawFd, out: &mut Vec<Completion>) {
        let token = self.accepts[&listener];
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
            match errno {
                libc::EAGAIN | libc::ECONNABORTED | libc::EINTR => {}
                _ => out.push(Completion {
                    token,
                    result: CompletionResult::Error { errno, buf: None },
                }),
            }
            break;
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
        let dst = pool.bytes_mut(buf);
        // SAFETY: dst is a live unique borrow of the leased buffer, valid
        // for `capacity` bytes.
        let n = unsafe { libc::read(fd, dst.as_mut_ptr().cast(), capacity) };
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
/// whether the queue is now empty. Hard errors fail the head op and cancel
/// the rest (a broken stream cannot carry later sends), returning every
/// buffer.
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
        let failed = queue.pop_front().expect("head exists");
        out.push(Completion {
            token: failed.token,
            result: CompletionResult::Error { errno, buf: Some(failed.buf) },
        });
        for cancelled in queue.drain(..) {
            out.push(Completion {
                token: cancelled.token,
                result: CompletionResult::Error {
                    errno: libc::ECANCELED,
                    buf: Some(cancelled.buf),
                },
            });
        }
        return true;
    }
    true
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
