//! M4.5-S05 backfill gate bench (§4.1): ≥ 500k entries/s/cell **with**
//! foreground p99.9 < 2 ms in the same run — both numbers, one run
//! (the co-gate; plan S05 AC 1).
//!
//! Method: build a 10M-document corpus with no index declared, declare
//! one f64 chain index (`$.price` — one entry per document, so the walk
//! rate *is* the entry rate), then drive plane-shaped MAINTAIN ticks
//! (`max_docs = 1024`, the plane's hard cap) to completion while
//! foreground traffic interleaves between slices — bracketed `JSON.SET`
//! plus `GET` on corpus keys, so the mutations race the walk through
//! the real hook.
//!
//! Foreground latency uses the worst-case arrival model: a command
//! arriving during a slice queues behind it, so each sampled op's
//! reported latency is (preceding slice duration + its own execution).
//! The p99.9 of that distribution is the co-gate number — honest about
//! the stall a slice can impose, not just op cost on an idle cell.
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench index_backfill`
//! Artifact: 3 replicates recorded under `.artifacts/m4.5/s05/`.

use std::hint::black_box;
use std::time::Instant;

use inf_doc::JsonParser;
use inf_doc::path::compile;
use inf_foundation::time::Nanos;
use inf_store::{
    BackfillBudget, IndexId, IndexKeyType, IndexSpec, IndexState, Keyspace, NsId, StoreConfig,
};

const DOCS: u64 = 10_000_000;
/// The plane's per-tick cap (`MAX_BACKFILL_DOCS_PER_TICK`).
const SLICE_DOCS: u32 = 1024;
const SLICE_STEPS: u32 = 8192;
/// Foreground ops sampled after every slice.
const FG_PER_SLICE: usize = 4;

fn parse(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("valid bench JSON")
}

fn key_of(i: u64) -> Vec<u8> {
    format!("doc:{i:07}").into_bytes()
}

fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let at = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[at]
}

fn main() {
    let ns = NsId(0);
    let mut ks = Keyspace::new(StoreConfig::default());
    let now = Nanos(1_000_000_000);

    // ---- corpus (no index exists yet — the population the walk covers)
    let build_started = Instant::now();
    let doc_a = parse(r#"{"price":1234.5,"pad":"xxxxxxxxxxxxxxxx"}"#);
    let doc_b = parse(r#"{"price":77.5,"pad":"yyyyyyyyyyyyyyyy"}"#);
    for i in 0..DOCS {
        let key = key_of(i);
        let doc = if i % 2 == 0 { &doc_a } else { &doc_b };
        ks.db_mut(0).json_set(&key, doc, Default::default(), now).expect("corpus set");
    }
    println!(
        "corpus: {DOCS} docs in {:.1}s ({} MiB attributed)",
        build_started.elapsed().as_secs_f64(),
        ks.used_bytes() >> 20
    );

    // ---- declare + walk to completion, foreground racing between slices
    let program = compile(b"$.price").expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id: IndexId(1),
        generation: 1,
        ns,
        name: b"by-price".to_vec(),
        program,
        key_type: IndexKeyType::F64,
        state: IndexState::Declared,
    })
    .expect("declare");

    let budget = BackfillBudget { max_docs: SLICE_DOCS, max_steps: SLICE_STEPS };
    let mut walk_ns = 0u64;
    let mut entries = 0u64;
    let mut docs_scanned = 0u64;
    let mut slices = 0u64;
    let mut slice_max_ns = 0u64;
    let mut fg_lat_ns: Vec<u64> = Vec::with_capacity(1 << 16);
    let mut fg_rng = 0x5EED_0512u64;
    let wall_started = Instant::now();
    loop {
        let slice_started = Instant::now();
        let stats = ks.idx_backfill_tick(now, budget);
        let slice_ns = slice_started.elapsed().as_nanos() as u64;
        walk_ns += slice_ns;
        slice_max_ns = slice_max_ns.max(slice_ns);
        entries += stats.entries_inserted;
        docs_scanned += stats.docs_scanned;
        slices += 1;
        let done = ks.idx_registry().cell_state(IndexId(1)) == Some(IndexState::Ready);
        // Foreground between slices: worst-case arrival queues behind
        // the slice just measured.
        for f in 0..FG_PER_SLICE {
            fg_rng = fg_rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let key = key_of(fg_rng % DOCS);
            let op_started = Instant::now();
            if f % 2 == 0 {
                let doc = if fg_rng & 2 == 0 { &doc_a } else { &doc_b };
                ks.idx_bracket_begin(ns, &[&key], None).expect("headroom");
                ks.db_mut(0).json_set(&key, black_box(doc), Default::default(), now).expect("set");
                ks.idx_bracket_commit(ns, &[&key]);
            } else {
                black_box(ks.db_mut(0).json_get(&key, now).expect("type").is_some());
            }
            let op_ns = op_started.elapsed().as_nanos() as u64;
            fg_lat_ns.push(slice_ns + op_ns);
        }
        if done {
            break;
        }
    }
    let wall_ns = wall_started.elapsed().as_nanos() as u64;

    fg_lat_ns.sort_unstable();
    // The gate rate counts entries the walk *processed* (docs × 1 entry
    // each): foreground SETs racing the walk insert some entries through
    // the hook first, so the walk's fresh-insert count runs below the
    // corpus by exactly the raced share (idempotence at work — reported).
    let walk_rate = docs_scanned as f64 / (walk_ns as f64 / 1e9);
    let wall_rate = docs_scanned as f64 / (wall_ns as f64 / 1e9);
    let p50 = quantile(&fg_lat_ns, 0.50);
    let p99 = quantile(&fg_lat_ns, 0.99);
    let p999 = quantile(&fg_lat_ns, 0.999);
    let max = *fg_lat_ns.last().unwrap_or(&0);
    println!(
        "backfill: {docs_scanned} entries processed ({entries} fresh, rest raced by the \
         hook) in {slices} slices; walk {:.2}s → {:.0} entries/s walk-time, \
         {:.0} entries/s wall",
        walk_ns as f64 / 1e9,
        walk_rate,
        wall_rate
    );
    println!(
        "slice: max {:.3} ms (cap {SLICE_DOCS} docs); foreground (queued-arrival model, \
         {} samples): p50 {:.3} ms · p99 {:.3} ms · p99.9 {:.3} ms · max {:.3} ms",
        slice_max_ns as f64 / 1e6,
        fg_lat_ns.len(),
        p50 as f64 / 1e6,
        p99 as f64 / 1e6,
        p999 as f64 / 1e6,
        max as f64 / 1e6,
    );
    let rate_ok = walk_rate >= 500_000.0;
    let lat_ok = p999 < 2_000_000;
    println!(
        "co-gate: entries/s {} (≥ 500k) · foreground p99.9 {} (< 2 ms) → {}",
        if rate_ok { "PASS" } else { "FAIL" },
        if lat_ok { "PASS" } else { "FAIL" },
        if rate_ok && lat_ok { "PASS" } else { "FAIL" }
    );
    // Completeness: one entry per live document, whichever side (walk or
    // hook) inserted it — the convergence property, asserted on the tree.
    let tree_len = ks.idx_tree(ns, IndexId(1)).expect("attached").len();
    assert_eq!(tree_len, DOCS, "one entry per document at convergence");
}
