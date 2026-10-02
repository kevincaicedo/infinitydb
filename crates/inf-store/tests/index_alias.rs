#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
//! ARCH-W0.2 — index entry identity under a 64-bit hash alias (ADR-0139
//! D2, D4, D9, D12; record `docs/drr/ARCH-W0.2.md` §6).
//!
//! Every collision here is **real**: distinct 48-byte keys under the
//! `{shadow-collide}` hashtag hash their first 32 bytes only
//! (`collision-oracle`, ADR-0094 D3), so they share one `PkRef` and —
//! when their indexed values are equal — one tree entry.
//!
//! The oracle is a hand-kept model `pk → value`: the expected tree is
//! `{(enc(value), hash(pk))}` over it. It shares the keyed hasher (it
//! must: the ref is that hash) and `index_key_encode` (proven by the S02
//! golden vectors), and nothing of `index_maint`.

#![cfg(feature = "doc")]

#[path = "../../../tests/crash-matrix/receipt.rs"]
mod receipt;

use std::collections::{BTreeMap, BTreeSet};

use inf_doc::path::compile;
use inf_doc::{CanonicalDoc, JsonParser};
use inf_foundation::fault::{self, FaultSpec};
use inf_foundation::time::Nanos;
use inf_store::limits::{BRACKET_KEY_BYTES_MAX, IDX_ALIAS_GROUP_MAX, IDX_ALIAS_REHASH_MAX};
use inf_store::{
    ArenaConfig, COLLISION_KEY_PREFIX, CellStore, EvictionPolicy, IdxMaintRefusal, IndexId,
    IndexKeyBuf, IndexKeyType, IndexScalar, IndexSpec, IndexState, JsonSetOptions, KeyHasher,
    Keyspace, NsId, OrderedCursor, PkRef, PressureConfig, SetCond, SetExpire, SetOptions,
    StoreConfig, index_key_encode,
};

const NS: NsId = NsId(0);
const IDX_V: IndexId = IndexId(1);
const T0: Nanos = Nanos(1_000_000_000);

/// One member of the alias group `tag`: the hashed 32-byte head is a
/// function of `tag` alone, the unhashed 16-byte tail is `member`.
fn alias_key(tag: u64, member: u8) -> [u8; 48] {
    let mut out = [0u8; 48];
    out[..16].copy_from_slice(COLLISION_KEY_PREFIX);
    out[16..24].copy_from_slice(&tag.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    out[24..32].copy_from_slice(&tag.to_le_bytes());
    out[32..].copy_from_slice(&[member; 16]);
    out
}

fn hash_of(key: &[u8]) -> u64 {
    KeyHasher::default().hash(key)
}

fn parse(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("valid test JSON")
}

fn declare(ks: &mut Keyspace, id: IndexId, path: &str, key_type: IndexKeyType) {
    let program = compile(path.as_bytes()).expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id,
        generation: u64::from(id.0),
        ns: NS,
        name: format!("idx-{}", id.0).into_bytes(),
        program,
        key_type,
        state: IndexState::Declared,
    })
    .expect("declare");
}

/// One `i64` index on `$.v`, converged so the `Strict` found/fresh
/// asserts are live ("`Strict` silent" is part of every row's verdict).
fn fixture_with(config: StoreConfig) -> Keyspace {
    let mut ks = Keyspace::new(config);
    declare(&mut ks, IDX_V, "$.v", IndexKeyType::I64);
    ks.db_mut(0);
    ks.idx_set_converged(NS, IDX_V, true);
    ks
}

fn fixture() -> Keyspace {
    fixture_with(StoreConfig::default())
}

/// The plane's bracket order (ADR-0139 step table): pre-half → gate →
/// body → commit-half. `gate` is where the inline eviction runs.
fn bracketed<R>(
    ks: &mut Keyspace,
    keys: &[&[u8]],
    path: Option<&str>,
    gate: impl FnOnce(&mut Keyspace),
    body: impl FnOnce(&mut CellStore) -> R,
) -> R {
    let program = path.map(|p| compile(p.as_bytes()).expect("valid path"));
    ks.idx_bracket_begin(NS, keys, program.as_ref()).expect("pre-half admits");
    gate(ks);
    let result = body(ks.db_mut(0));
    ks.idx_bracket_commit(NS, keys);
    result
}

fn no_gate(_: &mut Keyspace) {}

fn put(ks: &mut Keyspace, key: &[u8], json: &str, expire: SetExpire) {
    let idoc = parse(json);
    bracketed(ks, &[key], None, no_gate, |store| {
        let opts = JsonSetOptions { expire, ..JsonSetOptions::default() };
        store
            .json_set(key, &CanonicalDoc::validate(&idoc).expect("canonical fixture"), opts, T0)
            .expect("json_set");
    });
}

