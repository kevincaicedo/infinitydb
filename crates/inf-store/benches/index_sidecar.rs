//! M4.5-S06 sidecar load-path A/B (ADR-0078 D5; the ledger's L4 budget:
//! **≤ 60 ns/entry** so 40M entries fit the < 15 s recovery envelope).
//!
//! Rows, per key scheme:
//!   append   `OrderedMap::append` over a strictly-ascending pair
//!            stream — the loader's path (rightmost-spine hop, no
//!            descent, no leaf scan).
//!   insert   `OrderedMap::insert` over the same stream — the general
//!            path the budget rejected on paper; measured so the
//!            verdict is an artifact, not an assumption.
//!
//! Slack is reported for both: the append path fills leaves full/empty,
//! so the loaded tree must come out *denser* (L5).
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench index_sidecar`
//! Env: `INF_BENCH_ENTRIES` (default 10_000_000), `INF_BENCH_REPS` (3).
//! Artifact: `.artifacts/m4.5/s06/`.

use std::hint::black_box;
use std::time::Instant;

use inf_store::{Fixed8, OrderedMap, VarKey};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Ascending Fixed8 pairs with duplicate-key runs (multiple refs per
/// key — the secondary-index shape; ~8 refs per distinct key). Refs
/// ascend within a run: the stream is canonical by construction.
fn fixed_pair(i: u64) -> ([u8; 8], u64) {
    ((i / 8).to_be_bytes(), i)
}

fn bench_fixed(entries: u64, reps: u64) {
    for rep in 0..reps {
        let mut map: OrderedMap<Fixed8> = OrderedMap::new();
        let t = Instant::now();
        for i in 0..entries {
            let (key, entry_ref) = fixed_pair(i);
            map.append(&key, entry_ref).expect("ascending");
        }
        let append_ns = t.elapsed().as_nanos() as f64 / entries as f64;
        let append_mem = map.memory();
        black_box(map.len());
        drop(map);

        let mut map: OrderedMap<Fixed8> = OrderedMap::new();
        let t = Instant::now();
        for i in 0..entries {
            let (key, entry_ref) = fixed_pair(i);
            assert!(map.insert(&key, entry_ref).expect("capacity"));
        }
        let insert_ns = t.elapsed().as_nanos() as f64 / entries as f64;
        let insert_mem = map.memory();
        black_box(map.len());
        println!(
            "  fixed8 rep {rep}: append {append_ns:.1} ns/entry ({} B/entry, {} B slack) \
             vs insert {insert_ns:.1} ns/entry ({} B/entry, {} B slack)",
            append_mem.total_bytes() / entries,
            append_mem.slack_bytes,
            insert_mem.total_bytes() / entries,
            insert_mem.slack_bytes,
        );
    }
}

fn bench_var(entries: u64, reps: u64) {
    // The S02 var shape: short utf8 keys, shared prefixes, ~4 refs per
    // distinct key.
    let keys: Vec<Vec<u8>> =
        (0..entries).map(|i| format!("v:{:012}", i / 4).into_bytes()).collect();
    for rep in 0..reps {
        let mut map: OrderedMap<VarKey> = OrderedMap::new();
        let t = Instant::now();
        for (i, key) in keys.iter().enumerate() {
            map.append(key, i as u64).expect("ascending");
        }
        let append_ns = t.elapsed().as_nanos() as f64 / entries as f64;
        let append_mem = map.memory();
        black_box(map.len());
        drop(map);

        let mut map: OrderedMap<VarKey> = OrderedMap::new();
        let t = Instant::now();
        for (i, key) in keys.iter().enumerate() {
            assert!(map.insert(key, i as u64).expect("capacity"));
        }
        let insert_ns = t.elapsed().as_nanos() as f64 / entries as f64;
        let insert_mem = map.memory();
        black_box(map.len());
        println!(
            "  varkey rep {rep}: append {append_ns:.1} ns/entry ({} B/entry, {} B slack) \
             vs insert {insert_ns:.1} ns/entry ({} B/entry, {} B slack)",
            append_mem.total_bytes() / entries,
            append_mem.slack_bytes,
            insert_mem.total_bytes() / entries,
            insert_mem.slack_bytes,
        );
    }
}

fn main() {
    let entries = env_u64("INF_BENCH_ENTRIES", 10_000_000);
    let reps = env_u64("INF_BENCH_REPS", 3);
    println!("index_sidecar load A/B: {entries} entries, {reps} replicates");
    bench_fixed(entries, reps);
    bench_var(entries, reps);
}
