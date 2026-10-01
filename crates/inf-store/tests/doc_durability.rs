//! M3-S17 document durability contracts: deterministic cadence, modular
//! exactly-once replay, typed checkpoint images, and version-bearing state
//! digests (ADR-0043 D5–D7).
#![cfg(feature = "doc")]

use inf_doc::apply::{ApplyError, ApplyOp, Number};
use inf_doc::limits::DEPTH_MAX;
use inf_doc::model::{self, Value};
use inf_doc::path::compile;
use inf_doc::{CanonicalDoc, HEADER_LEN, JsonParser, encode_apply_op};
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
    store
        .json_set(
            key,
            &CanonicalDoc::validate(idoc).expect("canonical fixture"),
            JsonSetOptions::default(),
            NOW,
        )
        .expect("set");
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
        tier: None,
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
        tier: None,
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

// ---- the stored-document bound (ADR-0169 D2/D5, Falsifier 4) -----------------

/// The largest body a path mutation may produce: the record value cap
/// (2^24 − 1) less the document value prefix (15) and the idoc header (8).
const BODY_BYTES_ACCEPTED_MAX: usize = (1 << 24) - 1 - 15 - 8;
/// `{"s":…}` around a str24: object header 4, key `"s"` 2, string header 4.
const S_DOC_BODY_OVERHEAD: usize = 10;
/// The appended payload: small beside the document, so the cadence would
/// log the mutation as a delta.
const SIZE_ROW_PAYLOAD: usize = 1_000;

fn s_doc(string_len: usize) -> Vec<u8> {
    let value = Value::Obj(vec![("s".into(), Value::Str("x".repeat(string_len)))]);
    model::encode(&value).expect("under the format ceiling")
}

/// One leg's end: accepted and reproducing the live bytes, or the typed
/// refusal it answered.
type Leg = Result<(), String>;

/// A `STRAPPEND` whose output body is exactly `body` bytes: the live
/// writer's verdict, the store before and after, and — when the writer
/// accepted — the `DocFull` and `DocDelta` replays of what it would log.
struct SizeRow {
    live: Result<Vec<u8>, inf_doc::ApplyError>,
    before: (Vec<u8>, u32),
    after: (Vec<u8>, u32),
    full: Option<Leg>,
    delta: Option<Leg>,
}

fn stored(store: &mut CellStore) -> (Vec<u8>, u32) {
    let version = store.json_get(b"doc", NOW).unwrap().expect("document").version;
    (store.json_freeze(b"doc", NOW).unwrap().expect("document"), version)
}

fn replay_full(post: &[u8]) -> Leg {
    let mut ks = durable_keyspace();
    let full =
        RecordView::DocFull { ns: NS, key: b"doc", lineage: LINEAGE, version: 2, idoc: post };
    ks.apply_record(&full, NOW, ANCHOR).map_err(|error| format!("{error:?}"))?;
    let replayed = ks.ns_store_mut(NS).unwrap().json_freeze(b"doc", NOW).unwrap();
    if replayed.as_deref() != Some(post) {
        return Err("DocFull replay differs from the live bytes".into());
    }
    Ok(())
}

/// A durable keyspace holding `pre` at `doc`, version 1, from a `DocFull`.
fn keyspace_holding(pre: &[u8]) -> Keyspace {
    let mut ks = durable_keyspace();
    let image =
        RecordView::DocFull { ns: NS, key: b"doc", lineage: LINEAGE, version: 1, idoc: pre };
    ks.apply_record(&image, NOW, ANCHOR).expect("the pre-image fits a DocFull");
    ks
}

/// The one-match `DocDelta` on `doc` at base version 1, in the record
/// codec's bytes.
fn delta_wire(program: &inf_doc::PathProgram, op: &ApplyOp<'_>, post_len: u32) -> Vec<u8> {
    delta_wire_matching(program, op, post_len, 1)
}

