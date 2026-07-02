//! `inf-server` — dispatch and transports (master plan §20). M0 contents:
//! the command execution layer (M0-S15) mapping parsed argv through the
//! `inf-wire` registry onto `inf-store` ops with RESP2/RESP3 replies, and
//! the node assembly: [`ServerPlane`], one cell's complete data plane over
//! any backend driver (`infinityd` = uring/kqueue, `inf-sim` = sim).
//!
//! `unsafe` posture (M2-S08, ADR-0015 D4): `deny` at the crate root with
//! exactly one audited opt-out module (`log_bytes` — see `SAFETY.md`).
#![deny(unsafe_code)]

mod admin;
mod clients;
mod config;
mod control;
mod durable;
mod exec;
mod glob;
mod log_bytes;
mod plane;
mod pubsub;
mod recover;

pub use clients::{ClientInfo, ClientRegistry};
pub use config::{ConfigSetError, ConfigStore, MAXMEMORY_POLICIES, ReloadClass};
pub use control::{ControlHandle, load_catalog, spawn as spawn_control};
pub use durable::{DurableConfig, DurableStats};
#[doc(hidden)]
pub use exec::parse_cursor;
pub use exec::{ConnCx, NodeInfo, execute, execute_slices, stall_request};
pub use glob::glob_match;
pub use plane::{ExecOrigin, NoopObserver, OwnedOutcome, PlaneObserver, ServerPlane};
pub use recover::{RecoverStats, open_cell_log};
