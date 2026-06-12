//! Slot router (M0-S16, master plan §4.1): `crc16(hashtag(key)) % 16384`
//! over the same 16,384-slot space as Redis Cluster, with a static
//! contiguous slot→cell assignment for the M0 topology. Every cross-cell
//! decision in the node flows through here — the seam M1 namespaces and the
//! post-1.0 cluster mode extend.

use inf_foundation::{CellId, KeySlot, SLOT_COUNT, crc16, hashtag};

/// Static slot→cell map: cell `c` owns the contiguous range
/// `[c·16384/N, (c+1)·16384/N)`.
#[derive(Copy, Clone, Debug)]
pub struct SlotRouter {
    cells: u16,
}

impl SlotRouter {
    /// A router over `cells` shard cells.
    ///
    /// # Panics
    /// Panics if `cells` is 0 or exceeds the slot count.
    pub fn new_contiguous(cells: u16) -> SlotRouter {
        assert!(cells > 0 && cells <= SLOT_COUNT, "cell count must be in 1..=16384");
        SlotRouter { cells }
    }

    /// The Redis Cluster slot of `key` (hash-tag aware).
    #[inline]
    pub fn slot_of(key: &[u8]) -> KeySlot {
        KeySlot::new(crc16(hashtag(key)) % SLOT_COUNT).expect("mod keeps slot in range")
    }

    /// The owning cell of `slot` (contiguous ranges).
    #[inline]
    pub fn cell_of(&self, slot: KeySlot) -> CellId {
        CellId((u32::from(slot.get()) * u32::from(self.cells) / u32::from(SLOT_COUNT)) as u16)
    }

    /// One-call ownership test: `key` belongs to `cell`.
    #[inline]
    pub fn is_local(&self, key: &[u8], cell: CellId) -> bool {
        self.cell_of(Self::slot_of(key)) == cell
    }

    /// Number of cells in the topology.
    #[inline]
    pub fn cells(&self) -> u16 {
        self.cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redis Cluster spec vectors: well-known key→slot pairs (the crc16
    /// XMODEM vectors live in `inf-foundation`; these pin the full
    /// hashtag+mod pipeline).
    #[test]
    fn redis_cluster_slot_vectors() {
        // Canonical examples from the Redis Cluster spec & community corpora.
        for (key, slot) in [
            (&b""[..], 0u16),
            (b"foo", 12182),
            (b"bar", 5061),
            (b"hello", 866),
            (b"123456789", 12739),
            (b"{user1000}.following", crc16(b"user1000") % SLOT_COUNT),
            (b"{user1000}.followers", crc16(b"user1000") % SLOT_COUNT),
        ] {
            assert_eq!(SlotRouter::slot_of(key).get(), slot, "key {key:?}");
        }
    }

    #[test]
    fn hashtag_colocation_matches_cluster_rules() {
        // Same tag ⇒ same slot; the spec's edge cases.
        assert_eq!(SlotRouter::slot_of(b"{user1}:a"), SlotRouter::slot_of(b"{user1}:b"));
        // Empty tag `{}` hashes the whole key.
        assert_eq!(SlotRouter::slot_of(b"foo{}{bar}").get(), crc16(b"foo{}{bar}") % SLOT_COUNT);
        // Only the FIRST tag counts.
        assert_eq!(SlotRouter::slot_of(b"foo{{bar}}zap").get(), crc16(b"{bar") % SLOT_COUNT);
        assert_eq!(SlotRouter::slot_of(b"foo{bar}{zap}").get(), crc16(b"bar") % SLOT_COUNT);
        // No closing brace ⇒ whole key.
        assert_eq!(SlotRouter::slot_of(b"foo{bar").get(), crc16(b"foo{bar") % SLOT_COUNT);
    }

    #[test]
    fn contiguous_assignment_covers_all_slots_evenly() {
        for cells in [1u16, 2, 3, 4, 8, 16, 128] {
            let router = SlotRouter::new_contiguous(cells);
            let mut counts = vec![0u32; cells as usize];
            let mut prev = router.cell_of(KeySlot::new(0).expect("slot"));
            for s in 0..SLOT_COUNT {
                let cell = router.cell_of(KeySlot::new(s).expect("slot"));
                assert!(cell.as_usize() < cells as usize, "cell in range");
                assert!(cell >= prev, "assignment is contiguous (monotonic)");
                prev = cell;
                counts[cell.as_usize()] += 1;
            }
            let (min, max) =
                (counts.iter().min().expect("nonempty"), counts.iter().max().expect("nonempty"));
            assert!(max - min <= 1, "{cells} cells: ranges within one slot of even");
        }
    }

    /// M0-S16 oracle stand-in (the full DST run is M0-S20): 10⁵ random keys
    /// routed across 4 cells, then the same ops replayed against a
    /// single-cell store must agree with the per-cell shards.
    #[test]
    fn multi_cell_routing_matches_single_cell_oracle() {
        use crate::{CellStore, SetOptions, StoreConfig};
        use inf_foundation::time::Nanos;

        let n: usize = if cfg!(miri) { 500 } else { 100_000 };
        let router = SlotRouter::new_contiguous(4);
        let mut shards: Vec<CellStore> =
            (0..4).map(|_| CellStore::new(StoreConfig::default())).collect();
        let mut oracle = CellStore::new(StoreConfig::default());
        let now = Nanos(0);

        let mut x: u64 = 0xFEED_FACE_CAFE_BEEF;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..n {
            let key = format!("k:{}", rand() % 4096).into_bytes();
            let cell = router.cell_of(SlotRouter::slot_of(&key));
            let shard = &mut shards[cell.as_usize()];
            match rand() % 3 {
                0 => {
                    let value = format!("v:{}", rand() % 1024).into_bytes();
                    shard.set(&key, &value, SetOptions::default(), now).expect("set");
                    oracle.set(&key, &value, SetOptions::default(), now).expect("set");
                }
                1 => {
                    assert_eq!(shard.del(&key, now), oracle.del(&key, now));
                }
                _ => {
                    assert_eq!(
                        shard.get(&key, now).map(<[u8]>::to_vec),
                        oracle.get(&key, now).map(<[u8]>::to_vec)
                    );
                }
            }
        }
        assert_eq!(shards.iter().map(CellStore::len).sum::<usize>(), oracle.len());
    }
}
