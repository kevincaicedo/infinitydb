//! `inf-foundation` — shared vocabulary for InfinityDB (master plan §20).
//!
//! Types, ids, time/randomness injection seams (L7), stable hashing, CRC16
//! slot math, varints, the always-on latency histogram, and the frozen
//! tripwire counter names. This crate is dependency-free and fully safe.
#![forbid(unsafe_code)]
// ADR-0144 D1: a production `match` names every variant of its enum.
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

// The supported targets are 64-bit (io_uring / kqueue hosts). `bound:`
// proofs across the workspace (ADR-0144 D2/D3) rest on it: a sum of a
// `Vec` length (<= isize::MAX) and u32-ranged on-disk lengths fits `usize`.
const _: () = assert!(usize::BITS >= 64, "InfinityDB targets 64-bit hosts");

mod addr;
mod crc;
mod device;
pub mod fault;
mod footprint;
mod hash;
mod hist;
mod ids;
mod local;
pub mod rng;
pub mod time;
pub mod tripwire;
pub mod varint;

pub use addr::LogicalAddr;
pub use crc::{crc16, hashtag};
pub use device::{DeviceIdentity, IdentityVerdict};
pub use footprint::rc_allocation_bytes;
pub use hash::{
    BuildIntHasher, COLLISION_KEY_PREFIX, COLLISION_ORACLE, IntHasher, KeyHashId, KeyHasher,
    hash64, siphash13,
};
pub use hist::LogHistogram;
pub use ids::{CellId, KeySlot, SLOT_COUNT};
pub use local::{CachePadded, LocalCounter};
