//! M4.5-S04 — the maintenance hook's correctness suite (plan S04 ACs;
//! contract ADR-0072 D3/D5/D6/D7, mechanics ADR-0076).
//!
//! The compass property is **local equivalence**: after any interleaving
//! of mutations, expiry, and eviction, every attached tree's contents
//! equal the entries derived from scratch off the live documents — the
//! same derivation the DST oracle (S15) uses, implemented here
//! independently of the hook (a second implementation on purpose: the
//! oracle side re-derives evaluation + coercion + encoding from public
//! APIs, so a hook bug cannot hide in shared code).
//!
//! The big storm follows the `ordered_storm` precedent: deterministic
//! seeds, 10⁶ ops in the release lane with the full from-scratch oracle
//! every 4 Ki ops and at the end; the always-check lane (every op) runs
//! a smaller storm in every profile. Churn legs drive the TTL wheel and
//! both eviction shapes — removal sites are mutation sources, proven,
//! not assumed (plan §3.3).

#![cfg(feature = "doc")]

use std::collections::BTreeSet;

use inf_doc::JsonParser;
use inf_doc::path::{EvalLimits, compile, eval, resolve};
use inf_foundation::time::Nanos;
use inf_store::KeyHasher;
use inf_store::{
    CellStore, EvictBudget, EvictionPolicy, ExpiryBudget, IndexId, IndexKeyBuf, IndexKeyType,
    IndexScalar, IndexSpec, IndexState, Keyspace, NsId, OrderedCursor, PressureConfig, SetOptions,
    StoreConfig, index_key_encode,
};

// ---- deterministic driver ---------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64 — the house L7 posture: seeds in the test, never
        // ambient randomness.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn parse(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("valid test JSON")
}

/// One indexed namespace under test: id, path text, declared type.
const INDEXES: &[(u32, &str, IndexKeyType)] = &[
    (1, "$.price", IndexKeyType::F64),
    (2, "$.name", IndexKeyType::Utf8),
    (3, "$.qty", IndexKeyType::I64),
    (4, "$.tags[*]", IndexKeyType::Utf8),
];

fn fixture(ns: NsId) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    for &(id, path, key_type) in INDEXES {
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
    ks
}

