//! M1-S02 AC: the SCAN guarantee proptest shape — random interleaved
//! inserts/deletes (driving both doubling growth and tombstone-recycling
//! same-size rehashes) while a cursor walk is in flight; every key present
//! for the WHOLE scan must be returned at least once.
//!
//! The construction under test: home-group enumeration in reverse-binary
//! cursor order (`Index::scan_home_group` + the dictScan increment).

use std::collections::HashSet;

use inf_foundation::time::Nanos;
use inf_store::{CellStore, SetExpire, SetOptions, StoreConfig};

const NOW: Nanos = Nanos(1_000_000);

fn scan_step(store: &mut CellStore, cursor: u64, seen: &mut HashSet<Vec<u8>>) -> u64 {
    store.scan(cursor, 8, NOW, |key| {
        seen.insert(key.to_vec());
    })
}

#[test]
fn persistent_keys_always_emitted_under_interleaved_churn() {
    let rounds: usize = if cfg!(miri) { 3 } else { 40 };
    let mut x: u64 = 0x5CA0_6A2D ^ 0xFFFF;
    let mut rand = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..rounds {
        let mut store = CellStore::new(StoreConfig::default());
        // The persistent set: present before the scan starts, never touched.
        let persistent: Vec<Vec<u8>> =
            (0..200 + rand() % 300).map(|i| format!("p:{round}:{i}").into_bytes()).collect();
        for key in &persistent {
            store.set(key, b"v", SetOptions::default(), NOW).expect("set");
        }
        // Pre-existing churn keys some of which die mid-scan.
        let mut churn_id = 0u64;
        for _ in 0..rand() % 200 {
            let key = format!("c:{round}:{churn_id}").into_bytes();
            churn_id += 1;
            store.set(&key, b"v", SetOptions::default(), NOW).expect("set");
        }

        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut cursor = scan_step(&mut store, 0, &mut seen);
        let mut guard = 0u32;
        while cursor != 0 {
            // Interleave: bursts of inserts (forces doubling) and
            // insert/delete cycles (forces tombstone same-size rebuilds).
            match rand() % 3 {
                0 => {
                    for _ in 0..rand() % 64 {
                        let key = format!("c:{round}:{churn_id}").into_bytes();
                        churn_id += 1;
                        store.set(&key, b"v", SetOptions::default(), NOW).expect("set");
                    }
                }
                1 => {
                    for i in 0..rand() % 64 {
                        let key = format!("c:{round}:{i}").into_bytes();
                        store.del(&key, NOW);
                    }
                }
                _ => {
                    // Expired-mid-scan records must be reaped, never emitted.
                    let key = format!("x:{round}:{churn_id}").into_bytes();
                    churn_id += 1;
                    let opts = SetOptions {
                        expire: SetExpire::At(Nanos(NOW.0 - 1)),
                        ..Default::default()
                    };
                    store.set(&key, b"v", opts, NOW).expect("set");
                }
            }
            cursor = scan_step(&mut store, cursor, &mut seen);
            guard += 1;
            assert!(guard < 1_000_000, "scan failed to terminate");
        }
        for key in &persistent {
            assert!(
                seen.contains(key),
                "round {round}: persistent key missed by scan: {:?}",
                String::from_utf8_lossy(key)
            );
        }
        for key in &seen {
            assert!(!key.starts_with(b"x:"), "expired key emitted: {key:?}");
        }
    }
}

#[test]
fn scan_terminates_and_covers_on_static_tables_of_all_sizes() {
    for size in [0usize, 1, 15, 16, 17, 100, 1_000, if cfg!(miri) { 1_001 } else { 50_000 }] {
        let mut store = CellStore::new(StoreConfig::default());
        for i in 0..size {
            let key = format!("k:{i}");
            store.set(key.as_bytes(), b"v", SetOptions::default(), NOW).expect("set");
        }
        let mut seen = HashSet::new();
        let mut cursor = scan_step(&mut store, 0, &mut seen);
        let mut steps = 0u32;
        while cursor != 0 {
            cursor = scan_step(&mut store, cursor, &mut seen);
            steps += 1;
            assert!(steps < 10_000_000, "scan failed to terminate at size {size}");
        }
        assert_eq!(seen.len(), size, "static table must be covered exactly");
    }
}