fn put_v(ks: &mut Keyspace, key: &[u8], v: i64) {
    put(ks, key, &format!(r#"{{"v":{v},"w":1}}"#), SetExpire::Keep);
}

fn put_v_ttl(ks: &mut Keyspace, key: &[u8], v: i64, deadline_ms: u64) {
    let at = SetExpire::At(Nanos(deadline_ms * 1_000_000));
    put(ks, key, &format!(r#"{{"v":{v},"w":1}}"#), at);
}

fn tree_entries(ks: &Keyspace, id: IndexId) -> BTreeSet<(Vec<u8>, PkRef)> {
    let mut out = BTreeSet::new();
    let Some(tree) = ks.idx_tree(NS, id) else { return out };
    let mut cursor = OrderedCursor::from_start();
    while let Some((key, entry_ref)) = tree.cursor_next(&mut cursor) {
        out.insert((key.to_vec(), entry_ref));
    }
    out
}

/// The model's tree: `{(enc(v), hash(pk))}` — a set, so two aliases
/// holding one value are one entry (ADR-0139 D2 rule 1).
fn expected_i64(model: &BTreeMap<Vec<u8>, i64>) -> BTreeSet<(Vec<u8>, PkRef)> {
    let mut buf = IndexKeyBuf::new();
    model
        .iter()
        .map(|(pk, v)| {
            index_key_encode(IndexKeyType::I64, IndexScalar::I64(*v), &mut buf).expect("i64");
            (buf.as_bytes().to_vec(), PkRef::from_key_hash(hash_of(pk)))
        })
        .collect()
}

fn model_of(rows: &[(&[u8], i64)]) -> BTreeMap<Vec<u8>, i64> {
    rows.iter().map(|(k, v)| (k.to_vec(), *v)).collect()
}

/// The OOM gate under `volatile-ttl`: the nearest deadline dies first,
/// so a test names the victim order with two deadlines. A 1-byte limit
/// is never satisfied — every volatile record dies and the verdict is
/// the honest `-OOM`; the pressure is lifted again so the body can run.
fn gate_evict_all_volatile(ks: &mut Keyspace) {
    ks.set_pressure(PressureConfig {
        limit_bytes: 1,
        policy: EvictionPolicy::VolatileTtl,
        samples: 5,
    });
    let verdict = ks.free_for_write(T0);
    assert!(verdict.is_err(), "a 1-byte limit is unfreeable: the gate answers -OOM");
    ks.set_pressure(PressureConfig {
        limit_bytes: 0,
        policy: EvictionPolicy::NoEviction,
        samples: 5,
    });
}

// ---- ADR-0139 proof 1–2: the alias pair ------------------------------------

/// Two aliases hold the same indexed value, so they share one entry.
/// Deleting one must leave the survivor indexed (rule 2): the entry is a
/// fact about the group, not about the dying document.
#[test]
fn alias_pair_equal_value_delete_one_keeps_the_survivor_indexed() {
    let (a, b) = (alias_key(1, b'a'), alias_key(1, b'b'));
    assert_eq!(hash_of(&a), hash_of(&b), "the oracle collides the pair");
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    assert_eq!(tree_entries(&ks, IDX_V).len(), 1, "one entry for the group");
    bracketed(&mut ks, &[&a], None, no_gate, |store| assert!(store.del(&a, T0)));
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7)])));
    // The last holder's death removes it.
    bracketed(&mut ks, &[&b], None, no_gate, |store| assert!(store.del(&b, T0)));
    assert!(tree_entries(&ks, IDX_V).is_empty());
}

/// The same pair through every other way a holder stops holding:
/// overwrite to another value, wheel expiry, lazy expiry, eviction.
#[test]
fn alias_pair_equal_value_survives_every_death_shape() {
    let (a, b) = (alias_key(2, b'a'), alias_key(2, b'b'));
    // Overwrite `a` to another value: `(7, h)` stays for `b`.
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    put_v(&mut ks, &a, 9);
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&a, 9), (&b, 7)])));

    // Wheel expiry of `a` (the MAINTAIN reap — no bracket).
    let mut ks = fixture();
    put_v_ttl(&mut ks, &a, 7, 2_000);
    put_v(&mut ks, &b, 7);
    let later = Nanos(10_000 * 1_000_000);
    let reaped = ks.expire_tick(later, inf_store::ExpiryBudget::default()).reaped;
    assert_eq!(reaped, 1, "the wheel reaped the expired alias");
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7)])));

    // Lazy expiry of `a`: a read reaps it outside any bracket.
    let mut ks = fixture();
    put_v_ttl(&mut ks, &a, 7, 2_000);
    put_v(&mut ks, &b, 7);
    assert!(ks.db_mut(0).json_get(&a, later).expect("read").is_none());
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7)])));

    // Eviction of `a` (volatile: `b` has no TTL and cannot be a victim).
    let mut ks = fixture();
    put_v_ttl(&mut ks, &a, 7, 900_000);
    put_v(&mut ks, &b, 7);
    gate_evict_all_volatile(&mut ks);
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7)])));
}

/// Different values: two entries under one ref; only the dying
/// document's goes.
#[test]
fn alias_pair_different_values_delete_removes_only_its_entry() {
    let (a, b) = (alias_key(3, b'a'), alias_key(3, b'b'));
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 8);
    bracketed(&mut ks, &[&a], None, no_gate, |store| assert!(store.del(&a, T0)));
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 8)])));
}

// ---- ADR-0139 proof 3: gate deaths inside an open bracket ------------------

#[derive(Copy, Clone, Debug)]
enum GateBody {
    SameValue,
    ChangedValue,
    RefusedOom,
}

