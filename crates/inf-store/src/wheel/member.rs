//! The wheel's membership table (ADR-0008 A1 rules 1 and 7): key hash →
//! the one node that schedules it. Keyed by hash, so a second live node
//! for one hash has no representation — placement is an upsert (I1).
//!
//! An entry is 4 bytes, `node:24 | fingerprint:8`, in
//! [`WHEEL_MEMBER_SHARDS`] open-addressed, linearly probed shards chosen
//! by the hash's top byte; a shard doubles at 7/8 load, so one growth
//! rehashes at most one shard (≈ 75 k entries at the node bound). The
//! full hash is not stored: the node holds it, so a fingerprint match is
//! confirmed with one pool read, and a growth or a backward-shift delete
//! reads the pool for each entry it moves. Deletion shifts entries back
//! instead of leaving tombstones, so a probe never walks a dead entry.

use super::{NIL, Node};
use crate::limits::WHEEL_MEMBER_SHARDS;

/// An empty entry: node `NIL` never names a pool node.
const EMPTY: u32 = u32::MAX;
/// A shard's first allocation, in entries.
const SHARD_SLOTS_MIN: usize = 8;
/// A shard doubles before an insert would pass `LOAD_NUM / LOAD_DEN` of
/// its slots (ADR-0008 A1 rule 7), so a probe always meets an `EMPTY`.
const LOAD_NUM: usize = 7;
const LOAD_DEN: usize = 8;
const NODE_MASK: u32 = NIL;

const _: () = assert!(WHEEL_MEMBER_SHARDS == 256, "the shard is the hash's top byte");
const _: () = assert!(EMPTY & NODE_MASK == NIL);

/// A shard growth the allocator refused: the placement that needed it is
/// `Refused` and the record is swept (ADR-0008 A1 rule 3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct GrowthRefused;

pub(super) struct Membership {
    /// Empty until the first entry, then exactly `WHEEL_MEMBER_SHARDS`:
    /// a store that never schedules a deadline pays no shard table.
    shards: Vec<Shard>,
    /// Entries across every shard (= live wheel nodes).
    len: u64,
    /// Entry capacity across every shard, in bytes (the L5 attribution).
    slot_bytes: u64,
}

struct Shard {
    /// Capacity is 0 or a power of two ≥ `SHARD_SLOTS_MIN`; every slot is
    /// an entry or `EMPTY`.
    slots: Vec<u32>,
    len: u32,
}

#[inline]
fn shard_of(hash: u64) -> usize {
    (hash >> 56) as usize
}

#[inline]
fn fingerprint(hash: u64) -> u32 {
    ((hash >> 48) & 0xFF) as u32
}

#[inline]
fn home(hash: u64, mask: usize) -> usize {
    (hash as usize) & mask
}

#[inline]
fn entry(node: u32, hash: u64) -> u32 {
    debug_assert!(node < NIL, "a membership entry names a pool node");
    node | (fingerprint(hash) << 24)
}

#[inline]
fn node_of(entry: u32) -> u32 {
    entry & NODE_MASK
}

impl Membership {
    pub(super) fn new() -> Membership {
        Membership { shards: Vec::new(), len: 0, slot_bytes: 0 }
    }

    /// Live entries (one per scheduled key hash).
    #[cfg(test)]
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    /// Entry capacity in bytes.
    #[inline]
    pub(super) fn slot_bytes(&self) -> u64 {
        self.slot_bytes
    }

    /// The shard table itself, allocated at the first entry.
    pub(super) fn fixed_bytes(&self) -> u64 {
        (self.shards.capacity() * size_of::<Shard>()) as u64
    }

    /// The node scheduling `hash`, if any.
    #[inline]
    pub(super) fn lookup(&self, hash: u64, pool: &[Node]) -> Option<u32> {
        let shard = self.shards.get(shard_of(hash))?;
        shard.position(hash, pool).map(|at| node_of(shard.slots[at]))
    }