/// A durable named namespace carrying the same index set (the replay
/// arm's host — defaults never log, L2).
fn durable_fixture(ns: NsId) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(inf_store::NsSpec {
        id: ns,
        name: b"ledger".to_vec(),
        mode: inf_store::NsMode::Durable,
        fsync: Some(inf_store::FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("create durable ns");
    for &(id, path, key_type) in INDEXES {
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
    ks
}

/// Drive one mutation through the bracket exactly as the plane does
/// (ADR-0072 D3): pre-half → mutate → commit-half. The mutation runs
/// even when the pre-half refuses — tests that want refusal semantics
/// call the halves directly instead.
fn bracketed<R>(
    ks: &mut Keyspace,
    ns: NsId,
    keys: &[&[u8]],
    path: Option<&str>,
    mutate: impl FnOnce(&mut CellStore) -> R,
) -> R {
    let program = path.map(|p| compile(p.as_bytes()).expect("valid path"));
    ks.idx_bracket_begin(ns, keys, program.as_ref()).expect("reservation headroom");
    let store = if ns.0 < inf_store::FIRST_NAMED_NS_ID {
        ks.db_mut(ns.0 as usize)
    } else {
        ks.ns_store_mut(ns).expect("named store exists")
    };
    let result = mutate(store);
    ks.idx_bracket_commit(ns, keys);
    result
}

// ---- the from-scratch oracle ------------------------------------------------

/// Entries one live document contributes to one index — derived from
/// public APIs only (independent of the hook's implementation).
fn doc_entries(
    store: &mut CellStore,
    key: &[u8],
    path: &str,
    key_type: IndexKeyType,
    now: Nanos,
    out: &mut BTreeSet<(Vec<u8>, u64)>,
) {
    let Ok(Some(read)) = store.json_get(key, now) else { return };
    let program = compile(path.as_bytes()).expect("valid path");
    let matches = eval(&program, read.root, &EvalLimits::default()).expect("test docs are small");
    let hash = KeyHasher::default().hash(key);
    let mut buf = IndexKeyBuf::new();
    for steps in matches.iter() {
        let Some(value) = resolve(read.root, steps) else { continue };
        let scalar = match value {
            inf_doc::DocValue::Null => IndexScalar::Null,
            inf_doc::DocValue::Bool(b) => IndexScalar::Bool(b),
            inf_doc::DocValue::I64(v) => IndexScalar::I64(v),
            inf_doc::DocValue::F64(f) => IndexScalar::F64(f),
            inf_doc::DocValue::Str(s) => IndexScalar::Utf8(s.to_str()),
            _ => continue,
        };
        if index_key_encode(key_type, scalar, &mut buf).is_ok() {
            out.insert((buf.as_bytes().to_vec(), hash));
        }
    }
}

/// The scan-derived truth for one index (plan §3.3: an index is over
/// *live* documents).
fn oracle_entries(
    ks: &mut Keyspace,
    ns: NsId,
    path: &str,
    key_type: IndexKeyType,
    now: Nanos,
) -> BTreeSet<(Vec<u8>, u64)> {
    let store = ks.db_mut(ns.0 as usize);
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.scan(cursor, 512, now, |k| keys.push(k.to_vec()));
        if cursor == 0 {
            break;
        }
    }
    let mut expected = BTreeSet::new();
    for key in &keys {
        doc_entries(store, key, path, key_type, now, &mut expected);
    }
    expected
}

fn tree_entries(ks: &Keyspace, ns: NsId, id: IndexId) -> BTreeSet<(Vec<u8>, u64)> {
    let mut out = BTreeSet::new();
    let Some(tree) = ks.idx_tree(ns, id) else { return out };
    let mut cursor = OrderedCursor::from_start();
    while let Some((key, entry_ref)) = tree.cursor_next(&mut cursor) {
        out.insert((key.to_vec(), entry_ref));
    }
    out
}

/// The local equivalence property (the S04 compass): every tree ≡ its
/// from-scratch derivation, and tree cardinality ≡ attribution.
fn assert_equivalence(ks: &mut Keyspace, ns: NsId, now: Nanos, context: &str) {
    for &(id, path, key_type) in INDEXES {
        let expected = oracle_entries(ks, ns, path, key_type, now);
        let actual = tree_entries(ks, ns, IndexId(id));
        assert_eq!(
            actual, expected,
            "{context}: index {id} ({path}) diverged from scan-derived truth"
        );
    }
}

// ---- corpus -----------------------------------------------------------------

/// A doc whose fields exercise every index: numeric coercion pairs on
/// `price`, strings on `name`, integers on `qty`, and a multi-match
/// array with deliberate duplicates on `tags` (the classic dedup case).
fn random_doc(rng: &mut Rng) -> String {
    let price = match rng.below(5) {
        0 => format!("{}", rng.below(100)),   // i64 into an f64 index
        1 => format!("{}.5", rng.below(100)), // fractional
        2 => "\"not-a-number\"".to_string(),  // sparse (type mismatch)
        3 => format!("{}", (1u64 << 60) + 1), // inexact in f64 (counted)
        _ => format!("-{}.25", rng.below(50)),
    };
    let name = match rng.below(4) {
        0 => "\"alpha\"",
        1 => "\"beta\"",
        2 => "null", // sparse (null-absent)
        _ => "\"gamma\"",
    };
    let qty = match rng.below(4) {
        0 => format!("{}", rng.below(1000)),
        1 => format!("{}.0", rng.below(1000)), // integral f64 into i64 index
        2 => "3.5".to_string(),                // inexact in i64 (counted)
        _ => format!("-{}", rng.below(10)),
    };
    let tags = match rng.below(4) {
        0 => r#"["a","a","b"]"#, // duplicates — the dedup AC case
        1 => r#"["x"]"#,
        2 => "[]",
        _ => r#"["a","b","a","b"]"#,
    };
    format!(r#"{{"price":{price},"name":{name},"qty":{qty},"tags":{tags},"other":1}}"#)
}

fn key_of(i: u64) -> Vec<u8> {
    format!("doc:{i:04}").into_bytes()
}

// ---- the storms -------------------------------------------------------------

/// One storm step: a random mutation through the bracket, exactly as
/// the plane drives it, plus clock advance and MAINTAIN slices.
fn storm_step(ks: &mut Keyspace, ns: NsId, rng: &mut Rng, now: &mut Nanos) {
    let key = key_of(rng.below(256));
    let db = ns.0 as usize;
    match rng.below(10) {
        // Full-document set (creation or overwrite).
        0..=3 => {
            let doc = parse(&random_doc(rng));
            bracketed(ks, ns, &[&key], None, |s| {
                // WrongType over a string-typed key is the ADR-0037 D6
                // contract — a refused mutation leaves post ≡ pre.
                let _ = s.json_set(&key, &doc, Default::default(), *now);
            });
        }
        // Delete inside the bracket (the D6 write-set responsibility).
        4 => {
            bracketed(ks, ns, &[&key], None, |s| s.del(&key, *now));
        }
        // String overwrite of a document key — the overwrite death.
        5 => {
            bracketed(ks, ns, &[&key], None, |s| {
                let _ = s.set(&key, b"plain", SetOptions::default(), *now);
            });
        }
        // Set with a TTL that will fire inside the storm window.
        6 => {
            let doc = parse(&random_doc(rng));
            let at = Nanos(now.0 + 1_000_000 * u64::from(1 + rng.below(20) as u32));
            let opts = inf_store::JsonSetOptions {
                cond: inf_store::SetCond::Always,
                expire: inf_store::SetExpire::At(at),
            };
            bracketed(ks, ns, &[&key], None, |s| {
                let _ = s.json_set(&key, &doc, opts, *now);
            });
        }
        // Lazy-expiry probe: a plain read reaps dead records (the death
        // hook outside any bracket).
        7 => {
            let _ = ks.db_mut(db).get(&key, *now);
        }
        // Active expiry MAINTAIN slice.
        8 => {
            ks.expire_tick(*now, ExpiryBudget::default());
        }
        // RENAME under the bracket (both keys in the write set — the
        // deliberate free_record bypass, ADR-0072 D6).
        _ => {
            let dst = key_of(rng.below(256));
            if dst != key {
                bracketed(ks, ns, &[&key, &dst], None, |s| {
                    let _ = s.rename(&key, &dst, *now);
                });
            }
        }
    }
    // Advance ~1 ms per step so TTL classes fire throughout.
    now.0 += 1_000_000;
}

/// The always-check lane: every step verifies the full equivalence
/// property (small storm, every profile).
#[test]
fn local_equivalence_every_step() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let mut rng = Rng(0x5EED_0401);
    let mut now = Nanos(1_000_000_000);
    for step in 0..600u32 {
        storm_step(&mut ks, ns, &mut rng, &mut now);
        assert_equivalence(&mut ks, ns, now, &format!("step {step}"));
    }
}

/// The 10⁶-op AC storm (release lane — the `ordered_storm` precedent):
/// full from-scratch oracle every 4 Ki ops and at the end, two seeds.
#[test]
fn local_equivalence_million_op_storm() {
    if cfg!(debug_assertions) {
        // The debug lane runs the every-step storm above; the million-op
        // count is the release storm's job (`--release`, CI storm lane).
        return;
    }
    for seed in [0x5EED_0402u64, 0xD15C_0403] {
        let ns = NsId(0);
        let mut ks = fixture(ns);
        let mut rng = Rng(seed);
        let mut now = Nanos(1_000_000_000);
        for step in 0..1_000_000u32 {
            storm_step(&mut ks, ns, &mut rng, &mut now);
            if step % 4096 == 0 {
                assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} step {step}"));
            }
        }
        assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} end"));
    }
}

