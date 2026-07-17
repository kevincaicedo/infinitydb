//! M3-S03 lifecycle matrix (ADR-0037): `JsonDoc` records tier by the two
//! thresholds, every free/overwrite/rename/copy site keeps the document
//! domain exact, versions bump exactly once per logical mutation and
//! never on morph, and cross-type operations answer `WrongType` instead
//! of transmuting state.
#![cfg(feature = "doc")]

use inf_alloc::ArenaConfig;
use inf_doc::JsonParser;
use inf_doc::apply::{ApplyOp, Number};
use inf_doc::model::{self, Value};
use inf_doc::path::compile;
use inf_store::{
    CellStore, CopyResult, EvictionPolicy, ExpireCond, ExpiryBudget, JsonScalarPatch,
    JsonSetOptions, JsonSetOutcome, Keyspace, OpError, SetCond, SetExpire, SetOptions, StoreConfig,
    TypeTag,
};

use inf_foundation::time::Nanos;

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn store() -> CellStore {
    CellStore::new(StoreConfig::default())
}

/// ADR-0046: ingest tree residency is config-opt-in (the default is tape
/// at every size). Tests exercising the tree arm pin the pre-ADR-0046
/// threshold explicitly so the tree lifecycle stays covered.
fn tree_config() -> StoreConfig {
    StoreConfig { doc_morph_bytes_min: 4096, ..StoreConfig::default() }
}

fn tree_store() -> CellStore {
    CellStore::new(tree_config())
}

/// A document whose idoc size lands in the requested placement tier:
/// an object with one string padded to the target size.
fn doc_of_size(total_idoc_bytes: usize) -> Vec<u8> {
    // Overhead: 8 header + obj(4) + key "pad"(4) + str24/str8 framing.
    let mut pad = total_idoc_bytes.saturating_sub(24);
    loop {
        let v = Value::Obj(vec![("pad".into(), Value::Str("x".repeat(pad)))]);
        let bytes = model::encode(&v).expect("encodes");
        if bytes.len() >= total_idoc_bytes || pad > total_idoc_bytes {
            return bytes;
        }
        pad += total_idoc_bytes - bytes.len();
    }
}

/// An array-rooted document sized past the morph threshold (tree form).
fn tree_doc() -> Vec<u8> {
    model::encode(&Value::Arr((0..2000i64).map(Value::I64).collect())).expect("encodes")
}

fn set(store: &mut CellStore, key: &[u8], idoc: &[u8]) {
    let outcome = store.json_set(key, idoc, JsonSetOptions::default(), now()).expect("set");
    assert_eq!(outcome, JsonSetOutcome::Applied);
}

/// The reconciliation invariant (ADR-0037 D5): the doc arena's live bytes
/// are exactly the blob + tree domains; slack never exceeds tree bytes.
fn reconcile(store: &CellStore) {
    let d = store.doc_domain();
    assert_eq!(store.doc_live_bytes(), d.tape_bytes + d.arena_bytes, "domain partitions arena");
    assert!(d.slack_bytes <= d.arena_bytes, "slack is a subset of tree bytes");
}

#[test]
fn placement_tiers_by_the_two_thresholds() {
    let mut s = store();
    // ≤ 512 B: inline (nothing in the doc arena).
    set(&mut s, b"small", &doc_of_size(200));
    let d = s.doc_domain();
    assert_eq!((d.inline_docs, d.docs_live), (1, 1));
    assert_eq!(s.doc_live_bytes(), 0);
    // > 512: doc-arena tape blob.
    set(&mut s, b"medium", &doc_of_size(1024));
    let d = s.doc_domain();
    assert_eq!(d.docs_live, 2);
    assert!(d.tape_bytes >= 1024, "blob bytes attributed: {}", d.tape_bytes);
    assert_eq!(d.arena_bytes, 0);
    // ADR-0046: tape at every size — a past-the-old-threshold document
    // stays a blob under the default config; no tree is ever built at
    // ingest.
    set(&mut s, b"large", &tree_doc());
    let d = s.doc_domain();
    assert_eq!(d.docs_live, 3);
    assert_eq!(d.arena_bytes, 0, "ingest never builds the tree form (ADR-0046)");
    assert!(d.tape_bytes >= 1024 + tree_doc().len() as u64, "both blobs attributed");
    reconcile(&s);
    // Tree residency remains config-opt-in for the forced-form arms.
    let mut t = tree_store();
    set(&mut t, b"large", &tree_doc());
    let d = t.doc_domain();
    assert!(d.arena_bytes > 0, "tree bytes attributed under the opt-in config");
    reconcile(&t);
    // Reads resolve through the form-agnostic cursor on every tier.
    for key in [b"small".as_slice(), b"medium", b"large"] {
        let read = s.json_get(key, now()).expect("doc").expect("present");
        assert_eq!(read.version, 1, "fresh keys start at version 1");
    }
    // used_bytes sees document memory (the maxmemory comparable).
    assert!(s.used_bytes() > s.doc_live_bytes());
}

