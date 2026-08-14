//! M4.5-S05 — the backfill state machine's correctness suite (plan S05
//! ACs; mechanics ADR-0077, consuming ADR-0072 D5 idempotence and the
//! S04 always-on hook).
//!
//! # The interleaving case table (written before the tests — plan S05)
//!
//! Every interleaving of the walk (W) with concurrent activity on a key
//! K falls in one of these classes; the always-on hook (H) plus globally
//! idempotent entry ops (insert-if-absent / remove-if-present) converge
//! each one to scan-derived truth at walk end. "Scanned" means W already
//! emitted K's home region; the single-threaded cell means W and H never
//! interleave *within* one record.
//!
//! | # | class | who inserts | who removes stale | note |
//! |---|-------|-------------|-------------------|------|
//! | 1 | K pre-existing, untouched for the whole walk | W (only source) | — | the plain backfill case |
//! | 2 | K created mid-walk, region already scanned | H at write time | — | W never sees it; H is the only source |
//! | 3 | K created mid-walk, region not yet scanned | H at write time; W re-inserts | — | W's insert is a no-op (idempotent) |
//! | 4 | K mutated after W scanned it | H's diff inserts new | H's diff removes W's old entries | the pre-image bracket sees the physical record |
//! | 5 | K mutated before W scanned it | H's diff inserts new; W re-inserts current | H's remove may miss (legal — unconverged) | `Strict` asserts are scoped to converged indexes |
//! | 6 | K deleted after W scanned it | — | H's diff / death hook removes W's entries | |
//! | 7 | K deleted before W scanned it | — | removal misses (legal); W never emits a dead key | |
//! | 8 | K deleted then re-created mid-walk | composition of 6/7 then 2/3 | composition | |
//! | 9 | K expired-but-unreaped when W arrives | — | W reaps on encounter; the death hook removes write-time entries | keeps the ADR-0076 D4 physical-view invariant |
//! | 10 | K expires after W scanned it | W inserted | the later reap's death hook removes them | post-convergence this is the `Strict` found assert |
//! | 11 | rehash (doubling) mid-walk | W may emit K twice | — | at-least-once emission; duplicates are no-ops |
//! | 12 | eviction victim mid-walk | — | the death hook at the eviction site | both eviction shapes |
//!
//! There is no third source and no third remover, so the compass
//! property — every converged tree ≡ its from-scratch derivation off the
//! live documents — is asserted at every completion below, storms
//! included. The watermark is never consulted for membership (the M1
//! SCAN lesson): the table above never mentions the cursor except to
//! define "scanned".

#![cfg(feature = "doc")]

use std::collections::BTreeSet;

use inf_doc::JsonParser;
use inf_doc::path::{EvalLimits, compile, eval, resolve};
use inf_foundation::time::Nanos;
use inf_store::{
    BackfillBudget, BackfillPhase, CellStore, EvictBudget, EvictionPolicy, ExpiryBudget, IndexId,
    IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec, IndexState, Keyspace, NsId, OrderedCursor,
    PressureConfig, SetOptions, StoreConfig, index_key_encode,
};

// ---- deterministic driver (the S04 storm harness, backfill-shaped) ----------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64 — seeds in the test, never ambient randomness (L7).
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

/// The S04 index set: scalar chains plus the `[*]` multi-match case.
const INDEXES: &[(u32, &str, IndexKeyType)] = &[
    (1, "$.price", IndexKeyType::F64),
    (2, "$.name", IndexKeyType::Utf8),
    (3, "$.qty", IndexKeyType::I64),
    (4, "$.tags[*]", IndexKeyType::Utf8),
];

fn declare_indexes(ks: &mut Keyspace, ns: NsId) {
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
}