/// Eviction churn (both shapes — plan S04 AC 2): the global rotation
/// under node pressure, then a per-namespace budget pass; the removal
/// sites are mutation sources and the equivalence property holds.
#[test]
fn eviction_churn_holds_equivalence() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let mut rng = Rng(0x0E1C_0404);
    let mut now = Nanos(1_000_000_000);
    for _ in 0..300 {
        storm_step(&mut ks, ns, &mut rng, &mut now);
    }
    // Squeeze: a limit below current usage forces the global hand to
    // evict documents; every victim must run the death hook.
    let used = ks.used_bytes();
    ks.set_pressure(PressureConfig {
        limit_bytes: used / 2,
        policy: EvictionPolicy::AllKeysRandom,
        samples: 5,
    });
    for round in 0..40 {
        ks.evict_tick(now, EvictBudget { max_evictions: 32 });
        now.0 += 1_000_000;
        assert_equivalence(&mut ks, ns, now, &format!("evict round {round}"));
    }
    assert!(ks.used_bytes() < used, "pressure evicted something");
    // The inline (write-blocking) escalation path too. An OOM verdict
    // here is legal and honest: tree pool reservations do not shrink
    // when their entries evict (ADR-0075 D6 — index capacity tightens
    // the document budget), so the verdict depends on the corpus.
    let _ = ks.free_for_write(now);
    assert_equivalence(&mut ks, ns, now, "after inline eviction");
}