/// [`delta_wire`] for a record whose program matched `match_count` values.
fn delta_wire_matching(
    program: &inf_doc::PathProgram,
    op: &ApplyOp<'_>,
    post_len: u32,
    match_count: u32,
) -> Vec<u8> {
    let mut operand = Vec::new();
    let opcode = encode_apply_op(op, &mut operand) as u8;
    let delta = RecordView::DocDelta {
        ns: NS,
        key: b"doc",
        lineage: LINEAGE,
        base_version: 1,
        match_count,
        post_len,
        opcode,
        program: program.as_bytes(),
        operand: &operand,
    };
    let mut wire = Vec::new();
    delta.encode_into(&mut wire);
    wire
}

/// The delta crosses the real record codec: encode, `decode_record`, then
/// `Keyspace::apply_record`, as recovery reads it.
fn replay_delta(pre: &[u8], program: &inf_doc::PathProgram, op: &ApplyOp<'_>, post: &[u8]) -> Leg {
    let mut ks = keyspace_holding(pre);
    let post_len = u32::try_from(post.len()).expect("a document length fits u32");
    let wire = delta_wire(program, op, post_len);
    let (decoded, _) = inf_log::decode_record(&wire).map_err(|error| format!("{error:?}"))?;
    ks.apply_record(&decoded, NOW, ANCHOR).map_err(|error| format!("{error:?}"))?;
    let replayed = ks.ns_store_mut(NS).unwrap().json_freeze(b"doc", NOW).unwrap();
    if replayed.as_deref() != Some(post) {
        return Err("DocDelta replay differs from the live bytes".into());
    }
    Ok(())
}

fn size_row(body: usize) -> SizeRow {
    let string_len = body - S_DOC_BODY_OVERHEAD;
    let pre = s_doc(string_len - SIZE_ROW_PAYLOAD);
    let payload = vec![b'y'; SIZE_ROW_PAYLOAD];
    let op = ApplyOp::StrAppend(&payload);
    let program = compile(b"$.s").expect("path");
    let mut store = CellStore::new(StoreConfig::default());
    set_doc(&mut store, b"doc", &pre);
    let before = stored(&mut store);
    let doc = inf_doc::TapeDoc::from_validated_bytes(&before.0);
    let limits = inf_doc::path::EvalLimits::default();
    let live = inf_doc::apply::apply(&doc, &program, &op, &limits, store.doc_limits())
        .map(|outcome| outcome.document.expect("the append edits"));
    if let Ok(post) = &live {
        assert!(store.json_replace(b"doc", post, NOW).expect("commit"), "the key is live");
    }
    let live = live.map(|post| post.as_bytes().to_vec());
    let after = stored(&mut store);
    let (full, delta) = match &live {
        Ok(post) => (Some(replay_full(post)), Some(replay_delta(&pre, &program, &op, post))),
        Err(_) => (None, None),
    };
    SizeRow { live, before, after, full, delta }
}

/// ADR-0169 Falsifier 4: every record the live writer would log for a
/// mutation replays to its bytes, and the writer refuses a body past
/// `BODY_BYTES_ACCEPTED_MAX` in `apply`, before any output byte, with
/// nothing stored.
fn assert_size_row(body: usize) {
    let row = size_row(body);
    if row.live.is_ok() {
        let readers = (row.full.clone().expect("logged"), row.delta.clone().expect("logged"));
        assert_eq!(readers, (Ok(()), Ok(())), "body {body}: a logged record replays");
    }
    if body <= BODY_BYTES_ACCEPTED_MAX {
        assert!(row.live.is_ok(), "body {body}: accepted, got {:?}", row.live.as_ref().err());
        assert_eq!(row.after.1, row.before.1 + 1, "one version bump");
    } else {
        assert_eq!(row.live.as_ref().err(), Some(&inf_doc::ApplyError::TooLarge), "body {body}");
        assert!(row.after == row.before, "body {body}: a refusal stores nothing");
    }
}

#[test]
fn size_row_body_16777192() {
    assert_size_row(BODY_BYTES_ACCEPTED_MAX);
}

#[test]
fn size_row_body_16777193() {
    assert_size_row(BODY_BYTES_ACCEPTED_MAX + 1);
}

/// The idoc is exactly 2^24 bytes: a `post_len` that wraps to 0.
#[test]
fn size_row_body_16777208() {
    assert_size_row((1 << 24) - inf_doc::HEADER_LEN);
}

