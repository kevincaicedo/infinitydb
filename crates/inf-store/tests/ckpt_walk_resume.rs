//! ADR-0117 D2 (review of 2026-08-30, F-L03-02): the checkpoint walk's
//! in-chain resume. A refused image stops the walk *before* that entry;
//! the next slice re-enters the home group at exactly it — no miss, no
//! duplicate on a quiescent index, and the at-least-once contract under
//! interleaved churn (doublings, tombstone recycles, deletes) exactly as
//! `scan_guarantee.rs` proves for the group-only cursor.

use std::collections::{BTreeSet, HashMap};

use inf_foundation::time::Nanos;
use inf_store::{
    AddressSpaceConfig, CellStore, CheckpointImage, DemotionConfig, KeyHasher, LogicalAddr,
    SetOptions, StoreConfig, TieredTable, WalkCursor,
};

const NOW: Nanos = Nanos(1_000_000);

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// One walk over a quiescent store with every `refuse_every`-th emission
/// refused once: the emitted sequence names every key exactly once, and
/// the first emission after each refusal is the refused key.
fn walk_refusing(store: &mut CellStore, refuse_every: u64) -> Vec<Vec<u8>> {
    let mut cursor = WalkCursor::START;
    let mut emitted: Vec<Vec<u8>> = Vec::new();
    let mut calls = 0u64;
    let mut refused: Option<Vec<u8>> = None;
    loop {
        let mut first_this_call: Option<Vec<u8>> = None;
        let mut just_refused: Option<Vec<u8>> = None;
        let done = store.scan_checkpoint_images_bounded(&mut cursor, 32, NOW, |key, _img, _| {
            calls += 1;
            if first_this_call.is_none() {
                first_this_call = Some(key.to_vec());
            }
            if just_refused.is_none() && calls.is_multiple_of(refuse_every) {
                just_refused = Some(key.to_vec());
                return false;
            }
            emitted.push(key.to_vec());
            true
        });
        if let Some(want) = refused.take() {
            assert_eq!(first_this_call.as_deref(), Some(want.as_slice()), "resume at the refusal");
        }
        if let Some(key) = just_refused {
            assert!(cursor.chain.is_some(), "a refusal leaves an in-chain resume");
            refused = Some(key);
        }
        if done {
            assert_eq!(cursor, WalkCursor::START);
            return emitted;
        }
    }
}

#[test]
fn a_refusal_resumes_at_the_refused_entry_with_no_miss_and_no_duplicate() {
    let mut store = CellStore::new(StoreConfig::default());
    let keys: BTreeSet<Vec<u8>> = (0..3000u32).map(|i| format!("k:{i:05}").into_bytes()).collect();
    for key in &keys {
        store.set(key, b"v", SetOptions::default(), NOW).expect("set");
    }
    for refuse_every in [2u64, 3, 7, 50] {
        let emitted = walk_refusing(&mut store, refuse_every);
        assert_eq!(
            emitted.len(),
            keys.len(),
            "every key exactly once (refuse every {refuse_every})"
        );
        let set: BTreeSet<Vec<u8>> = emitted.iter().cloned().collect();
        assert_eq!(set, keys);
    }
}