#[test]
fn document_root_prefetch_is_hint_only_for_every_record_form() {
    let mut s = tree_store();
    let cases = [
        (b"inline".as_slice(), doc_of_size(200)),
        (b"tape", doc_of_size(1_024)),
        (b"tree", tree_doc()),
    ];
    for (key, doc) in &cases {
        set(&mut s, key, doc);
    }
    s.set(b"string", b"value", SetOptions::default(), now()).expect("string set");

    let report = s.report();
    let stats = s.stats();
    for key in [b"inline".as_slice(), b"tape", b"tree", b"string", b"missing"] {
        let hash = CellStore::hash_key(key);
        s.prefetch(hash);
        s.probe_prefetch(hash);
        s.prefetch_doc_root(hash);
    }
    assert_eq!(s.report(), report, "hints cannot alter attribution");
    assert_eq!(
        (s.stats().keyspace_hits, s.stats().keyspace_misses),
        (stats.keyspace_hits, stats.keyspace_misses),
        "hints cannot alter access counters"
    );
    for (key, doc) in &cases {
        assert_eq!(s.json_freeze(key, now()).unwrap().unwrap(), *doc);
    }
    assert_eq!(s.get_str(b"string", now()).unwrap(), Some(b"value".as_slice()));
}

#[test]
fn json_set_version_chains_and_conditions() {
    let mut s = tree_store();
    let doc = doc_of_size(100);
    set(&mut s, b"k", &doc);
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 1);
    // NX on existing: skipped, version untouched.
    let opts = JsonSetOptions { cond: SetCond::IfAbsent, ..JsonSetOptions::default() };
    assert_eq!(s.json_set(b"k", &doc, opts, now()).expect("nx"), JsonSetOutcome::Skipped);
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 1);
    // Plain set over existing: exactly one bump — including across tiers.
    set(&mut s, b"k", &doc_of_size(1024));
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 2);
    set(&mut s, b"k", &tree_doc());
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 3);
    reconcile(&s);
    // XX on missing: skipped, nothing created.
    let opts = JsonSetOptions { cond: SetCond::IfPresent, ..JsonSetOptions::default() };
    assert_eq!(s.json_set(b"missing", &doc, opts, now()).expect("xx"), JsonSetOutcome::Skipped);
    assert!(s.json_get(b"missing", now()).expect("ok").is_none());
}

#[test]
fn replace_bumps_once_and_retiers() {
    let mut s = store();
    set(&mut s, b"k", &doc_of_size(100));
    assert!(s.json_replace(b"k", &doc_of_size(2000), now()).expect("replace"));
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 2);
    let d = s.doc_domain();
    assert_eq!((d.inline_docs, d.docs_live), (0, 1), "re-tiered inline → blob");
    reconcile(&s);
    assert!(!s.json_replace(b"gone", &doc_of_size(100), now()).expect("missing"), "no-op");
}

