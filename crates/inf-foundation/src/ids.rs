use core::fmt;
use core::num::NonZeroU16;

use crate::crc::{crc16, hashtag};

/// Number of keyspace slots — identical to Redis Cluster so hash tags and
/// client expectations carry over unchanged (master plan §4.1).
pub const SLOT_COUNT: u16 = 16384;

/// The cells of one boot's topology, `1 <= N <= SLOT_COUNT` (ADR-0159
/// A1.1): every cell owns at least one slot. [`CellCount::new`] is the one
/// check; the binary, the embedded assembly and the simulator hand the type
/// to every control-plane constructor, so a count past the bound is refused
/// before any mesh, board or thread exists.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellCount(NonZeroU16);

/// Why a cell count was refused.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CellCountError {
    /// A topology has at least one cell.
    Zero,
    /// A cell with no slot to own: more cells than [`SLOT_COUNT`].
    AboveSlotCount,
}

impl CellCount {
    /// # Errors
    /// [`CellCountError::Zero`] for 0, [`CellCountError::AboveSlotCount`]
    /// past [`SLOT_COUNT`].
    pub const fn new(cells: u16) -> Result<CellCount, CellCountError> {
        if cells > SLOT_COUNT {
            return Err(CellCountError::AboveSlotCount);
        }
        match NonZeroU16::new(cells) {
            Some(cells) => Ok(CellCount(cells)),
            None => Err(CellCountError::Zero),
        }
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    #[must_use]
    pub const fn non_zero(self) -> NonZeroU16 {
        self.0
    }

    #[must_use]
    pub fn as_usize(self) -> usize {
        usize::from(self.0.get())
    }
}

impl fmt::Display for CellCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellCountError::Zero => write!(f, "must be >= 1"),
            CellCountError::AboveSlotCount => write!(f, "must be <= {SLOT_COUNT}"),
        }
    }
}

impl std::error::Error for CellCountError {}

/// Identity of one cell (one pinned core — L1).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellId(pub u16);

impl CellId {
    #[inline]
    pub fn as_usize(self) -> usize {
        usize::from(self.0)
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cell{}", self.0)
    }
}

/// A keyspace slot in `0..16384`. The constructor set makes an out-of-range
/// slot unrepresentable; there is no public way to fabricate an invalid one.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct KeySlot(u16);

impl KeySlot {
    /// Slot of a key per the Redis Cluster rule: `crc16(hashtag(key)) % 16384`.
    #[inline]
    pub fn of_key(key: &[u8]) -> KeySlot {
        // SLOT_COUNT is a power of two, so the mask equals the modulo.
        KeySlot(crc16(hashtag(key)) & (SLOT_COUNT - 1))
    }

    #[inline]
    pub fn new(raw: u16) -> Option<KeySlot> {
        (raw < SLOT_COUNT).then_some(KeySlot(raw))
    }

    #[inline]
    pub fn get(self) -> u16 {
        self.0
    }

    #[inline]
    pub fn as_usize(self) -> usize {
        usize::from(self.0)
    }
}

impl fmt::Display for KeySlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_bounds_unrepresentable() {
        assert!(KeySlot::new(16383).is_some());
        assert!(KeySlot::new(16384).is_none());
        assert!(KeySlot::new(u16::MAX).is_none());
    }

    /// ADR-0159 A1.1: the bound's edges, and the integer maximum.
    #[test]
    fn cell_count_refuses_zero_and_past_the_slot_count() {
        assert_eq!(CellCount::new(0), Err(CellCountError::Zero));
        assert_eq!(CellCount::new(1).map(CellCount::get), Ok(1));
        assert_eq!(CellCount::new(SLOT_COUNT).map(CellCount::get), Ok(16_384));
        assert_eq!(CellCount::new(SLOT_COUNT + 1), Err(CellCountError::AboveSlotCount));
        assert_eq!(CellCount::new(u16::MAX), Err(CellCountError::AboveSlotCount));
        assert_eq!(CellCountError::AboveSlotCount.to_string(), "must be <= 16384");
    }

    #[test]
    fn hash_tags_colocate() {
        assert_eq!(KeySlot::of_key(b"{user:42}.cart"), KeySlot::of_key(b"{user:42}.profile"));
    }
}