/// Write set `{A}`, alias `B`, equal value; the gate evicts both, in the
/// order the deadlines name. The body then rewrites `A`, changes it, or
/// never runs (`-OOM`). In all six the tree equals the model.
#[test]
fn gate_deaths_inside_a_bracket_converge_in_both_orders() {
    for a_first in [true, false] {
        for body in [GateBody::SameValue, GateBody::ChangedValue, GateBody::RefusedOom] {
            let (a, b) = (alias_key(4, b'a'), alias_key(4, b'b'));
            let mut ks = fixture();
            let (a_deadline, b_deadline) =
                if a_first { (500_000, 900_000) } else { (900_000, 500_000) };
            put_v_ttl(&mut ks, &a, 7, a_deadline);
            put_v_ttl(&mut ks, &b, 7, b_deadline);
            let next = match body {
                GateBody::SameValue => Some(7),
                GateBody::ChangedValue => Some(9),
                GateBody::RefusedOom => None,
            };
            bracketed(&mut ks, &[&a], None, gate_evict_all_volatile, |store| {
                if let Some(v) = next {
                    let idoc = parse(&format!(r#"{{"v":{v},"w":1}}"#));
                    store
                        .json_set(
                            &a,
                            &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                            JsonSetOptions::default(),
                            T0,
                        )
                        .expect("json_set");
                }
            });
            let model = next.map_or_else(BTreeMap::new, |v| model_of(&[(&a, v)]));
            assert_eq!(
                tree_entries(&ks, IDX_V),
                expected_i64(&model),
                "a_first = {a_first}, body = {body:?}"
            );
        }
    }
}

// ---- ADR-0139 proof 4: the prune siblings (no alias needed) ----------------

type PrunedBody = fn(&mut CellStore, &[u8], Nanos) -> bool;

/// Every prunable verb (`JSON.SET` at a path, `NUMINCRBY`, `NUMMULTBY`,
/// `DEL`/`FORGET` at a path, `TOGGLE`, `CLEAR`, `ARRPOP`, `ARRAPPEND`,
/// `ARRINSERT`, `ARRTRIM`, `MERGE`, `STRAPPEND`) reaches the store through
/// one of these funnels; each returns whether it found the document.
const PRUNED_FUNNELS: [(&str, PrunedBody); 5] = [
    ("json_replace", |store, key, now| {
        store
            .json_replace(
                key,
                &CanonicalDoc::validate(&parse(r#"{"v":7,"w":2}"#)).expect("canonical fixture"),
                now,
            )
            .expect("replace")
    }),
    ("json_patch_scalar", |store, key, now| {
        let path = compile(b"$.w").expect("valid path");
        let op = inf_doc::ApplyOp::SetReplace { fragment: b"2" };
        store.json_patch_scalar(key, &path, &op, now).expect("patch").is_some()
    }),
    ("json_morph", |store, key, now| store.json_morph(key, now).expect("morph")),
    ("json_edit_tree", |store, key, now| {
        store.json_edit_tree(key, now, |_, _| Ok(())).expect("edit").is_some()
    }),
    // The read half of a read-modify-write verb.
    ("json_get", |store, key, now| store.json_get(key, now).expect("read").is_some()),
];

/// A non-root write at `$.w` prunes the `$.v` index: neither half
/// evaluates it. (a) The document is evicted at the gate. The body then
/// fails on the missing key; no entry may survive the document.
#[test]
fn pruned_bracket_whose_document_is_evicted_at_the_gate_leaves_no_entry() {
    for (name, body) in PRUNED_FUNNELS {
        let key = b"doc:pruned-evicted";
        let mut ks = fixture();
        put_v_ttl(&mut ks, key, 7, 900_000);
        let found = bracketed(&mut ks, &[key], Some("$.w"), gate_evict_all_volatile, |store| {
            body(store, key, T0)
        });
        assert!(!found, "{name}: the body fails on the missing key");
        assert!(tree_entries(&ks, IDX_V).is_empty(), "{name}: the dead document's entry left");
    }
}

/// (b) No eviction at all: the document's TTL has passed, and the body's
/// own lookup reaps it — a death inside the bracket, under the prune.
/// Each funnel's lookup is that reap.
#[test]
fn pruned_bracket_whose_document_the_body_reaps_leaves_no_entry() {
    let later = Nanos(10_000 * 1_000_000);
    for (name, body) in PRUNED_FUNNELS {
        let key = b"doc:pruned-expired";
        let mut ks = fixture();
        put_v_ttl(&mut ks, key, 7, 2_000);
        let found =
            bracketed(&mut ks, &[key], Some("$.w"), no_gate, |store| body(store, key, later));
        assert!(!found, "{name}: the body's lookup reaped the expired document");
        assert!(tree_entries(&ks, IDX_V).is_empty(), "{name}: the dead document's entry left");
        assert_eq!(ks.idx_counters_total().prune_void, 1, "{name}: the prune was voided");
    }
}

// ---- ADR-0139 D3: `COPY` onto a dead target --------------------------------

/// The `COPY` mini-bracket resolves its target inside the bracket: an
/// expired indexed target is reaped there. A later error must not drop
/// the bracket's `old` — the target's entries leave through the diff.
#[test]
fn copy_refused_after_reaping_its_expired_target_removes_the_targets_entries() {
    // A document arena that holds the big source once, never twice: the
    // copy's allocation fails after the target resolved.
    let doc_arena = ArenaConfig { chunk_size: 64 << 10, max_resident: Some(64 << 10) };
    let mut ks = fixture_with(StoreConfig { doc_arena, ..StoreConfig::default() });
    let target = b"doc:target";
    put_v_ttl(&mut ks, target, 7, 2_000);
    let filler = "x".repeat(40 << 10);
    put(&mut ks, b"doc:big", &format!(r#"{{"v":1,"pad":"{filler}"}}"#), SetExpire::Keep);
    let later = Nanos(10_000 * 1_000_000);
    let result = ks.db_mut(0).copy(b"doc:big", target, true, later);
    assert!(result.is_err(), "the copy is refused after the target resolve: {result:?}");
    assert_eq!(
        tree_entries(&ks, IDX_V),
        expected_i64(&model_of(&[(b"doc:big".as_slice(), 1)])),
        "the reaped target's entry left the tree"
    );
}

/// The cross-database form: `Keyspace::copy_between` opens its own
/// mini-bracket on the **destination** store. Database 1's document
/// arena is full, so the import fails after its target resolve reaped
/// the expired indexed target — whose entry must still leave db 1's tree.
#[test]
fn cross_db_copy_refused_after_reaping_its_expired_target_removes_the_targets_entries() {
    const DB1: NsId = NsId(1);
    const IDX_DB1: IndexId = IndexId(3);
    let doc_arena = ArenaConfig { chunk_size: 64 << 10, max_resident: Some(64 << 10) };
    let mut ks = fixture_with(StoreConfig { doc_arena, ..StoreConfig::default() });
    let program = compile(b"$.v").expect("valid path").as_bytes().to_vec();
    ks.idx_create(IndexSpec {
        id: IDX_DB1,
        generation: 3,
        ns: DB1,
        name: b"idx-db1".to_vec(),
        program,
        key_type: IndexKeyType::I64,
        state: IndexState::Declared,
    })
    .expect("declare on db 1");
    ks.db_mut(1);
    ks.idx_set_converged(DB1, IDX_DB1, true);
    let put_db1 = |ks: &mut Keyspace, key: &[u8], json: &str, expire: SetExpire| {
        let idoc = parse(json);
        ks.idx_bracket_begin(DB1, &[key], None).expect("pre-half admits");
        let opts = JsonSetOptions { expire, ..JsonSetOptions::default() };
        ks.db_mut(1)
            .json_set(key, &CanonicalDoc::validate(&idoc).expect("canonical fixture"), opts, T0)
            .expect("json_set");
        ks.idx_bracket_commit(DB1, &[key]);
    };
    let filler = "x".repeat(40 << 10);
    let big = format!(r#"{{"v":1,"pad":"{filler}"}}"#);
    let target = b"doc:target";
    put_db1(&mut ks, target, r#"{"v":7,"w":1}"#, SetExpire::At(Nanos(2_000 * 1_000_000)));
    put_db1(&mut ks, b"doc:pad", &big.replacen("1", "2", 1), SetExpire::Keep);
    put(&mut ks, b"doc:big", &big, SetExpire::Keep);

    let later = Nanos(10_000 * 1_000_000);
    let result = ks.copy_between(0, b"doc:big", 1, target, true, later);
    assert!(result.is_err(), "db 1 cannot hold the import: {result:?}");
    assert!(ks.db_mut(1).json_get(target, later).expect("read").is_none(), "the target is gone");
    let tree = ks.idx_tree(DB1, IDX_DB1).expect("db 1's tree");
    let mut cursor = OrderedCursor::from_start();
    let mut left = Vec::new();
    while let Some((_, entry_ref)) = tree.cursor_next(&mut cursor) {
        left.push(entry_ref);
    }
    assert_eq!(left, [PkRef::from_key_hash(hash_of(b"doc:pad"))], "only the pad's entry stays");
}

// ---- ADR-0139 D12: the participating set is complete at open ---------------

/// Three wildcard indexes; a **create** whose post-image floods the
/// second. Every index the bracket can touch degrades — the third one
/// included, which otherwise stays "healthy" while lacking the document.
#[test]
fn a_create_that_floods_one_index_degrades_every_participating_index() {
    let config = StoreConfig { doc_max_path_matches: 8, ..StoreConfig::default() };
    let mut ks = Keyspace::new(config);
    let ids = [IndexId(1), IndexId(2), IndexId(3)];
    declare(&mut ks, ids[0], "$.a[*]", IndexKeyType::I64);
    declare(&mut ks, ids[1], "$.b[*]", IndexKeyType::I64);
    declare(&mut ks, ids[2], "$.c[*]", IndexKeyType::I64);
    ks.db_mut(0);
    let key = b"doc:flood";
    let idoc = parse(r#"{"a":[1],"b":[1,2,3,4,5,6,7,8,9,10],"c":[1]}"#);
    bracketed(&mut ks, &[key], None, no_gate, |store| {
        store
            .json_set(
                key,
                &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                JsonSetOptions::default(),
                T0,
            )
            .expect("json_set");
    });
    for id in ids {
        assert_eq!(ks.idx_degraded(NS, id), Some(true), "index {} degraded", id.0);
    }
    assert_eq!(ks.idx_counters_total().degraded_trips, 3);
}

// ---- ADR-0139 D9: the held mark beside duplicates --------------------------

/// `DEL a a` puts two equal `old` entries beside an alias that holds the
/// same value: the diff keeps a run's last, so the held mark must be on
/// every entry of the run.
#[test]
fn held_mark_covers_a_duplicated_write_set_key() {
    let (a, b) = (alias_key(5, b'a'), alias_key(5, b'b'));
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    bracketed(&mut ks, &[&a, &a], None, no_gate, |store| {
        assert!(store.del(&a, T0));
        assert!(!store.del(&a, T0));
    });
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7)])));
    assert_eq!(ks.idx_counters_total().alias_kept, 1);
}

const IDX_TAGS: IndexId = IndexId(2);

fn tags_fixture() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    declare(&mut ks, IDX_TAGS, "$.tags[*]", IndexKeyType::I64);
    ks.db_mut(0);
    ks.idx_set_converged(NS, IDX_TAGS, true);
    ks
}

fn expected_tags(rows: &[(&[u8], &[i64])]) -> BTreeSet<(Vec<u8>, PkRef)> {
    let mut buf = IndexKeyBuf::new();
    let mut out = BTreeSet::new();
    for (pk, tags) in rows {
        for tag in *tags {
            index_key_encode(IndexKeyType::I64, IndexScalar::I64(*tag), &mut buf).expect("i64");
            out.insert((buf.as_bytes().to_vec(), PkRef::from_key_hash(hash_of(pk))));
        }
    }
    out
}

/// A wildcard index over repeated values: the dying document yields `1`
/// twice, and its alias holds `1` — a strict subset of the dying
/// document's keys. Exactly the unshared keys go, by bracket and by
/// eviction (the death hook's own dedup).
#[test]
fn wildcard_alias_holding_a_subset_keeps_exactly_the_shared_keys() {
    let (a, b) = (alias_key(6, b'a'), alias_key(6, b'b'));
    for evict in [false, true] {
        let mut ks = tags_fixture();
        let expire =
            if evict { SetExpire::At(Nanos(900_000 * 1_000_000)) } else { SetExpire::Keep };
        put(&mut ks, &a, r#"{"tags":[1,1,2,3]}"#, expire);
        put(&mut ks, &b, r#"{"tags":[1]}"#, SetExpire::Keep);
        if evict {
            gate_evict_all_volatile(&mut ks);
        } else {
            bracketed(&mut ks, &[&a], None, no_gate, |store| assert!(store.del(&a, T0)));
        }
        assert_eq!(tree_entries(&ks, IDX_TAGS), expected_tags(&[(&b, &[1])]), "evict = {evict}");
        assert_eq!(ks.idx_counters_total().alias_kept, 1, "evict = {evict}");
    }
}

/// The held marking is linear in a repeated value. Both aliases yield
/// one value `REPEATS` times: the dying document's equal run is marked
/// **once** — by the member's first emission; the other `REPEATS − 1`
/// find the run held and stop. Re-marking the run per emission is
/// `REPEATS²` marks on the cell thread, inside every walk budget. The
/// count is the work itself, so the row is deterministic.
fn held_marks_for_a_repeated_value(evict: bool) {
    const REPEATS: usize = 2_000;
    let (a, b) = (alias_key(13, b'a'), alias_key(13, b'b'));
    let doc = format!(r#"{{"tags":[{}]}}"#, vec!["1"; REPEATS].join(","));
    let mut ks = tags_fixture();
    let expire = if evict { SetExpire::At(Nanos(900_000 * 1_000_000)) } else { SetExpire::Keep };
    put(&mut ks, &a, &doc, expire);
    put(&mut ks, &b, &doc, SetExpire::Keep);
    if evict {
        gate_evict_all_volatile(&mut ks);
    } else {
        bracketed(&mut ks, &[&a], None, no_gate, |store| assert!(store.del(&a, T0)));
    }
    assert_eq!(tree_entries(&ks, IDX_TAGS), expected_tags(&[(&b, &[1])]));
    let counters = ks.idx_counters_total();
    assert_eq!(counters.alias_kept, 1);
    assert_eq!(counters.alias_held_marks, REPEATS as u64, "each entry of the run, once");
}

#[test]
fn a_repeated_value_is_marked_held_once_by_the_bracket() {
    held_marks_for_a_repeated_value(false);
}

#[test]
fn a_repeated_value_is_marked_held_once_by_the_death_hook() {
    held_marks_for_a_repeated_value(true);
}

// ---- ADR-0139 D4: the forget against the diff's dedup ----------------------

fn set_string(store: &mut CellStore, key: &[u8]) {
    store.set(key, b"now a string", SetOptions::default(), T0).expect("set");
}

/// `MSET A B`, aliases, both hold `k`; the gate evicts `A`. `A`'s range
/// is forgotten and **compacted out** before the sort: left in place, a
/// dead twin can be the run's last and shadow `B`'s live entry. Both
/// argv orders, so the shadowing order is covered whichever way the
/// sort leaves an equal run.
#[test]
fn forget_is_compacted_before_the_diff_dedups() {
    for a_first in [true, false] {
        let (a, b) = (alias_key(7, b'a'), alias_key(7, b'b'));
        let mut ks = fixture();
        put_v_ttl(&mut ks, &a, 7, 900_000);
        put_v(&mut ks, &b, 7);
        let keys: [&[u8]; 2] = if a_first { [&a, &b] } else { [&b, &a] };
        bracketed(&mut ks, &keys, None, gate_evict_all_volatile, |store| {
            for key in keys {
                set_string(store, key);
            }
        });
        assert!(tree_entries(&ks, IDX_V).is_empty(), "a_first = {a_first}: `(k, h)` left once");
        assert_eq!(ks.idx_counters_total().gate_forget, 1);
    }
}

/// `MSET k v k v`: the write set names `k` twice, so it has two `old`
/// ranges; the gate's forget marks both.
#[test]
fn forget_marks_every_range_of_a_repeated_key() {
    let key = b"doc:twice";
    let mut ks = fixture();
    put_v_ttl(&mut ks, key, 7, 900_000);
    bracketed(&mut ks, &[key, key], None, gate_evict_all_volatile, |store| {
        set_string(store, key);
        set_string(store, key);
    });
    assert!(tree_entries(&ks, IDX_V).is_empty());
}

// ---- one command, both aliases ---------------------------------------------

/// The diff merges the write set as a group: `DEL a b` removes the
/// shared entry once; `MSET a b` likewise; `RENAME a → c` where `c`
/// aliases `b`; `COPY` onto an alias. The entry is present iff a holder
/// remains.
#[test]
fn one_command_over_both_aliases_is_one_diff() {
    let (a, b, c) = (alias_key(8, b'a'), alias_key(8, b'b'), alias_key(8, b'c'));
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    bracketed(&mut ks, &[&a, &b], None, no_gate, |store| {
        assert!(store.del(&a, T0));
        assert!(store.del(&b, T0));
    });
    assert!(tree_entries(&ks, IDX_V).is_empty(), "DEL a b");

    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    bracketed(&mut ks, &[&a, &b], None, no_gate, |store| {
        set_string(store, &a);
        set_string(store, &b);
    });
    assert!(tree_entries(&ks, IDX_V).is_empty(), "MSET a b");

    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 7);
    bracketed(&mut ks, &[&a, &c], None, no_gate, |store| {
        assert!(store.rename(&a, &c, T0).expect("rename"));
    });
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&b, 7), (&c, 7)])));
    bracketed(&mut ks, &[&b, &c], None, no_gate, |store| {
        assert!(store.del(&b, T0));
        assert!(store.del(&c, T0));
    });
    assert!(tree_entries(&ks, IDX_V).is_empty(), "RENAME then DEL of both holders");

    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &b, 8);
    ks.db_mut(0).copy(&a, &b, true, T0).expect("copy onto an alias");
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&a, 7), (&b, 7)])));
}

