//! io_uring backend (M0-S04) — the performance tier and the direct answer to
//! Vortex's 75%-syscall flamegraph: every queued op rides ONE
//! `io_uring_enter` per loop iteration (L3), with multishot accept/recv and
//! kernel-side provided buffers where the kernel offers them.
//!
//! ## Modes (boot-time probe → [`Capabilities`], logged at startup)
//! - **Modern** (≥ 6.0): `SINGLE_ISSUER | DEFER_TASKRUN`, multishot accept,
//!   multishot recv over a provided-buffer group.
//! - **Degraded** (5.15-class kernels, or `INF_URING_FORCE_DEGRADED=1`):
//!   oneshot accept/recv re-armed internally with explicit driver-leased
//!   buffers. Identical observable contract — the kernel-matrix CI job
//!   asserts the correctness suite passes in both modes.
//!
//! ## Buffer lifecycle (the Vortex proof, carried)
//! Modern mode **stages** recv buffers into the kernel's provided group —
//! driver→kernel custody, accounted via `pool.staged()`, never a consumer
//! lease — bounded to HALF the pool (min 1) so the send/response path can
//! always lease (a recv ring that swallowed the whole pool would starve
//! RESPOND; first live Linux run caught exactly that). A delivered buffer is
//! `promote_staged` → ordinary `Recv` lease. Degraded mode pins one leased
//! buffer under each oneshot recv; while the pool is dry it arms a `PollAdd`
//! readability watch instead, so `RecvDropped` is reported only when data is
//! actually pending (kqueue-equivalent timing). Every CQE — success, error,
//! cancellation — resolves custody deterministically; `pool.reconcile()`
//! (no consumer leaks) must hold after any storm (conformance suite).
//!
//! ## Validation status
//! Conformance suite green on Linux 7.0 in probed and `INF_URING_FORCE_DEGRADED`
//! modes (2026-06-11); kernel-matrix CI legs and reference-box performance
//! evidence tracked in `reviews/infinity-m0-skeleton.md`.

use std::collections::{HashMap, VecDeque};
use std::io;

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use io_uring::types::Fd;
use io_uring::{IoUring, Probe, cqueue, opcode, squeue, types};

use crate::driver::{
    BackendDriver, Capabilities, Completion, CompletionResult, IoOp, RawFd, SubmitStats, Wait,
};
use crate::token::CompletionToken;

/// Provided-buffer group id used by this driver (one cell = one ring = one
/// group; the id only needs to be unique within the ring).
const BGID: u16 = 0;

/// In-kernel op bookkeeping, keyed by an internal `user_data` id — consumer
/// tokens are never used as `user_data`, so token reuse across ops can never
/// misroute a CQE.
enum OpState {
    Accept {
        listener: RawFd,
        token: CompletionToken,
    },
    RecvMulti {
        fd: RawFd,
        token: CompletionToken,
    },
    RecvOneshot {
        fd: RawFd,
        token: CompletionToken,
        buf: BufferId,
    },
    Send {
        fd: RawFd,
        token: CompletionToken,
        buf: BufferId,
        len: u32,
        written: u32,
    },
    Close {
        fd: RawFd,
    },
    Cancel,
    /// One provided buffer in flight to the kernel group; on failure the
    /// staging unwinds to the pool.
    Provide {
        buf: BufferId,
    },
    /// Degraded-mode readability watch while the pool is dry: `RecvDropped`
    /// is only honest when data is actually pending.
    PollDry {
        fd: RawFd,
        token: CompletionToken,
    },
}

struct RecvArm {
    token: CompletionToken,
    /// Live kernel op id (multishot arm or current oneshot), if any.
    op_id: Option<u64>,
    disarmed: bool,
    /// Out of buffers; consumer was told `RecvDropped` once.
    paused: bool,
}

struct CloseWait {
    token: CompletionToken,
    close_seen: bool,
    close_result: i32,
}

