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

/// Live wakers one lane of one wake header counts (ADR-0003 A1.2.6): 2⁶⁰ − 1. A header's wakers
/// are `w = o + c`, the owner lane's count `o` and the foreign lane's `c`; a lane falls below 0
/// only by drops of wakers the other lane counted, so neither passes ±2⁶⁰ − 1, `c` fits the lane
/// word's 61-bit field and `w` an `i64`. Owner: the wake header's lanes, each of which compares
/// its count with this one const before the count grows (the foreign lane's in
/// `inf_foundation::wake`). Crossing: fail-stop — a clone, or a `close` folding `o` into `c`, that
/// would pass it aborts the process before the count is written (`std::process::abort`, as `Rc`
/// and `Arc` do at theirs). Memory bounds no count here (`mem::forget` of a waker is safe and
/// holds none), and a header's reuse resets neither, so only code that forgets 2⁶⁰ clones, or
/// moves 2⁶⁰ net clones between the lanes over one header's life, reaches it: about 36 years of one
/// core at 10⁹ a second (arithmetic, not a measurement).
pub const LANE_COUNT_MAX: i64 = (1 << 60) - 1;

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

/// Index bits of a fabric gate's key word (ADR-0177 D4: `tag:1 |
/// generation:23 | index:24` in the fabric token's 48-bit sequence, the tag
/// above both fields): a gate numbers at most 2²⁴ holders. Owner: the
/// fabric gate, whose word adapter is built from its holder slab, and the
/// mesh's token mint, which wraps below the tag. Crossing: a holder slab of
/// more slots is refused `CapError::Layout` when the gate builds its
/// adapter, at boot, before anything is served.
pub const GATE_KEY_INDEX_BITS: u32 = 24;

/// Generation bits of a fabric gate's key word (ADR-0177 D4): the low 23 bits
/// of a holder's `u32` generation, which a reply's word is resolved by
/// (ADR-0194 D6 with A2). Owner: the fabric gate. Crossing: none; the field
/// wraps every 2²³ uses of a slot, and a reply aliasing a later occupant is
/// a defect the holder slab's `WrapWithProof` proof rules out, never reuse.
pub const GATE_KEY_GENERATION_BITS: u32 = 23;

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

/// Bits of one `CappedMap` key (ADR-0151 D1, D5): 128, an integer word of
/// up to 128 bits or a pair of `u64`s, compared whole and read bit by bit.
/// A lookup visits at most this many branches. Owner:
/// `inf_foundation::bounded::ExactKey`, a sealed set of key types. Crossing:
/// unrepresentable — a wider or inexact key cannot implement the sealed
/// trait, so no map over it is written.
pub const MAP_KEY_BITS_MAX: u32 = 128;

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

/// The most slots one chunk of a slab's or a map's storage holds (ADR-0151
/// D5): one chunk is one allocation, built or retired in one step, and a
/// store's chunk holds the count `chunk_slots_for` answers for its slot's
/// bytes, this many for a slot of at most 1,024 B (ADR-0194 A1). Owner: the
/// storage. Crossing: the next slots take the next chunk, built by a later
/// step; a slot no chunk holds is `CapError::Layout`.
pub const MAP_CHUNK_SLOTS_MAX: usize = 64;

/// The slots one chunk of a capped store holds for a slot of `slot_bytes`
/// bytes (ADR-0194 A1): the most, a power of two up to
/// `MAP_CHUNK_SLOTS_MAX`, whose bytes fit one backing allocation,
/// `min(64, 2^⌊log₂(65,536 ÷ slot_bytes)⌋)`: 64 up to 1,024 B a slot, 32 up
/// to 2,048, halving per doubling, 1 up to 65,536. Owner: the slab's and the
/// map's storage, whose builder takes a chunk of exactly this count. Crossing:
/// `None` past `MAP_BACKING_ALLOCATION_BYTES_MAX` bytes a slot; that slot, and
/// a store built at any other count, is `CapError::Layout` at construction,
/// before anything is registered. One canary answers 64 at every width.
#[must_use]
pub const fn chunk_slots_for(slot_bytes: usize) -> Option<usize> {
    #[cfg(inf_canary_chunk_slots_fixed)]
    {
        let _ = slot_bytes;
        Some(MAP_CHUNK_SLOTS_MAX)
    }
    #[cfg(not(inf_canary_chunk_slots_fixed))]
    {
        let mut slots = MAP_CHUNK_SLOTS_MAX;
        while slots > 0 {
            if let Some(bytes) = slots.checked_mul(slot_bytes)
                && bytes <= MAP_BACKING_ALLOCATION_BYTES_MAX
            {
                return Some(slots);
            }
            slots /= 2;
        }
        None
    }
}