/// Coverage is by full key: the gate evicts `b`, an **alias** of the
/// write-set key `a` (different values). `b`'s death is not `a`'s — the
/// bracket must not forget `a`'s `old`, or `DEL a` leaves `a`'s entry.
#[test]
fn an_evicted_alias_of_a_write_set_key_is_not_the_write_set_key() {
    let (a, b) = (alias_key(9, b'a'), alias_key(9, b'b'));
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v_ttl(&mut ks, &b, 8, 900_000);
    bracketed(&mut ks, &[&a], None, gate_evict_all_volatile, |store| {
        assert!(store.del(&a, T0));
    });
    assert!(tree_entries(&ks, IDX_V).is_empty());
    let counters = ks.idx_counters_total();
    assert_eq!(counters.cover_alias, 1, "the hash prefilter hit, the key compare said no");
    assert_eq!(counters.gate_forget, 0);
}

// ---- the fragment neighbour ------------------------------------------------

/// Two keys that agree on the record table's whole 22-bit filter and on
/// the home group, but not on all 64 bits — found by a birthday search
/// over the keyed hash. The table cannot tell them apart; the
/// enumeration's re-hash must.
#[test]
fn a_fragment_neighbour_is_never_treated_as_an_alias() {
    let mut seen: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
    let mut pair = None;
    for i in 0u64.. {
        let key = format!("neighbour:{i}").into_bytes();
        let hash = hash_of(&key);
        // Control tag + fp15 (the top 22 bits) and the 8-group home.
        let code = (hash >> 42) << 3 | (hash & 7);
        if let Some(other) = seen.insert(code, key.clone()) {
            pair = Some((other, key));
            break;
        }
    }
    let (a, n) = pair.expect("the search is unbounded");
    assert_ne!(hash_of(&a), hash_of(&n), "a neighbour, not an alias");
    let mut ks = fixture();
    put_v(&mut ks, &a, 7);
    put_v(&mut ks, &n, 7);
    assert_eq!(tree_entries(&ks, IDX_V).len(), 2, "two refs, two entries");
    bracketed(&mut ks, &[&a], None, no_gate, |store| assert!(store.del(&a, T0)));
    assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model_of(&[(&n, 7)])));
    assert_eq!(ks.idx_counters_total().alias_kept, 0);
}

