//! Index **v0** (M0-S14, master plan §7.3): per-cell open-addressing table,
//! Swiss-style — 1 control byte per slot (7-bit hash fragment, SIMD
//! group-probed 16 at a time via `inf-simd`) + an 8-byte slot
//! `{addr:48, fp:15, used:1}`. No keys, no TTLs, no values in the table:
//! key verification fetches the record (the store provides the comparison
//! closure), which the batch prefetch pipeline overlaps (L3/L4).
//!
//! Probing is hashbrown-shape: triangular group stride over a power-of-two
//! group count (visits every group). Deletion writes tombstones; tombstones
//! recycle on insert and are swept by rehash. Growth doubles capacity and
//! re-places every live slot via the caller's `hash_of(addr)` (the index
//! stores only 22 fingerprint bits, deliberately — §7.3 keeps it dense).
//!
//! M1 reserve: incremental split-order migration replaces the stop-and-copy
//! `grow` below; the `(live, tombstones, growth_left)` bookkeeping is
//! already per-table so the migration can move one group per MAINTAIN slice.

use inf_alloc::ArenaAddr;
use inf_simd::{eq_mask16, high_bit_mask16, prefetch_read};

const GROUP: usize = 16;
const CTRL_EMPTY: u8 = 0x80;
const CTRL_TOMB: u8 = 0xFE;
/// Numerator of the maximum load factor (live + tombstones ≤ 85% of slots).
const LOAD_NUM: usize = 85;
const LOAD_DEN: usize = 100;

/// 8-byte slot: `addr:48 | fp:15 | used:1` (frozen layout, §3.2).
#[derive(Copy, Clone, Default)]
struct Slot(u64);

impl Slot {
    const ADDR_MASK: u64 = (1 << 48) - 1;

    #[inline]
    fn new(addr: ArenaAddr, fp15: u16) -> Slot {
        debug_assert!(fp15 < (1 << 15));
        Slot(addr.to_raw() | (u64::from(fp15) << 48) | (1 << 63))
    }

    #[inline]
    fn addr(self) -> ArenaAddr {
        ArenaAddr::from_raw(self.0 & Self::ADDR_MASK).expect("slot addr is 48-bit by masking")
    }

    #[inline]
    fn fp15(self) -> u16 {
        ((self.0 >> 48) & 0x7FFF) as u16
    }
}

/// Hash fragments: group index from the low bits, 7-bit control tag from the
/// top, 15-bit slot fingerprint from the bits between — disjoint, so the
/// effective filter is 22 bits before a record fetch.
#[inline]
fn h2(hash: u64) -> u8 {
    (hash >> 57) as u8 & 0x7F
}

#[inline]
fn fp15(hash: u64) -> u16 {
    ((hash >> 42) & 0x7FFF) as u16
}

/// Per-cell record index. Stores 48-bit arena addresses only.
pub struct Index {
    ctrl: Box<[u8]>,
    slots: Box<[Slot]>,
    /// Power-of-two slot count; `capacity / 16` groups.
    capacity: usize,
    live: usize,
    tombstones: usize,
}

impl Index {
    /// A table that can hold `at_least` entries without growing.
    pub fn with_capacity(at_least: usize) -> Index {
        let slots = (at_least.max(1) * LOAD_DEN).div_ceil(LOAD_NUM);
        let capacity = slots.next_power_of_two().max(GROUP);
        Index {
            ctrl: vec![CTRL_EMPTY; capacity].into_boxed_slice(),
            slots: vec![Slot::default(); capacity].into_boxed_slice(),
            capacity,
            live: 0,
            tombstones: 0,
        }
    }

    /// Live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }

    /// True when empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Slot capacity (the table grows itself; this is for reporting).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Exact table footprint in bytes (feeds `index_bytes`, L5).
    #[inline]
    pub fn memory_bytes(&self) -> usize {
        self.capacity * (1 + size_of::<Slot>())
    }

    #[inline]
    fn group_mask(&self) -> usize {
        self.capacity / GROUP - 1
    }

