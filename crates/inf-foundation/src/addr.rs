//! Logical addresses for the tiered address space (M4-S01, master plan §9,
//! ADR-0051).
//!
//! A [`LogicalAddr`] is a 48-bit coordinate in one cell's per-namespace
//! monotonically-growing address space. The raw value is exactly what an
//! index slot's `addr:48` field stores for a tiered table (the M0 slot
//! layout reinterpreted — M4-S02); for memory-mode tables the same field
//! keeps meaning `inf_alloc::ArenaAddr`, discriminated at table
//! granularity, never per op.
//!
//! The newtype lives here (not in `inf-store`, the semantics owner) because
//! `inf-log`'s `TierStore` seam takes addresses in and the dep DAG points
//! `inf-store → inf-log` — neither can host a type the other consumes
//! (ADR-0051). Semantics — watermarks, regions, resolution — stay in
//! `inf-store`; this type is arithmetic only.

/// A 48-bit logical address in a per-cell-per-namespace address space.
///
/// Invariant: `raw ≤ MAX_RAW` (checked at every constructor). Addresses
/// are ordered, never reused within a boot life, and only meaningful
/// relative to the owning namespace's address space.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalAddr(u64);

impl LogicalAddr {
    /// Largest representable raw value (the index slot stores 48 bits).
    pub const MAX_RAW: u64 = (1 << 48) - 1;

    /// Address zero — the origin of a namespace's first boot life.
    pub const ZERO: LogicalAddr = LogicalAddr(0);

    /// Wraps a raw 48-bit value (an index-slot payload, a MANIFEST field).
    /// `None` when the value exceeds 48 bits.
    #[inline]
    pub fn from_raw(raw: u64) -> Option<LogicalAddr> {
        (raw <= Self::MAX_RAW).then_some(LogicalAddr(raw))
    }

    /// The raw 48-bit value — exactly what an index slot stores.
    #[inline]
    pub fn to_raw(self) -> u64 {
        self.0
    }

    /// This address advanced by `len` bytes. `None` past the 48-bit end of
    /// the space (allocation must refuse, never wrap — the space is
    /// monotonic for a namespace's whole existence).
    #[inline]
    pub fn advanced(self, len: u64) -> Option<LogicalAddr> {
        let raw = self.0.checked_add(len)?;
        LogicalAddr::from_raw(raw)
    }

    /// Distance from `origin` (which must not exceed `self` — the caller
    /// compares addresses before subtracting; this is checked arithmetic,
    /// not a range test).
    #[inline]
    pub fn offset_from(self, origin: LogicalAddr) -> u64 {
        debug_assert!(origin.0 <= self.0, "offset_from below origin");
        self.0 - origin.0
    }
}

impl core::fmt::Debug for LogicalAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "LogicalAddr({:#x})", self.0)
    }
}

impl core::fmt::Display for LogicalAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_round_trip_and_bounds() {
        assert_eq!(LogicalAddr::ZERO.to_raw(), 0);
        let top = LogicalAddr::from_raw(LogicalAddr::MAX_RAW).expect("max fits");
        assert_eq!(top.to_raw(), LogicalAddr::MAX_RAW);
        assert!(LogicalAddr::from_raw(LogicalAddr::MAX_RAW + 1).is_none());
    }

    #[test]
    fn advanced_refuses_past_end_of_space() {
        let near_top = LogicalAddr::from_raw(LogicalAddr::MAX_RAW - 7).expect("fits");
        assert!(near_top.advanced(7).is_some());
        assert!(near_top.advanced(8).is_none());
        assert!(near_top.advanced(u64::MAX).is_none());
    }

    #[test]
    fn offset_from_is_plain_distance() {
        let a = LogicalAddr::from_raw(0x1000).expect("fits");
        let b = LogicalAddr::from_raw(0x1F00).expect("fits");
        assert_eq!(b.offset_from(a), 0xF00);
        assert_eq!(a.offset_from(a), 0);
    }
}