/// Children per directory of a slab's or a map's storage (ADR-0151 D5).
/// Owner: the storage. Crossing: the next children take a sibling
/// directory, or a directory level above, within `MAP_DIRECTORY_LEVELS_MAX`.
pub const MAP_DIRECTORY_CHILDREN_MAX: usize = 256;

// A slot index splits into a chunk offset and one child per directory level
// by shifts and masks, which is exact only for powers of two; every chunk
// `chunk_slots_for` answers halves down from `MAP_CHUNK_SLOTS_MAX`, so it is
// one too.
const _: () = assert!(MAP_CHUNK_SLOTS_MAX.is_power_of_two(), "a chunk's slots are a power of two");
const _: () = assert!(
    MAP_DIRECTORY_CHILDREN_MAX.is_power_of_two(),
    "a directory's children are a power of two"
);

/// Directory levels above a store's chunks (ADR-0151 D5, ADR-0194 A1),
/// derived from the slot range and the fan-out at the smallest chunk any
/// slot takes, one slot: the fewest levels whose chunks cover
/// `MAP_SLOT_COUNT_MAX`, 4 (256⁴ = 2³² slots). A store of `n` slots in
/// chunks of `c` takes `directory_levels(n, c)`, never more at a larger
/// chunk; a slot access follows that many links, one up to `256 × c` slots.
/// Owner: the storage. Crossing: a store needing more levels than its root
/// can hold is `CapError::Layout`, which this derivation and a const
/// assertion beside the store's root (four directory levels) make
/// unreachable for any `u32` cap at any chunk.
pub const MAP_DIRECTORY_LEVELS_MAX: u32 = directory_levels(MAP_SLOT_COUNT_MAX, 1);

/// The directory levels a store of `slots` slots in chunks of `chunk_slots`
/// needs: the fewest whose chunks cover it (0 for one chunk, at most
/// `chunk_slots` slots). A chunk of no slots covers none at any depth, so a
/// store of slots in it needs more levels than any root holds.
#[must_use]
pub const fn directory_levels(slots: u64, chunk_slots: usize) -> u32 {
    if chunk_slots == 0 {
        return if slots == 0 { 0 } else { u32::MAX };
    }
    let mut covered = chunk_slots as u64;
    let mut levels = 0;
    while covered < slots {
        covered = covered.saturating_mul(MAP_DIRECTORY_CHILDREN_MAX as u64);
        levels += 1;
    }
    levels
}

/// Tiered namespace lifetimes one node holds at once (ADR-0148 D1, A1.1):
/// 64, counting every grant the catalog writer has Reserved, Declared or
/// still Retiring. One grant is one position on every cell, so a cell holds
/// at most 64 tiered runtime owners, a dropped one counted until each of its
/// owners ends. Owner: the catalog writer's grant book (`inf-server`'s
/// `limits` re-exports this one value). Crossing: a tiered `INF.NS CREATE`
/// is refused before any effect, `-TRYAGAIN tiered namespace capacity in
/// use` while a grant is Retiring, else `-ERR tiered namespace limit
/// reached`; a boot whose catalog names more live tiered definitions stops
/// before any cell with a typed error.
pub const TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX: usize = 64;

// A position is a `u8`, and the lane a position carries leaves a flush
// token's slot room for its 8-bit op index.
const _: () = assert!(TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX <= 256, "a position fits a u8");

/// Tier-file cleanup jobs one cell holds out at once (ADR-0156 D2, ADR-0157
/// D1): 8 job positions, each from its offer to its consumed result, and
/// the slots of each of the cell's two retained rings (requests to the
/// control thread, results back), so a ring slot is free whenever a
/// position is. Owner: the cell's job table; `inf-fabric` sizes the
/// retained ring by it. Crossing: an offer with no position free is
/// deferred, its owner left where it is, until a result is consumed.
pub const TIER_RETIRE_JOBS_PER_CELL: usize = 8;

// The retained ring indexes its slots by mask.
const _: () = assert!(TIER_RETIRE_JOBS_PER_CELL.is_power_of_two(), "ring slots are a power of two");