// ---- ADR-0139 D9: the member budget, on real aliases -----------------------

/// Nine aliases: deleting one enumerates the other eight — the view's
/// capacity — and serves. Ten: the ninth member ends the walk `Over`;
/// the question is unanswered, so the index degrades, counted, and the
/// mutation stands.
#[test]
fn alias_group_of_eight_serves_and_of_nine_degrades() {
    for extra in [0usize, 1] {
        let members = IDX_ALIAS_GROUP_MAX + 1 + extra;
        let keys: Vec<[u8; 48]> = (0..members).map(|m| alias_key(10, b'a' + m as u8)).collect();
        let mut ks = fixture();
        for key in &keys {
            put_v(&mut ks, key, 7);
        }
        bracketed(&mut ks, &[&keys[0]], None, no_gate, |store| assert!(store.del(&keys[0], T0)));
        let counters = ks.idx_counters_total();
        if extra == 0 {
            assert_eq!(ks.idx_degraded(NS, IDX_V), Some(false));
            assert_eq!(counters.alias_kept, 1);
            assert_eq!(tree_entries(&ks, IDX_V).len(), 1, "the survivors still hold the entry");
        } else {
            assert_eq!(ks.idx_degraded(NS, IDX_V), Some(true), "`Over` decides nothing");
            assert_eq!(counters.alias_walk_over, 1);
            assert!(ks.db_mut(0).json_get(&keys[0], T0).expect("read").is_none(), "DEL stands");
        }
    }
}