/// The idoc is 2^24 + 7 bytes: a `post_len` that wraps to 7.
#[test]
fn size_row_body_16777215() {
    assert_size_row((1 << 24) - 1);
}

// ---- replay's own refusals (ADR-0169 D2/D6, ADR-0042 A1 §Existing data) ------

/// What a planted `DocDelta` did to the keyspace replaying it.
struct Planted {
    verdict: Result<ReplayOutcome, ReplayError>,
    /// The stored bytes after the replay.
    after: Vec<u8>,
    /// The stored bytes, the version and the state digest all held.
    unchanged: bool,
}

/// Replay a planted `DocDelta` over `pre` as recovery reads it: the record
/// codec, then `Keyspace::apply_record`.
fn replay_planted(pre: &[u8], path: &[u8], op: &ApplyOp<'_>, post_len: u32) -> Planted {
    let mut ks = keyspace_holding(pre);
    let wire = delta_wire(&compile(path).expect("path"), op, post_len);
    let (decoded, _) = inf_log::decode_record(&wire).expect("a planted delta decodes");
    let before = (stored(ks.ns_store_mut(NS).expect("store")), ks.state_digest(NOW));
    let verdict = ks.apply_record(&decoded, NOW, ANCHOR);
    let after = (stored(ks.ns_store_mut(NS).expect("store")), ks.state_digest(NOW));
    let unchanged = after == before;
    Planted { verdict, after: after.0.0, unchanged }
}

