//! `inf-alloc` — per-cell memory: wire buffer pools, record arenas, slabs,
//! and byte-exact accounting (L5). Unsafe leaf crate: any `unsafe` here is
//! inventoried in `SAFETY.md` and covered by Miri in CI.
//!
//! M0 contents: `BufferPool` (wire buffers, registered with the backend
//! driver) and the record `Arena` (size-class slabs over mmap chunks).

pub mod arena;
pub mod buffer_pool;
#[cfg(any(test, feature = "test-counting-allocator"))]
mod counting_allocator;
pub mod region;

pub use arena::{Arena, ArenaAddr, ArenaConfig, ArenaReport};
pub use buffer_pool::{BufferId, BufferPool, LeaseKind, LeaseLeak};
#[cfg(any(test, feature = "test-counting-allocator"))]
pub use counting_allocator::CountingAllocator;
pub use region::{REGION_PAGE_BYTES, Region, RegionConfig, RegionReport};
