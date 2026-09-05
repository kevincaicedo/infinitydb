#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4.5-S01 budget bench (§4.1): ordered-map point probe, range next(),
//! the leaf-fanout 32-vs-64 A/B, the SIMD-vs-scalar leaf-search A/B, and
//! bytes/entry attribution at N entries — the §7 memory gate measured
//! early, on random AND sequential (rightmost-split) corpora.
//!
//! Custom harness (the `store`/`resolver` bench precedent): steady-state
//! sweeps over a shuffled hot working set (the "hot" qualifier in the
//! ≤ 150 ns budget), medians over ROUNDS.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench ordered`
//! Size override: `ORDERED_BENCH_N=1000000` (default 10M — the AC shape).
//! Artifact: 3–5 replicates recorded under `.artifacts/m4.5/s01/`.

use std::hint::black_box;
use std::time::Instant;

use inf_store::{Fixed8, KeyScheme, OrderedCursor, OrderedMap, VarKey};

const ROUNDS: usize = 15;
const HOT_SET: usize = 100_000;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn shuffle<T>(items: &mut [T], mut seed: u64) {
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn entry_count() -> usize {
    std::env::var("ORDERED_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000_000)
}

/// Build a fixed8 tree of `n` pairs; returns the tree and its key set.
fn build_fixed<const F: usize>(n: usize, sequential: bool) -> (OrderedMap<Fixed8, F>, Vec<u64>) {
    let mut map = OrderedMap::new();
    let mut keys = Vec::with_capacity(n);
    let mut rng = XorShift(0xBEEF_0001);
    let started = Instant::now();
    // Convention: entry_ref == key, so probes can name the exact pair
    // (contains() is pair-exact) and every probe below is a hit.
    for i in 0..n {
        let key = if sequential { i as u64 } else { rng.next() };
        if map.insert(&key.to_be_bytes(), key).expect("capacity") {
            keys.push(key);
        }
    }
    let per_insert = started.elapsed().as_nanos() as f64 / n as f64;
    let corpus = if sequential { "sequential" } else { "random" };
    println!("row=insert scheme=fixed8 fanout={F} corpus={corpus} n={n} ns_per_op={per_insert:.1}");
    (map, keys)
}

/// Point probes over a shuffled hot subset (cache-warm after round 1).
fn bench_probe<const F: usize>(
    map: &OrderedMap<Fixed8, F>,
    keys: &[u64],
    hot_set: usize,
    scalar: bool,
) -> f64 {
    let mut hot: Vec<u64> = keys.iter().copied().take(hot_set.min(keys.len())).collect();
    shuffle(&mut hot, 0xBEEF_0002);
    let mut rounds = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let mut found = 0u64;
        for &key in &hot {
            let bytes = key.to_be_bytes();
            let hit = if scalar {
                map.contains_scalar_search(black_box(&bytes), black_box(key))
            } else {
                map.contains(black_box(&bytes), black_box(key))
            };
            found += u64::from(hit);
        }
        // Guard: a miss path is a different measurement — refuse it.
        assert_eq!(found, hot.len() as u64, "every probe must hit");
        rounds.push(started.elapsed().as_nanos() as f64 / hot.len() as f64);
    }
    median(rounds)
}

/// Full-scan cursor sweep: amortized ns per `next()`.
fn bench_next<const F: usize>(map: &OrderedMap<Fixed8, F>) -> f64 {
    let mut rounds = Vec::with_capacity(5);
    for _ in 0..5 {
        let mut cursor = OrderedCursor::from_start();
        let started = Instant::now();
        let mut n = 0u64;
        while let Some((key, entry_ref)) = cursor.next(map) {
            black_box((key, entry_ref));
            n += 1;
        }
        rounds.push(started.elapsed().as_nanos() as f64 / n as f64);
    }
    median(rounds)
}

fn report_memory<S: KeyScheme, const F: usize>(map: &OrderedMap<S, F>, scheme: &str, corpus: &str) {
    let memory = map.memory();
    let per_entry = memory.total_bytes() as f64 / memory.entries as f64;
    println!(
        "row=memory scheme={scheme} fanout={F} corpus={corpus} entries={} \
         total_bytes={} slack_bytes={} bytes_per_entry={per_entry:.2}",
        memory.entries,
        memory.total_bytes(),
        memory.slack_bytes,
    );
}

fn fixed_rows<const F: usize>(n: usize) {
    let (map, keys) = build_fixed::<F>(n, false);
    // Three hot-set widths: 1k (everything cache-resident — the floor),
    // 10k (upper levels + probed leaves mostly resident — the budget's
    // "hot"), 100k (leaf working set past L3 — the DRAM-facing shape,
    // reported beside it, never blended).
    for hot_set in [1_000usize, 10_000, HOT_SET] {
        let early = bench_probe(&map, &keys, hot_set, false);
        let count = bench_probe(&map, &keys, hot_set, true);
        println!(
            "row=probe scheme=fixed8 fanout={F} hot_set={hot_set} search=early \
             ns_per_op={early:.1}"
        );
        println!(
            "row=probe scheme=fixed8 fanout={F} hot_set={hot_set} search=count \
             ns_per_op={count:.1}"
        );
    }
    let next = bench_next(&map);
    println!("row=next scheme=fixed8 fanout={F} ns_per_op={next:.2}");
    report_memory(&map, "fixed8", "random");
    drop(map);

    let (map, _) = build_fixed::<F>(n, true);
    report_memory(&map, "fixed8", "sequential");
}

fn var_rows(n: usize) {
    // 16-byte string-shaped keys: 8-byte prefix + 8-byte heap suffix.
    let mut map: OrderedMap<VarKey, 64> = OrderedMap::new();
    let mut rng = XorShift(0xBEEF_0003);
    for i in 0..n {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&rng.next().to_be_bytes());
        key[8..].copy_from_slice(&(i as u64).to_be_bytes());
        map.insert(&key, i as u64).expect("capacity");
    }
    report_memory(&map, "var16", "random");
}

fn main() {
    let n = entry_count();
    println!("# ordered bench: n={n} rounds={ROUNDS} hot_set={HOT_SET}");
    fixed_rows::<64>(n);
    fixed_rows::<32>(n);
    var_rows(n);
}
