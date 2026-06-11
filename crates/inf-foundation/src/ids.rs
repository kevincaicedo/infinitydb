use core::fmt;

use crate::crc::{crc16, hashtag};

/// Number of keyspace slots — identical to Redis Cluster so hash tags and
/// client expectations carry over unchanged (master plan §4.1).
pub const SLOT_COUNT: u16 = 16384;

/// Identity of one shard cell (one pinned core — L1).
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

    #[test]
    fn hash_tags_colocate() {
        assert_eq!(KeySlot::of_key(b"{user:42}.cart"), KeySlot::of_key(b"{user:42}.profile"));
    }
}