#[test]
fn morph_is_version_neutral_and_edit_bumps_once() {
    let mut s = store();
    let arr = model::encode(&Value::Arr(vec![Value::I64(1)])).expect("encodes");
    set(&mut s, b"k", &arr);
    assert!(s.json_morph(b"k", now()).expect("morph"));
    let read = s.json_get(b"k", now()).unwrap().unwrap();
    assert_eq!(read.version, 1, "morph never bumps (§3.4 R3)");
    assert!(s.json_morph(b"k", now()).expect("idempotent"), "tree stays tree");
    reconcile(&s);
    // One edit call = one bump, however many pushes it makes.
    let edited = s
        .json_edit_tree(b"k", now(), |doc, arena| {
            let mut root = doc.root_ref();
            for i in 0..10 {
                let v = doc.alloc_i64(arena, i)?;
                root = doc.arr_push(arena, root, v)?;
            }
            Ok(())
        })
        .expect("edit");
    assert_eq!(edited, Some(()));
    let read = s.json_get(b"k", now()).unwrap().unwrap();
    assert_eq!(read.version, 2, "exactly one bump per edit command");
    reconcile(&s);
    // A failing edit leaves the version untouched.
    let err = s
        .json_edit_tree::<()>(b"k", now(), |_, _| Err(inf_doc::DocError::ArenaExhausted))
        .expect_err("propagates");
    assert_eq!(err, OpError::OutOfMemory);
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 2, "no bump on Err");
    // Edit on a non-tree form refuses (S16 morphs first).
    set(&mut s, b"flat", &doc_of_size(100));
    let err = s.json_edit_tree::<()>(b"flat", now(), |_, _| Ok(())).expect_err("wrong form");
    assert_eq!(err, OpError::WrongType);
    assert_eq!(s.json_edit_tree::<()>(b"gone", now(), |_, _| Ok(())).expect("missing"), None);
}

#[test]
fn freeze_reproduces_canonical_bytes_on_every_tier() {
    let mut s = tree_store();
    for (key, doc) in [
        (b"small".as_slice(), doc_of_size(100)),
        (b"medium", doc_of_size(1024)),
        (b"large", tree_doc()),
    ] {
        set(&mut s, key, &doc);
        let frozen = s.json_freeze(key, now()).expect("doc").expect("present");
        assert_eq!(frozen, doc, "freeze == ingested canonical bytes");
    }
    assert_eq!(s.json_freeze(b"gone", now()).expect("ok"), None);
}

