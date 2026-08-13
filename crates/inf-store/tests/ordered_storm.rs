//! M4.5-S01 AC storm: 10⁶ random ops against the `BTreeSet` model —
//! identical contents and iteration order, invariants audited by the
//! full-scan equality (a wrong split/merge/borrow surfaces as a missing
//! or misordered pair).
//!
//! Deterministic (fixed xorshift seeds — L7 posture for tests) and
//! release-sized: run explicitly with
//! `cargo test -p inf-store --release --test ordered_storm -- --ignored`.

use std::collections::BTreeSet;

use inf_store::{Fixed8, OrderedCursor, OrderedMap, VarKey};

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Drive `ops` random insert/removes over a small key space (dense
/// duplicate/merge traffic), asserting per-op agreement with the model
/// and full-scan equality at the checkpoints.
fn storm_var(seed: u64, ops: usize, key_space: u64, ref_space: u64) {
    let mut rng = XorShift(seed);
    let mut map: OrderedMap<VarKey, 64> = OrderedMap::new();
    let mut model: BTreeSet<(Vec<u8>, u64)> = BTreeSet::new();
    for op in 0..ops {
        let key_val = rng.next() % key_space;
        // Variable-length keys around the 8-byte prefix edge.
        let len = 1 + (rng.next() % 12) as usize;
        let mut key = key_val.to_be_bytes().to_vec();
        key.resize(len, 0xAB);
        let entry_ref = rng.next() % ref_space;
        if rng.next() % 5 < 3 {
            let inserted = map.insert(&key, entry_ref).expect("capacity");
            assert_eq!(inserted, model.insert((key, entry_ref)), "insert verdict @op {op}");
        } else {
            let removed = map.remove(&key, entry_ref);
            assert_eq!(removed, model.remove(&(key, entry_ref)), "remove verdict @op {op}");
        }
        if op % 100_000 == 0 {
            assert_eq!(map.len(), model.len() as u64, "len @op {op}");
        }
    }
    let mut cursor = OrderedCursor::from_start();
    let mut scanned = Vec::with_capacity(model.len());
    while let Some((key, entry_ref)) = cursor.next(&map) {
        scanned.push((key.to_vec(), entry_ref));
    }
    let want: Vec<(Vec<u8>, u64)> = model.into_iter().collect();
    assert_eq!(scanned, want, "full scan must equal the model in order");
}

/// The fixed8 twin: 8-byte keys, heavy ref-duplication.
fn storm_fixed(seed: u64, ops: usize, key_space: u64) {
    let mut rng = XorShift(seed);
    let mut map: OrderedMap<Fixed8, 64> = OrderedMap::new();
    let mut model: BTreeSet<(u64, u64)> = BTreeSet::new();
    for op in 0..ops {
        let key_val = rng.next() % key_space;
        let entry_ref = rng.next() % 4;
        if rng.next() % 5 < 3 {
            let inserted = map.insert(&key_val.to_be_bytes(), entry_ref).expect("capacity");
            assert_eq!(inserted, model.insert((key_val, entry_ref)), "insert verdict @op {op}");
        } else {
            let removed = map.remove(&key_val.to_be_bytes(), entry_ref);
            assert_eq!(removed, model.remove(&(key_val, entry_ref)), "remove verdict @op {op}");
        }
    }
    let mut cursor = OrderedCursor::from_start();
    let mut scanned = Vec::with_capacity(model.len());
    while let Some((key, entry_ref)) = cursor.next(&map) {
        scanned.push((u64::from_be_bytes(key.try_into().unwrap()), entry_ref));
    }
    let want: Vec<(u64, u64)> = model.into_iter().collect();
    assert_eq!(scanned, want, "full scan must equal the model in order");
}

#[test]
#[ignore = "10^6-op storm (S01 AC artifact) — run in release explicitly"]
fn million_op_model_storm() {
    // Three key-space densities: churn-heavy (constant merges), mixed,
    // and growth-heavy (deep trees) — 10^6 ops total across schemes.
    storm_var(0x51CE_D001, 250_000, 1_000, 4);
    storm_var(0x51CE_D002, 250_000, 100_000, 2);
    storm_fixed(0x51CE_D003, 250_000, 2_000);
    storm_fixed(0x51CE_D004, 250_000, 500_000);
}
