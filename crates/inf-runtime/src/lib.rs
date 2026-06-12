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

mod driver;
mod executor;
pub mod gate;
pub mod net;
mod reactor;
mod sched;
mod timer;
mod token;

#[cfg(target_os = "macos")]
mod kqueue;
#[cfg(all(target_os = "linux", feature = "uring"))]
mod uring;

pub use driver::{
    BackendDriver, Capabilities, Completion, CompletionResult, IoOp, RawFd, SubmitStats, Wait,
};
pub use executor::{CellExecutor, PollImmediate, TaskId};
pub use gate::{FabricGate, IoGate, WaitList, WatermarkGate};
pub use reactor::{CellLoop, CellPlane, IterStats, LoopConfig, LoopCx};
pub use sched::{GroupClass, GroupScheduler};
pub use timer::{TimerId, TimerWheel};
pub use token::{CompletionToken, TokenClass};

#[cfg(target_os = "macos")]
pub use kqueue::KqueueDriver;
#[cfg(all(target_os = "linux", feature = "uring"))]
pub use uring::UringDriver;
