//! `inf-alloc` — per-cell memory: wire buffer pools, record arenas, slabs,
//! and byte-exact accounting (L5). Unsafe leaf crate: any `unsafe` here is
//! inventoried in `SAFETY.md` and covered by Miri in CI.
//!
//! M0 contents: `BufferPool` (wire buffers, registered with the backend
//! driver) and the record `Arena` (size-class slabs over mmap chunks).

pub mod buffer_pool;

pub use buffer_pool::{BufferId, BufferPool, LeaseKind, LeaseLeak};

// The record arena (M0-S13) is added by the store/alloc story; the buffer
// pool above is frozen contract (docs/interfaces-m0.md §2).
