//! `inf-store` — per-cell records, index, string ops, and slot routing
//! (master plan §7, milestone M0-E5). Never sees a socket or a RESP byte
//! (§3.3): commands arrive as parsed arguments through `inf-server`, time
//! arrives injected as `Nanos` (L7), and memory is attributed byte-exactly
//! (L5).
#![forbid(unsafe_code)]

mod index;
mod record;
mod router;
mod store;

pub use index::Index;
pub use inf_alloc::ArenaConfig;
pub use record::{MAX_KEY_LEN, MAX_VAL_LEN, TypeTag};
pub use router::SlotRouter;
pub use store::{
    CellStore, ExpireCond, MemoryReport, OpError, SetCond, SetExpire, SetOptions, SetOutcome,
    StoreConfig, Ttl,
};