#[test]
fn same_width_scalar_lane_patches_every_storage_form_once() {
    let before = JsonParser::new().parse(br#"{"n":40,"enabled":false}"#).expect("fixture parses");
    let after_num =
        JsonParser::new().parse(br#"{"n":42,"enabled":false}"#).expect("fixture parses");
    let after_toggle =
        JsonParser::new().parse(br#"{"n":42,"enabled":true}"#).expect("fixture parses");
    let number_path = compile(b"$.n").expect("path");
    let bool_path = compile(b"$.enabled").expect("path");

    let configs = [
        StoreConfig::default(),
        StoreConfig {
            doc_inline_bytes_max: 0,
            doc_morph_bytes_min: usize::MAX,
            ..Default::default()
        },
        StoreConfig { doc_morph_bytes_min: 0, ..Default::default() },
    ];
    for cfg in configs {
        let mut store = CellStore::new(cfg);
        set(&mut store, b"doc", &before);
        let verdict = store
            .json_patch_scalar(b"doc", &number_path, &ApplyOp::NumIncrBy(Number::I64(2)), now())
            .expect("patch")
            .expect("present");
        assert_eq!(verdict, JsonScalarPatch::Number(Number::I64(42)));
        assert_eq!(store.json_get(b"doc", now()).unwrap().unwrap().version, 2);
        assert_eq!(store.json_freeze(b"doc", now()).unwrap().unwrap(), after_num);

        let verdict = store
            .json_patch_scalar(b"doc", &bool_path, &ApplyOp::Toggle, now())
            .expect("patch")
            .expect("present");
        assert_eq!(verdict, JsonScalarPatch::Toggled(true));
        assert_eq!(store.json_get(b"doc", now()).unwrap().unwrap().version, 3);
        assert_eq!(store.json_freeze(b"doc", now()).unwrap().unwrap(), after_toggle);
        reconcile(&store);
    }
}

#[test]
fn scalar_lane_falls_back_before_width_change_or_error() {
    let idoc = JsonParser::new().parse(br#"{"n":127}"#).expect("fixture parses");
    let path = compile(b"$.n").expect("path");
    for cfg in
        [StoreConfig::default(), StoreConfig { doc_morph_bytes_min: 0, ..Default::default() }]
    {
        let mut store = CellStore::new(cfg);
        set(&mut store, b"doc", &idoc);
        let before_domain = store.doc_domain();
        let verdict = store
            .json_patch_scalar(b"doc", &path, &ApplyOp::NumIncrBy(Number::I64(1)), now())
            .expect("probe")
            .expect("present");
        assert_eq!(verdict, JsonScalarPatch::Unsupported);
        assert_eq!(store.json_freeze(b"doc", now()).unwrap().unwrap(), idoc);
        assert_eq!(store.json_get(b"doc", now()).unwrap().unwrap().version, 1);
        assert_eq!(store.doc_domain(), before_domain);
    }

    let max = JsonParser::new()
        .parse(format!(r#"{{"n":{}}}"#, i64::MAX).as_bytes())
        .expect("fixture parses");
    let mut store = store();
    set(&mut store, b"doc", &max);
    let before_domain = store.doc_domain();
    assert_eq!(
        store.json_patch_scalar(b"doc", &path, &ApplyOp::NumIncrBy(Number::I64(1)), now(),),
        Err(OpError::Overflow)
    );
    assert_eq!(store.json_freeze(b"doc", now()).unwrap().unwrap(), max);
    assert_eq!(store.json_get(b"doc", now()).unwrap().unwrap().version, 1);
    assert_eq!(store.doc_domain(), before_domain);
}

#[test]
fn wrong_type_is_refused_in_both_directions() {
    let mut s = store();
    s.set(b"str", b"v", SetOptions::default(), now()).expect("set");
    let doc = doc_of_size(100);
    assert_eq!(s.json_set(b"str", &doc, JsonSetOptions::default(), now()), Err(OpError::WrongType));
    assert!(matches!(s.json_get(b"str", now()), Err(OpError::WrongType)));
    assert_eq!(s.json_replace(b"str", &doc, now()), Err(OpError::WrongType));
    assert_eq!(s.json_morph(b"str", now()), Err(OpError::WrongType));
    assert_eq!(s.json_freeze(b"str", now()), Err(OpError::WrongType));

    set(&mut s, b"doc", &doc);
    assert_eq!(s.incr_by(b"doc", 1, now()), Err(OpError::WrongType));
    assert_eq!(s.incr_by_float(b"doc", 1.0, now()), Err(OpError::WrongType));
    assert_eq!(s.append(b"doc", b"x", now()), Err(OpError::WrongType));
    assert_eq!(s.set_range(b"doc", 0, b"x", now()), Err(OpError::WrongType));
    assert_eq!(s.getdel(b"doc", now()), None, "string API never leaks handles");
    assert!(s.json_get(b"doc", now()).expect("ok").is_some(), "GETDEL refusal deleted nothing");
    assert_eq!(s.type_of(b"doc", now()), Some(TypeTag::JsonDoc));
}

#[test]
fn every_free_site_releases_the_payload() {
    let base = tree_config();
    // DEL, across all tiers.
    let mut s = CellStore::new(base);
    for (key, doc) in
        [(b"a".as_slice(), doc_of_size(100)), (b"b", doc_of_size(1024)), (b"c", tree_doc())]
    {
        set(&mut s, key, &doc);
    }
    reconcile(&s);
    for key in [b"a".as_slice(), b"b", b"c"] {
        assert!(s.del(key, now()));
    }
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "DEL drains the domain");
    assert_eq!(s.doc_live_bytes(), 0);

    // Plain SET overwrites a document key (Redis semantics) and releases.
    set(&mut s, b"k", &doc_of_size(1024));
    s.set(b"k", b"now-a-string", SetOptions::default(), now()).expect("overwrite");
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default());
    assert!(s.del(b"k", now()), "clear the string so later emptiness checks hold");

    // Expire-on-read (lazy) and EXPIRE-in-the-past.
    set(&mut s, b"ttl", &doc_of_size(1024));
    assert!(s.expire(b"ttl", Some(Nanos::from_millis(5)), ExpireCond::Always, now()));
    assert!(s.json_get(b"ttl", Nanos::from_millis(6)).expect("ok").is_none(), "lazy reap");
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default());
    set(&mut s, b"ttl2", &tree_doc());
    assert!(s.expire(b"ttl2", Some(now()), ExpireCond::Always, Nanos::from_millis(2)));
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "expire-now releases");

    // Wheel slice reap.
    set(&mut s, b"wheel", &doc_of_size(1024));
    assert!(s.expire(b"wheel", Some(Nanos::from_millis(10)), ExpireCond::Always, now()));
    let stats = s.expire_tick(Nanos::from_millis(50), ExpiryBudget::default());
    assert_eq!(stats.reaped, 1);
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "wheel releases");

    // TTL rewrite carries the handle: the blob survives an EXPIRE.
    set(&mut s, b"keep", &doc_of_size(1024));
    assert!(s.expire(b"keep", Some(Nanos::from_millis(1_000)), ExpireCond::Always, now()));
    let read = s.json_get(b"keep", now()).expect("ok").expect("alive");
    assert_eq!(read.version, 2, "EXPIRE rewrite bumps like any key mutation");
    reconcile(&s);
    assert!(s.del(b"keep", now()));

    // SCAN reap.
    set(&mut s, b"scan", &doc_of_size(1024));
    assert!(s.expire(b"scan", Some(Nanos::from_millis(5)), ExpireCond::Always, now()));
    let mut seen = 0;
    let mut cursor = 0;
    loop {
        cursor = s.scan(cursor, 64, Nanos::from_millis(6), |_| seen += 1);
        if cursor == 0 {
            break;
        }
    }
    assert_eq!(seen, 0, "expired doc never emitted");
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "scan reap releases");

    // Eviction victim.
    s.set_eviction_policy(EvictionPolicy::AllKeysRandom);
    set(&mut s, b"victim", &doc_of_size(1024));
    let mut evicted = 0;
    for _ in 0..64 {
        evicted += s.evict_step(5, now()).evicted;
        if evicted > 0 {
            break;
        }
    }
    assert_eq!(evicted, 1, "the single record is the victim");
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "eviction releases");

    // FLUSH.
    set(&mut s, b"f1", &doc_of_size(1024));
    set(&mut s, b"f2", &tree_doc());
    s.flush(now());
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default(), "flush drains the domain");
    assert_eq!(s.doc_live_bytes(), 0);
}

