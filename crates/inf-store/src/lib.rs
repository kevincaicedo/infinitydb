//! `inf-store` — per-cell records, index, string ops, TTL wheel, and slot
//! routing (master plan §7, milestones M0-E5, M1-E1/E2). Never sees a socket
//! or a RESP byte (§3.3): commands arrive as parsed arguments through
//! `inf-server`, time arrives injected as `Nanos` (L7), and memory is
//! attributed byte-exactly (L5).
#![forbid(unsafe_code)]

mod index;
mod record;
mod router;
mod store;
mod wheel;

pub use index::Index;
pub use inf_alloc::ArenaConfig;
pub use record::{MAX_KEY_LEN, MAX_VAL_LEN, TypeTag};
pub use router::SlotRouter;
pub use store::{
    CellStore, CopyResult, Encoding, ExpireCond, ExpiryBudget, ExpiryStats, MemoryReport, OpError,
    SetCond, SetExpire, SetOptions, SetOutcome, StoreConfig, StoreStats, Ttl, TtlUpdate,
};