fn random_doc(rng: &mut Rng) -> String {
    let price = match rng.below(5) {
        0 => format!("{}", rng.below(100)),
        1 => format!("{}.5", rng.below(100)),
        2 => "\"not-a-number\"".to_string(),
        3 => format!("{}", (1u64 << 60) + 1),
        _ => format!("-{}.25", rng.below(50)),
    };
    let name = match rng.below(4) {
        0 => "\"alpha\"",
        1 => "\"beta\"",
        2 => "null",
        _ => "\"gamma\"",
    };
    let qty = match rng.below(4) {
        0 => format!("{}", rng.below(1000)),
        1 => format!("{}.0", rng.below(1000)),
        2 => "3.5".to_string(),
        _ => format!("-{}", rng.below(10)),
    };
    let tags = match rng.below(4) {
        0 => r#"["a","a","b"]"#,
        1 => r#"["x"]"#,
        2 => "[]",
        _ => r#"["a","b","a","b"]"#,
    };
    format!(r#"{{"price":{price},"name":{name},"qty":{qty},"tags":{tags},"other":1}}"#)
}

fn key_of(i: u64) -> Vec<u8> {
    format!("doc:{i:04}").into_bytes()
}

/// The pre-declaration corpus: `count` random documents in `ns`, written
/// while no index exists — the population the walk exists to cover.
fn populated_fixture(ns: NsId, count: u64, seed: u64) -> (Keyspace, Rng, Nanos) {
    let mut ks = Keyspace::new(StoreConfig::default());
    let mut rng = Rng(seed);
    let now = Nanos(1_000_000_000);
    for i in 0..count {
        let key = key_of(i);
        let doc = parse(&random_doc(&mut rng));
        ks.db_mut(ns.0 as usize).json_set(&key, &doc, Default::default(), now).expect("set");
    }
    (ks, rng, now)
}

/// Drive one mutation through the bracket exactly as the plane does.
fn bracketed<R>(
    ks: &mut Keyspace,
    ns: NsId,
    keys: &[&[u8]],
    mutate: impl FnOnce(&mut CellStore) -> R,
) -> R {
    ks.idx_bracket_begin(ns, keys, None).expect("reservation headroom");
    let store = ks.db_mut(ns.0 as usize);
    let result = mutate(store);
    ks.idx_bracket_commit(ns, keys);
    result
}

/// One storm step racing the walk: the S04 mutation mix (create,
/// overwrite, delete, string overwrite, TTL set, lazy-expiry probe,
/// active-expiry slice, rename) — every case-table class arises from
/// these under seeded interleaving.
fn storm_step(ks: &mut Keyspace, ns: NsId, rng: &mut Rng, now: &mut Nanos) {
    let key = key_of(rng.below(256));
    let db = ns.0 as usize;
    match rng.below(10) {
        0..=3 => {
            let doc = parse(&random_doc(rng));
            bracketed(ks, ns, &[&key], |s| {
                let _ = s.json_set(&key, &doc, Default::default(), *now);
            });
        }
        4 => {
            bracketed(ks, ns, &[&key], |s| s.del(&key, *now));
        }
        5 => {
            bracketed(ks, ns, &[&key], |s| {
                let _ = s.set(&key, b"plain", SetOptions::default(), *now);
            });
        }
        6 => {
            let doc = parse(&random_doc(rng));
            let at = Nanos(now.0 + 1_000_000 * u64::from(1 + rng.below(20) as u32));
            let opts = inf_store::JsonSetOptions {
                cond: inf_store::SetCond::Always,
                expire: inf_store::SetExpire::At(at),
            };
            bracketed(ks, ns, &[&key], |s| {
                let _ = s.json_set(&key, &doc, opts, *now);
            });
        }
        7 => {
            let _ = ks.db_mut(db).get(&key, *now);
        }
        8 => {
            ks.expire_tick(*now, ExpiryBudget::default());
        }
        _ => {
            let dst = key_of(rng.below(256));
            if dst != key {
                bracketed(ks, ns, &[&key, &dst], |s| {
                    let _ = s.rename(&key, &dst, *now);
                });
            }
        }
    }
    now.0 += 1_000_000;
}

// ---- the from-scratch oracle (S04's, unchanged on purpose) ------------------

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
    let hash = CellStore::hash_key(key);
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

// ---- drivers ----------------------------------------------------------------

fn all_cell_ready(ks: &Keyspace) -> bool {
    INDEXES
        .iter()
        .all(|&(id, ..)| ks.idx_registry().cell_state(IndexId(id)) == Some(IndexState::Ready))
}

/// Ticks until every declared index reports cell `Ready` (panics past
/// `max_ticks` — a hung walk is a failed test, not a hung suite).
fn run_to_ready(ks: &mut Keyspace, now: Nanos, budget: BackfillBudget, max_ticks: u32) -> u32 {
    for tick in 0..max_ticks {
        ks.idx_backfill_tick(now, budget);
        if all_cell_ready(ks) {
            return tick + 1;
        }
    }
    panic!("backfill did not complete within {max_ticks} ticks");
}

// ---- the tests --------------------------------------------------------------

/// Case 1 (the plain backfill): a pre-declaration corpus converges to
/// scan-derived truth, the cell machine reaches `Ready`, and convergence
/// arms the `Strict` asserts (subsequent maintenance runs against them).
#[test]
fn prepopulated_corpus_converges() {
    let ns = NsId(0);
    let (mut ks, mut rng, mut now) = populated_fixture(ns, 512, 0x5EED_0501);
    declare_indexes(&mut ks, ns);
    // Declared → Backfilling at both scopes happens at the first tick.
    let ticks = run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    assert!(ticks > 1, "512 docs under a 256-doc budget is at least two ticks");
    assert_equivalence(&mut ks, ns, now, "after backfill");
    // Convergence armed the Strict asserts: post-walk maintenance (diff
    // removes + death removals) must find its entries — these ops run
    // under debug_asserts in this profile.
    for _ in 0..100 {
        storm_step(&mut ks, ns, &mut rng, &mut now);
    }
    assert_equivalence(&mut ks, ns, now, "post-convergence storm");
}

/// The interleaving storm (S05 AC 2): every case-table class arises from
/// the seeded mutation mix racing one-doc slices; the property is
/// convergence to scan-derived truth at completion, for every seed.
#[test]
fn create_during_storm_converges() {
    for seed in [0x5EED_0502u64, 0xD15C_0503, 0xBEEF_0504] {
        let ns = NsId(0);
        let (mut ks, mut rng, mut now) = populated_fixture(ns, 256, seed);
        declare_indexes(&mut ks, ns);
        // Tiny slices (1 doc) maximize interleaving surface: every storm
        // step lands ahead of or behind the walk position.
        let slice = BackfillBudget { max_docs: 1, max_steps: 4 };
        let mut ticks = 0u32;
        while !all_cell_ready(&ks) {
            storm_step(&mut ks, ns, &mut rng, &mut now);
            ks.idx_backfill_tick(now, slice);
            ticks += 1;
            assert!(ticks < 500_000, "seed {seed:#x}: walk starved");
        }
        assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} at convergence"));
        // And the trees stay converged under continued storm (case 4/6/10
        // post-convergence — the Strict-assert regime).
        for _ in 0..200 {
            storm_step(&mut ks, ns, &mut rng, &mut now);
        }
        assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} post-storm"));
    }
}