#[test]
fn rename_transfers_and_copy_deep_copies() {
    let mut s = tree_store();
    let doc = doc_of_size(1024);
    set(&mut s, b"src", &doc);
    let inf_store::JsonLogDecision::Full { lineage: source_lineage, .. } =
        s.json_log_full(b"src", now()).expect("source full")
    else {
        unreachable!("full probe")
    };
    let before = s.doc_domain();
    // RENAME: the handle moves; totals unchanged; source gone.
    assert!(s.rename(b"src", b"dst", now()).expect("rename"));
    assert_eq!(s.doc_domain(), before, "transfer keeps the domain identical");
    assert!(s.json_get(b"src", now()).expect("ok").is_none());
    assert_eq!(s.json_freeze(b"dst", now()).expect("doc").expect("present"), doc);
    let inf_store::JsonLogDecision::Full { lineage: destination_lineage, .. } =
        s.json_log_full(b"dst", now()).expect("destination full")
    else {
        unreachable!("full probe")
    };
    assert!(destination_lineage > source_lineage, "destination key gets a fresh incarnation");
    // RENAME over an existing document releases the destination's payload.
    set(&mut s, b"src2", &tree_doc());
    assert!(s.rename(b"src2", b"dst", now()).expect("rename over doc"));
    let d = s.doc_domain();
    assert_eq!(d.docs_live, 1, "old dst payload released");
    assert_eq!(d.tape_bytes, 0, "the blob died with the old dst");
    reconcile(&s);

    // COPY: deep copy — two independent documents, byte-identical frozen.
    let mut s = store();
    set(&mut s, b"src", &doc);
    assert_eq!(s.copy(b"src", b"copy", false, now()).expect("copy"), CopyResult::Copied);
    let d = s.doc_domain();
    assert_eq!(d.docs_live, 2);
    assert_eq!(d.tape_bytes, 2 * doc.len() as u64, "two independent blobs");
    assert_eq!(s.json_freeze(b"copy", now()).expect("doc").expect("present"), doc);
    // Mutating the copy leaves the source untouched.
    assert!(s.json_replace(b"copy", &doc_of_size(100), now()).expect("replace"));
    assert_eq!(s.json_freeze(b"src", now()).expect("doc").expect("present"), doc);
    // Deleting both drains everything (no double-free, no leak).
    assert!(s.del(b"src", now()));
    assert!(s.del(b"copy", now()));
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default());
}

