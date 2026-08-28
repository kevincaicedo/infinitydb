//! M3-S20 placement + document-root prefetch evidence harness.
//!
//! Gate run (10M-document working set, ABBA x3):
//! `taskset -c 4 cargo bench -p inf-store --bench doc_prefetch`
//!
//! A quick rehearsal may lower `INF_DOC_PREFETCH_DOCS`,
//! `INF_DOC_PREFETCH_OPS`, and `INF_DOC_THRESHOLD_DOCS`. Every override is
//! printed; an artifact with fewer than 10M prefetch documents is not the AC.

use std::hint::black_box;
use std::time::Instant;

use inf_doc::JsonParser;
use inf_foundation::time::Nanos;
use inf_store::KeyHasher;
use inf_store::{CellStore, ExpireCond, JsonSetOptions, StoreConfig};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

const NOW: Nanos = Nanos(1);
const BATCH: usize = 32;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Arm {
    Off,
    Record,
    Root,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Off => "off",
            Arm::Record => "record",
            Arm::Root => "record+root",
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

fn key_of(mut value: usize) -> [u8; 12] {
    let mut key = *b"d:0000000000";
    for byte in key[2..].iter_mut().rev() {
        *byte = b'0' + (value % 10) as u8;
        value /= 10;
    }
    key
}

fn trace(documents: usize, operations: usize, seed: u64) -> Vec<[u8; 12]> {
    assert!(documents > 0);
    let mut state = seed;
    let mut keys = Vec::with_capacity(operations);
    for _ in 0..operations {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^= value >> 31;
        keys.push(key_of((value % documents as u64) as usize));
    }
    keys
}

fn run(store: &mut CellStore, keys: &[[u8; 12]], arm: Arm) -> f64 {
    let started = Instant::now();
    let mut observed = 0u64;
    let mut hashes = [0u64; BATCH];
    for chunk in keys.chunks(BATCH) {
        if arm != Arm::Off {
            for (index, key) in chunk.iter().enumerate() {
                hashes[index] = KeyHasher::default().hash(key);
                store.prefetch(hashes[index]);
            }
            for hash in &hashes[..chunk.len()] {
                store.probe_prefetch(*hash);
            }
            if arm == Arm::Root {
                for hash in &hashes[..chunk.len()] {
                    store.prefetch_doc_root(*hash);
                }
            }
        }
        for key in chunk {
            let read = store.json_get(key, NOW).expect("document read").expect("loaded key");
            observed = observed.wrapping_add(u64::from(read.version));
            black_box(read.root);
        }
    }
    black_box(observed);
    keys.len() as f64 / started.elapsed().as_secs_f64()
}

fn run_ttl(store: &mut CellStore, keys: &[[u8; 12]]) -> f64 {
    let started = Instant::now();
    for key in keys {
        assert!(store.expire(key, Some(Nanos::from_millis(10_000)), ExpireCond::Always, NOW));
        assert!(store.expire(key, None, ExpireCond::Always, NOW));
    }
    (2 * keys.len()) as f64 / started.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Three ABBA blocks are three drift-balanced replicates; each replicate
/// averages its two A and two B legs before joining the median.
fn abba(store: &mut CellStore, keys: &[[u8; 12]], a: Arm, b: Arm, replicates: usize) {
    let mut a_reps = Vec::with_capacity(replicates);
    let mut b_reps = Vec::with_capacity(replicates);
    println!("pair a={} b={} schedule=ABBA replicates={replicates}", a.name(), b.name());
    for replicate in 0..replicates {
        let order = if replicate % 2 == 0 { [a, b, b, a] } else { [b, a, a, b] };
        let mut a_sum = 0.0;
        let mut b_sum = 0.0;
        for (leg, arm) in order.into_iter().enumerate() {
            let rate = run(store, keys, arm);
            println!(
                "prefetch replicate={} leg={} arm={} mops={:.6}",
                replicate + 1,
                leg + 1,
                arm.name(),
                rate / 1e6
            );
            if arm == a {
                a_sum += rate;
            } else {
                b_sum += rate;
            }
        }
        a_reps.push(a_sum / 2.0);
        b_reps.push(b_sum / 2.0);
    }
    let a_median = median(&mut a_reps);
    let b_median = median(&mut b_reps);
    println!(
        "prefetch verdict a={} a_mops={:.6} b={} b_mops={:.6} gain={:+.3}%",
        a.name(),
        a_median / 1e6,
        b.name(),
        b_median / 1e6,
        (b_median / a_median - 1.0) * 100.0
    );
}

fn threshold_rows(corpus: &[(String, Vec<u8>)]) {
    let documents = env_usize("INF_DOC_THRESHOLD_DOCS", 400_000);
    let operations = env_usize("INF_DOC_THRESHOLD_OPS", documents);
    let ttl_operations = env_usize("INF_DOC_THRESHOLD_TTL_OPS", operations.min(20_000));
    let reps = env_usize("INF_DOC_THRESHOLD_REPS", 3);
    let thresholds = [0usize, 256, 512, 1_024, 2_048];
    let keys = trace(documents, operations, doc_corpus::CANONICAL_SEED ^ 0x5448_5245_5348);
    println!(
        "threshold corpus=small-200B,deep-32,gate-1KiB,medium-2KiB documents={documents} operations={operations} reps={reps}"
    );
    for threshold in thresholds {
        let mut store = CellStore::new(StoreConfig {
            initial_keys: documents,
            doc_inline_bytes_max: threshold,
            ..StoreConfig::default()
        });
        let started = Instant::now();
        for index in 0..documents {
            let idoc = &corpus[index % corpus.len()].1;
            store
                .json_set(&key_of(index), idoc, JsonSetOptions::default(), NOW)
                .expect("threshold load");
        }
        let load_rate = documents as f64 / started.elapsed().as_secs_f64();
        let report = store.report();
        let domain = store.doc_domain();
        let mut rates = Vec::with_capacity(reps);
        for _ in 0..reps {
            rates.push(run(&mut store, &keys, Arm::Off));
        }
        let read_rate = median(&mut rates);
        let ttl_rate = run_ttl(&mut store, &keys[..ttl_operations.min(keys.len())]);
        println!(
            "threshold bytes={} inline_docs={} records_B_per_doc={:.3} doc_resident_B_per_doc={:.3} attributed_B_per_doc={:.3} load_mops={:.6} read_mops={:.6} ttl_mops={:.6}",
            threshold,
            domain.inline_docs,
            report.records_resident_bytes as f64 / documents as f64,
            report.doc_resident_bytes as f64 / documents as f64,
            report.attributed_bytes() as f64 / documents as f64,
            load_rate / 1e6,
            read_rate / 1e6,
            ttl_rate / 1e6,
        );
    }
}

fn prefetch_rows(gate: &[u8]) {
    let documents = env_usize("INF_DOC_PREFETCH_DOCS", 10_000_000);
    let operations = env_usize("INF_DOC_PREFETCH_OPS", 1_000_000);
    let replicates = env_usize("INF_DOC_PREFETCH_REPS", 3);
    assert!((3..=5).contains(&replicates), "gate evidence requires 3..=5 replicates");
    let mut store = CellStore::new(StoreConfig {
        initial_keys: documents,
        // Hold the 953-byte gate idoc externally even if the threshold
        // decision changes: this row isolates the dependent tape-root miss.
        doc_inline_bytes_max: 512,
        ..StoreConfig::default()
    });
    println!(
        "prefetch corpus=gate-1KiB idoc_bytes={} documents={documents} operations={operations} batch={BATCH} seed=0x{:08X}",
        gate.len(),
        doc_corpus::CANONICAL_SEED
    );
    let started = Instant::now();
    for index in 0..documents {
        store
            .json_set(&key_of(index), gate, JsonSetOptions::default(), NOW)
            .expect("prefetch load");
    }
    let elapsed = started.elapsed().as_secs_f64();
    let report = store.report();
    println!(
        "prefetch load_seconds={elapsed:.6} load_mops={:.6} attributed_bytes={} doc_resident_bytes={} index_bytes={}",
        documents as f64 / elapsed / 1e6,
        report.attributed_bytes(),
        report.doc_resident_bytes,
        report.index_bytes,
    );
    let keys = trace(documents, operations, doc_corpus::CANONICAL_SEED ^ 0x5052_4546_4554);
    abba(&mut store, &keys, Arm::Off, Arm::Root, replicates);
    abba(&mut store, &keys, Arm::Record, Arm::Root, replicates);
}

fn main() {
    let mut parser = JsonParser::new();
    let generated = doc_corpus::generate(doc_corpus::CANONICAL_SEED);
    let mut corpus = Vec::new();
    for name in ["small-200B", "deep-32", "gate-1KiB", "medium-2KiB"] {
        let doc = generated.iter().find(|doc| doc.name == name).expect("named corpus shape");
        corpus.push((name.to_string(), parser.parse(doc.json.as_bytes()).expect("corpus parses")));
    }
    threshold_rows(&corpus);
    let gate = &corpus.iter().find(|(name, _)| name == "gate-1KiB").expect("gate shape").1;
    prefetch_rows(gate);
}