    /// Adds `hash → node`. Precondition: `hash` has no entry (placement
    /// looks it up first).
    ///
    /// # Errors
    /// The shard needed to grow and the allocator refused.
    pub(super) fn insert(
        &mut self,
        hash: u64,
        node: u32,
        pool: &[Node],
    ) -> Result<(), GrowthRefused> {
        debug_assert!(self.lookup(hash, pool).is_none(), "one entry per key hash (I1)");
        if self.shards.is_empty() {
            self.shards.try_reserve_exact(WHEEL_MEMBER_SHARDS).map_err(|_| GrowthRefused)?;
            self.shards.resize_with(WHEEL_MEMBER_SHARDS, || Shard { slots: Vec::new(), len: 0 });
        }
        let shard = &mut self.shards[shard_of(hash)];
        if (shard.len as usize + 1) * LOAD_DEN > shard.slots.len() * LOAD_NUM {
            let before = shard.slots.capacity();
            shard.grow(pool)?;
            self.slot_bytes += ((shard.slots.capacity() - before) * size_of::<u32>()) as u64;
        }
        shard.place(entry(node, hash), hash);
        shard.len += 1;
        self.len += 1;
        Ok(())
    }

    /// Removes `hash`'s entry, which names `node`.
    pub(super) fn remove(&mut self, hash: u64, node: u32, pool: &[Node]) {
        let Some(shard) = self.shards.get_mut(shard_of(hash)) else {
            debug_assert!(false, "remove from an empty membership table");
            return;
        };
        let Some(at) = shard.position(hash, pool) else {
            debug_assert!(false, "remove of an absent membership entry");
            return;
        };
        debug_assert_eq!(node_of(shard.slots[at]), node, "the entry names the removed node");
        shard.delete_at(at, pool);
        shard.len -= 1;
        self.len -= 1;
    }

    /// Points `hash`'s entry, which names `from`, at `to` — a successor
    /// copy moved the node's contents (rule 4).
    pub(super) fn retarget(&mut self, hash: u64, from: u32, to: u32, pool: &[Node]) {
        let Some(shard) = self.shards.get_mut(shard_of(hash)) else {
            debug_assert!(false, "retarget in an empty membership table");
            return;
        };
        let Some(at) = shard.position(hash, pool) else {
            debug_assert!(false, "retarget of an absent membership entry");
            return;
        };
        debug_assert_eq!(node_of(shard.slots[at]), from, "the entry names the moved node");
        shard.slots[at] = entry(to, hash);
    }

    /// Every `(hash, node)` entry (test-support audits).
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn entries<'a>(&'a self, pool: &'a [Node]) -> impl Iterator<Item = (u64, u32)> + 'a {
        self.shards.iter().flat_map(|shard| shard.slots.iter()).filter(|e| **e != EMPTY).map(
            move |e| {
                let node = node_of(*e);
                (pool[node as usize].hash, node)
            },
        )
    }
}

