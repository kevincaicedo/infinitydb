//! `inf-alloc` — per-cell memory: wire buffer pools, record arenas, slabs,
//! and byte-exact accounting (L5). Unsafe leaf crate: any `unsafe` here is
//! inventoried in `SAFETY.md` and covered by Miri in CI.
//!
//! M0 contents: `BufferPool` (wire buffers, registered with the backend
//! driver) and the record `Arena` (size-class slabs over mmap chunks).

pub mod arena;
pub mod buffer_pool;

pub use arena::{Arena, ArenaAddr, ArenaConfig, ArenaReport};
pub use buffer_pool::{BufferId, BufferPool, LeaseKind, LeaseLeak};
