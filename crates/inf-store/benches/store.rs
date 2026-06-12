//! M0-S13/S14 gate bench: the L4 A/B for the hash→prefetch→execute batch
//! pipeline at a cache-miss-bound working set, the probe-length histogram
//! at load factor 0.85, and the per-key memory overhead artifact.
//!
//! Custom harness (recorded deviation, same rationale as `uring_echo`):
//! the measurements are steady-state sweeps over a multi-GiB working set
//! and one-shot attribution reports, not closure timings.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench store`
//! Env: `INF_STORE_BENCH_KEYS` overrides the key count (default: sized to
//! load factor 0.85 of a 16M-slot table when RAM allows, else 4M-slot).

use std::time::Instant;

use inf_foundation::time::Nanos;
use inf_store::{CellStore, SetOptions, StoreConfig};

const BATCH: usize = 32;

fn make_key(i: u64) -> [u8; 16] {
    let mut key = *b"k:00000000000000";
    let digits = format!("{i:014}");
    key[2..].copy_from_slice(digits.as_bytes());
    key
}

fn mem_available_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|kb| kb.parse::<u64>().ok()))
        })
        .map_or(u64::MAX, |kb| kb * 1024)
}

fn main() {
    // Load factor 0.85 by construction: 85% of a power-of-two slot count.
    let default_keys = if mem_available_bytes() > 6 << 30 {
        (16u64 << 20) * 85 / 100 // 14.26M keys → ~1.4 GiB records+index
    } else {
        (4u64 << 20) * 85 / 100 // 3.57M keys → ~350 MiB
    };
    let n: u64 = std::env::var("INF_STORE_BENCH_KEYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_keys);
    let now = Nanos(1);
    let value = [0xABu8; 64];

    println!("--- inf-store gate bench: {n} keys x (16 B key, 64 B value) ---");
    let mut store = CellStore::new(StoreConfig { initial_keys: n as usize, ..Default::default() });

    let t = Instant::now();
    for i in 0..n {
        store.set(&make_key(i), &value, SetOptions::default(), now).expect("load");
    }
    println!(
        "load            {:.1}s ({:.2}M sets/s)",
        t.elapsed().as_secs_f64(),
        n as f64 / t.elapsed().as_secs_f64() / 1e6
    );

    // ---- per-key overhead artifact (M0-S13 AC, feeds the RSS gate) ----
    let report = store.report();
    let payload = n * (16 + 64);
    let total = report.records_resident_bytes + report.index_bytes;
    let overhead_per_key = (total - payload) as f64 / n as f64;
    println!("records_live    {} B", report.records_live_bytes);
    println!("records_slack   {} B", report.records_slack_bytes);
    println!("records_resident {} B", report.records_resident_bytes);
    println!(
        "index_bytes     {} B ({:.1} B/key)",
        report.index_bytes,
        report.index_bytes as f64 / n as f64
    );
    println!("OVERHEAD/KEY    {overhead_per_key:.1} B (gate: <= 24 B at LF 0.85)");

    // ---- probe-length histogram at LF 0.85 (M0-S14 AC) ----
    let mut counts = [0u64; 8]; // groups visited: 1..=7+ (capped)
    let samples = (n / 17).max(1);
    let mut idx: u64 = 7;
    for _ in 0..samples {
        idx = (idx.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)) % n;
        let groups = store.probe_groups(&make_key(idx));
        counts[groups.min(7)] += 1;
    }
    let total_probes: u64 = counts.iter().sum();
    let mut acc = 0u64;
    for (groups, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        acc += count;
        println!(
            "probe {} group(s): {:6.3}% (cum {:.3}%)",
            groups,
            count as f64 / total_probes as f64 * 100.0,
            acc as f64 / total_probes as f64 * 100.0
        );
    }

    // ---- L4 A/B: batch pipeline ON vs OFF over a shuffled sweep ----
    // The LCG walk keeps the access pattern identical between variants
    // while defeating the hardware's sequential prefetch.
    let sweeps: u64 = 3;
    let run = |store: &mut CellStore, pipelined: bool| -> f64 {
        let t = Instant::now();
        let mut hits = 0u64;
        for sweep in 0..sweeps {
            let mut idx = 12345 + sweep;
            let mut batch_keys: [[u8; 16]; BATCH] = [[0; 16]; BATCH];
            let batches = n / BATCH as u64;
            for _ in 0..batches {
                for key in &mut batch_keys {
                    idx = (idx.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
                        % n;
                    *key = make_key(idx);
                }
                if pipelined {
                    let refs: [&[u8]; BATCH] = core::array::from_fn(|i| &batch_keys[i][..]);
                    store.get_many(&refs, now, |_, value| hits += u64::from(value.is_some()));
                } else {
                    for key in &batch_keys {
                        hits += u64::from(store.get(key, now).is_some());
                    }
                }
            }
        }
        let elapsed = t.elapsed().as_secs_f64();
        assert_eq!(hits, sweeps * (n / BATCH as u64) * BATCH as u64, "all keys present");
        hits as f64 / elapsed
    };

    // Interleave variants so both share thermal/clock conditions.
    let mut on = 0.0;
    let mut off = 0.0;
    for _ in 0..2 {
        off += run(&mut store, false);
        on += run(&mut store, true);
    }
    let (on, off) = (on / 2.0, off / 2.0);
    println!("GET off-pipeline {:.2}M ops/s", off / 1e6);
    println!("GET on-pipeline  {:.2}M ops/s", on / 1e6);
    println!(
        "PIPELINE GAIN   {:+.1}% (gate: >= +25% on the cache-miss-bound set, else demote to flag + ADR)",
        (on / off - 1.0) * 100.0
    );
}
