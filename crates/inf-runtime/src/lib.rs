//! `inf-runtime` — the shard-cell runtime (master plan §5, milestone M0-E2):
//! backend drivers behind one typed contract, the single-threaded cell
//! executor, typed suspension gates, the timer wheel, scheduler groups, and
//! the 10-step reactor loop.
//!
//! This is the only crate allowed to name io_uring/kqueue symbols (§3.3).
//! `unsafe` is confined to three audited areas — backend FFI, the Rc waker
//! vtable, and type-erased task storage — see `SAFETY.md`.
//!
//! Backends: `UringDriver` (Linux, `--features uring`, the performance
//! tier), [`KqueueDriver`] (macOS, correctness-only dev tier), and the
//! simulator driver implemented in `inf-sim` (M0-S20) against
//! [`BackendDriver`].

// §17.3 as amended (ADR-0121, batch 44): "parts of `inf-runtime`" is a
// root `deny(unsafe_code)` with the backend/affinity/executor modules as
// named `allow`s (SAFETY.md); the reactor, scheduler, timer, token,
// budget and gate modules are safe and stay so.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod affinity;
mod budget;
#[allow(unsafe_code)]
mod cold;
#[allow(unsafe_code)]
mod driver;
#[allow(unsafe_code)]
mod executor;
pub mod gate;
#[allow(unsafe_code)]
pub mod net;
mod reactor;
mod sched;
pub mod signal;
mod timer;
mod token;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod kqueue;
#[cfg(all(target_os = "linux", feature = "uring"))]
#[allow(unsafe_code)]
mod uring;

pub use affinity::unpin_current_thread;
pub use budget::{
    Admission, BURST_HORIZON_NS, ClassCounters, ClassSlice, DeviceBudget, DeviceModel,
    FLOOR_DIVISOR, IoClass, SealPace,
};
pub use cold::{
    ColdDone, ColdLeak, ColdReadConfig, ColdReadCounters, ColdReads, ColdRefused, ColdWait,
    ReadClass, TierFileId,
};
pub use driver::{
    AcceptFailure, BackendDriver, Capabilities, Completion, CompletionResult, IoOp, RawFd,
    StableBytes, StableBytesMut, SubmitStats, Wait, WriteBarrier, classify_accept_errno,
};
pub use executor::{CellExecutor, PollImmediate, TaskId};
pub use gate::{FabricGate, GateWait, IoGate, WaitList, WatermarkGate, WatermarkWait};
pub use reactor::{CellLoop, CellPlane, IterStats, LoopConfig, LoopCx};
pub use sched::{GroupClass, GroupScheduler};
pub use timer::{TimerId, TimerWheel};
pub use token::{CompletionToken, MAX_SLOT, TokenClass};

#[cfg(target_os = "macos")]
pub use kqueue::KqueueDriver;
#[cfg(all(target_os = "linux", feature = "uring"))]
pub use uring::UringDriver;
