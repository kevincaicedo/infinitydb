//! `inf-server` — dispatch and transports (master plan §20). M0 contents:
//! the command execution layer (M0-S15) mapping parsed argv through the
//! `inf-wire` registry onto `inf-store` ops with RESP2/RESP3 replies, and
//! the node assembly: [`ServerPlane`], one cell's complete data plane over
//! any backend driver (`infinityd` = uring/kqueue, `inf-sim` = sim).
#![forbid(unsafe_code)]

mod exec;
mod plane;

pub use exec::{ConnCx, NodeInfo, execute, execute_slices};
pub use plane::{ExecOrigin, NoopObserver, OwnedOutcome, PlaneObserver, ServerPlane};