/// A `SetMember` delta no writer logs still replays to a typed verdict:
/// its program matches an object that has the key and, as that member's
/// value, an object that lacks it, so one edit replaces the range the
/// other appends into. The append is superseded, as reverse-order
/// mutation gives. The planner used to keep it: a record carrying the
/// true length then failed `TooLarge`, and one carrying the length the
/// kept append sums to (the pre-image's, here) sliced out of bounds — a
/// cell panic at boot. That record now fails typed and changes nothing.
#[test]
fn replay_of_a_member_append_inside_a_replaced_member_is_typed() {
    let pre = JsonParser::new().parse(br#"{"k":{"k":{}}}"#).expect("fixture");
    let post = JsonParser::new().parse(br#"{"k":{"k":7}}"#).expect("fixture");
    let fragment = fragment_of("7");
    let op = ApplyOp::SetMember { key: b"k", fragment: &fragment };
    let program = compile(b"$..k").expect("path");
    let replay = |recorded_len: usize| {
        let mut ks = keyspace_holding(&pre);
        let post_len = u32::try_from(recorded_len).expect("a small document");
        let wire = delta_wire_matching(&program, &op, post_len, 2);
        let (decoded, _) = inf_log::decode_record(&wire).expect("the planted delta decodes");
        let verdict = ks.apply_record(&decoded, NOW, ANCHOR);
        (verdict, stored(ks.ns_store_mut(NS).expect("store")).0)
    };
    let (verdict, after) = replay(post.len());
    assert!(matches!(verdict, Ok(ReplayOutcome::Applied)), "{verdict:?}");
    assert_eq!(after, post, "the replayed bytes");
    let (verdict, after) = replay(pre.len());
    assert!(matches!(verdict, Err(ReplayError::CorruptDocument(_))), "{verdict:?}");
    assert_eq!(after, pre, "a refused replay changes nothing");
}

/// The header-less canonical fragment of `json`.
fn fragment_of(json: &str) -> Vec<u8> {
    JsonParser::new().parse(json.as_bytes()).expect("fixture")[HEADER_LEN..].to_vec()
}

/// ADR-0169 D6 under ADR-0042 A1 §Existing data: a decodable `DocDelta`
/// whose re-execution nests 129 — what the unbounded writer logged for
/// `JSON.SET doc $.d` of a 128-level value — fails the replay with
/// `DepthExceeded` and changes nothing. The 127-level value applies.
#[test]
fn replay_refuses_a_delta_whose_output_nests_past_the_bound() {
    let pre = JsonParser::new().parse(br#"{"d":0}"#).expect("fixture");
    for levels in [DEPTH_MAX - 1, DEPTH_MAX] {
        let value = format!("{}0{}", "[".repeat(levels), "]".repeat(levels));
        let fragment = fragment_of(&value);
        // The writer's post-image: canonical headers are fixed-width, so the
        // fragment's bytes take the site's place.
        let post_len = pre.len() - fragment_of("0").len() + fragment.len();
        let post_len = u32::try_from(post_len).expect("a small document");
        let op = ApplyOp::SetReplace { fragment: &fragment };
        let planted = replay_planted(&pre, b"$.d", &op, post_len);
        let what = format!("composed depth {}", levels + 1);
        if levels < DEPTH_MAX {
            let verdict = &planted.verdict;
            assert!(matches!(verdict, Ok(ReplayOutcome::Applied)), "{what}: {verdict:?}");
            let expected = JsonParser::new().parse(format!(r#"{{"d":{value}}}"#).as_bytes());
            assert_eq!(Ok(planted.after), expected, "{what}: the replayed bytes");
        } else {
            let verdict = &planted.verdict;
            let refused =
                matches!(verdict, Err(ReplayError::InvalidMutation(ApplyError::DepthExceeded)));
            assert!(refused, "{what}: {verdict:?}");
            assert!(planted.unchanged, "{what}: a refused replay changes nothing");
        }
    }
}

/// ADR-0169 D2/D6: replay clamps the recorded body to the record bound, so
/// a `STRAPPEND` delta the unbounded writer logged past it fails with
/// `TooLarge` before the sink sees it, and changes nothing: body 16,777,193
/// (`post_len` 16,777,201) and 16,777,207 (the u24 maximum, 16,777,215).
/// The bound itself, body 16,777,192, applies.
#[test]
fn replay_clamps_a_delta_to_the_record_bound() {
    let u24_body_max = (1 << 24) - 1 - HEADER_LEN;
    let payload = vec![b'y'; SIZE_ROW_PAYLOAD];
    let op = ApplyOp::StrAppend(&payload);
    for body in [BODY_BYTES_ACCEPTED_MAX, BODY_BYTES_ACCEPTED_MAX + 1, u24_body_max] {
        let pre = s_doc(body - S_DOC_BODY_OVERHEAD - SIZE_ROW_PAYLOAD);
        let post_len = u32::try_from(HEADER_LEN + body).expect("within the u24 field");
        let planted = replay_planted(&pre, b"$.s", &op, post_len);
        let verdict = &planted.verdict;
        if body <= BODY_BYTES_ACCEPTED_MAX {
            assert!(matches!(verdict, Ok(ReplayOutcome::Applied)), "body {body}: {verdict:?}");
            assert_eq!(planted.after.len(), HEADER_LEN + body, "body {body}: the replayed bytes");
        } else {
            let refused =
                matches!(verdict, Err(ReplayError::InvalidMutation(ApplyError::TooLarge)));
            assert!(refused, "body {body}: {verdict:?}");
            assert!(planted.unchanged, "body {body}: a refused replay changes nothing");
        }
    }
}

/// ADR-0169 D2: a `post_len` the record decoder admits (it refuses only 0)
/// but shorter than the idoc header is corrupt, not a body bound. At the
/// header length the body bound is 0, so any output is `TooLarge`. Neither
/// changes anything.
#[test]
fn replay_refuses_a_delta_length_below_the_header() {
    let pre = JsonParser::new().parse(br#"{"n":1}"#).expect("fixture");
    let fragment = fragment_of("2");
    let op = ApplyOp::SetReplace { fragment: &fragment };
    for post_len in [1, HEADER_LEN - 1, HEADER_LEN] {
        let planted = replay_planted(&pre, b"$.n", &op, u32::try_from(post_len).expect("small"));
        let verdict = &planted.verdict;
        let refused = match post_len < HEADER_LEN {
            true => matches!(
                verdict,
                Err(ReplayError::CorruptDocument("document delta length is below the header"))
            ),
            false => matches!(verdict, Err(ReplayError::InvalidMutation(ApplyError::TooLarge))),
        };
        assert!(refused, "post_len {post_len}: {verdict:?}");
        assert!(planted.unchanged, "post_len {post_len}: a refused replay changes nothing");
    }
}
