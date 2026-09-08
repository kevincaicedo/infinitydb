//! Per-cell log addressing (master plan §8.1): a record's identity is its
//! **LSN = (segment, offset)**, scoped to the owning cell. There is no
//! global LSN — cross-cell ordering, when it matters, is established by
//! `txid` records (M4), and recovery replays cells independently (L1).

use core::fmt;
use std::io;

/// Largest physical segment length whose one-past-the-end byte is still a
/// representable [`Lsn`] offset. The log's address range is the format's,
/// not the current rotation size: an older segment may be larger than
/// today's `segment_bytes` and still be replayable.
pub const MAX_SEGMENT_LEN: u64 = u32::MAX as u64;

/// Monotonic id of one segment file within a cell's log directory.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SegmentId(pub u32);

impl SegmentId {
    /// The next segment in the sequence. Panics on id-space exhaustion —
    /// 2³² segments × 256 MiB is an exbibyte of log per cell, an internal
    /// invariant rather than a reachable runtime condition.
    #[inline]
    #[must_use]
    pub fn next(self) -> SegmentId {
        SegmentId(self.0.checked_add(1).expect("segment id space exhausted"))
    }
}

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "seg-{:06}", self.0)
    }
}

/// Log sequence number: byte address of a record (or frame) within the
/// owning cell's log. Ordering is lexicographic (segment, then offset) —
/// exactly append order, which the derived `Ord` provides field-by-field.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Lsn {
    pub segment: SegmentId,
    pub offset: u32,
}

impl Lsn {
    #[inline]
    #[must_use]
    pub fn new(segment: SegmentId, offset: u32) -> Lsn {
        Lsn { segment, offset }
    }

    /// The LSN `bytes` further into the same segment. Panics on offset
    /// overflow — offsets are bounded by the segment size (≤ 4 GiB by
    /// construction), so overflow is an internal invariant violation.
    #[inline]
    #[must_use]
    pub fn advance(self, bytes: u32) -> Lsn {
        Lsn {
            segment: self.segment,
            offset: self.offset.checked_add(bytes).expect("LSN offset overflow"),
        }
    }

    /// Order-preserving packing `(segment << 32) | offset` — the durability
    /// watermark key (M2-S06, ADR-0013 D3): LSN order is lexicographic
    /// (segment, offset), which is exactly `u64` order on this value, so
    /// `WatermarkGate`'s "wake ≤ watermark" agrees with append order.
    #[inline]
    #[must_use]
    pub fn to_u64(self) -> u64 {
        (u64::from(self.segment.0) << 32) | u64::from(self.offset)
    }

    /// Inverse of [`to_u64`](Self::to_u64) — decodes the order-preserving
    /// packed form (the `.ick` header's `begin-LSN` field, M2-S10).
    #[inline]
    #[must_use]
    pub fn from_u64(packed: u64) -> Lsn {
        Lsn { segment: SegmentId((packed >> 32) as u32), offset: packed as u32 }
    }
}

/// Refuse a segment file whose end cannot be addressed by an [`Lsn`]
/// offset (ADR-0018). The rotor seals before a frame would cross
/// `segment_bytes` (a `u32`), so an oversized file is planted or foreign;
/// recovery and both read paths name it rather than wrap the cursor.
///
/// # Errors
/// [`io::ErrorKind::InvalidData`] naming the segment, its length and the limit.
pub fn check_segment_len(segment: SegmentId, len: u64) -> io::Result<()> {
    if len > MAX_SEGMENT_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{segment}: size {len} exceeds u32 segment address limit {MAX_SEGMENT_LEN}"),
        ));
    }
    Ok(())
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:08x}", self.segment, self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_orders_by_segment_then_offset() {
        let a = Lsn::new(SegmentId(1), 900);
        let b = Lsn::new(SegmentId(2), 100);
        let c = Lsn::new(SegmentId(2), 200);
        assert!(a < b && b < c);
    }

    #[test]
    fn advance_stays_in_segment() {
        let base = Lsn::new(SegmentId(17), 0x100);
        assert_eq!(base.advance(0x20), Lsn::new(SegmentId(17), 0x120));
    }

    #[test]
    fn segment_len_bound_is_the_representable_end() {
        assert!(check_segment_len(SegmentId(3), MAX_SEGMENT_LEN).is_ok());
        let err = check_segment_len(SegmentId(3), MAX_SEGMENT_LEN + 1).expect_err("one past");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("seg-000003"), "{err}");
        assert_eq!(Lsn::new(SegmentId(3), 0).advance(u32::MAX).offset, u32::MAX);
    }

    #[test]
    fn to_u64_preserves_order() {
        let ordered = [
            Lsn::new(SegmentId(0), 0),
            Lsn::new(SegmentId(0), 20),
            Lsn::new(SegmentId(0), u32::MAX),
            Lsn::new(SegmentId(1), 0),
            Lsn::new(SegmentId(1), 900),
            Lsn::new(SegmentId(2), 100),
            Lsn::new(SegmentId(u32::MAX), u32::MAX),
        ];
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(pair[0].to_u64() < pair[1].to_u64(), "{} vs {}", pair[0], pair[1]);
        }
    }
}