/// TTL wheel churn: arm deadlines across the horizon, advance, and
/// drive MAINTAIN slices until the wheel drains — equivalence at every
/// slice boundary (active expiry is the D6 structural exception site).
#[test]
fn expiry_wheel_churn_holds_equivalence() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let mut rng = Rng(0x77EE_0405);
    let mut now = Nanos(1_000_000_000);
    for i in 0..200u64 {
        let key = key_of(i);
        let doc = parse(&random_doc(&mut rng));
        let at = Nanos(now.0 + 1_000_000 * (1 + rng.below(50)));
        let opts = inf_store::JsonSetOptions {
            cond: inf_store::SetCond::Always,
            expire: inf_store::SetExpire::At(at),
        };
        bracketed(&mut ks, ns, &[&key], None, |s| s.json_set(&key, &doc, opts, now).expect("set"));
    }
    for round in 0..60 {
        now.0 += 1_000_000;
        ks.expire_tick(now, ExpiryBudget::default());
        assert_equivalence(&mut ks, ns, now, &format!("expiry round {round}"));
    }
    // Everything armed has fired; all trees must be empty.
    now.0 += 60_000_000;
    for _ in 0..20 {
        ks.expire_tick(now, ExpiryBudget::default());
    }
    for &(id, ..) in INDEXES {
        assert!(
            tree_entries(&ks, ns, IndexId(id)).is_empty(),
            "index {id} drained with its documents"
        );
    }
}

/// FLUSH* is the whole-namespace truncate (ADR-0072 D6): trees empty,
/// declarations survive, maintenance resumes.
#[test]
fn flush_truncates_trees_but_keeps_declarations() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let mut rng = Rng(0xF105_0406);
    let mut now = Nanos(1_000_000_000);
    for _ in 0..100 {
        storm_step(&mut ks, ns, &mut rng, &mut now);
    }
    ks.db_mut(0).flush(now);
    for &(id, ..) in INDEXES {
        assert!(tree_entries(&ks, ns, IndexId(id)).is_empty(), "index {id} truncated");
    }
    assert!(ks.ns_has_indexes(ns), "declarations survive FLUSH");
    let key = b"doc:after-flush";
    let doc = parse(r#"{"price":9.5,"name":"post","qty":1,"tags":["z"]}"#);
    bracketed(&mut ks, ns, &[key.as_slice()], None, |s| {
        s.json_set(key, &doc, Default::default(), now).expect("set")
    });
    assert_equivalence(&mut ks, ns, now, "after flush + reinsert");
}