/// Case 11 (S05 AC 2's named class): a doubling rehash mid-walk — the
/// reverse-binary enumeration re-emits moved records, insert-if-absent
/// absorbs the duplicates, and the walk still terminates and converges.
#[test]
fn rehash_mid_walk_converges() {
    let ns = NsId(0);
    let (mut ks, mut rng, mut now) = populated_fixture(ns, 128, 0x5EED_0505);
    declare_indexes(&mut ks, ns);
    // A few small slices to plant the cursor mid-table…
    for _ in 0..8 {
        ks.idx_backfill_tick(now, BackfillBudget { max_docs: 8, max_steps: 8 });
    }
    assert!(!all_cell_ready(&ks), "walk must still be mid-table");
    // …then quadruple the corpus so the record index doubles under the
    // cursor (through the bracket — the hook covers the new keys).
    for i in 1000..1512u64 {
        let key = key_of(i);
        let doc = parse(&random_doc(&mut rng));
        bracketed(&mut ks, ns, &[&key], |s| {
            s.json_set(&key, &doc, Default::default(), now).expect("set");
        });
        now.0 += 10_000;
    }
    run_to_ready(&mut ks, now, BackfillBudget::default(), 256);
    assert_equivalence(&mut ks, ns, now, "after mid-walk rehash");
}