/// io_uring [`BackendDriver`]. See module docs.
pub struct UringDriver {
    ring: IoUring,
    caps: Capabilities,
    pending_ops: Vec<IoOp>,
    /// SQEs that did not fit the SQ; flushed first next submit.
    backlog: VecDeque<squeue::Entry>,
    states: HashMap<u64, OpState>,
    next_id: u64,
    accepts: HashMap<RawFd, CompletionToken>,
    recvs: HashMap<RawFd, RecvArm>,
    /// Outstanding sends per fd — `Closed` is delivered only after they
    /// resolve (cancelled sends return their buffers first, per contract).
    sends_inflight: HashMap<RawFd, u32>,
    closing: HashMap<RawFd, CloseWait>,
    /// Buffers currently owned by the kernel's provided group, by bid.
    /// CQE `buffer_select` ids resolve through this map — never minted.
    provided: HashMap<u16, BufferId>,
    stats: SubmitStats,
}

impl UringDriver {
    /// Build a ring with `entries` SQ slots, probing setup flags and
    /// features with graceful degradation.
    ///
    /// # Errors
    /// Only if no io_uring at all can be created (kernel too old, seccomp).
    pub fn new(entries: u32) -> io::Result<UringDriver> {
        let force_degraded = std::env::var_os("INF_URING_FORCE_DEGRADED").is_some();

        // Setup-flag fallback chain: the builder flags are 6.0/6.1 features.
        let mut single_issuer = true;
        let mut defer_taskrun = true;
        let ring = loop {
            let mut builder = IoUring::builder();
            if single_issuer {
                builder.setup_single_issuer();
            }
            if defer_taskrun && single_issuer {
                builder.setup_defer_taskrun();
            }
            match builder.build(entries) {
                Ok(ring) => break ring,
                Err(_) if defer_taskrun => defer_taskrun = false,
                Err(_) if single_issuer => single_issuer = false,
                Err(e) => return Err(e),
            }
        };

        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        let provide_supported = probe.is_supported(opcode::ProvideBuffers::CODE);
        let (kmajor, kminor) = kernel_version();
        // Multishot accept: 5.19+. Multishot recv (provided buffers): 6.0+.
        // Flags aren't probeable (same opcodes); version-gate, then the
        // runtime EINVAL fallback in `dispatch_cqe` catches liars.
        let multishot_accept = !force_degraded && (kmajor, kminor) >= (5, 19);
        let multishot_recv = !force_degraded && provide_supported && (kmajor, kminor) >= (6, 0);

        Ok(UringDriver {
            ring,
            caps: Capabilities {
                backend: "io_uring",
                multishot_accept,
                multishot_recv,
                provided_buffers: multishot_recv,
                fixed_buffers: false, // set by register_pool
                single_issuer,
                defer_taskrun,
                performance_tier: true,
            },
            pending_ops: Vec::with_capacity(64),
            backlog: VecDeque::new(),
            states: HashMap::new(),
            next_id: 0,
            accepts: HashMap::new(),
            recvs: HashMap::new(),
            sends_inflight: HashMap::new(),
            closing: HashMap::new(),
            provided: HashMap::new(),
            stats: SubmitStats::default(),
        })
    }

    fn alloc_id(&mut self, state: OpState) -> u64 {
        self.next_id += 1;
        self.states.insert(self.next_id, state);
        self.next_id
    }

    /// Queue an SQE (backlog when the SQ is full; flushed next submit).
    fn push_sqe(&mut self, entry: squeue::Entry) {
        self.backlog.push_back(entry);
    }

    fn flush_backlog(&mut self) -> io::Result<()> {
        // M0-S19 deliberate-regression canary: submit after every SQE so the
        // `sqes_per_submit` tripwire must trip in the gate report. Test-only.
        let submit_per_op = std::env::var_os("INF_URING_SUBMIT_PER_OP").is_some();
        while let Some(entry) = self.backlog.pop_front() {
            if submit_per_op {
                self.ring.submitter().submit()?;
                self.stats.syscalls += 1;
            }
            // SAFETY: every entry was built over resources (fds, buffer
            // addresses) that stay live until its CQE arrives — fds are not
            // closed before their cancellations complete, and pool buffer
            // addresses are stable for the pool's lifetime.
            if unsafe { self.ring.submission().push(&entry) }.is_err() {
                // SQ full: hand the kernel what we have and retry once.
                self.ring.submitter().submit()?;
                self.stats.syscalls += 1;
                // SAFETY: as above.
                if unsafe { self.ring.submission().push(&entry) }.is_err() {
                    self.backlog.push_front(entry);
                    return Err(io::Error::other("io_uring SQ stuck full after submit"));
                }
            }
            self.stats.sqes += 1;
        }
        Ok(())
    }