/// COPY maintains the destination's entries under new pk refs — the
/// same-db mini-bracket and the cross-db `copy_between` leg (ADR-0076
/// D3's named exception).
#[test]
fn copy_maintains_destination_entries() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    let doc = parse(r#"{"price":4.5,"name":"src","qty":7,"tags":["a","a"]}"#);
    bracketed(&mut ks, ns, &[b"src".as_slice()], None, |s| {
        s.json_set(b"src", &doc, Default::default(), now).expect("set")
    });
    // Same-db COPY: no plane bracket — the store's own mini-bracket.
    ks.db_mut(0).copy(b"src", b"dst", false, now).expect("copy");
    assert_equivalence(&mut ks, ns, now, "same-db copy");
    // Cross-db COPY onto an indexed destination db.
    let ns2 = NsId(1);
    for &(id, path, key_type) in INDEXES {
        let program = compile(path.as_bytes()).expect("valid path").as_bytes().to_vec();
        ks.idx_create(IndexSpec {
            id: IndexId(id + 10),
            generation: u64::from(id + 10),
            ns: ns2,
            name: format!("idx2-{id}").into_bytes(),
            program,
            key_type,
            state: IndexState::Declared,
        })
        .expect("declare");
    }
    ks.copy_between(0, b"src", 1, b"far", false, now).expect("cross-db copy");
    for &(id, path, key_type) in INDEXES {
        let expected = oracle_entries(&mut ks, ns2, path, key_type, now);
        let actual = tree_entries(&ks, ns2, IndexId(id + 10));
        assert_eq!(actual, expected, "cross-db index {id} diverged");
    }
}