    #[inline]
    fn ctrl_group(&self, group: usize) -> &[u8; 16] {
        self.ctrl[group * GROUP..group * GROUP + GROUP].try_into().expect("group-aligned")
    }

    /// Prefetch the probe path for `hash` — the batch pipeline calls this
    /// for every key in a parse batch before any `find` (L3/L4).
    #[inline]
    pub fn prefetch(&self, hash: u64) {
        let group = (hash as usize) & self.group_mask();
        prefetch_read(&raw const self.ctrl[group * GROUP]);
        prefetch_read((&raw const self.slots[group * GROUP]).cast());
    }

    /// Finds the address whose record matches, where `verify(addr)` performs
    /// the key comparison (fingerprint false positives reach it; full-key
    /// equality is the store's job).
    #[inline]
    pub fn find(&self, hash: u64, mut verify: impl FnMut(ArenaAddr) -> bool) -> Option<ArenaAddr> {
        let (tag, fp) = (h2(hash), fp15(hash));
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let slot = self.slots[group * GROUP + i];
                if slot.fp15() == fp && verify(slot.addr()) {
                    return Some(slot.addr());
                }
            }
            // An EMPTY anywhere in the group terminates the probe chain
            // (tombstones do not — deleted slots were once links).
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return None;
            }
            stride += 1;
            if stride > mask {
                return None; // every group visited (full-of-tombstones guard)
            }
            group = (group + stride) & mask;
        }
    }

    /// Diagnostics: groups visited until the probe for `hash` terminates
    /// (found-and-verified, or empty slot). Feeds the probe-length
    /// histogram artifact (M0-S14 AC) — not a hot-path API.
    pub fn probe_groups(&self, hash: u64, mut verify: impl FnMut(ArenaAddr) -> bool) -> usize {
        let (tag, fp) = (h2(hash), fp15(hash));
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        let mut visited = 1;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let slot = self.slots[group * GROUP + i];
                if slot.fp15() == fp && verify(slot.addr()) {
                    return visited;
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 || stride >= mask {
                return visited;
            }
            stride += 1;
            visited += 1;
            group = (group + stride) & mask;
        }
    }

    /// True when the next insert must be preceded by [`grow`](Self::grow).
    /// Split out (rather than growing inside `insert`) because re-placement
    /// needs the caller's `hash_of` — the table doesn't store full hashes.
    #[inline]
    pub fn needs_grow(&self) -> bool {
        (self.live + self.tombstones + 1) * LOAD_DEN > self.capacity * LOAD_NUM
    }

    /// Inserts `addr` under `hash`. Precondition: the key is absent (the
    /// caller always `find`s first) and `needs_grow()` is false.
    pub fn insert(&mut self, hash: u64, addr: ArenaAddr) {
        debug_assert!(!self.needs_grow(), "caller must grow first");
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            // First special slot (empty or tombstone) in probe order.
            let specials = high_bit_mask16(self.ctrl_group(group));
            if specials != 0 {
                let i = specials.trailing_zeros() as usize;
                let pos = group * GROUP + i;
                if self.ctrl[pos] == CTRL_TOMB {
                    self.tombstones -= 1;
                }
                self.ctrl[pos] = h2(hash);
                self.slots[pos] = Slot::new(addr, fp15(hash));
                self.live += 1;
                return;
            }
            stride += 1;
            assert!(stride <= mask, "insert found no slot — load invariant broken");
            group = (group + stride) & mask;
        }
    }

    /// Swaps the address stored for an existing entry (same key, record
    /// moved by an update). Panics if `(hash, old)` is not present — that is
    /// an index/store desync.
    pub fn replace(&mut self, hash: u64, old: ArenaAddr, new: ArenaAddr) {
        let pos = self.position_of(hash, old).expect("replace target present");
        self.slots[pos] = Slot::new(new, fp15(hash));
    }

    /// Removes the entry holding `addr`. Panics if absent (desync).
    pub fn remove(&mut self, hash: u64, addr: ArenaAddr) {
        let pos = self.position_of(hash, addr).expect("remove target present");
        // If the slot's group has an empty, no probe chain passes THROUGH
        // this group — the slot can return to EMPTY instead of tombstoning.
        let group = pos / GROUP;
        let has_empty = eq_mask16(self.ctrl_group(group), CTRL_EMPTY) != 0;
        if has_empty {
            self.ctrl[pos] = CTRL_EMPTY;
        } else {
            self.ctrl[pos] = CTRL_TOMB;
            self.tombstones += 1;
        }
        self.slots[pos] = Slot::default();
        self.live -= 1;
    }

    fn position_of(&self, hash: u64, addr: ArenaAddr) -> Option<usize> {
        let tag = h2(hash);
        let mask = self.group_mask();
        let mut group = (hash as usize) & mask;
        let mut stride = 0;
        loop {
            let ctrl = self.ctrl_group(group);
            let mut candidates = eq_mask16(ctrl, tag);
            while candidates != 0 {
                let i = candidates.trailing_zeros() as usize;
                candidates &= candidates - 1;
                let pos = group * GROUP + i;
                if self.slots[pos].addr() == addr {
                    return Some(pos);
                }
            }
            if eq_mask16(ctrl, CTRL_EMPTY) != 0 {
                return None;
            }
            stride += 1;
            if stride > mask {
                return None;
            }
            group = (group + stride) & mask;
        }
    }

    /// Doubles capacity (also sweeping tombstones), re-placing every live
    /// address via `hash_of(addr)` — the store hashes the record's key.
    /// M0 is stop-and-copy; M1 replaces this with split-order increments.
    pub fn grow(&mut self, mut hash_of: impl FnMut(ArenaAddr) -> u64) {
        // Tombstone-heavy tables rehash at the same size (recycle), others double.
        let new_capacity =
            if self.tombstones >= self.live { self.capacity } else { self.capacity * 2 };
        let mut next = Index {
            ctrl: vec![CTRL_EMPTY; new_capacity].into_boxed_slice(),
            slots: vec![Slot::default(); new_capacity].into_boxed_slice(),
            capacity: new_capacity,
            live: 0,
            tombstones: 0,
        };
        for pos in 0..self.capacity {
            if self.ctrl[pos] & 0x80 == 0 {
                let addr = self.slots[pos].addr();
                next.insert(hash_of(addr), addr);
            }
        }
        *self = next;
    }
}

