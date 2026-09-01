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
mod ckpt;
mod clients;
mod config;
mod control;
mod data_dir_lock;
mod durable;
mod exec;
pub mod fault;
mod glob;
mod io_properties;
#[cfg(feature = "doc")]
mod json;
mod key_hash;
mod log_bytes;
mod plane;
mod pubsub;
mod readahead;
mod recover;
mod tier_cell;
mod topology;

/// Process exit code for a durable-path fail-stop (§8.4, the fsyncgate
/// rule): an fsync/log-write error freezes the watermark — no ack for the
/// affected batch ever fires — and the process exits with this code
/// (M2-S17, ADR-0020 D3). Boot-recovery failure keeps exit code 1
/// (`infinityd`'s `take_boot_error` path).
pub const EXIT_DURABLE_FAILSTOP: i32 = 3;

pub use ckpt::{CkptStats, ManifestStats};
pub use clients::{ClientInfo, ClientRegistry};
pub use config::{ConfigSetError, ConfigStore, MAXMEMORY_POLICIES, ReloadClass};
pub use control::{
    CellRecoverySlot, ControlHandle, ControlInbox, INDEX_SLOTS, IndexBoard, RecoveryBoard,
    load_catalog, load_catalog_from, spawn as spawn_control,
};
pub use data_dir_lock::{DataDirLock, DataDirLockError, LOCK_FILE};
pub use durable::{
    DeviceConfig, DurableConfig, DurableStats, FillConfig, GroupDecision, GroupHoldConfig,
    RecoverConfig,
};
// Durable-config vocabulary re-exported for the assembly tier: bins name
// `inf-server` only (dep-DAG), and `DurableConfig`'s fields are these
// types — an assembly cannot fill one without naming them.
#[doc(hidden)]
pub use exec::parse_cursor;
pub use exec::{ConnCx, ConnNamespace, NodeInfo, execute, execute_slices, stall_request};
pub use glob::glob_match;
pub use inf_log::ckpt::{DEFAULT_CKPT_INTERVAL_BYTES, DEFAULT_REPLAY_BYTES_PER_S};
pub use inf_log::fs::StdSegmentFs;
#[cfg(feature = "doc")]
pub use json::{JSON_REPLY_SHAPES, ReplyShape};
// M2-S18 (ADR-0020 D6/D7): the sim tier's disk, re-exported for the
// assembly/simulator tier exactly like `StdSegmentFs` above — bins name
// `inf-server` only (dep-DAG).
pub use inf_log::fs::sim::{SimDisk, SimDiskConfig, StallConfig};
pub use inf_log::{
    CkptConfig, DEFAULT_FUA_MAX_FRAME_BYTES, DEFAULT_RECYCLE_SLOTS, DEFAULT_SEGMENT_BYTES,
    FRAME_ALIGN, FramesInFlight, MAX_FRAMES_IN_FLIGHT, PoolWaitBound, PreallocPolicy,
    SegmentConfig, SegmentIoMode, StagingConfig,
};
pub use io_properties::{
    IO_PROPERTIES_FILE, IoProperties, IoPropertiesError, IoPropertiesSource, IoProvenance,
};
pub use key_hash::{
    KEY_HASH_FILE, KeyHashBinding, KeyHashError, KeyHashSource, create_key_hash,
    directory_has_data, load_key_hash, parse_key_hash, render_key_hash, resolve_key_hash,
    verify_key_hash_binding,
};
pub use plane::{ExecOrigin, NoopObserver, OwnedOutcome, PlaneObserver, ServerPlane};
pub use readahead::{ReadAheadFile, ReadAheadFs};
pub use recover::{
    RecoverPhase, RecoverPhases, RecoverStats, RecoveredManifest, Recovery, RecoveryProgress,
    open_cell_log,
};
pub use topology::{
    TOPOLOGY_FILE, TopologyError, TopologySource, create_topology, load_topology, parse_topology,
    render_topology, resolve_topology,
};
