#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4.5-S04 budget bench (§4.1): maintenance ≤ 600 ns per (entry
//! remove + insert) pair at 10M entries, plus the prune A/B (pruned
//! vs unpruned rows — the §4.1 budget↔gate arithmetic made an
//! artifact).
//!
//! Method (the `index_key` steady-state precedent — medians over ROUNDS,
//! checksum against dead-code elimination):
//!
//! - `json_set_zero_index` — the same mutation on a namespace with no
//!   indexes: the baseline the pair cost is differenced against, and
//!   the degenerate-case row (one cached branch).
//! - `json_set_one_index_10m` — a root `JSON.SET` alternating one f64
//!   value against an index tree pre-filled to 10M entries: every
//!   iteration is exactly one (remove + insert) pair through the whole
//!   hook (peek, eval, encode, reserve, diff, two tree ops). The
//!   **pair row is the difference** of these two.
//! - `bracket_4idx_unpruned` / `bracket_4idx_pruned` — the bracket
//!   around a no-op mutation with four indexes declared: unpruned pays
//!   both evaluations per index; the pruned row (a provably-disjoint
//!   `$.other` mutation path) pays one decoded-step comparison per
//!   index. Their ratio is the prune's contribution (plan §4.1: the
//!   S04 A/B reports both so the gate number is an artifact, not
//!   folklore).
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench index_maint`
//! Artifact: 3 replicates recorded under `.artifacts/m4.5/s04/`.

use std::hint::black_box;
use std::time::Instant;

use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::time::Nanos;
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, Keyspace, NsId, StoreConfig};

const ROUNDS: usize = 15;
const OPS_PER_ROUND: usize = 20_000;
const TREE_FILL: u64 = 10_000_000;

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN rounds"));
    xs[xs.len() / 2]
}

fn parse(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("valid bench JSON")
}

fn declare(ks: &mut Keyspace, ns: NsId, id: u32, path: &str, key_type: IndexKeyType) {
    let program = compile(path.as_bytes()).expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id: IndexId(id),
        generation: u64::from(id),
        ns,
        name: format!("idx-{id}").into_bytes(),
        program,
        key_type,
        state: IndexState::Declared,
    })
    .expect("declare");
}

/// Median ns per bracketed `JSON.SET` alternating two documents.
fn sweep_set(label: &str, ks: &mut Keyspace, ns: NsId, indexed: bool) -> f64 {
    let key = b"bench:hot";
    let doc_a = parse(r#"{"price":1234.5,"pad":"xxxxxxxxxxxxxxxx"}"#);
    let doc_b = parse(r#"{"price":6789.5,"pad":"xxxxxxxxxxxxxxxx"}"#);
    let now = Nanos(1_000_000_000);
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut checksum = 0u64;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for i in 0..OPS_PER_ROUND {
            let doc = if i % 2 == 0 { &doc_a } else { &doc_b };
            if indexed {
                ks.idx_bracket_begin(ns, &[key], None).expect("headroom");
            }
            ks.db_mut(ns.0 as usize)
                .json_set(key, black_box(doc), Default::default(), now)
                .expect("set");
            if indexed {
                ks.idx_bracket_commit(ns, &[key]);
            }
            checksum = checksum.wrapping_add(i as u64);
        }
        rounds.push(started.elapsed().as_nanos() as f64 / OPS_PER_ROUND as f64);
    }
    black_box(checksum);
    let ns_op = median(rounds);
    println!("row={label} ops={OPS_PER_ROUND} ns_per_op={ns_op:.1}");
    ns_op
}

/// Median ns per bracket around a no-op mutation (pure hook overhead).
fn sweep_bracket(label: &str, ks: &mut Keyspace, ns: NsId, path: Option<&str>) -> f64 {
    let key = b"bench:hot";
    let program = path.map(|p| compile(p.as_bytes()).expect("valid path"));
    let mut rounds = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..OPS_PER_ROUND {
            ks.idx_bracket_begin(ns, &[key], black_box(program.as_ref())).expect("headroom");
            ks.idx_bracket_commit(ns, &[key]);
        }
        rounds.push(started.elapsed().as_nanos() as f64 / OPS_PER_ROUND as f64);
    }
    let ns_op = median(rounds);
    println!("row={label} ops={OPS_PER_ROUND} ns_per_op={ns_op:.1}");
    ns_op
}

fn main() {
    let ns = NsId(0);
    let now = Nanos(1_000_000_000);

    // Baseline: no indexes anywhere — the zero-index branch.
    let mut plain = Keyspace::new(StoreConfig::default());
    let zero = sweep_set("json_set_zero_index", &mut plain, ns, false);

    // One f64 index, tree pre-filled to 10M synthetic entries (§4.1:
    // the budget binds at 10M). The hot doc's own entry rides on top.
    let mut ks = Keyspace::new(StoreConfig::default());
    declare(&mut ks, ns, 1, "$.price", IndexKeyType::F64);
    {
        let seed = parse(r#"{"price":0.5,"pad":"xxxxxxxxxxxxxxxx"}"#);
        ks.idx_bracket_begin(ns, &[b"bench:hot"], None).expect("headroom");
        ks.db_mut(0).json_set(b"bench:hot", &seed, Default::default(), now).expect("seed");
        ks.idx_bracket_commit(ns, &[b"bench:hot"]);
        let tree = ks.idx_tree_mut(ns, IndexId(1)).expect("tree");
        let started = Instant::now();
        for i in 0..TREE_FILL {
            // Shuffled-ish fill (odd multiplier walks the space) so the
            // tree shape matches a random corpus, not an append run.
            let v = (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11) as f64;
            let word = v.to_bits() | 0x8000_0000_0000_0000;
            tree.insert(&word.to_be_bytes(), i).expect("fill");
        }
        eprintln!(
            "# fill: {TREE_FILL} entries in {:.1}s (len {})",
            started.elapsed().as_secs_f64(),
            ks.idx_tree(ns, IndexId(1)).expect("tree").len()
        );
    }
    let indexed = sweep_set("json_set_one_index_10m", &mut ks, ns, true);
    println!("row=maintenance_pair_at_10m ns_per_pair={:.1} budget=600", indexed - zero);

    // Prune A/B: four indexes on paths disjoint from `$.other`.
    let mut ks4 = Keyspace::new(StoreConfig::default());
    declare(&mut ks4, ns, 1, "$.price", IndexKeyType::F64);
    declare(&mut ks4, ns, 2, "$.name", IndexKeyType::Utf8);
    declare(&mut ks4, ns, 3, "$.qty", IndexKeyType::I64);
    declare(&mut ks4, ns, 4, "$.tags[*]", IndexKeyType::Utf8);
    let doc = parse(
        r#"{"price":10.5,"name":"alpha","qty":7,"tags":["a","b"],"other":1,"pad":"xxxxxxxx"}"#,
    );
    ks4.idx_bracket_begin(ns, &[b"bench:hot"], None).expect("headroom");
    ks4.db_mut(0).json_set(b"bench:hot", &doc, Default::default(), now).expect("seed");
    ks4.idx_bracket_commit(ns, &[b"bench:hot"]);
    let unpruned = sweep_bracket("bracket_4idx_unpruned", &mut ks4, ns, None);
    let pruned = sweep_bracket("bracket_4idx_pruned", &mut ks4, ns, Some("$.other"));
    println!(
        "row=prune_delta unpruned_ns={unpruned:.1} pruned_ns={pruned:.1} ratio={:.1}x",
        unpruned / pruned.max(0.1)
    );
}
