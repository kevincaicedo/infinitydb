//! M3-S17 document durability contracts: deterministic cadence, modular
//! exactly-once replay, typed checkpoint images, and version-bearing state
//! digests (ADR-0043 D5–D7).
#![cfg(feature = "doc")]

use inf_doc::apply::{ApplyOp, Number};
use inf_doc::model::{self, Value};
use inf_doc::path::compile;
use inf_doc::{JsonParser, encode_apply_op};
use inf_foundation::time::Nanos;
use inf_log::{DOC_VERSION_MASK, DocLineage, FsyncClass, NsId, RecordView};
use inf_store::{
    CellStore, CheckpointImage, JsonLogDecision, JsonScalarPatch, JsonSetOptions, Keyspace, NsMode,
    NsSpec, ReplayError, ReplayOutcome, StoreConfig, WallAnchor,
};

const NOW: Nanos = Nanos::from_millis(1);
const NS: NsId = NsId(16);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
const LINEAGE: DocLineage = DocLineage::FIRST;
const LINEAGE_2: DocLineage = match DocLineage::new(2) {
    Some(lineage) => lineage,
    None => unreachable!(),
};
const LINEAGE_3: DocLineage = match DocLineage::new(3) {
    Some(lineage) => lineage,
    None => unreachable!(),
};

fn set_doc(store: &mut CellStore, key: &[u8], idoc: &[u8]) {
    store.json_set(key, idoc, JsonSetOptions::default(), NOW).expect("set");
}

fn durable_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"docs".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
    })
    .expect("namespace");
    ks
}

#[test]
fn cadence_substitutes_one_full_at_count_and_byte_boundaries() {
    let idoc = model::encode(&Value::Obj(vec![
        ("n".into(), Value::F64(1.5)),
        ("pad".into(), Value::Str("x".repeat(5_000))),
    ]))
    .expect("fixture");
    let mut store = CellStore::new(StoreConfig::default());
    set_doc(&mut store, b"doc", &idoc);
    let program = compile(b"$.n").expect("path");
    let op = ApplyOp::NumIncrBy(Number::F64(0.25));

    for mutation in 1..=64 {
        assert!(matches!(
            store.json_patch_scalar(b"doc", &program, &op, NOW),
            Ok(Some(JsonScalarPatch::Number(_)))
        ));
        let decision =
            store.json_log_delta_decision(b"doc", 32, 9, NOW).expect("document remains live");
        if mutation < 64 {
            assert!(matches!(decision, JsonLogDecision::Delta { .. }));
        } else {
            let JsonLogDecision::Full { version, idoc, .. } = decision else {
                panic!("the 64th delta is replaced by one full image")
            };
            assert_eq!(version, 65);
            assert_eq!(idoc, store.json_freeze(b"doc", NOW).unwrap().unwrap());
        }
    }

    // A full resets cadence: the next mutation is a delta again.
    store.json_patch_scalar(b"doc", &program, &op, NOW).unwrap();
    assert!(matches!(
        store.json_log_delta_decision(b"doc", 32, 9, NOW),
        Some(JsonLogDecision::Delta { base_version: 65, .. })
    ));

    // The byte-ratio arm independently substitutes a full image.
    let mut second = CellStore::new(StoreConfig::default());
    set_doc(&mut second, b"doc", &idoc);
    second.json_patch_scalar(b"doc", &program, &op, NOW).unwrap();
    let bytes = second.json_log_image_bytes(b"doc", NOW).expect("canonical size");
    assert!(matches!(
        second.json_log_delta_decision(b"doc", bytes, 9, NOW),
        Some(JsonLogDecision::Full { version: 2, .. })
    ));

    // A single operand at least as large as the document is never called
    // a delta even when accumulated bytes are still below the threshold.
    let mut third = CellStore::new(StoreConfig::default());
    set_doc(&mut third, b"doc", &idoc);
    third.json_patch_scalar(b"doc", &program, &op, NOW).unwrap();
    assert!(matches!(
        third.json_log_delta_decision(b"doc", 1, bytes, NOW),
        Some(JsonLogDecision::Full { version: 2, .. })
    ));
}