impl Shard {
    /// The slot holding `hash`'s entry. A probe ends at an `EMPTY`; the
    /// 7/8 load bound guarantees one within `slots.len()` steps.
    fn position(&self, hash: u64, pool: &[Node]) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = home(hash, mask);
        for _ in 0..self.slots.len() {
            let e = self.slots[at];
            if e == EMPTY {
                return None;
            }
            if e >> 24 == fingerprint(hash) && pool[node_of(e) as usize].hash == hash {
                return Some(at);
            }
            at = (at + 1) & mask;
        }
        None
    }

    /// Writes `e` at the first free slot of `hash`'s probe sequence.
    fn place(&mut self, e: u32, hash: u64) {
        let mask = self.slots.len() - 1;
        let mut at = home(hash, mask);
        while self.slots[at] != EMPTY {
            at = (at + 1) & mask;
        }
        self.slots[at] = e;
    }

    /// Doubles the shard, re-placing every entry by its node's hash (one
    /// pool read each). The new table is reserved before the old one is
    /// touched, so a refusal leaves the shard unchanged.
    fn grow(&mut self, pool: &[Node]) -> Result<(), GrowthRefused> {
        let slots_new = (self.slots.len() * 2).max(SHARD_SLOTS_MIN);
        let mut next: Vec<u32> = Vec::new();
        next.try_reserve_exact(slots_new).map_err(|_| GrowthRefused)?;
        next.resize(slots_new, EMPTY);
        let old = core::mem::replace(&mut self.slots, next);
        for e in old.into_iter().filter(|e| *e != EMPTY) {
            self.place(e, pool[node_of(e) as usize].hash);
        }
        Ok(())
    }

    /// Backward-shift delete: every later entry of the run whose probe
    /// path crosses the hole moves into it, so no probe chain breaks and
    /// no tombstone is left.
    fn delete_at(&mut self, at: usize, pool: &[Node]) {
        let mask = self.slots.len() - 1;
        let mut hole = at;
        let mut next = at;
        loop {
            next = (next + 1) & mask;
            let e = self.slots[next];
            if e == EMPTY {
                break;
            }
            let from_home = next.wrapping_sub(home(pool[node_of(e) as usize].hash, mask)) & mask;
            if from_home >= next.wrapping_sub(hole) & mask {
                self.slots[hole] = e;
                hole = next;
            }
        }
        self.slots[hole] = EMPTY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_of(hashes: &[u64]) -> Vec<Node> {
        hashes.iter().map(|h| Node::new(*h, 0, NIL)).collect()
    }

    /// Hashes that share one shard, one fingerprint and one home slot, so
    /// every insert and delete runs the longest probe the shard allows.
    fn clustered(n: u64) -> Vec<u64> {
        (0..n).map(|i| 0x2A00_0000_0000_0000 | (0x5A << 48) | (i << 16)).collect()
    }

    #[test]
    fn insert_lookup_remove_through_one_cluster() {
        let hashes = clustered(40);
        let pool = pool_of(&hashes);
        let mut members = Membership::new();
        for (node, hash) in hashes.iter().enumerate() {
            members.insert(*hash, node as u32, &pool).expect("growth");
        }
        assert_eq!(members.len(), 40);
        for (node, hash) in hashes.iter().enumerate() {
            assert_eq!(members.lookup(*hash, &pool), Some(node as u32));
        }
        // Remove every other entry; the survivors stay reachable (the
        // backward shift keeps each probe chain whole).
        for (node, hash) in hashes.iter().enumerate().step_by(2) {
            members.remove(*hash, node as u32, &pool);
        }
        for (node, hash) in hashes.iter().enumerate() {
            let want = (node % 2 == 1).then_some(node as u32);
            assert_eq!(members.lookup(*hash, &pool), want, "entry {node}");
        }
        assert_eq!(members.len(), 20);
    }

    #[test]
    fn a_shard_stays_under_seven_eighths_load() {
        let hashes = clustered(1_000);
        let pool = pool_of(&hashes);
        let mut members = Membership::new();
        for (node, hash) in hashes.iter().enumerate() {
            members.insert(*hash, node as u32, &pool).expect("growth");
            let shard = &members.shards[shard_of(*hash)];
            assert!(shard.len as usize * LOAD_DEN <= shard.slots.len() * LOAD_NUM);
        }
        assert_eq!(members.slot_bytes(), (2_048 * size_of::<u32>()) as u64);
    }

    #[test]
    fn retarget_moves_the_entry_to_the_successor_slot() {
        let hashes = [0x0100_0000_0000_0007u64, 0x0200_0000_0000_0009];
        let mut pool = pool_of(&hashes);
        let mut members = Membership::new();
        members.insert(hashes[0], 0, &pool).expect("growth");
        pool.push(Node::new(hashes[0], 0, NIL));
        members.retarget(hashes[0], 0, 2, &pool);
        assert_eq!(members.lookup(hashes[0], &pool), Some(2));
        assert_eq!(members.lookup(hashes[1], &pool), None);
    }
}
