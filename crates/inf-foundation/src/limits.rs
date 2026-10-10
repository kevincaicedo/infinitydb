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
/// 2.5 KiB. Owner: `inf_foundation::bounded::CapCensus`. Crossing: the 65th
/// name is refused at construction with `CapError::Census`, before anything
/// is served; a name registered again with the same cap and fill shares its
/// row (one row per namespace-instanced container).
pub const CAP_CENSUS_ROWS_MAX: usize = 64;

/// Bytes of a `CappedDeque`'s one backing allocation, `entries × stride`
/// (ADR-0151 D5's per-allocation bound, 64 KiB, lifted to the deque). Owner:
/// `inf_foundation::bounded::CappedDeque`. Crossing: a cap whose entries
/// times its payload stride passes it is refused at construction with
/// `CapError::Layout`; a site that needs more says so in its record.
pub const DEQUE_BACKING_BYTES_MAX: usize = 64 * 1024;