/// Cases 9/10: TTL deaths on both sides of the walk position — expired
/// records are reaped on encounter (never inserted), and entries the
/// walk inserted drain when their documents die later. The trees end
/// empty with the corpus.
#[test]
fn expiry_across_the_walk_converges() {
    let ns = NsId(0);
    let mut ks = Keyspace::new(StoreConfig::default());
    let mut rng = Rng(0x77EE_0506);
    let mut now = Nanos(1_000_000_000);
    for i in 0..200u64 {
        let key = key_of(i);
        let doc = parse(&random_doc(&mut rng));
        let at = Nanos(now.0 + 1_000_000 * (1 + rng.below(50)));
        let opts = inf_store::JsonSetOptions {
            cond: inf_store::SetCond::Always,
            expire: inf_store::SetExpire::At(at),
        };
        ks.db_mut(0).json_set(&key, &doc, opts, now).expect("set");
    }
    declare_indexes(&mut ks, ns);
    // Advance the clock while walking so deadlines fire mid-walk: some
    // records are already dead when the walk arrives (case 9), some die
    // after it scanned them (case 10 — the death hook must find the
    // walk's entries once converged).
    let slice = BackfillBudget { max_docs: 4, max_steps: 8 };
    let mut ticks = 0u32;
    while !all_cell_ready(&ks) {
        now.0 += 1_000_000;
        ks.expire_tick(now, ExpiryBudget::default());
        ks.idx_backfill_tick(now, slice);
        ticks += 1;
        assert!(ticks < 100_000, "walk starved");
    }
    assert_equivalence(&mut ks, ns, now, "at convergence under expiry");
    // Drain every remaining deadline: the trees empty with the corpus.
    now.0 += 60_000_000;
    for _ in 0..30 {
        ks.expire_tick(now, ExpiryBudget::default());
    }
    for &(id, ..) in INDEXES {
        assert!(
            tree_entries(&ks, ns, IndexId(id)).is_empty(),
            "index {id} drained with its documents"
        );
    }
}

/// Case 12: eviction racing the walk — victims on both sides of the
/// cursor run the death hook, and the property holds at convergence.
#[test]
fn eviction_during_backfill_converges() {
    let ns = NsId(0);
    let (mut ks, rng, mut now) = populated_fixture(ns, 400, 0x0E1C_0507);
    declare_indexes(&mut ks, ns);
    let used = ks.used_bytes();
    ks.set_pressure(PressureConfig {
        limit_bytes: used / 2,
        policy: EvictionPolicy::AllKeysRandom,
        samples: 5,
    });
    let slice = BackfillBudget { max_docs: 16, max_steps: 32 };
    let mut ticks = 0u32;
    while !all_cell_ready(&ks) {
        ks.evict_tick(now, EvictBudget { max_evictions: 8 });
        ks.idx_backfill_tick(now, slice);
        now.0 += 1_000_000;
        ticks += 1;
        assert!(ticks < 100_000, "walk starved");
    }
    // Continue evicting after convergence (case 12 under Strict).
    for _ in 0..20 {
        ks.evict_tick(now, EvictBudget { max_evictions: 8 });
        now.0 += 1_000_000;
    }
    assert_equivalence(&mut ks, ns, now, "under eviction");
    let _ = rng;
}

/// The restart regression (S05 AC 3's keyspace-level leg; ADR-0075 D4 +
/// ADR-0077 D2): a "boot" mid-walk regresses every index to
/// `backfilling`, the binding gate refuses typed while rebuilding, the
/// walk restarts from zero (never resumes a stale cursor), and the
/// rebuilt fleet re-reports ready with oracle-verified contents. The
/// power-cut/full-node leg is the `m45-backfill` inf-sim scenario.
#[test]
fn restart_regresses_and_rebuilds_cleanly() {
    let ns = NsId(0);
    let (mut ks, _rng, now) = populated_fixture(ns, 300, 0x5EED_0508);
    declare_indexes(&mut ks, ns);
    // Partial walk, then "crash": seed_catalog re-seeds registries the
    // way boot does (default-db contents survive in place of replay).
    for _ in 0..4 {
        ks.idx_backfill_tick(now, BackfillBudget { max_docs: 16, max_steps: 16 });
    }
    assert!(!all_cell_ready(&ks), "mid-walk by construction");
    let scanned_before: u64 = ks.idx_backfill_progress().iter().map(|p| p.docs_scanned).sum();
    assert!(scanned_before > 0, "the walk had progress to lose");
    let catalog = ks.export_catalog(17, 100, 100);
    ks.seed_catalog(&catalog).expect("seed");
    // Regression: every state is backfilling, no job survives, binding
    // refuses typed — partial results are unrepresentable.
    for &(id, ..) in INDEXES {
        assert_eq!(ks.idx_registry().cell_state(IndexId(id)), Some(IndexState::Backfilling));
        assert!(matches!(
            ks.idx_registry().validate_binding(ns, IndexId(id), u64::from(id)),
            Err(inf_store::IndexBindError::NotReady(_))
        ));
        assert!(tree_entries(&ks, ns, IndexId(id)).is_empty(), "attach trees reset at seed");
    }
    assert!(ks.idx_backfill_progress().is_empty(), "no watermark survives a boot (ADR-0077 D2)");
    // The rebuild walks from zero and converges.
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    assert_equivalence(&mut ks, ns, now, "after restart rebuild");
    // Pre-crash-ready indexes carry the sidecar-eligibility hint (S06
    // reads it; the walk still rebuilt everything this milestone).
    let ready_catalog = {
        for &(id, ..) in INDEXES {
            ks.idx_registry_mut().set_catalog_state(IndexId(id), IndexState::Ready).expect("edge");
        }
        ks.export_catalog(17, 100, 100)
    };
    ks.seed_catalog(&ready_catalog).expect("seed");
    for &(id, ..) in INDEXES {
        assert_eq!(ks.idx_registry().cell_state(IndexId(id)), Some(IndexState::Backfilling));
        assert_eq!(ks.idx_registry().was_ready(IndexId(id)), Some(true), "the D4 hint");
    }
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    assert_equivalence(&mut ks, ns, now, "after ready-regression rebuild");
}