#[test]
fn delete_recreate_allocates_a_fresh_monotonic_lineage() {
    let idoc = JsonParser::new().parse(br#"{"n":1}"#).expect("fixture");
    let mut store = CellStore::new(StoreConfig::default());
    set_doc(&mut store, b"doc", &idoc);
    let Some(JsonLogDecision::Full { lineage: first, .. }) = store.json_log_full(b"doc", NOW)
    else {
        panic!("document full")
    };
    assert!(store.del(b"doc", NOW));
    set_doc(&mut store, b"doc", &idoc);
    let Some(JsonLogDecision::Full { lineage: second, .. }) = store.json_log_full(b"doc", NOW)
    else {
        panic!("document full")
    };
    assert!(second > first, "a key incarnation never reuses delta identity");
}

#[test]
fn replay_rule_handles_wrap_stale_missing_and_gap() {
    let initial = JsonParser::new().parse(br#"{"n":1}"#).expect("fixture");
    let program = compile(b"$.n").expect("path");
    let op = ApplyOp::NumIncrBy(Number::I64(1));
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&op, &mut operand) as u8;
    let mut ks = durable_keyspace();

    let full = RecordView::DocFull {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE,
        version: DOC_VERSION_MASK,
        idoc: &initial,
    };
    assert_eq!(ks.apply_record(&full, NOW, ANCHOR).unwrap(), ReplayOutcome::Applied);
    let wrap = RecordView::DocDelta {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE,
        base_version: DOC_VERSION_MASK,
        match_count: 1,
        post_len: initial.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    assert_eq!(ks.apply_record(&wrap, NOW, ANCHOR).unwrap(), ReplayOutcome::Applied);
    assert_eq!(
        ks.ns_store_mut(NS).unwrap().json_get(b"doc", NOW).unwrap().unwrap().version,
        0,
        "u24 version wraps exactly"
    );
    assert_eq!(
        ks.apply_record(&wrap, NOW, ANCHOR).unwrap(),
        ReplayOutcome::SkippedDocDeltaStale,
        "re-applying the covered delta is stale across wrap"
    );

    let missing = RecordView::DocDelta {
        ns: NS,
        key: b"missing",
        lineage: LINEAGE,
        base_version: 1,
        match_count: 1,
        post_len: initial.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    assert_eq!(
        ks.apply_record(&missing, NOW, ANCHOR).unwrap(),
        ReplayOutcome::SkippedDocDeltaMissing
    );

    let gap = RecordView::DocDelta {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE,
        base_version: 1,
        match_count: 1,
        post_len: initial.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    assert!(matches!(
        ks.apply_record(&gap, NOW, ANCHOR),
        Err(ReplayError::CorruptDocument("document delta base version is ahead"))
    ));
}

#[test]
fn replay_skips_prior_incarnation_instead_of_binding_by_version() {
    let current = JsonParser::new().parse(br#"{"other":true}"#).expect("fixture");
    let old_post = JsonParser::new().parse(br#"{"n":2}"#).expect("fixture");
    let program = compile(b"$.n").expect("path");
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&ApplyOp::NumIncrBy(Number::I64(1)), &mut operand) as u8;
    let mut ks = durable_keyspace();
    ks.apply_record(
        &RecordView::DocFull {
            ns: NS,
            key: b"doc",
            lineage: LINEAGE_2,
            version: 1,
            idoc: &current,
        },
        NOW,
        ANCHOR,
    )
    .expect("checkpoint image");
    let before = ks.state_digest(NOW);
    let old_delta = RecordView::DocDelta {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE,
        base_version: 1,
        match_count: 1,
        post_len: old_post.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    assert_eq!(
        ks.apply_record(&old_delta, NOW, ANCHOR).unwrap(),
        ReplayOutcome::SkippedDocDeltaStale
    );
    assert_eq!(ks.state_digest(NOW), before, "old incarnation cannot touch the new document");

    let future_delta = RecordView::DocDelta {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE_3,
        base_version: 1,
        match_count: 1,
        post_len: old_post.len() as u32,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    assert!(matches!(
        ks.apply_record(&future_delta, NOW, ANCHOR),
        Err(ReplayError::CorruptDocument("document delta lineage is ahead"))
    ));
    assert_eq!(ks.state_digest(NOW), before, "future lineage fails before mutation");

    ks.apply_record(
        &RecordView::StringPostImage { ns: NS, key: b"doc", value: b"plain" },
        NOW,
        ANCHOR,
    )
    .expect("later type change");
    assert_eq!(
        ks.apply_record(&old_delta, NOW, ANCHOR).unwrap(),
        ReplayOutcome::SkippedDocDeltaStale,
        "a later non-document incarnation is also a stale delta skip"
    );
}

#[test]
fn replay_uses_recorded_bounds_not_lowered_boot_config() {
    let initial = JsonParser::new().parse(br#"{"a":[1,2],"pad":"xxxxxxxx"}"#).expect("fixture");
    let expected = JsonParser::new().parse(br#"{"a":[2,3],"pad":"xxxxxxxx"}"#).expect("fixture");
    let mut ks = Keyspace::new(StoreConfig {
        doc_max_bytes: 8,
        doc_max_path_matches: 1,
        ..StoreConfig::default()
    });
    ks.ns_create(NsSpec {
        id: NS,
        name: b"docs".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
    })
    .expect("namespace");
    ks.apply_record(
        &RecordView::DocFull { ns: NS, key: b"doc", lineage: LINEAGE, version: 1, idoc: &initial },
        NOW,
        ANCHOR,
    )
    .expect("full images use the format bound");
    let program = compile(b"$.a[*]").expect("path");
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&ApplyOp::NumIncrBy(Number::I64(1)), &mut operand) as u8;
    let outcome = ks
        .apply_record(
            &RecordView::DocDelta {
                ns: NS,
                key: b"doc",
                lineage: LINEAGE,
                base_version: 1,
                match_count: 2,
                post_len: expected.len() as u32,
                opcode,
                program: program.as_bytes(),
                operand: &operand,
            },
            NOW,
            ANCHOR,
        )
        .expect("recorded acceptance bounds survive config reduction");
    assert_eq!(outcome, ReplayOutcome::Applied);
    assert_eq!(ks.ns_store_mut(NS).unwrap().json_freeze(b"doc", NOW).unwrap().unwrap(), expected);
}

#[test]
fn replay_rejects_root_delete_atomically() {
    let initial = JsonParser::new().parse(br#"{"n":1}"#).expect("fixture");
    let program = compile(b"$").expect("root path");
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&ApplyOp::Del, &mut operand) as u8;
    let mut ks = durable_keyspace();
    ks.apply_record(
        &RecordView::DocFull { ns: NS, key: b"doc", lineage: LINEAGE, version: 1, idoc: &initial },
        NOW,
        ANCHOR,
    )
    .expect("initial image");
    let before = ks.state_digest(NOW);
    let before_domain = ks.ns_store(NS).expect("store").doc_domain();

    let error = ks
        .apply_record(
            &RecordView::DocDelta {
                ns: NS,
                key: b"doc",
                lineage: LINEAGE,
                base_version: 1,
                match_count: 1,
                post_len: initial.len() as u32,
                opcode,
                program: program.as_bytes(),
                operand: &operand,
            },
            NOW,
            ANCHOR,
        )
        .expect_err("root delete must use the generic key Delete record");
    assert!(matches!(error, ReplayError::InvalidMutation(inf_doc::ApplyError::RootDelete)));
    assert_eq!(ks.state_digest(NOW), before);
    assert_eq!(ks.ns_store(NS).expect("store").doc_domain(), before_domain);
}

#[test]
fn checkpoint_walk_and_digest_use_canonical_bytes_and_version() {
    let idoc = JsonParser::new().parse(br#"{"n":40}"#).expect("fixture");
    let mut ks = durable_keyspace();
    let store = ks.ns_store_mut(NS).expect("store");
    set_doc(store, b"doc", &idoc);
    let path = compile(b"$.n").expect("path");
    store.json_patch_scalar(b"doc", &path, &ApplyOp::NumIncrBy(Number::I64(2)), NOW).unwrap();
    let frozen = store.json_freeze(b"doc", NOW).unwrap().unwrap();
    let mut seen = None;
    let cursor = store.scan_checkpoint_images(0, 64, NOW, |key, image, expiry| {
        let CheckpointImage::JsonDoc { lineage, version, idoc } = image else {
            panic!("document dispatches as DocFull material")
        };
        seen = Some((key.to_vec(), lineage, version, idoc.to_vec(), expiry));
    });
    assert_eq!(cursor, 0);
    assert_eq!(seen, Some((b"doc".to_vec(), LINEAGE, 2, frozen, None)));

    let before = ks.state_digest(NOW);
    let replacement = JsonParser::new().parse(br#"{"n":42}"#).expect("fixture");
    // Same canonical value but a different exact version is a different
    // logical replay state (M6 WATCH consumes this epoch).
    ks.apply_record(
        &RecordView::DocFull {
            ns: NS,
            key: b"doc",
            lineage: LINEAGE,
            version: 9,
            idoc: &replacement,
        },
        NOW,
        ANCHOR,
    )
    .expect("test replay");
    assert_ne!(ks.state_digest(NOW), before);
}
