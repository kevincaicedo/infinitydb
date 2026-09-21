//! `inf-alloc` — per-cell memory: wire buffer pools, record arenas, slabs,
//! and byte-exact accounting (L5). Unsafe leaf crate: any `unsafe` here is
//! inventoried in `SAFETY.md` and covered by Miri in CI.
//!
//! M0 contents: `BufferPool` (wire buffers, registered with the backend
//! driver) and the record `Arena` (size-class slabs over mmap chunks).

// §17.3 as amended (ADR-0121, batch 44): an audited leaf is still
// `deny(unsafe_code)` at the root — every unsafe-bearing module is a
// named `allow` that SAFETY.md inventories; `buffer_pool` stays safe.
#![deny(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

#[allow(unsafe_code)]
pub mod aligned;
#[allow(unsafe_code)]
pub mod arena;
pub mod buffer_pool;
#[cfg(any(test, feature = "test-counting-allocator"))]
#[allow(unsafe_code)]
mod counting_allocator;
#[allow(unsafe_code)]
pub mod region;

pub use aligned::{AlignedBox, AlignedBufId, AlignedLeak, AlignedPool, TIER_READ_ALIGN};
pub use arena::{Arena, ArenaAddr, ArenaConfig, ArenaReport};
pub use buffer_pool::{BufferId, BufferPool, LeaseKind, LeaseLeak};
#[cfg(any(test, feature = "test-counting-allocator"))]
pub use counting_allocator::CountingAllocator;
pub use region::{REGION_PAGE_BYTES, Region, RegionConfig, RegionReport};
