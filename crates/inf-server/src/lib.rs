//! `inf-server` — dispatch and transports (master plan §20). M0 contents:
//! the command execution layer (M0-S15) mapping parsed argv through the
//! `inf-wire` registry onto `inf-store` ops with RESP2/RESP3 replies, and
//! the node assembly: [`ServerPlane`], one cell's complete data plane over
//! any backend driver (`infinityd` = uring/kqueue, `inf-sim` = sim).
#![forbid(unsafe_code)]

mod admin;
mod clients;
mod config;
mod exec;
mod glob;
mod plane;
mod pubsub;

pub use clients::{ClientInfo, ClientRegistry};
pub use config::{ConfigSetError, ConfigStore, MAXMEMORY_POLICIES, ReloadClass};
pub use exec::{ConnCx, NodeInfo, execute, execute_slices, stall_request};
pub use glob::glob_match;
pub use plane::{ExecOrigin, NoopObserver, OwnedOutcome, PlaneObserver, ServerPlane};
