//! `inf-foundation` limits: one `const` per bound, with its unit, its owner
//! and what a caller observes when it is crossed (INFINITY_STYLE "Put a
//! limit on everything").

/// Bytes one positional driver op may move (ADR-0167 D1): `u32::MAX`, the
/// width of a driver op's byte range (`StableBytes` and `StableBytesMut`
/// carry a `u32` length). Owner: the driver seam. Crossing: nothing wider is
/// ever built; a cold-read pool whose buffer is wider is refused when
/// `ColdReads::with_config` builds it (a misconfiguration, caught before
/// anything is served).
pub const DRIVER_OP_BYTES_MAX: u64 = u32::MAX as u64;

/// Largest file position a positional driver op may carry (ADR-0167 D1):
/// `i64::MAX − DRIVER_OP_BYTES_MAX`, so an op's span end (position + bytes)
/// is at most `i64::MAX`. That keeps every position the kernel reads inside
/// its `loff_t` range: never negative, never io_uring's `−1` "current file
/// position" sentinel, never a wrap. It is the positional-op contract, not a
/// filesystem's size limit (past that a write fails `EFBIG` and a read
/// returns 0 bytes). Owner: the driver seam; `FileOffset::new` is the one
/// check. Crossing: `FileOffsetRefused` carrying the value, and no op is
/// built. The producer answers it: a cold read is refused `Unrepresentable`,
/// a tier flush round fails stop `Unaddressable`, a checkpoint aborts.
pub const FILE_OFFSET_BYTES_MAX: u64 = i64::MAX.cast_unsigned() - DRIVER_OP_BYTES_MAX;

// Every position and span end a `FileOffset` yields is in `0..=i64::MAX`.
const _: () = assert!(
    FILE_OFFSET_BYTES_MAX + DRIVER_OP_BYTES_MAX == i64::MAX.cast_unsigned(),
    "a driver op's span end is at most i64::MAX"
);
// `FileOffset::from_u32_bytes` is total.
const _: () =
    assert!(DRIVER_OP_BYTES_MAX <= FILE_OFFSET_BYTES_MAX, "every u32 position is addressable");

/// Distinct cap names one cell's census holds (ADR-0163 D2): the rows of
/// `CapCensus`, one per capped-container name registered on that cell, about
/// 3 KiB. Owner: `inf_foundation::bounded::CapCensus`. Crossing: the 65th
/// name is refused at construction with `CapError::Census`, before anything
/// is served; a name registered again with the same cap and fill shares its
/// row (one row per namespace-instanced container).
pub const CAP_CENSUS_ROWS_MAX: usize = 64;

// A container's row handle keeps its row's index in one byte.
const _: () = assert!(CAP_CENSUS_ROWS_MAX <= 256, "a census row index fits a row handle's u8");

/// Slab identities one cell's census mints (ADR-0171 A2.1): a
/// `SlabInstance` is the cell's id beside a 16-bit ordinal from 1, minted
/// once per capped store (a slab's one, a map's two) and never twice. Owner:
/// `inf_foundation::bounded::CapCensus`. Crossing: the next store's
/// construction is refused with `CapError::Census` before anything is
/// built; a site that builds stores while serving derives its count.
pub const CAP_CENSUS_SLABS_MAX: u16 = u16::MAX;

/// Bytes one backing allocation of a capped container requests: a deque's
/// one allocation, a slab's or a map's chunk or directory (ADR-0151 D5).
/// Owner: the capped types. Crossing: a shape whose allocation passes it is
/// refused at construction with `CapError::Layout`; nothing is allocated
/// and no oversized fallback is tried.
pub const MAP_BACKING_ALLOCATION_BYTES_MAX: usize = 64 * 1024;

/// Bytes of a `CappedDeque`'s one backing allocation, `entries × stride`:
/// the per-allocation bound lifted to the deque. Owner:
/// `inf_foundation::bounded::CappedDeque`. Crossing: a cap whose entries
/// times its payload stride passes it is refused at construction with
/// `CapError::Layout`; a site that needs more says so in its record.
pub const DEQUE_BACKING_BYTES_MAX: usize = MAP_BACKING_ALLOCATION_BYTES_MAX;

/// Slots one capped store holds at most (ADR-0151 D5): `u32::MAX`, so a
/// slot index fits `SlotId`'s `u32` and stays below `u32::MAX`, which the
/// free chain keeps as its end. Owner: the slab's and the map's storage.
/// Crossing: unwritable — a cap is a `u32` const generic, so no larger cap
/// can be declared, and 10¹² entries in one container are refused before
/// construction by the type.
pub const MAP_SLOT_COUNT_MAX: u64 = u32::MAX as u64;

// Every slot index is below `u32::MAX`, the free chain's end mark.
const _: () = assert!(
    MAP_SLOT_COUNT_MAX <= u32::MAX as u64,
    "a slot index fits SlotId's u32 index below the free chain's end"
);

/// Slots per chunk of a slab's or a map's storage (ADR-0151 D5): one chunk
/// is one allocation, built or retired in one step. Owner: the storage.
/// Crossing: the next slots take the next chunk, built by a later step; a
/// chunk whose slots times their stride passes
/// `MAP_BACKING_ALLOCATION_BYTES_MAX` is `CapError::Layout`.
pub const MAP_CHUNK_SLOTS_MAX: usize = 64;

/// Children per directory of a slab's or a map's storage (ADR-0151 D5).
/// Owner: the storage. Crossing: the next children take a sibling
/// directory, or a directory level above, within `MAP_DIRECTORY_LEVELS_MAX`.
pub const MAP_DIRECTORY_CHILDREN_MAX: usize = 256;

/// Directory levels above a store's chunks (ADR-0151 D5), derived from the
/// slot range and the chunk and fan-out constants: the fewest levels whose
/// chunks cover `MAP_SLOT_COUNT_MAX`, 4 (64 × 256⁴ slots). A store of
/// `n` slots takes `directory_levels(n)`; a slot access follows that many
/// links, one up to 16,384 slots. Owner: the storage. Crossing: a store
/// needing more levels than its root can hold is `CapError::Layout`, which
/// this derivation makes unreachable for any `u32` cap.
pub const MAP_DIRECTORY_LEVELS_MAX: u32 = directory_levels(MAP_SLOT_COUNT_MAX);

/// The directory levels a store of `slots` slots needs: the fewest whose
/// chunks cover it (0 for one chunk, at most 64 slots).
#[must_use]
pub const fn directory_levels(slots: u64) -> u32 {
    let mut covered = MAP_CHUNK_SLOTS_MAX as u64;
    let mut levels = 0;
    while covered < slots {
        covered = covered.saturating_mul(MAP_DIRECTORY_CHILDREN_MAX as u64);
        levels += 1;
    }
    levels
}