impl core::fmt::Debug for Index {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Index")
            .field("live", &self.live)
            .field("tombstones", &self.tombstones)
            .field("capacity", &self.capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::hash64;
    use std::collections::HashMap;

    /// Test rig: "records" are entries in a Vec; ArenaAddr = index into it.
    struct Rig {
        keys: Vec<Vec<u8>>,
        index: Index,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { keys: Vec::new(), index: Index::with_capacity(4) }
        }

        fn hash(key: &[u8]) -> u64 {
            hash64(key, 0xC0FFEE)
        }

        fn addr(i: usize) -> ArenaAddr {
            ArenaAddr::from_raw(i as u64).expect("small")
        }

        fn get(&self, key: &[u8]) -> Option<u64> {
            self.index
                .find(Self::hash(key), |a| self.keys[a.to_raw() as usize] == key)
                .map(|a| a.to_raw())
        }

        fn upsert(&mut self, key: &[u8]) -> u64 {
            let hash = Self::hash(key);
            let keys = &self.keys;
            if let Some(old) = self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                return old.to_raw();
            }
            if self.index.needs_grow() {
                let keys = &self.keys;
                self.index.grow(|a| Self::hash(&keys[a.to_raw() as usize]));
            }
            self.keys.push(key.to_vec());
            let addr = Self::addr(self.keys.len() - 1);
            self.index.insert(hash, addr);
            addr.to_raw()
        }

