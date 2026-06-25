//! `inf-store` — per-cell records, index, string ops, TTL wheel, and slot
//! routing (master plan §7, milestones M0-E5, M1-E1/E2). Never sees a socket
//! or a RESP byte (§3.3): commands arrive as parsed arguments through
//! `inf-server`, time arrives injected as `Nanos` (L7), and memory is
//! attributed byte-exactly (L5).
#![forbid(unsafe_code)]

mod effect;
mod evict;
mod index;
mod keyspace;
mod ns;
mod record;
mod router;
mod store;
mod wheel;

pub use effect::{MutationEffect, MutationSink, NoMutationSink};
pub use evict::{EvictStats, EvictionPolicy};
pub use index::Index;
pub use inf_alloc::ArenaConfig;
pub use keyspace::{DEFAULT_DBS, EvictBudget, Keyspace, PressureConfig};
pub use ns::{
    MAX_NAMED_NAMESPACES, MAX_NAMESPACE_CATALOG_BYTES, MAX_NS_NAME_LEN, NsCatalog, NsCatalogError,
    NsCreateSpec, NsError, NsFsyncPolicy, NsId, NsMode, NsSpec, decode_namespace_catalog,
    encode_namespace_catalog, valid_ns_name,
};
pub use record::{MAX_EXPIRE_MS, MAX_KEY_LEN, MAX_VAL_LEN, TypeTag};
pub use router::SlotRouter;
pub use store::{
    CellStore, CheckpointStoreRecord, CheckpointWalk, CheckpointWalkBudget, CheckpointWalkError,
    CopyResult, Encoding, ExpireCond, ExpiryBudget, ExpiryStats, MemoryReport, OpError, SetCond,
    SetExpire, SetOptions, SetOutcome, StoreConfig, StoreStats, Ttl, TtlUpdate,
};