    fn arm_accept_sqe(&mut self, listener: RawFd, token: CompletionToken) {
        let id = self.alloc_id(OpState::Accept { listener, token });
        let entry = if self.caps.multishot_accept {
            opcode::AcceptMulti::new(Fd(listener)).build().user_data(id)
        } else {
            opcode::Accept::new(Fd(listener), core::ptr::null_mut(), core::ptr::null_mut())
                .build()
                .user_data(id)
        };
        self.push_sqe(entry);
    }

    fn set_recv_op(&mut self, fd: RawFd, id: u64) {
        if let Some(arm) = self.recvs.get_mut(&fd) {
            arm.op_id = Some(id);
        }
    }

    /// Arm (or re-arm) recv on `fd`: multishot over the provided group when
    /// available; otherwise a oneshot recv over a pool lease, degrading to a
    /// `PollDry` readability watch while the pool is dry (so the eventual
    /// `RecvDropped` coincides with actual pending data).
    fn arm_recv_sqe(&mut self, fd: RawFd, token: CompletionToken, pool: &mut BufferPool) {
        if self.caps.multishot_recv {
            let id = self.alloc_id(OpState::RecvMulti { fd, token });
            let entry = opcode::RecvMulti::new(Fd(fd), BGID).build().user_data(id);
            self.push_sqe(entry);
            self.set_recv_op(fd, id);
            return;
        }
        match pool.try_lease(LeaseKind::Recv) {
            Some(buf) => self.arm_oneshot_recv(fd, token, buf, pool),
            None => {
                let id = self.alloc_id(OpState::PollDry { fd, token });
                let entry = opcode::PollAdd::new(Fd(fd), libc::POLLIN as u32).build().user_data(id);
                self.push_sqe(entry);
                self.set_recv_op(fd, id);
            }
        }
    }

    fn arm_oneshot_recv(
        &mut self,
        fd: RawFd,
        token: CompletionToken,
        buf: BufferId,
        pool: &mut BufferPool,
    ) {
        let len = u32::try_from(pool.buf_size()).expect("buffer size fits u32");
        let addr = pool.bytes_mut(buf).as_mut_ptr();
        let id = self.alloc_id(OpState::RecvOneshot { fd, token, buf });
        let entry = opcode::Recv::new(Fd(fd), addr, len).build().user_data(id);
        self.push_sqe(entry);
        self.set_recv_op(fd, id);
    }

    /// Provided-group recv budget: half the pool, min 1. The other half is
    /// the send/response headroom — staging the whole pool would let inbound
    /// traffic starve RESPOND (deadlock by buffer exhaustion).
    fn recv_budget(pool: &BufferPool) -> usize {
        (pool.capacity() / 2).max(1)
    }

    /// Keep the kernel's provided group stocked up to the recv budget, then
    /// auto-resume recvs that paused on buffer exhaustion — but only once a
    /// buffer is actually available again (re-arming an empty group would
    /// just re-drop).
    fn replenish_and_resume(&mut self, pool: &mut BufferPool) {
        if self.caps.multishot_recv {
            assert!(pool.capacity() <= usize::from(u16::MAX), "provided bids are u16");
            let len = i32::try_from(pool.buf_size()).expect("buffer size fits i32");
            while self.provided.len() < Self::recv_budget(pool) {
                let Some(buf) = pool.try_stage() else { break };
                let bid = u16::try_from(buf.as_u32()).expect("checked capacity above");
                let addr = pool.bytes_mut(buf).as_mut_ptr();
                let id = self.alloc_id(OpState::Provide { buf });
                self.provided.insert(bid, buf);
                // One SQE per buffer: pool buffers are individually boxed
                // (not one contiguous region), so nbufs > 1 cannot describe
                // them.
                let entry =
                    opcode::ProvideBuffers::new(addr, len, 1, BGID, bid).build().user_data(id);
                self.push_sqe(entry);
            }
        }
        let paused: Vec<(RawFd, CompletionToken)> = self
            .recvs
            .iter()
            .filter(|(_, a)| a.paused && !a.disarmed && a.op_id.is_none())
            .map(|(fd, a)| (*fd, a.token))
            .collect();
        for (fd, token) in paused {
            let resumable = if self.caps.multishot_recv {
                !self.provided.is_empty()
            } else {
                pool.available() > 0
            };
            if !resumable {
                break;
            }
            self.recvs.get_mut(&fd).expect("collected above").paused = false;
            self.arm_recv_sqe(fd, token, pool);
        }
    }

