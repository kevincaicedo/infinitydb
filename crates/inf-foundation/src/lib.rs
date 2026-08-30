//! `inf-foundation` — shared vocabulary for InfinityDB (master plan §20).
//!
//! Types, ids, time/randomness injection seams (L7), stable hashing, CRC16
//! slot math, varints, the always-on latency histogram, and the frozen
//! tripwire counter names. This crate is dependency-free and fully safe.
#![forbid(unsafe_code)]

mod addr;
mod crc;
mod device;
pub mod fault;
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
pub use hash::{
    BuildIntHasher, COLLISION_KEY_PREFIX, IntHasher, KeyHashId, KeyHasher, hash64, siphash13,
};
pub use hist::LogHistogram;
pub use ids::{CellId, KeySlot, SLOT_COUNT};
pub use local::{CachePadded, LocalCounter};
