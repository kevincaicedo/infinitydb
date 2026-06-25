//! `inf-server` — dispatch and transports (master plan §20). M0 contents:
//! the command execution layer (M0-S15) mapping parsed argv through the
//! `inf-wire` registry onto `inf-store` ops with RESP2/RESP3 replies, and
//! the node assembly: [`ServerPlane`], one cell's complete data plane over
//! any backend driver (`infinityd` = uring/kqueue, `inf-sim` = sim).
#![forbid(unsafe_code)]

mod admin;
pub mod checkpoint;
mod clients;
mod config;
pub mod durability;
mod exec;
mod glob;
pub mod log_bootstrap;
pub mod log_maintenance;
pub mod log_writer;
pub mod manifest;
pub mod ns_catalog;
mod plane;
mod pubsub;
pub mod recovery;

pub use clients::{ClientInfo, ClientRegistry};
pub use config::{ConfigSetError, ConfigStore, MAXMEMORY_POLICIES, ReloadClass};
#[doc(hidden)]
pub use exec::parse_cursor;
pub use exec::{ConnCx, NodeInfo, execute, execute_durable, execute_slices, stall_request};
pub use glob::glob_match;
pub use plane::{ExecOrigin, NoopObserver, OwnedOutcome, PlaneObserver, ServerPlane};