/// ADR-0077 D7 + the fault point: a planted headroom trip mid-walk
/// degrades the index, parks the build, and never reports ready; the
/// untripped indexes complete normally; rebuild resets the parked one
/// and it converges.
#[test]
fn planted_backfill_trip_parks_and_rebuild_recovers() {
    use inf_foundation::fault::{self, FaultSpec};
    let ns = NsId(0);
    let (mut ks, _rng, now) = populated_fixture(ns, 64, 0x5EED_0509);
    declare_indexes(&mut ks, ns);
    fault::arm(inf_store::fault::IDX_BACKFILL_TRIP, FaultSpec::Nth(1));
    for _ in 0..64 {
        ks.idx_backfill_tick(now, BackfillBudget::default());
    }
    fault::disarm(inf_store::fault::IDX_BACKFILL_TRIP);
    // Exactly one build tripped (the fault fires once): it is parked +
    // degraded + not ready; the other three converged.
    let progress = ks.idx_backfill_progress();
    let parked: Vec<_> = progress.iter().filter(|p| p.phase == BackfillPhase::Parked).collect();
    assert_eq!(parked.len(), 1, "one planted trip parks one build");
    let victim = parked[0].id;
    assert_eq!(ks.idx_degraded(ns, victim), Some(true), "parked ⇒ degraded veto");
    assert_ne!(ks.idx_registry().cell_state(victim), Some(IndexState::Ready));
    let ready = INDEXES
        .iter()
        .filter(|&&(id, ..)| ks.idx_registry().cell_state(IndexId(id)) == Some(IndexState::Ready))
        .count();
    assert_eq!(ready, INDEXES.len() - 1, "unaffected builds complete");
    assert!(ks.idx_backfill_info().parked >= 1, "the park is visible in INFO");
    // Rebuild is the recovery path: generation bump, veto cleared, the
    // fresh job walks from zero and converges.
    ks.idx_registry_mut().set_catalog_state(victim, IndexState::Ready).expect("edge");
    ks.idx_rebuild(victim, 100).expect("rebuild");
    assert_eq!(ks.idx_degraded(ns, victim), Some(false));
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    assert_equivalence(&mut ks, ns, now, "after rebuild of the parked index");
}