#[test]
fn cross_db_copy_re_tiers_in_the_destination_store() {
    let mut ks = Keyspace::new(tree_config());
    let doc = tree_doc();
    ks.db_mut(0).json_set(b"k", &doc, JsonSetOptions::default(), now()).expect("set");
    assert_eq!(ks.copy_between(0, b"k", 3, b"k", false, now()).expect("copy"), CopyResult::Copied);
    let frozen = ks.db_mut(3).json_freeze(b"k", now()).expect("doc").expect("present");
    assert_eq!(frozen, doc, "destination holds an independent, identical document");
    assert!(ks.db_mut(3).doc_domain().arena_bytes > 0, "re-tiered into db3's own arena");
    assert!(ks.db_mut(0).del(b"k", now()));
    assert!(ks.db_mut(3).del(b"k", now()));
    assert_eq!(ks.db_mut(0).doc_domain(), inf_store::DocDomain::default());
    assert_eq!(ks.db_mut(3).doc_domain(), inf_store::DocDomain::default());
}

#[test]
fn ingest_failure_aborts_leak_free_and_keeps_the_old_record() {
    // Doc arena budget too small for the blob: json_set fails typed,
    // nothing leaks, and a previous document survives untouched.
    let cfg = StoreConfig {
        doc_arena: ArenaConfig { chunk_size: 64 << 10, max_resident: Some(64 << 10) },
        ..StoreConfig::default()
    };
    let mut s = CellStore::new(cfg);
    let small = doc_of_size(600); // blob tier, fits the budget
    set(&mut s, b"k", &small);
    // Blob-tier exhaustion: fill the budget with blobs until one refuses.
    let filler = doc_of_size(1500);
    let mut stored = 0u32;
    let refused = loop {
        let key = format!("fill:{stored}");
        match s.json_set(key.as_bytes(), &filler, JsonSetOptions::default(), now()) {
            Ok(JsonSetOutcome::Applied) => stored += 1,
            Err(e) => break e,
            other => panic!("unexpected outcome {other:?}"),
        }
        assert!(stored < 1_000, "budget must bite before 1000 blobs");
    };
    assert_eq!(refused, OpError::OutOfMemory);
    let before = s.doc_domain();
    assert_eq!(before.docs_live as u32, stored + 1, "every accepted doc is live");
    reconcile(&s);
    assert_eq!(s.json_get(b"k", now()).unwrap().unwrap().version, 1, "old record intact");
    assert_eq!(s.json_freeze(b"k", now()).expect("doc").expect("present"), small);
    // Tree-tier failure aborts the morph leak-free too.
    let huge_tree =
        model::encode(&Value::Arr((0..40_000i64).map(Value::I64).collect())).expect("encodes");
    assert_eq!(
        s.json_set(b"tree", &huge_tree, JsonSetOptions::default(), now()),
        Err(OpError::OutOfMemory)
    );
    assert_eq!(s.doc_domain(), before, "aborted morph released everything");
    reconcile(&s);
}

#[test]
fn ttl_semantics_ride_json_set_options() {
    let mut s = store();
    let doc = doc_of_size(100);
    let opts = JsonSetOptions {
        expire: SetExpire::At(Nanos::from_millis(500)),
        ..JsonSetOptions::default()
    };
    assert_eq!(s.json_set(b"k", &doc, opts, now()).expect("set"), JsonSetOutcome::Applied);
    assert!(s.json_get(b"k", Nanos::from_millis(400)).expect("ok").is_some());
    assert!(s.json_get(b"k", Nanos::from_millis(600)).expect("ok").is_none(), "deadline fires");
    assert_eq!(s.doc_domain(), inf_store::DocDomain::default());
    // Keep preserves, Clear drops.
    assert_eq!(s.json_set(b"k", &doc, opts, now()).expect("set"), JsonSetOutcome::Applied);
    let keep = JsonSetOptions { expire: SetExpire::Keep, ..JsonSetOptions::default() };
    assert_eq!(s.json_set(b"k", &doc, keep, now()).expect("keep"), JsonSetOutcome::Applied);
    assert!(s.json_get(b"k", Nanos::from_millis(600)).expect("ok").is_none(), "TTL kept");
    assert_eq!(s.json_set(b"k", &doc, opts, now()).expect("set"), JsonSetOutcome::Applied);
    let clear = JsonSetOptions::default();
    assert_eq!(s.json_set(b"k", &doc, clear, now()).expect("clear"), JsonSetOutcome::Applied);
    assert!(s.json_get(b"k", Nanos::from_millis(600)).expect("ok").is_some(), "TTL cleared");
}