/// The at-least-once contract with refusals *and* churn between slices:
/// inserts that double the table, insert/delete cycles that recycle it
/// at the same size, deletes of pre-existing keys ahead of and behind
/// the resume. Every key present for the whole walk is emitted.
#[test]
fn persistent_keys_survive_refusals_under_interleaved_churn() {
    let rounds = if cfg!(miri) { 2 } else { 24 };
    let mut x: u64 = 0x1DEA_0117;
    for round in 0..rounds {
        let mut store = CellStore::new(StoreConfig::default());
        let persistent: Vec<Vec<u8>> = (0..200 + xorshift(&mut x) % 400)
            .map(|i| format!("p:{round}:{i}").into_bytes())
            .collect();
        for key in &persistent {
            store.set(key, b"v", SetOptions::default(), NOW).expect("set");
        }
        let mut churn: Vec<Vec<u8>> = Vec::new();
        for i in 0..xorshift(&mut x) % 300 {
            let key = format!("c:{round}:{i}").into_bytes();
            store.set(&key, b"v", SetOptions::default(), NOW).expect("set");
            churn.push(key);
        }
        let mut next_churn = churn.len() as u64;
        let mut seen: HashMap<Vec<u8>, u32> = HashMap::new();
        let mut cursor = WalkCursor::START;
        let mut guard = 0u32;
        loop {
            let refuse_at = xorshift(&mut x) % 6;
            let mut n = 0u64;
            let done = store.scan_checkpoint_images_bounded(&mut cursor, 16, NOW, |key, img, _| {
                assert!(matches!(img, CheckpointImage::String(b"v")));
                if n == refuse_at {
                    return false;
                }
                n += 1;
                *seen.entry(key.to_vec()).or_default() += 1;
                true
            });
            if done {
                break;
            }
            guard += 1;
            assert!(guard < 100_000, "the walk must terminate");
            match xorshift(&mut x) % 4 {
                0 => {
                    // A burst that forces a doubling.
                    for _ in 0..xorshift(&mut x) % 128 {
                        let key = format!("c:{round}:{next_churn}").into_bytes();
                        next_churn += 1;
                        store.set(&key, b"v", SetOptions::default(), NOW).expect("set");
                        churn.push(key);
                    }
                }
                1 => {
                    // Insert/delete cycles: tombstones pile up until a
                    // same-size recycle rebuilds the table.
                    for _ in 0..xorshift(&mut x) % 96 {
                        let key = format!("c:{round}:{next_churn}").into_bytes();
                        next_churn += 1;
                        store.set(&key, b"v", SetOptions::default(), NOW).expect("set");
                        store.del(&key, NOW);
                    }
                }
                _ => {
                    // Deletes of pre-existing churn keys, wherever they sit.
                    for _ in 0..xorshift(&mut x) % 8 {
                        if churn.is_empty() {
                            break;
                        }
                        let at = (xorshift(&mut x) % churn.len() as u64) as usize;
                        let key = churn.swap_remove(at);
                        store.del(&key, NOW);
                    }
                }
            }
        }
        for key in &persistent {
            assert!(seen.contains_key(key), "round {round}: persistent key {key:?} never emitted");
        }
    }
}

/// The tiered walk's image half resumes the same way (pass 1 of the
/// hybrid walk, ADR-0057 D1): with nothing flushed every entry images,
/// refusals stop before the entry and the next slice re-enters at it.
#[test]
fn the_tiered_image_walk_resumes_at_the_refused_entry() {
    let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
    let space = AddressSpaceConfig {
        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
        page_bytes: 4 << 10,
        life_origin: LogicalAddr::ZERO,
    };
    let hasher = KeyHasher::default();
    let mut table = TieredTable::new(space, demote, 2048, hasher).expect("ring");
    let keys: BTreeSet<Vec<u8>> = (0..1200u32).map(|i| format!("t:{i:04}").into_bytes()).collect();
    for key in &keys {
        table.insert(key, b"value", hasher.hash(key)).expect("fits");
    }
    table.begin_ckpt_walk(1);
    let mut cursor = WalkCursor::START;
    let mut emitted: Vec<Vec<u8>> = Vec::new();
    let mut calls = 0u64;
    let mut refused: Option<Vec<u8>> = None;
    loop {
        let mut first: Option<Vec<u8>> = None;
        let mut just_refused: Option<Vec<u8>> = None;
        let done = table.ckpt_walk_slice_bounded(
            &mut cursor,
            64,
            |_, _| panic!("nothing is flushed: no entry sits below the watermark"),
            |parts| {
                calls += 1;
                if first.is_none() {
                    first = Some(parts.key.to_vec());
                }
                if just_refused.is_none() && calls.is_multiple_of(5) {
                    just_refused = Some(parts.key.to_vec());
                    return false;
                }
                emitted.push(parts.key.to_vec());
                true
            },
        );
        if let Some(want) = refused.take() {
            assert_eq!(first.as_deref(), Some(want.as_slice()), "resume at the refusal");
        }
        refused = just_refused;
        if done {
            break;
        }
    }
    table.end_ckpt_walk();
    assert_eq!(emitted.len(), keys.len(), "every entry exactly once");
    assert_eq!(emitted.into_iter().collect::<BTreeSet<_>>(), keys);
}