        fn remove(&mut self, key: &[u8]) -> bool {
            let hash = Self::hash(key);
            let keys = &self.keys;
            match self.index.find(hash, |a| keys[a.to_raw() as usize] == key) {
                Some(addr) => {
                    self.index.remove(hash, addr);
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn insert_find_remove_basics() {
        let mut rig = Rig::new();
        assert_eq!(rig.get(b"k1"), None);
        let a = rig.upsert(b"k1");
        assert_eq!(rig.get(b"k1"), Some(a));
        assert_eq!(rig.upsert(b"k1"), a, "upsert of present key is a find");
        assert!(rig.remove(b"k1"));
        assert_eq!(rig.get(b"k1"), None);
        assert!(!rig.remove(b"k1"));
        assert!(rig.index.is_empty());
    }

    #[test]
    fn replace_swaps_address_in_place() {
        let mut rig = Rig::new();
        rig.upsert(b"key");
        let hash = Rig::hash(b"key");
        rig.keys.push(b"key".to_vec()); // the "moved record"
        let new_addr = Rig::addr(rig.keys.len() - 1);
        rig.index.replace(hash, Rig::addr(0), new_addr);
        assert_eq!(rig.get(b"key"), Some(new_addr.to_raw()));
        assert_eq!(rig.index.len(), 1);
    }

    /// M0-S14 AC shape: random op sequence vs a HashMap oracle.
    #[test]
    fn storm_matches_hashmap_oracle() {
        let ops: usize = if cfg!(miri) { 2_000 } else { 100_000 };
        let mut rig = Rig::new();
        let mut oracle: HashMap<Vec<u8>, u64> = HashMap::new();
        let mut x: u64 = 0x1234_5678_9ABC_DEF1;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for op in 0..ops {
            let key = format!("key:{}", rand() % 512).into_bytes();
            match rand() % 3 {
                0 => {
                    let got = rig.upsert(&key);
                    let want = *oracle.entry(key.clone()).or_insert(got);
                    assert_eq!(got, want, "op {op}: upsert disagreed");
                }
                1 => {
                    let got = rig.remove(&key);
                    let want = oracle.remove(&key).is_some();
                    assert_eq!(got, want, "op {op}: remove disagreed");
                }
                _ => {
                    let got = rig.get(&key);
                    let want = oracle.get(&key).copied();
                    assert_eq!(got, want, "op {op}: get disagreed");
                }
            }
            assert_eq!(rig.index.len(), oracle.len(), "op {op}: len drift");
        }
        for (key, want) in &oracle {
            assert_eq!(rig.get(key), Some(*want), "final sweep");
        }
    }

    #[test]
    fn tombstone_recycling_bounds_capacity() {
        let mut rig = Rig::new();
        // Insert/delete cycles over a fixed working set must not grow the
        // table unboundedly: rehash-in-place recycles tombstones.
        for round in 0..200 {
            for i in 0..64 {
                rig.upsert(format!("cycle:{i}").as_bytes());
            }
            for i in 0..64 {
                assert!(rig.remove(format!("cycle:{i}").as_bytes()), "round {round}");
            }
        }
        assert!(rig.index.capacity() <= 1024, "capacity ballooned: {:?}", rig.index);
    }

    #[test]
    fn growth_keeps_every_key_findable() {
        let mut rig = Rig::new();
        let n = if cfg!(miri) { 300 } else { 50_000 };
        let mut addrs = Vec::new();
        for i in 0..n {
            addrs.push(rig.upsert(format!("grow:{i}").as_bytes()));
        }
        for (i, want) in addrs.iter().enumerate() {
            assert_eq!(rig.get(format!("grow:{i}").as_bytes()), Some(*want));
        }
        // Load factor honored after growth churn.
        assert!(rig.index.len() * 100 <= rig.index.capacity() * 85);
    }

    #[test]
    fn memory_bytes_is_nine_per_slot() {
        let index = Index::with_capacity(10_000);
        assert_eq!(index.memory_bytes(), index.capacity() * 9);
    }
}