/// ADR-0077 D7's other class: a pre-declaration document whose wildcard
/// matches exceed the eval cap cannot be indexed whole — the build
/// degrades typed instead of serving a partial projection.
#[test]
fn eval_overflow_doc_degrades_the_build() {
    let ns = NsId(0);
    let cfg = StoreConfig { doc_max_path_matches: 4, ..StoreConfig::default() };
    let mut ks = Keyspace::new(cfg);
    let now = Nanos(1_000_000_000);
    // Six tags > the 4-match cap; written before any index exists.
    let flood = parse(r#"{"price":1.5,"name":"f","qty":1,"tags":["a","b","c","d","e","f"]}"#);
    ks.db_mut(0).json_set(b"doc:flood", &flood, Default::default(), now).expect("set");
    declare_indexes(&mut ks, ns);
    for _ in 0..32 {
        ks.idx_backfill_tick(now, BackfillBudget::default());
    }
    // The wildcard index degraded + parked; the chain indexes converged
    // (their evals never exceed one match).
    assert_eq!(ks.idx_degraded(ns, IndexId(4)), Some(true), "flood doc degrades $.tags[*]");
    assert_ne!(ks.idx_registry().cell_state(IndexId(4)), Some(IndexState::Ready));
    assert!(ks.idx_counters(ns, IndexId(4)).expect("attached").degraded_trips >= 1);
    for &(id, ..) in &INDEXES[..3] {
        assert_eq!(ks.idx_registry().cell_state(IndexId(id)), Some(IndexState::Ready));
    }
}

/// ADR-0077 D5/D6 surfaces: slots are id-rank, ready reports carry the
/// exact generation, fleet candidates are the backfilling catalog
/// entries, and a drop shifts later ranks without inventing state.
#[test]
fn slots_reports_and_candidates_track_the_registry() {
    let ns = NsId(0);
    let (mut ks, _rng, now) = populated_fixture(ns, 32, 0x5EED_050A);
    declare_indexes(&mut ks, ns);
    for (rank, &(id, ..)) in INDEXES.iter().enumerate() {
        assert_eq!(ks.idx_slot_of(IndexId(id)), Some(rank), "rank = id order");
    }
    assert!(ks.idx_ready_reports().is_empty(), "nothing ready before the walk");
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    let mut reports = ks.idx_ready_reports();
    reports.sort_unstable();
    assert_eq!(reports, vec![(0, 1), (1, 2), (2, 3), (3, 4)], "slot → generation");
    let candidates = ks.idx_fleet_candidates();
    assert_eq!(candidates.len(), INDEXES.len(), "catalog still backfilling pre-flip");
    // The flip (the plane's half, driven manually here) retires the
    // candidates and the published rows.
    for &(id, ..) in INDEXES {
        ks.idx_registry_mut().set_catalog_state(IndexId(id), IndexState::Ready).expect("edge");
    }
    ks.idx_backfill_tick(now, BackfillBudget::default());
    assert!(ks.idx_fleet_candidates().is_empty());
    assert!(ks.idx_backfill_progress().is_empty(), "published rows retire on the flip");
    assert_eq!(ks.idx_ready_reports().len(), INDEXES.len(), "republication continues");
    // Dropping index 2 shifts later ranks; generation-exact matching
    // keeps stale board words harmless (ADR-0077 D5 — asserted at the
    // board level in inf-server's tests).
    ks.idx_registry_mut().set_catalog_state(IndexId(2), IndexState::Dropping).expect("edge");
    ks.idx_drop_finish(IndexId(2)).expect("drop");
    assert_eq!(ks.idx_slot_of(IndexId(1)), Some(0));
    assert_eq!(ks.idx_slot_of(IndexId(3)), Some(1), "rank shifted down");
    assert_eq!(ks.idx_slot_of(IndexId(4)), Some(2));
    assert_eq!(ks.idx_slot_of(IndexId(2)), None, "dropped id has no slot");
}

/// An index on an empty namespace converges in one tick (ADR-0077 D4
/// materializes the store so the converged flag has a home), and a
/// mid-walk `FLUSH` empties the corpus out from under the walk without
/// breaking convergence.
#[test]
fn empty_and_flushed_namespaces_converge() {
    let ns = NsId(3);
    let mut ks = Keyspace::new(StoreConfig::default());
    let now = Nanos(1_000_000_000);
    declare_indexes(&mut ks, ns);
    run_to_ready(&mut ks, now, BackfillBudget::default(), 16);
    for &(id, ..) in INDEXES {
        assert!(tree_entries(&ks, ns, IndexId(id)).is_empty());
    }
    // FLUSH mid-walk on a fresh build: re-declare on a populated db,
    // walk part way, flush, finish — trees equal the (empty) truth.
    let ns2 = NsId(5);
    for i in 0..128u64 {
        let key = key_of(i);
        let doc = parse(r#"{"price":1.5,"name":"x","qty":1,"tags":["a"]}"#);
        ks.db_mut(5).json_set(&key, &doc, Default::default(), now).expect("set");
    }
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
    for _ in 0..3 {
        ks.idx_backfill_tick(now, BackfillBudget { max_docs: 8, max_steps: 8 });
    }
    ks.db_mut(5).flush(now);
    for _ in 0..64 {
        ks.idx_backfill_tick(now, BackfillBudget::default());
        if INDEXES.iter().all(|&(id, ..)| {
            ks.idx_registry().cell_state(IndexId(id + 10)) == Some(IndexState::Ready)
        }) {
            break;
        }
    }
    for &(id, ..) in INDEXES {
        assert!(
            tree_entries(&ks, ns2, IndexId(id + 10)).is_empty(),
            "flushed corpus ⇒ empty converged tree"
        );
        assert_eq!(ks.idx_registry().cell_state(IndexId(id + 10)), Some(IndexState::Ready));
    }
}

/// Rebuild mid-fleet (ADR-0077 D2's generation rule): a rebuild bumps
/// the generation, the old job dies with its watermark, and the fresh
/// walk converges at the new generation only.
#[test]
fn rebuild_resets_the_job_at_the_new_generation() {
    let ns = NsId(0);
    let (mut ks, _rng, now) = populated_fixture(ns, 200, 0x5EED_050B);
    declare_indexes(&mut ks, ns);
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
    ks.idx_rebuild(IndexId(1), 50).expect("rebuild");
    assert!(tree_entries(&ks, ns, IndexId(1)).is_empty(), "rebuild resets the tree");
    // Mid-rebuild, bump again (drop + re-create class): the half-walked
    // job at generation 50 must die, never leaking its cursor into 51.
    for _ in 0..2 {
        ks.idx_backfill_tick(now, BackfillBudget { max_docs: 8, max_steps: 8 });
    }
    ks.idx_registry_mut().set_catalog_state(IndexId(1), IndexState::Ready).expect("edge");
    ks.idx_rebuild(IndexId(1), 51).expect("second rebuild");
    run_to_ready(&mut ks, now, BackfillBudget::default(), 64);
    let spec = ks.idx_registry().get_by_id(IndexId(1)).expect("entry");
    assert_eq!(spec.generation, 51);
    assert_eq!(
        ks.idx_ready_reports().iter().find(|&&(slot, _)| slot == 0),
        Some(&(0usize, 51u64)),
        "the report carries the rebuilt generation"
    );
    assert_equivalence(&mut ks, ns, now, "after double rebuild");
}

/// ADR-0077 D8: progress rows and the INFO fold move with the walk.
#[test]
fn progress_and_info_track_the_walk() {
    let ns = NsId(0);
    let (mut ks, _rng, now) = populated_fixture(ns, 100, 0x5EED_050C);
    declare_indexes(&mut ks, ns);
    ks.idx_backfill_tick(now, BackfillBudget { max_docs: 10, max_steps: 16 });
    let progress = ks.idx_backfill_progress();
    assert_eq!(progress.len(), INDEXES.len(), "one job per declaration");
    assert!(progress.iter().all(|p| p.phase == BackfillPhase::Walking));
    assert!(progress.iter().any(|p| p.docs_scanned > 0), "the front job advanced");
    let info = ks.idx_backfill_info();
    assert_eq!(info.walking, INDEXES.len() as u32);
    assert!(info.docs_scanned_total > 0);
    run_to_ready(&mut ks, now, BackfillBudget::default(), 128);
    let info = ks.idx_backfill_info();
    assert_eq!(info.walking, 0);
    assert_eq!(info.published, INDEXES.len() as u32, "published rows await the fleet flip");
    assert!(info.docs_scanned_total >= 100 * INDEXES.len() as u64, "every job walked the corpus");
}

/// The 10⁶-op release storm (the `ordered_storm` precedent): backfill
/// racing the full mutation mix, two seeds, oracle at convergence and
/// after a post-convergence storm tail.
#[test]
fn backfill_storm_million() {
    if cfg!(debug_assertions) {
        // The debug lane runs the seeded storms above; the million-op
        // count is the release storm's job (`--release`, CI storm lane).
        return;
    }
    for seed in [0x5EED_0510u64, 0xD15C_0511] {
        let ns = NsId(0);
        let (mut ks, mut rng, mut now) = populated_fixture(ns, 2048, seed);
        declare_indexes(&mut ks, ns);
        let slice = BackfillBudget { max_docs: 2, max_steps: 4 };
        let mut steps = 0u64;
        while !all_cell_ready(&ks) {
            storm_step(&mut ks, ns, &mut rng, &mut now);
            ks.idx_backfill_tick(now, slice);
            steps += 1;
            assert!(steps < 10_000_000, "seed {seed:#x}: walk starved");
        }
        assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} at convergence"));
        for step in 0..1_000_000u32 {
            storm_step(&mut ks, ns, &mut rng, &mut now);
            if step % 65_536 == 0 {
                assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} step {step}"));
            }
        }
        assert_equivalence(&mut ks, ns, now, &format!("seed {seed:#x} end"));
    }
}
