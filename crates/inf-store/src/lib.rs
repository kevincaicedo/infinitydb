//! `inf-store` — per-cell records, index, string ops, TTL wheel, and slot
//! routing (master plan §7, milestones M0-E5, M1-E1/E2). Never sees a socket
//! or a RESP byte (§3.3): commands arrive as parsed arguments through
//! `inf-server`, time arrives injected as `Nanos` (L7), and memory is
//! attributed byte-exactly (L5). Since M2-S08 it consumes `inf-log`'s
//! effect/record vocabulary (ADR-0012 D1 / ADR-0015 D7) — it still never
//! opens a file.
#![forbid(unsafe_code)]

mod address_space;
mod catalog;
mod demote;
mod doc;
mod evict;
mod extents;
/// Named fault points (M4.5-S04): inventory consumed by
/// `scripts/check-fault-points.sh` and armed by tests.
pub mod fault;
mod index;
mod index_backfill;
mod index_key;
mod index_maint;
mod index_registry;
mod index_sidecar;
mod keyspace;
mod live_set;
mod ns;
mod ordered;
mod record;
mod router;
mod store;
mod tiered;
mod tiered_recover;
mod wall;
mod wheel;
mod write_accounting;

pub use address_space::{
    AddrClass, AddressSpace, AddressSpaceConfig, AddressSpaceReport, TieringCounters,
};
pub use catalog::{CatalogError, IndexCatalog, NsCatalog};
pub use demote::{DemoteStats, DemotionConfig, EvictionPressure, MUTABLE_PERMILLE_DEFAULT};
pub use doc::DocDomain;
#[cfg(feature = "doc")]
pub use doc::{JsonLogDecision, JsonRead, JsonScalarPatch, JsonSetOptions, JsonSetOutcome};
pub use evict::{EvictStats, EvictionPolicy};
pub use extents::{
    BLOB_MAX_BYTES_DEFAULT, BLOB_RECLAIM_PER_SLICE_DEFAULT, BLOB_THRESHOLD_DEFAULT, BlobConfig,
    ExtentRefs, ExtentStats,
};
pub use index::{Index, MemoryMode, SlotMode, TieredMode};
pub use index_backfill::{
    BackfillBudget, BackfillInfo, BackfillPhase, BackfillProgress, BackfillTickStats,
};
pub use index_key::{
    DecodedIndexKey, INDEX_KEY_ENCODING_VERSION, IndexKeyBuf, IndexKeyDecodeError, IndexKeyType,
    IndexScalar, KeySkip, compare_i64_f64, index_key_decode, index_key_encode,
    index_key_escape_prefix, index_scalar_coerce,
};
pub use index_maint::{BRACKET_ENTRY_CAP, IdxCounters, IdxMaintRefusal, MaintMode};
#[cfg(feature = "doc")]
pub use index_registry::validate_index_program;
pub use index_registry::{
    FIRST_INDEX_GENERATION, FIRST_INDEX_ID, INDEX_PROGRAM_MAX, INDEXES_PER_NODE_MAX,
    IndexBindError, IndexError, IndexId, IndexMemory, IndexRegistry, IndexSpec, IndexState,
    IndexTree, SidecarBootDecision, SidecarRebuildReason,
};
#[cfg(feature = "doc")]
pub use index_sidecar::SidecarLoader;
pub use index_sidecar::{SidecarBootInfo, SidecarBootRow};
pub use inf_alloc::ArenaConfig;
// One import point for the shared store↔log vocabulary (ADR-0015 D2/D5).
pub use inf_foundation::LogicalAddr;
pub use inf_log::{FsyncClass, NsId};
pub use keyspace::{
    DEFAULT_DBS, EvictBudget, Keyspace, PressureConfig, ReplayError, ReplayOutcome, StateDigest,
    TIERED_VA_LIMIT_DEFAULT, TieredCreateError, TieredUsage,
};
pub use live_set::{FileLiveSet, LiveSet};
pub use ns::{FIRST_NAMED_NS_ID, NsError, NsMode, NsSpec, TierSpec, valid_ns_name};
pub use ordered::{
    AppendError, Fixed8, KeyScheme, ORDERED_KEY_MAX, OrderedCursor, OrderedMap, OrderedMapError,
    OrderedMapMemory, VarKey,
};
pub use record::{EXTENT_REF_LEN, ExtentRef, MAX_KEY_LEN, MAX_VAL_LEN, TypeTag};
pub use router::SlotRouter;
pub use store::{
    CellStore, CheckpointImage, CopyResult, DiskFullCause, Encoding, ExpireCond, ExpiryBudget,
    ExpiryStats, LogFullImage, MemoryReport, OpError, PostImage, SetCond, SetExpire, SetOptions,
    SetOutcome, StoreConfig, StoreStats, Ttl, TtlUpdate,
};
pub use tiered::compact::{CompactionApplied, CompactionConfig, CompactionWork};
pub use tiered::{RecordParts, TieredLookup, TieredTable};
pub use tiered_recover::{
    RecoveredTier, TierRecoverStats, apply_blob_ref_section, apply_live_set_section,
    apply_ref_section, recover_tiered_ns,
};
pub use wall::WallAnchor;
pub use write_accounting::{
    WriteAccounting, WriteAccountingTotals, WriteAmpSummary, WriteAmplification,
};
