//! `inf-foundation` — shared vocabulary for InfinityDB (master plan §20).
//!
//! Types, ids, time/randomness injection seams (L7), stable hashing, CRC16
//! slot math, varints, the always-on latency histogram, and the frozen
//! tripwire counter names. This crate is dependency-free and fully safe.
#![forbid(unsafe_code)]

mod crc;
mod hash;
mod hist;
mod ids;
mod local;
pub mod rng;
pub mod time;
pub mod tripwire;
pub mod varint;

pub use crc::{crc16, hashtag};
pub use hash::hash64;
pub use hist::LogHistogram;
pub use ids::{CellId, KeySlot, SLOT_COUNT};
pub use local::{CachePadded, LocalCounter};