    fn apply_ops(&mut self, pool: &mut BufferPool, out: &mut Vec<Completion>) {
        let ops = core::mem::take(&mut self.pending_ops);
        for op in ops {
            match op {
                IoOp::AcceptArm { listener, token } => {
                    self.accepts.insert(listener, token);
                    self.arm_accept_sqe(listener, token);
                }
                IoOp::RecvArm { fd, token } => {
                    self.recvs
                        .insert(fd, RecvArm { token, op_id: None, disarmed: false, paused: false });
                    self.arm_recv_sqe(fd, token, pool);
                }
                IoOp::RecvDisarm { fd } => {
                    let Some(arm) = self.recvs.get_mut(&fd) else { continue };
                    arm.disarmed = true;
                    if let Some(op_id) = arm.op_id {
                        let id = self.alloc_id(OpState::Cancel);
                        self.push_sqe(opcode::AsyncCancel::new(op_id).build().user_data(id));
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
                    let addr = pool.bytes(buf).as_ptr();
                    let id = self.alloc_id(OpState::Send { fd, token, buf, len, written: 0 });
                    self.push_sqe(opcode::Send::new(Fd(fd), addr, len).build().user_data(id));
                    *self.sends_inflight.entry(fd).or_insert(0) += 1;
                }
                IoOp::Close { fd, token } => {
                    // Cancel everything in flight on this fd (ops hold file
                    // refs; close alone would strand them), then close. The
                    // `Closed` completion is held until in-flight sends
                    // resolve so their buffers return first.
                    let cancel_ids: Vec<u64> = self
                        .states
                        .iter()
                        .filter_map(|(id, st)| match st {
                            OpState::Accept { listener, .. } if *listener == fd => Some(*id),
                            OpState::RecvMulti { fd: f, .. }
                            | OpState::RecvOneshot { fd: f, .. }
                            | OpState::PollDry { fd: f, .. }
                            | OpState::Send { fd: f, .. }
                                if *f == fd =>
                            {
                                Some(*id)
                            }
                            _ => None,
                        })
                        .collect();
                    for op_id in cancel_ids {
                        let id = self.alloc_id(OpState::Cancel);
                        self.push_sqe(opcode::AsyncCancel::new(op_id).build().user_data(id));
                    }
                    self.accepts.remove(&fd);
                    self.recvs.remove(&fd);
                    self.closing
                        .insert(fd, CloseWait { token, close_seen: false, close_result: 0 });
                    let id = self.alloc_id(OpState::Close { fd });
                    self.push_sqe(opcode::Close::new(Fd(fd)).build().user_data(id));
                }
            }
        }
    }

    /// Emit `Closed` once the close CQE arrived and no sends remain.
    fn maybe_finish_close(&mut self, fd: RawFd, out: &mut Vec<Completion>) {
        let sends_left = self.sends_inflight.get(&fd).copied().unwrap_or(0);
        let Some(wait) = self.closing.get(&fd) else { return };
        if !wait.close_seen || sends_left > 0 {
            return;
        }
        let wait = self.closing.remove(&fd).expect("checked above");
        self.sends_inflight.remove(&fd);
        out.push(Completion {
            token: wait.token,
            result: if wait.close_result >= 0 {
                CompletionResult::Closed
            } else {
                CompletionResult::Error { errno: -wait.close_result, buf: None }
            },
        });
    }

    fn send_resolved(&mut self, fd: RawFd, out: &mut Vec<Completion>) {
        if let Some(n) = self.sends_inflight.get_mut(&fd) {
            *n = n.saturating_sub(1);
        }
        self.maybe_finish_close(fd, out);
    }

    fn dispatch_cqe(
        &mut self,
        id: u64,
        result: i32,
        flags: u32,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        if cqueue::more(flags) {
            // Multishot CQE stream: state stays until the final (!more) CQE.
            self.dispatch_multishot_cqe(id, result, flags, pool, out);
            return;
        }
        let Some(state) = self.states.remove(&id) else { return };
        self.dispatch_terminal_cqe(state, result, flags, pool, out);
    }

    /// Non-terminal multishot CQE (`F_MORE` set): the op stays armed.
    fn dispatch_multishot_cqe(
        &mut self,
        id: u64,
        result: i32,
        flags: u32,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        enum Multi {
            Accept(CompletionToken),
            Recv(RawFd, CompletionToken),
        }
        // Copy out what the arm needs; the borrow must end before the
        // `&mut self` payload handlers run.
        let multi = match self.states.get(&id) {
            Some(OpState::Accept { token, .. }) => Multi::Accept(*token),
            Some(OpState::RecvMulti { fd, token }) => Multi::Recv(*fd, *token),
            _ => return,
        };
        match multi {
            Multi::Accept(token) => {
                if result >= 0 {
                    set_nonblocking(result);
                    out.push(Completion {
                        token,
                        result: CompletionResult::Accepted { fd: result },
                    });
                }
            }
            Multi::Recv(fd, token) => {
                self.handle_recv_payload(fd, token, result, flags, pool, out);
            }
        }
    }

    /// Terminal CQE: the op id is retired; multishot ops may re-arm.
    fn dispatch_terminal_cqe(
        &mut self,
        state: OpState,
        result: i32,
        flags: u32,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        match state {
            OpState::Accept { listener, token } => {
                if result >= 0 {
                    set_nonblocking(result);
                    out.push(Completion {
                        token,
                        result: CompletionResult::Accepted { fd: result },
                    });
                } else if result != -libc::ECANCELED {
                    out.push(Completion {
                        token,
                        result: CompletionResult::Error { errno: -result, buf: None },
                    });
                }
                // Multishot ended (or oneshot fired): re-arm while armed.
                if result != -libc::ECANCELED && self.accepts.contains_key(&listener) {
                    self.arm_accept_sqe(listener, token);
                }
            }
            OpState::RecvMulti { fd, token } => {
                if let Some(arm) = self.recvs.get_mut(&fd) {
                    arm.op_id = None;
                }
                self.handle_recv_payload(fd, token, result, flags, pool, out);
                // Multishot stream ended: re-arm unless disarmed/paused/EOF.
                let rearm =
                    result > 0 && self.recvs.get(&fd).is_some_and(|a| !a.disarmed && !a.paused);
                if rearm {
                    self.arm_recv_sqe(fd, token, pool);
                }
            }
            OpState::RecvOneshot { fd, token, buf } => {
                if let Some(arm) = self.recvs.get_mut(&fd) {
                    arm.op_id = None;
                }
                if result >= 0 {
                    out.push(Completion {
                        token,
                        result: CompletionResult::Recv { buf, len: result as u32 },
                    });
                } else {
                    // Cancelled/errored before data: the lease unwinds
                    // internally; the consumer never owned this buffer.
                    pool.release(buf);
                    if result != -libc::ECANCELED {
                        out.push(Completion {
                            token,
                            result: CompletionResult::Error { errno: -result, buf: None },
                        });
                    }
                }
                let rearm =
                    result > 0 && self.recvs.get(&fd).is_some_and(|a| !a.disarmed && !a.paused);
                if rearm {
                    // Dry pool degrades to the PollDry watch internally.
                    self.arm_recv_sqe(fd, token, pool);
                }
            }
            OpState::PollDry { fd, token } => {
                let Some(arm) = self.recvs.get_mut(&fd) else { return };
                arm.op_id = None;
                if result == -libc::ECANCELED || arm.disarmed {
                    return;
                }
                if result < 0 {
                    out.push(Completion {
                        token,
                        result: CompletionResult::Error { errno: -result, buf: None },
                    });
                    return;
                }
                // Readable while we had no buffer: deliver if one freed up;
                // otherwise THIS is the honest RecvDropped moment — data is
                // pending and the pool is dry. Auto-resume on release.
                match pool.try_lease(LeaseKind::Recv) {
                    Some(buf) => self.arm_oneshot_recv(fd, token, buf, pool),
                    None => {
                        let arm = self.recvs.get_mut(&fd).expect("checked above");
                        arm.paused = true;
                        out.push(Completion { token, result: CompletionResult::RecvDropped });
                    }
                }
            }
            OpState::Send { fd, token, buf, len, written } => {
                if result >= 0 {
                    let written = written + result as u32;
                    if written < len {
                        // Short write: resubmit the remainder; the op id is
                        // re-allocated, the buffer stays consumer-owned.
                        let addr = pool.bytes(buf)[written as usize..].as_ptr();
                        let id = self.alloc_id(OpState::Send { fd, token, buf, len, written });
                        self.push_sqe(
                            opcode::Send::new(Fd(fd), addr, len - written).build().user_data(id),
                        );
                        return;
                    }
                    out.push(Completion { token, result: CompletionResult::Sent { buf } });
                } else {
                    let errno = if result == -libc::ECANCELED { libc::ECANCELED } else { -result };
                    out.push(Completion {
                        token,
                        result: CompletionResult::Error { errno, buf: Some(buf) },
                    });
                }
                self.send_resolved(fd, out);
            }
            OpState::Close { fd, .. } => {
                if let Some(wait) = self.closing.get_mut(&fd) {
                    wait.close_seen = true;
                    wait.close_result = result;
                }
                self.maybe_finish_close(fd, out);
            }
            OpState::Cancel => {}
            OpState::Provide { buf } => {
                if result < 0 {
                    // Group rejected the buffer: unwind the staging.
                    let bid = u16::try_from(buf.as_u32()).expect("provided bids are u16");
                    self.provided.remove(&bid);
                    pool.unstage(buf);
                }
            }
        }
    }

    /// Shared recv-payload handling for multishot CQEs (terminal or not).
    fn handle_recv_payload(
        &mut self,
        fd: RawFd,
        token: CompletionToken,
        result: i32,
        flags: u32,
        pool: &mut BufferPool,
        out: &mut Vec<Completion>,
    ) {
        if result >= 0 {
            match cqueue::buffer_select(flags) {
                Some(bid) => {
                    // Buffer leaves the kernel group; custody → consumer.
                    let buf = self
                        .provided
                        .remove(&bid)
                        .expect("kernel returned a bid this driver never provided");
                    pool.promote_staged(buf);
                    out.push(Completion {
                        token,
                        result: CompletionResult::Recv { buf, len: result as u32 },
                    });
                }
                None => {
                    // EOF without a buffer (result == 0): the contract
                    // delivers EOF as a zero-length Recv with a buffer, so
                    // lease one; if dry, pause — EOF re-delivers on resume.
                    debug_assert_eq!(result, 0, "data CQE without buffer flag");
                    match pool.try_lease(LeaseKind::Recv) {
                        Some(buf) => out.push(Completion {
                            token,
                            result: CompletionResult::Recv { buf, len: 0 },
                        }),
                        None => {
                            if let Some(arm) = self.recvs.get_mut(&fd) {
                                arm.paused = true;
                            }
                            out.push(Completion { token, result: CompletionResult::RecvDropped });
                        }
                    }
                }
            }
        } else if result == -libc::ENOBUFS {
            if let Some(arm) = self.recvs.get_mut(&fd)
                && !arm.paused
            {
                arm.paused = true;
                out.push(Completion { token, result: CompletionResult::RecvDropped });
            }
        } else if result != -libc::ECANCELED {
            out.push(Completion {
                token,
                result: CompletionResult::Error { errno: -result, buf: None },
            });
        }
    }
}

impl BackendDriver for UringDriver {
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
        self.stats = SubmitStats::default();
        self.apply_ops(pool, out);
        self.replenish_and_resume(pool);
        self.flush_backlog()?;

