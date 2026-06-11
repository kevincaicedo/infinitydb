//! `BackendDriver` — the one typed contract for submission/completion
//! batches (milestone M0-S04, frozen at M0 exit per §3.2).
//!
//! Three implementations stay swappable behind it: `UringDriver` (Linux,
//! `--features uring`), [`KqueueDriver`](crate::KqueueDriver) (macOS dev
//! tier), and the simulator driver (`inf-sim`, M0-S20). The contract is
//! completion-shaped even on readiness backends: kqueue performs the syscall
//! at readiness and delivers the result as a completion, so cell code never
//! forks on backend.
//!
//! Buffer discipline (the Vortex lifecycle proof, carried): every
//! [`BufferId`] handed out by a completion is owned by the consumer until
//! released back to the pool; every buffer held by a failed or cancelled op
//! is returned in its terminal completion. `BufferPool::reconcile()` must
//! hold after any op storm.

use std::io;
use std::time::Duration;

use inf_alloc::{BufferId, BufferPool};

use crate::token::CompletionToken;

/// Raw platform fd. Cell code treats it as an opaque handle; only drivers
/// perform syscalls on it.
pub type RawFd = std::os::fd::RawFd;

/// Operations a cell may queue. `push` never performs a syscall — everything
/// goes out in the single `submit_and_reap` per loop iteration (L3).
#[derive(Debug)]
pub enum IoOp {
    /// Arm accepting on a listening socket. Multishot where the backend
    /// supports it (one arm yields `Accepted` completions until disarmed or
    /// terminal error); re-armed internally otherwise. The same token rides
    /// on every resulting completion.
    AcceptArm { listener: RawFd, token: CompletionToken },
    /// Arm receiving on a connection. The DRIVER leases recv buffers from
    /// the pool and delivers them in completions; the consumer owns each
    /// delivered buffer and must `release` it. Multishot where supported.
    RecvArm { fd: RawFd, token: CompletionToken },
    /// Backpressure seam: stop reading this connection (fabric credits
    /// exhausted, M0-S09). Level semantics — a later `RecvArm` re-arms.
    RecvDisarm { fd: RawFd },
    /// Write exactly `len` bytes from `buf`. Completes only when all bytes
    /// are written or a terminal error occurs. The caller leased the buffer;
    /// ownership returns in the completion (`Sent` or `Error`).
    Send { fd: RawFd, buf: BufferId, len: u32, token: CompletionToken },
    /// Close the fd. Pending sends on it complete with `Error(ECANCELED)`
    /// (returning their buffers) before `Closed` is delivered.
    Close { fd: RawFd, token: CompletionToken },
}

/// One reaped completion: the token that was armed plus the outcome.
#[derive(Debug)]
pub struct Completion {
    pub token: CompletionToken,
    pub result: CompletionResult,
}

#[derive(Debug)]
pub enum CompletionResult {
    /// A new connection. The fd is non-blocking and owned by the consumer.
    Accepted {
        fd: RawFd,
    },
    /// `len == 0` ⇒ peer closed (EOF). The buffer is owned by the consumer
    /// in both cases and must be released.
    Recv {
        buf: BufferId,
        len: u32,
    },
    /// The pool was dry when data arrived: recv on this fd is paused and
    /// will resume automatically once buffers return to the pool. No buffer
    /// changes hands. (Bounded-everything: pool exhaustion is backpressure.)
    RecvDropped,
    /// All bytes written; the buffer returns to the consumer.
    Sent {
        buf: BufferId,
    },
    Closed,
    /// Terminal failure. Any buffer the op still held ALWAYS comes back
    /// here; `None` means no consumer-owned buffer was involved.
    Error {
        errno: i32,
        buf: Option<BufferId>,
    },
}

/// How long `submit_and_reap` may block waiting for completions.
#[derive(Copy, Clone, Debug)]
pub enum Wait {
    /// Submit + harvest whatever is ready; never block (spin phase).
    Poll,
    /// Park until a completion or the timeout (next timer deadline) —
    /// `None` parks indefinitely.
    Park { timeout: Option<Duration> },
}

/// Boot-time feature probe, logged at startup and attached to gate reports
/// (a gate run on a degraded backend is a disposition note, not a pass).
#[derive(Copy, Clone, Debug)]
pub struct Capabilities {
    pub backend: &'static str,
    /// One accept arm yields completions until disarmed (vs internal re-arm).
    pub multishot_accept: bool,
    /// One recv arm yields completions until disarmed.
    pub multishot_recv: bool,
    /// Kernel-side provided-buffer pool (io_uring `ProvideBuffers`).
    pub provided_buffers: bool,
    /// Buffers pre-registered with the kernel (`register_buffers`).
    pub fixed_buffers: bool,
    /// `IORING_SETUP_SINGLE_ISSUER` accepted.
    pub single_issuer: bool,
    /// `IORING_SETUP_DEFER_TASKRUN` accepted.
    pub defer_taskrun: bool,
    /// This backend is measured for performance gates. The kqueue dev tier
    /// is correctness-only and must never appear in a gate artifact.
    pub performance_tier: bool,
}

/// Per-`submit_and_reap` batching stats, feeding the frozen tripwire
/// counters `sqes_per_submit` / `cqes_per_reap` (M0-S19).
#[derive(Copy, Clone, Debug, Default)]
pub struct SubmitStats {
    /// Syscalls made by the last `submit_and_reap` (uring target: 1).
    pub syscalls: u64,
    /// SQEs (or readiness ops) carried by the last submit.
    pub sqes: u64,
    /// Completions harvested by the last reap.
    pub cqes: u64,
}

/// The submission/completion contract (frozen at M0 exit).
///
/// Single-threaded by design (L1): a driver instance belongs to exactly one
/// cell thread. Nothing here is `Send`.
pub trait BackendDriver {
    /// Queue an op. No syscall happens here.
    fn push(&mut self, op: IoOp);

    /// Flush everything queued and harvest completions — ONE backend entry
    /// point per loop iteration (L3). Returns the number of completions
    /// appended to `out`.
    ///
    /// # Errors
    /// Only backend-fatal conditions (e.g. the ring/queue itself failed)
    /// surface as `Err`; per-op failures are `CompletionResult::Error`
    /// completions so buffer ownership always unwinds through the pool.
    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize>;

    /// Register the pool's buffers with the backend (fixed/provided buffers
    /// on io_uring; a no-op on readiness backends).
    ///
    /// # Errors
    /// Backend-fatal registration failures only; probe-driven fallbacks
    /// (e.g. no provided-buffer support) degrade silently into
    /// [`Capabilities`] instead of erroring.
    fn register_pool(&mut self, pool: &mut BufferPool) -> io::Result<()>;

    /// The boot-time feature probe result.
    fn capabilities(&self) -> Capabilities;

    /// Batching stats for the most recent `submit_and_reap`.
    fn submit_stats(&self) -> SubmitStats;
}
