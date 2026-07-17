//! `inf-store` — per-cell records, index, string ops, TTL wheel, and slot
//! routing (master plan §7, milestones M0-E5, M1-E1/E2). Never sees a socket
//! or a RESP byte (§3.3): commands arrive as parsed arguments through
//! `inf-server`, time arrives injected as `Nanos` (L7), and memory is
//! attributed byte-exactly (L5). Since M2-S08 it consumes `inf-log`'s
//! effect/record vocabulary (ADR-0012 D1 / ADR-0015 D7) — it still never
//! opens a file.
#![forbid(unsafe_code)]

mod catalog;
mod doc;
mod evict;
mod index;
mod keyspace;
mod ns;
mod record;
mod router;
mod store;
mod wall;
mod wheel;

pub use catalog::{CatalogError, NsCatalog};
pub use doc::DocDomain;
#[cfg(feature = "doc")]
pub use doc::{JsonLogDecision, JsonRead, JsonScalarPatch, JsonSetOptions, JsonSetOutcome};
pub use evict::{EvictStats, EvictionPolicy};
pub use index::Index;
pub use inf_alloc::ArenaConfig;
// One import point for the shared store↔log vocabulary (ADR-0015 D2/D5).
pub use inf_log::{FsyncClass, NsId};
pub use keyspace::{
    DEFAULT_DBS, EvictBudget, Keyspace, PressureConfig, ReplayError, ReplayOutcome, StateDigest,
};
pub use ns::{FIRST_NAMED_NS_ID, NsError, NsMode, NsSpec, valid_ns_name};
pub use record::{MAX_KEY_LEN, MAX_VAL_LEN, TypeTag};
pub use router::SlotRouter;
pub use store::{
    CellStore, CheckpointImage, CopyResult, Encoding, ExpireCond, ExpiryBudget, ExpiryStats,
    LogFullImage, MemoryReport, OpError, PostImage, SetCond, SetExpire, SetOptions, SetOutcome,
    StoreConfig, StoreStats, Ttl, TtlUpdate,
};
pub use wall::WallAnchor;