        // ONE io_uring_enter for everything queued (+ the wait). GETEVENTS
        // only rides `want > 0` in the io-uring crate, and DEFER_TASKRUN
        // posts CQEs only on a GETEVENTS enter — so the non-blocking paths
        // use want=1 with a ZERO timeout to harvest deferred completions
        // without sleeping (plain submit() would stall them until a park).
        let already_satisfied = out.len() > before;
        let zero_ts = types::Timespec::new();
        let submit_result = match wait {
            Wait::Park { timeout: Some(d) } if !already_satisfied => {
                let ts = types::Timespec::new().sec(d.as_secs()).nsec(d.subsec_nanos());
                let args = types::SubmitArgs::new().timespec(&ts);
                self.ring.submitter().submit_with_args(1, &args)
            }
            Wait::Park { timeout: None } if !already_satisfied => {
                self.ring.submitter().submit_and_wait(1)
            }
            _ => {
                let args = types::SubmitArgs::new().timespec(&zero_ts);
                self.ring.submitter().submit_with_args(1, &args)
            }
        };
        self.stats.syscalls += 1;
        match submit_result {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ETIME) | Some(libc::EINTR) | Some(libc::EBUSY)
                ) => {}
            Err(e) => return Err(e),
        }

        // Drain the CQ completely (bounded by CQ size).
        let cqes: Vec<(u64, i32, u32)> = self
            .ring
            .completion()
            .map(|cqe| (cqe.user_data(), cqe.result(), cqe.flags()))
            .collect();
        for (id, result, flags) in cqes {
            self.dispatch_cqe(id, result, flags, pool, out);
        }
        // Re-arms/replenishes triggered by CQEs ride the next submit (L3).

        let produced = out.len() - before;
        self.stats.cqes = produced as u64;
        Ok(produced)
    }

    fn register_pool(&mut self, pool: &mut BufferPool) -> io::Result<()> {
        // Fixed-buffer registration: enumerate stable addresses by leasing
        // the whole pool once (ids are dense), then releasing.
        let count = pool.capacity();
        let mut ids = Vec::with_capacity(count);
        let mut iovecs = Vec::with_capacity(count);
        for _ in 0..count {
            let id = pool.try_lease(LeaseKind::Recv).expect("pool fully free at registration");
            ids.push(id);
        }
        for &id in &ids {
            let bytes = pool.bytes_mut(id);
            iovecs.push(libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() });
        }
        // SAFETY: iovecs describe pool-owned boxed slices whose addresses
        // are stable for the pool's lifetime (inf-alloc invariant); the
        // kernel may read/write them only through ops we issue.
        let registered = unsafe { self.ring.submitter().register_buffers(&iovecs) };
        for id in ids {
            pool.release(id);
        }
        // Registration failure (RLIMIT_MEMLOCK, old kernel) degrades the
        // capability rather than failing boot — the data path uses plain
        // Send/Recv at M0 either way (fixed-buffer data path is an A3-tier
        // measured follow-up).
        self.caps.fixed_buffers = registered.is_ok();
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}

impl core::fmt::Debug for UringDriver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "UringDriver {{ caps: {:?}, in_kernel: {}, provided_held: {} }}",
            self.caps,
            self.states.len(),
            self.provided.len()
        )
    }
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl on an fd the kernel just handed us; failure leaves it
    // blocking, which is harmless under uring (ops are async regardless).
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    set_nodelay(fd);
}

/// Replies are usually sub-MSS; without NODELAY the Nagle timer + delayed
/// ACK serialize pipelined round-trips at ~40 ms (measured by the echo
/// bench). Accepted sockets are TCP at M0; failure (non-TCP fd) is ignored.
fn set_nodelay(fd: RawFd) {
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

/// `uname -r` major.minor for feature gates the probe can't express.
fn kernel_version() -> (u32, u32) {
    // SAFETY: utsname is plain-old-data; uname fills it or fails.
    let mut uts: libc::utsname = unsafe { core::mem::zeroed() };
    // SAFETY: passing a valid out-pointer.
    if unsafe { libc::uname(&mut uts) } != 0 {
        return (0, 0);
    }
    let release = uts.release.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char);
    let text: String = release.collect();
    let mut parts = text.split(['.', '-']);
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}