/// The dedup AC case verbatim: `$.tags[*]` with repeated values yields
/// one `(typed key, pk)` pair; the overwrite diff's remove side stays
/// exact (skipping dedup corrupts it — plan S04 pitfalls).
#[test]
fn multi_match_duplicates_dedupe_per_document() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    let key = b"doc:dup";
    let doc = parse(r#"{"price":1,"name":"d","qty":1,"tags":["a","a","b","a"]}"#);
    bracketed(&mut ks, ns, &[key.as_slice()], None, |s| {
        s.json_set(key, &doc, Default::default(), now).expect("set")
    });
    let tags = tree_entries(&ks, ns, IndexId(4));
    assert_eq!(tags.len(), 2, "duplicates collapse to one pair per value");
    // Shrink to a subset — the remove side must remove exactly "b".
    let doc2 = parse(r#"{"price":1,"name":"d","qty":1,"tags":["a","a"]}"#);
    bracketed(&mut ks, ns, &[key.as_slice()], None, |s| {
        s.json_set(key, &doc2, Default::default(), now).expect("set")
    });
    assert_equivalence(&mut ks, ns, now, "after duplicate shrink");
    assert_eq!(tree_entries(&ks, ns, IndexId(4)).len(), 1);
    bracketed(&mut ks, ns, &[key.as_slice()], None, |s| s.del(key, now));
    assert!(tree_entries(&ks, ns, IndexId(4)).is_empty());
}

/// Sparse/skip taxonomy is counted, never silent (ADR-0074 D6 wired):
/// the corpus above plants sparse, inexact-in-f64, and inexact-in-i64
/// values; the counters must move.
#[test]
fn skip_taxonomy_is_counted() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    let normal = parse(r#"{"price":1.5,"name":"n","qty":2,"tags":["t"]}"#);
    bracketed(&mut ks, ns, &[b"doc:normal".as_slice()], None, |s| {
        s.json_set(b"doc:normal", &normal, Default::default(), now).expect("set")
    });
    let doc =
        parse(&format!(r#"{{"price":{},"name":null,"qty":3.5,"tags":[]}}"#, (1u64 << 60) + 1));
    bracketed(&mut ks, ns, &[b"doc:skips".as_slice()], None, |s| {
        s.json_set(b"doc:skips", &doc, Default::default(), now).expect("set")
    });
    let price = ks.idx_counters(ns, IndexId(1)).expect("counters");
    assert_eq!(price.skipped_inexact, 1, "2^60+1 does not admit into f64");
    let name = ks.idx_counters(ns, IndexId(2)).expect("counters");
    assert_eq!(name.skipped_sparse, 1, "null is sparse-absent");
    let qty = ks.idx_counters(ns, IndexId(3)).expect("counters");
    assert_eq!(qty.skipped_inexact, 1, "3.5 does not admit into i64");
    let total = ks.idx_counters_total();
    assert_eq!(total.skipped_inexact, 2);
    assert!(total.maint_inserts > 0);
}

/// The prune AC (§4.1 arithmetic made real): a provably-disjoint path
/// mutation skips both evaluations for the index — observable through
/// `maint_prunes` — while overlapping mutations keep maintaining.
#[test]
fn static_prune_skips_disjoint_path_mutations() {
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    let key = b"doc:prune";
    let doc = parse(r#"{"price":10.5,"name":"p","qty":2,"tags":["a"],"other":1}"#);
    bracketed(&mut ks, ns, &[key.as_slice()], None, |s| {
        s.json_set(key, &doc, Default::default(), now).expect("set")
    });
    let before = ks.idx_counters(ns, IndexId(1)).expect("counters");
    // A mutation scoped to `$.other` is disjoint from every declared
    // index path except the wildcard (`$.tags[*]` prunes too — Child
    // "other" vs Child "tags" mismatch decides before the wildcard).
    bracketed(&mut ks, ns, &[key.as_slice()], Some("$.other"), |_| {});
    let after = ks.idx_counters(ns, IndexId(1)).expect("counters");
    assert_eq!(after.maint_prunes, before.maint_prunes + 1, "disjoint path pruned");
    // Equivalence still holds (the prune skipped work, not truth).
    assert_equivalence(&mut ks, ns, now, "after pruned no-op");
    // An overlapping path does not prune.
    let before = ks.idx_counters(ns, IndexId(1)).expect("counters");
    bracketed(&mut ks, ns, &[key.as_slice()], Some("$.price"), |_| {});
    let after = ks.idx_counters(ns, IndexId(1)).expect("counters");
    assert_eq!(after.maint_prunes, before.maint_prunes, "overlap keeps evaluating");
}

/// Declarations that predate materialization install their trees when
/// the store materializes; a re-seed resyncs stale attaches (the
/// ADR-0076 D1 sync points).
#[test]
fn attach_blocks_sync_at_materialization_and_seed() {
    let ns = NsId(5);
    let mut ks = Keyspace::new(StoreConfig::default());
    let program = compile(b"$.price").expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id: IndexId(1),
        generation: 1,
        ns,
        name: b"late".to_vec(),
        program,
        key_type: IndexKeyType::F64,
        state: IndexState::Declared,
    })
    .expect("declare before db 5 ever materializes");
    assert!(ks.idx_tree(ns, IndexId(1)).is_none(), "store not materialized yet");
    let now = Nanos(1_000_000_000);
    let doc = parse(r#"{"price":3.5}"#);
    bracketed(&mut ks, ns, &[b"k".as_slice()], None, |s| {
        s.json_set(b"k", &doc, Default::default(), now).expect("set")
    });
    assert_eq!(tree_entries(&ks, ns, IndexId(1)).len(), 1, "attach installed + maintained");
    // Rebuild resets the attach tree with the bumped generation.
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Backfilling).expect("edge");
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
    ks.idx_rebuild(IndexId(1), 2).expect("rebuild");
    assert!(tree_entries(&ks, ns, IndexId(1)).is_empty(), "rebuild resets contents");
}

/// Replay maintenance (ADR-0072 D4 / ADR-0076 D7): with the dial armed,
/// applying the log records reproduces exactly the live-path trees —
/// rebuild ≡ live, one code path. With the dial off (the boot default),
/// replay maintains nothing.
#[test]
fn replay_maintenance_matches_live() {
    use inf_log::{DocLineage, RecordView};
    use inf_store::WallAnchor;
    // Defaults never log (L2) — replay targets a durable named ns.
    let ns = NsId(16);
    let now = Nanos(1_000_000_000);
    let anchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
    // Live side: three docs, one overwrite, one delete, one string
    // overwrite — every record class the replay arm covers.
    let mut live = durable_fixture(ns);
    let docs = [
        (&b"k1"[..], r#"{"price":1.5,"name":"a","qty":1,"tags":["t","t"]}"#),
        (&b"k2"[..], r#"{"price":2.5,"name":"b","qty":2,"tags":["u"]}"#),
        (&b"k3"[..], r#"{"price":3.5,"name":"c","qty":3,"tags":[]}"#),
    ];
    for (key, json) in docs {
        let idoc = parse(json);
        bracketed(&mut live, ns, &[key], None, |s| {
            s.json_set(key, &idoc, Default::default(), now).expect("set")
        });
    }
    let overwrite = parse(r#"{"price":9.0,"name":"a2","qty":9,"tags":["v"]}"#);
    bracketed(&mut live, ns, &[b"k1".as_slice()], None, |s| {
        s.json_set(b"k1", &overwrite, Default::default(), now).expect("set")
    });
    bracketed(&mut live, ns, &[b"k2".as_slice()], None, |s| s.del(b"k2", now));
    // Replay side: the same history as DocFull/Delete records, dial
    // armed Strict (rebuild-from-scratch replay).
    let mut replayed = durable_fixture(ns);
    replayed.idx_set_replay_maintenance(ns, Some(inf_store::MaintMode::Strict));
    let mut version = 0u32;
    for (key, json) in docs {
        version += 1;
        let idoc = parse(json);
        let lineage = DocLineage::new(u64::from(version)).expect("nonzero");
        let rec = RecordView::DocFull { ns, key, lineage, version, idoc: &idoc };
        replayed.apply_record(&rec, now, anchor).expect("replay");
    }
    let rec = RecordView::DocFull {
        ns,
        key: b"k1",
        lineage: DocLineage::FIRST,
        version: 9,
        idoc: &overwrite,
    };
    replayed.apply_record(&rec, now, anchor).expect("replay");
    replayed.apply_record(&RecordView::Delete { ns, key: b"k2" }, now, anchor).expect("replay");
    for &(id, ..) in INDEXES {
        assert_eq!(
            tree_entries(&live, ns, IndexId(id)),
            tree_entries(&replayed, ns, IndexId(id)),
            "rebuild ≡ live for index {id}"
        );
    }
    // Dial off (the boot default): replay maintains nothing.
    let mut cold = durable_fixture(ns);
    let idoc = parse(docs[0].1);
    let lineage = DocLineage::new(7).expect("nonzero");
    let rec = RecordView::DocFull { ns, key: b"k9", lineage, version: 1, idoc: &idoc };
    cold.apply_record(&rec, now, anchor).expect("replay");
    for &(id, ..) in INDEXES {
        assert!(
            tree_entries(&cold, ns, IndexId(id)).is_empty(),
            "unarmed replay leaves index {id} to the S05 rebuild"
        );
    }
}

/// The plan-then-commit reservation refusal (ADR-0072 D7.1, S04 AC 3):
/// fault-injected via `idx_reserve_refuse` — the mutation fails typed
/// with document, index, and accounting unchanged, and the bracket
/// closes cleanly.
#[test]
fn reservation_refusal_is_typed_and_mutates_nothing() {
    use inf_foundation::fault::{self, FaultSpec};
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    let doc = parse(r#"{"price":5.5,"name":"pre","qty":1,"tags":["a"]}"#);
    bracketed(&mut ks, ns, &[b"doc:pre".as_slice()], None, |s| {
        s.json_set(b"doc:pre", &doc, Default::default(), now).expect("set")
    });
    let trees_before: Vec<_> =
        INDEXES.iter().map(|&(id, ..)| tree_entries(&ks, ns, IndexId(id))).collect();
    let counters_before = ks.idx_counters_total();
    let used_before = ks.used_bytes();

    fault::arm(inf_store::fault::IDX_RESERVE_REFUSE, FaultSpec::Always);
    let refusal = ks
        .idx_bracket_begin(ns, &[b"doc:new".as_slice()], None)
        .expect_err("armed reservation refuses");
    fault::disarm(inf_store::fault::IDX_RESERVE_REFUSE);
    assert_eq!(refusal, inf_store::IdxMaintRefusal::Reserve);
    assert!(refusal.message().starts_with("ERR index maintenance refused"));

    // The plane never executes a refused command: document absent,
    // trees, counters, and accounting all unchanged.
    assert!(ks.db_mut(0).get(b"doc:new", now).is_none());
    for (i, &(id, ..)) in INDEXES.iter().enumerate() {
        assert_eq!(tree_entries(&ks, ns, IndexId(id)), trees_before[i], "tree {id} unchanged");
    }
    assert_eq!(ks.idx_counters_total(), counters_before, "counters unchanged");
    assert_eq!(ks.used_bytes(), used_before, "accounting unchanged");

    // The bracket closed cleanly — the next mutation maintains normally.
    let doc2 = parse(r#"{"price":6.5,"name":"post","qty":2,"tags":["b"]}"#);
    bracketed(&mut ks, ns, &[b"doc:new".as_slice()], None, |s| {
        s.json_set(b"doc:new", &doc2, Default::default(), now).expect("set")
    });
    assert_equivalence(&mut ks, ns, now, "after disarm");
}

/// The degraded-marking backstop proven by a planted trip (ADR-0072
/// D7.2, S04 AC 3): the document mutation stands, every participating
/// index flips its cell-local serving veto (queries refuse through
/// `idx_degraded` — wrong results are never served), maintenance stops
/// touching the degraded trees, and rebuild clears the veto.
#[test]
fn planted_apply_trip_degrades_and_never_lies() {
    use inf_foundation::fault::{self, FaultSpec};
    let ns = NsId(0);
    let mut ks = fixture(ns);
    let now = Nanos(1_000_000_000);
    fault::arm(inf_store::fault::IDX_APPLY_TRIP, FaultSpec::Nth(1));
    let doc = parse(r#"{"price":7.5,"name":"trip","qty":3,"tags":["c"]}"#);
    bracketed(&mut ks, ns, &[b"doc:trip".as_slice()], None, |s| {
        s.json_set(b"doc:trip", &doc, Default::default(), now).expect("set")
    });
    fault::disarm(inf_store::fault::IDX_APPLY_TRIP);

    // The mutation stands (the log is truth; the projection broke).
    assert!(ks.db_mut(0).json_get(b"doc:trip", now).expect("type").is_some());
    for &(id, ..) in INDEXES {
        assert_eq!(ks.idx_degraded(ns, IndexId(id)), Some(true), "index {id} vetoes serving");
        assert!(tree_entries(&ks, ns, IndexId(id)).is_empty(), "no partial entries served");
    }
    assert!(ks.idx_counters_total().degraded_trips >= 1);

    // Degraded indexes are skipped by later maintenance — no asserts
    // trip, no entries appear.
    let doc2 = parse(r#"{"price":8.5,"name":"after","qty":4,"tags":["d"]}"#);
    bracketed(&mut ks, ns, &[b"doc:after".as_slice()], None, |s| {
        s.json_set(b"doc:after", &doc2, Default::default(), now).expect("set")
    });
    for &(id, ..) in INDEXES {
        assert!(tree_entries(&ks, ns, IndexId(id)).is_empty(), "degraded {id} unmaintained");
    }

    // Rebuild is the named recovery path: the veto clears and
    // maintenance resumes from the fresh tree.
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Backfilling).expect("edge");
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
    ks.idx_rebuild(IndexId(1), 100).expect("rebuild");
    assert_eq!(ks.idx_degraded(ns, IndexId(1)), Some(false), "rebuild clears the veto");
    let doc3 = parse(r#"{"price":9.5,"name":"fresh","qty":5,"tags":["e"]}"#);
    bracketed(&mut ks, ns, &[b"doc:fresh".as_slice()], None, |s| {
        s.json_set(b"doc:fresh", &doc3, Default::default(), now).expect("set")
    });
    assert_eq!(tree_entries(&ks, ns, IndexId(1)).len(), 1, "maintenance resumed post-rebuild");
}
