#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! ARCH-W0.2 falsifier bench (record `docs/drr/ARCH-W0.2.md` §8): does
//! the bounded alias walk cost anything a removing write can see?
//!
//! `maintenance_pair_at_10m` (`benches/index_maint.rs`) cannot answer:
//! it stores **one** record and fills synthetic pairs straight into the
//! tree, so the walk would run on a one-record table in L1. Here the
//! record table holds `RECORDS` **real** documents, and every op is a
//! removing update of a random key — one bracket whose diff removes an
//! entry, i.e. one enumeration on a cold probe chain.
//!
//! Rows:
//! - `removing_update_at_<n>` — median ns per bracketed `JSON.SET` that
//!   changes the indexed value of a random document.
//! - `each_exact_at_<n>` — median ns per whole-chain fragment walk over
//!   a record index of the same size: `each_exact`, which since ADR-0139
//!   D9 is `probe_exact_bounded` at `groups_max = group_count()`
//!   (falsifier 5: re-expressing it on the bounded walk must be free).
//! - `walk_tallies` — `idx_alias_walk_over` and the longest chain any
//!   enumeration loaded (falsifier 4: 0, and ≤ 16 groups).
//!
//! Instrument (STOP rule 6): estimator = median of `ROUNDS` rounds of
//! `OPS_PER_ROUND` ops, per-op wall time; resolution ≈ 1 ns per op;
//! spread budget ≤ 2 % between two runs of the **same binary** (the A/A
//! control leg — run it first, or the instrument is the finding); then
//! the before/after builds interleaved on one pinned core. A regression
//! above 5 % on `removing_update` rejects "the walk is free". The rows use
//! only API both builds share, so this file drops into the before-tree
//! unchanged apart from `walk_tallies`.
//!
//! Run: `INF_BENCH_RECORDS=10000000 taskset -c 4 cargo bench -p inf-store
//! --bench index_alias_walk`. Results go in the ticket, never the tree.

use std::hint::black_box;
use std::time::Instant;

use inf_alloc::ArenaAddr;
use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::time::Nanos;
use inf_store::{Index, IndexId, IndexKeyType, IndexSpec, IndexState, Keyspace, NsId, StoreConfig};

const ROUNDS: usize = 15;
const OPS_PER_ROUND: usize = 200_000;
const NOW: Nanos = Nanos(1_000_000_000);

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN rounds"));
    xs[xs.len() / 2]
}

fn records() -> u64 {
    std::env::var("INF_BENCH_RECORDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000_000)
}

fn key_of(i: u64) -> [u8; 12] {
    let mut key = *b"d:0000000000";
    key[2..].copy_from_slice(format!("{i:010}").as_bytes());
    key
}

fn doc_of(v: u64) -> Vec<u8> {
    JsonParser::new().parse(format!(r#"{{"v":{v}}}"#).as_bytes()).expect("valid bench JSON")
}

fn bracketed_set(ks: &mut Keyspace, key: &[u8], idoc: &[u8]) {
    ks.idx_bracket_begin(NsId(0), &[key], None).expect("headroom");
    ks.db_mut(0).json_set(key, black_box(idoc), Default::default(), NOW).expect("set");
    ks.idx_bracket_commit(NsId(0), &[key]);
}

/// `(idx_alias_walk_over, longest chain in groups)` — the one function
/// the before-build replaces (it has no such counters).
fn walk_tallies(ks: &Keyspace) -> (u64, u64) {
    let counters = ks.idx_counters_total();
    (counters.alias_walk_over, counters.alias_walk_groups_max)
}

fn removing_updates(n: u64) {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.idx_create(IndexSpec {
        id: IndexId(1),
        generation: 1,
        ns: NsId(0),
        name: b"by-v".to_vec(),
        program: compile(b"$.v").expect("valid path").as_bytes().to_vec(),
        key_type: IndexKeyType::I64,
        state: IndexState::Declared,
    })
    .expect("declare");
    let started = Instant::now();
    for i in 0..n {
        bracketed_set(&mut ks, &key_of(i), &doc_of(i));
    }
    eprintln!("# fill: {n} real records in {:.1}s", started.elapsed().as_secs_f64());

    let mut rng = SplitMix(0xA11A_BE4C);
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut next_value = n;
    for _ in 0..ROUNDS {
        // Documents are built outside the timed window: the row times
        // the bracket, not the JSON parser.
        let batch: Vec<([u8; 12], Vec<u8>)> = (0..OPS_PER_ROUND)
            .map(|_| {
                next_value += 1;
                (key_of(rng.next() % n), doc_of(next_value))
            })
            .collect();
        let started = Instant::now();
        for (key, idoc) in &batch {
            bracketed_set(&mut ks, key, idoc);
        }
        rounds.push(started.elapsed().as_nanos() as f64 / OPS_PER_ROUND as f64);
    }
    println!("row=removing_update_at_{n} ops={OPS_PER_ROUND} ns_per_op={:.1}", median(rounds));
    let (over, groups_max) = walk_tallies(&ks);
    println!("row=walk_tallies idx_alias_walk_over={over} alias_walk_groups_max={groups_max}");
}

fn each_exact_walks(n: u64) {
    let mut index: Index = Index::with_capacity(n as usize);
    let mut rng = SplitMix(0xA11A_1DE7);
    let hashes: Vec<u64> = (0..n).map(|_| rng.next()).collect();
    for (i, hash) in hashes.iter().enumerate() {
        index.insert(*hash, ArenaAddr::from_raw(i as u64).expect("48-bit"));
    }
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut visited = 0u64;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..OPS_PER_ROUND {
            let hash = hashes[(rng.next() % n) as usize];
            index.each_exact(black_box(hash), |addr| visited += addr.to_raw() & 1);
        }
        rounds.push(started.elapsed().as_nanos() as f64 / OPS_PER_ROUND as f64);
    }
    black_box(visited);
    println!("row=each_exact_at_{n} ops={OPS_PER_ROUND} ns_per_op={:.1}", median(rounds));
}

fn main() {
    let n = records();
    each_exact_walks(n);
    removing_updates(n);
}