/// The re-hash budget is **record fetches**, and the bracket's exclusion
/// is one: it reads the candidate's key. One command rewrites a whole
/// alias group (the records outlive the body, so the commit-half meets
/// every one): sixteen aliases are sixteen fetches and serve; a
/// seventeenth is never read — the walk is `Over`, the index degrades,
/// the write stands. (Charging only the keyed hash let one bracket read
/// every fragment match on the chain.)
#[test]
fn a_write_set_of_aliases_is_charged_per_record_fetch() {
    for extra in [0usize, 1] {
        let members = IDX_ALIAS_REHASH_MAX + extra;
        let keys: Vec<[u8; 48]> = (0..members).map(|m| alias_key(14, b'a' + m as u8)).collect();
        let mut ks = fixture();
        for key in &keys {
            put_v(&mut ks, key, 7);
        }
        let write_set: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let idoc = parse(r#"{"v":9,"w":1}"#);
        bracketed(&mut ks, &write_set, None, no_gate, |store| {
            for key in &keys {
                store
                    .json_set(
                        key,
                        &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                        JsonSetOptions::default(),
                        T0,
                    )
                    .expect("json_set");
            }
        });
        let counters = ks.idx_counters_total();
        if extra == 0 {
            assert_eq!(ks.idx_degraded(NS, IDX_V), Some(false));
            let model: BTreeMap<Vec<u8>, i64> = keys.iter().map(|k| (k.to_vec(), 9)).collect();
            assert_eq!(tree_entries(&ks, IDX_V), expected_i64(&model), "`(7, h)` left once");
            assert_eq!(counters.alias_walk_over, 0);
        } else {
            assert_eq!(ks.idx_degraded(NS, IDX_V), Some(true), "`Over` decides nothing");
            assert_eq!(counters.alias_walk_over, 1);
        }
    }
}

/// The death hook has no bracket mask: `Over` from an eviction degrades
/// **every** non-degraded index on the store.
#[test]
fn an_unfinished_walk_in_a_death_hook_degrades_every_index() {
    let keys: Vec<[u8; 48]> =
        (0..IDX_ALIAS_GROUP_MAX + 2).map(|m| alias_key(11, b'a' + m as u8)).collect();
    let mut ks = fixture();
    declare(&mut ks, IDX_TAGS, "$.w", IndexKeyType::I64);
    put_v_ttl(&mut ks, &keys[0], 7, 900_000);
    for key in &keys[1..] {
        put_v(&mut ks, key, 7);
    }
    gate_evict_all_volatile(&mut ks);
    assert_eq!(ks.idx_degraded(NS, IDX_V), Some(true));
    assert_eq!(ks.idx_degraded(NS, IDX_TAGS), Some(true));
    assert_eq!(ks.idx_counters_total().alias_walk_over, 1);
}

// ---- ADR-0139 D10: scratch — peak, failure, ownership ----------------------

/// A planted `try_reserve` failure at each growth site of the pre-half
/// is a typed `Reserve` refusal that leaves nothing behind. The plant
/// asserts its own engagement: a site count of zero would be vacuous.
#[test]
fn scratch_growth_failure_at_the_pre_half_is_a_typed_refusal() {
    let key = b"doc:scratch";
    let mut sites = 0u64;
    for nth in 1u64.. {
        // A fresh store owns no scratch, so every buffer must grow; the
        // seed is unbracketed for the same reason.
        let mut ks = fixture();
        {
            let store = ks.db_mut(0);
            let idoc = parse(r#"{"v":7,"w":1}"#);
            store
                .json_set(
                    key,
                    &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                    JsonSetOptions::default(),
                    T0,
                )
                .expect("unbracketed seed");
        }
        fault::arm(inf_store::fault::IDX_SCRATCH_REFUSE, FaultSpec::Nth(nth));
        let outcome = ks.idx_bracket_begin(NS, &[key], None);
        let fired = fault::fired(inf_store::fault::IDX_SCRATCH_REFUSE);
        fault::disarm_all();
        if fired == 0 {
            outcome.expect("past the last growth site the pre-half admits");
            ks.idx_bracket_commit(NS, &[key]);
            break;
        }
        sites += 1;
        assert_eq!(outcome, Err(IdxMaintRefusal::Reserve), "site {nth}");
        assert_eq!(ks.idx_degraded(NS, IDX_V), Some(false), "a refusal degrades nothing");
        // Nothing changed and no bracket is left open: the next one opens.
        ks.idx_bracket_begin(NS, &[key], None).expect("the refused bracket left nothing");
        ks.idx_bracket_commit(NS, &[key]);
    }
    assert!(sites >= 4, "write-set table, key bytes, `old`, encoded keys: {sites} sites reached");
    receipt::verified("idx_scratch_refuse", "scratch-refusal-leaves-no-trace");
}

/// `EXPIRE` reaches no publishing funnel, so the commit-half re-evaluates
/// the document — into scratch the pre-half already owns (`new ⊆ old`).
/// With **every** store scratch growth refused after the pre-half the
/// trees stay exact and nothing degrades. The control leg proves the
/// plant is live: a growing post-image under the same arming degrades.
#[test]
fn a_key_that_reaches_no_funnel_needs_no_scratch_after_the_pre_half() {
    let key = b"doc:expire";
    let mut ks = tags_fixture();
    put(&mut ks, key, r#"{"tags":[1,2,3,4,5,6,7,8]}"#, SetExpire::Keep);
    ks.idx_bracket_begin(NS, &[key], None).expect("pre-half");
    fault::arm(inf_store::fault::IDX_SCRATCH_REFUSE, FaultSpec::Always);
    let deadline = Some(Nanos(900_000 * 1_000_000));
    assert!(ks.db_mut(0).expire(key, deadline, inf_store::ExpireCond::Always, T0));
    ks.idx_bracket_commit(NS, &[key]);
    assert_eq!(fault::fired(inf_store::fault::IDX_SCRATCH_REFUSE), 0, "no growth was attempted");
    assert_eq!(ks.idx_degraded(NS, IDX_TAGS), Some(false));
    assert_eq!(tree_entries(&ks, IDX_TAGS), expected_tags(&[(key, &[1, 2, 3, 4, 5, 6, 7, 8])]));

    // A `JSON.SET` that publishes nothing — `NX` over the live document,
    // and a path write that finds no such path (the wire's error reply) —
    // is the same shape: the commit-half re-evaluates an unchanged image.
    let grown: Vec<String> = (0..4096).map(|i| i.to_string()).collect();
    let bigger = parse(&format!(r#"{{"tags":[{}]}}"#, grown.join(",")));
    for refused in ["nx", "no-such-path"] {
        ks.idx_bracket_begin(NS, &[key], None).expect("pre-half");
        let published = match refused {
            "nx" => {
                let opts = JsonSetOptions { cond: SetCond::IfAbsent, ..JsonSetOptions::default() };
                let outcome = ks
                    .db_mut(0)
                    .json_set(
                        key,
                        &CanonicalDoc::validate(&bigger).expect("canonical fixture"),
                        opts,
                        T0,
                    )
                    .expect("json_set");
                outcome == inf_store::JsonSetOutcome::Applied
            }
            _ => {
                let path = compile(b"$.absent.w").expect("valid path");
                let op = inf_doc::ApplyOp::SetReplace { fragment: b"2" };
                let patch = ks.db_mut(0).json_patch_scalar(key, &path, &op, T0).expect("patch");
                use inf_doc::ScalarPatch::{Number, Toggled};
                matches!(patch, Some(Number(_) | Toggled(_)))
            }
        };
        assert!(!published, "{refused}: nothing was published");
        ks.idx_bracket_commit(NS, &[key]);
        assert_eq!(fault::fired(inf_store::fault::IDX_SCRATCH_REFUSE), 0, "{refused}");
        assert_eq!(ks.idx_degraded(NS, IDX_TAGS), Some(false), "{refused}");
        let unchanged = expected_tags(&[(key, &[1, 2, 3, 4, 5, 6, 7, 8])]);
        assert_eq!(tree_entries(&ks, IDX_TAGS), unchanged, "{refused}");
    }

    // Control: the post-image outgrows what the pre-half reserved.
    ks.idx_bracket_begin(NS, &[key], None).expect("pre-half");
    let grown: Vec<String> = (0..4096).map(|i| i.to_string()).collect();
    let idoc = parse(&format!(r#"{{"tags":[{}]}}"#, grown.join(",")));
    ks.db_mut(0)
        .json_set(
            key,
            &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
            JsonSetOptions::default(),
            T0,
        )
        .expect("json_set");
    ks.idx_bracket_commit(NS, &[key]);
    assert!(fault::fired(inf_store::fault::IDX_SCRATCH_REFUSE) > 0, "the plant is live");
    fault::disarm_all();
    assert_eq!(
        ks.idx_degraded(NS, IDX_TAGS),
        Some(true),
        "the mutation stands, the index degrades"
    );
    receipt::verified("idx_scratch_refuse", "scratch-owned-before-publish");
}

/// A death cannot refuse: a scratch growth failure inside the hook
/// degrades the index, and the death proceeds.
#[test]
fn scratch_growth_failure_in_a_death_hook_degrades_the_index() {
    let key = b"doc:dying";
    let mut ks = fixture();
    {
        let idoc = parse(r#"{"v":7,"w":1}"#);
        ks.db_mut(0)
            .json_set(
                key,
                &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
                JsonSetOptions::default(),
                T0,
            )
            .expect("seed");
    }
    fault::arm(inf_store::fault::IDX_SCRATCH_REFUSE, FaultSpec::Always);
    assert!(ks.db_mut(0).del(key, T0), "an unbracketed death runs the hook");
    let fired = fault::fired(inf_store::fault::IDX_SCRATCH_REFUSE);
    fault::disarm_all();
    assert!(fired > 0, "the hook reached a growth site");
    assert_eq!(ks.idx_degraded(NS, IDX_V), Some(true));
}

/// 64 `utf8` indexes on one wildcard path: a ≈ 0.5 MiB document encodes
/// to exactly `BRACKET_KEY_BYTES_MAX` of keys per phase — admitted — and
/// one more short string crosses the cap. As a pre-image that is the
/// typed `EntryFlood` refusal; as a create's post-image every one of the
/// 64 participating indexes degrades.
#[test]
fn the_key_bytes_cap_bounds_a_phase() {
    const INDEXES: usize = 64;
    let mut buf = IndexKeyBuf::new();
    index_key_encode(IndexKeyType::Utf8, IndexScalar::Utf8("x"), &mut buf).expect("utf8");
    let overhead = buf.as_bytes().len() - 1;
    let per_string = 1024usize;
    let strings = BRACKET_KEY_BYTES_MAX / INDEXES / per_string;
    assert_eq!(strings * per_string * INDEXES, BRACKET_KEY_BYTES_MAX, "the cap is reached exactly");
    let tag = "t".repeat(per_string - overhead);
    let at_cap: Vec<String> = (0..strings).map(|_| format!("\"{tag}\"")).collect();
    let at_cap = format!(r#"{{"tags":[{}]}}"#, at_cap.join(","));
    let over_cap = at_cap.replacen('[', r#"["one more","#, 1);

    let build = || {
        let mut ks = Keyspace::new(StoreConfig::default());
        for id in 1..=INDEXES as u32 {
            declare(&mut ks, IndexId(id), "$.tags[*]", IndexKeyType::Utf8);
        }
        ks.db_mut(0);
        ks
    };
    let key = b"doc:wide";
    let mut ks = build();
    put(&mut ks, key, &at_cap, SetExpire::Keep);
    assert_eq!(ks.idx_counters_total().degraded_trips, 0, "exactly at the cap is admitted");
    // The pre-image at the cap is admitted too; one over is refused.
    ks.idx_bracket_begin(NS, &[key], None).expect("a pre-image at the cap");
    ks.idx_bracket_commit(NS, &[key]);

    let mut ks = build();
    put(&mut ks, key, &over_cap, SetExpire::Keep);
    assert_eq!(ks.idx_counters_total().degraded_trips, INDEXES as u64, "every index degrades");
    // Un-degraded indexes meet the same document as a pre-image.
    let mut ks = build();
    let idoc = parse(&over_cap);
    ks.db_mut(0)
        .json_set(
            key,
            &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
            JsonSetOptions::default(),
            T0,
        )
        .expect("unbracketed seed");
    assert_eq!(ks.idx_bracket_begin(NS, &[key], None), Err(IdxMaintRefusal::EntryFlood));
}

/// Liveness of the counters the rows above read: one gate pass over an
/// alias pair reaches the group, kept, forget and cover-alias events, so
/// none of them is a counter that cannot move.
#[test]
fn every_alias_counter_is_reachable() {
    let (a, b) = (alias_key(12, b'a'), alias_key(12, b'b'));
    let mut ks = fixture();
    put_v_ttl(&mut ks, &a, 7, 500_000);
    put_v_ttl(&mut ks, &b, 7, 900_000);
    bracketed(&mut ks, &[&a], None, gate_evict_all_volatile, |_| {});
    let counters = ks.idx_counters_total();
    assert_eq!(counters.alias_groups, 1, "A's hook found B");
    assert_eq!(counters.alias_kept, 1, "…which held the key");
    assert_eq!(counters.gate_forget, 1, "A was a write-set key");
    assert_eq!(counters.cover_alias, 1, "B was only its alias");
    assert!(counters.alias_walk_groups_max >= 1);
    assert_eq!(counters.alias_walk_over, 0);
}
