//! M3-S17 evidence generator: exact delta/full byte ratio for the named
//! 1 KiB path-mutation mix, plus CPU-side replay over a configurable
//! document cell with deep version histories.
//!
//! Default gate run (large: ~1 GiB document payload):
//! `taskset -c 4 cargo bench -p inf-store --bench doc_durability`
//!
//! Quick rehearsal:
//! `INF_DOC_REPLAY_DOCS=100000 INF_DOC_REPLAY_REPS=1 cargo bench ...`

use std::time::Instant;

use inf_doc::TapeDoc;
use inf_doc::apply::{ApplyOp, Number, apply};
use inf_doc::encode_apply_op;
use inf_doc::model::{self, Value};
use inf_doc::path::{EvalLimits, PathProgram, compile};
use inf_foundation::time::Nanos;
use inf_log::{DocLineage, FsyncClass, NsId, RecordView};
use inf_store::{
    CellStore, JsonLogDecision, JsonScalarPatch, JsonSetOptions, Keyspace, NsMode, NsSpec,
    ReplayOutcome, StoreConfig, WallAnchor,
};

const NS: NsId = NsId(16);
const NOW: Nanos = Nanos::from_millis(1);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };
const LINEAGE: DocLineage = DocLineage::FIRST;

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

fn gate_doc() -> Vec<u8> {
    let mut pad = 980usize;
    loop {
        let idoc = model::encode(&Value::Obj(vec![
            ("n".into(), Value::I64(1_000_000)),
            ("enabled".into(), Value::Bool(false)),
            ("s".into(), Value::Str("seed".into())),
            ("a".into(), Value::Arr(vec![Value::I64(1)])),
            ("o".into(), Value::Obj(vec![("x".into(), Value::I64(0))])),
            ("pad".into(), Value::Str("x".repeat(pad))),
        ]))
        .expect("fixture");
        match idoc.len().cmp(&1024) {
            std::cmp::Ordering::Equal => return idoc,
            std::cmp::Ordering::Less => pad += 1024 - idoc.len(),
            std::cmp::Ordering::Greater => pad -= idoc.len() - 1024,
        }
    }
}

fn keyspace(documents: usize) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig { initial_keys: documents, ..Default::default() });
    ks.ns_create(NsSpec {
        id: NS,
        name: b"bench".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("namespace");
    ks
}

fn apply_mutation(store: &mut CellStore, program: &PathProgram, op: &ApplyOp<'_>) {
    match store.json_patch_scalar(b"doc", program, op, NOW).expect("scalar probe") {
        Some(JsonScalarPatch::Number(_) | JsonScalarPatch::Toggled(_)) => return,
        Some(JsonScalarPatch::Unsupported) => {}
        Some(JsonScalarPatch::Missing | JsonScalarPatch::Skipped) | None => {
            panic!("the fixed volume-mix mutation must apply")
        }
    }
    let frozen = store.json_freeze(b"doc", NOW).expect("freeze").expect("document");
    let doc = TapeDoc::from_validated_bytes(&frozen);
    let outcome = apply(&doc, program, op, &EvalLimits::default(), store.doc_max_bytes())
        .expect("valid mutation");
    let bytes = outcome.bytes.expect("the fixed volume-mix mutation changes bytes");
    assert!(store.json_replace(b"doc", &bytes, NOW).expect("commit"));
}

fn volume_ratio(idoc: &[u8], histories: usize) -> (u64, u64, f64) {
    let programs = [
        compile(b"$.n").expect("numeric path"),
        compile(b"$.enabled").expect("boolean path"),
        compile(b"$.s").expect("string path"),
        compile(b"$.a").expect("array path"),
        compile(b"$.o").expect("object path"),
    ];
    let array = model::encode_fragment(&Value::Arr(vec![Value::I64(1)])).expect("fragment");
    let mut store = CellStore::new(StoreConfig::default());
    store.json_set(b"doc", idoc, JsonSetOptions::default(), NOW).expect("initial document");
    let mut actual = 0u64;
    let mut all_full = 0u64;
    for mutation in 0..histories {
        let merge = model::encode_fragment(&Value::Obj(vec![(
            "x".into(),
            Value::I64(mutation as i64 + 1),
        )]))
        .expect("fragment");
        // Named 20-op mix: 40% numeric, 20% toggle, 15% string append,
        // 15% array append, 10% merge. The real store evolves between
        // samples, so full-image size and cadence are implementation-exact.
        let (program, op) = match mutation % 20 {
            0..=7 => (&programs[0], ApplyOp::NumIncrBy(Number::I64(1))),
            8..=11 => (&programs[1], ApplyOp::Toggle),
            12..=14 => (&programs[2], ApplyOp::StrAppend(b"tail")),
            15..=17 => (&programs[3], ApplyOp::ArrAppend { elements: &array }),
            _ => (&programs[4], ApplyOp::Merge { patch: &merge }),
        };
        apply_mutation(&mut store, program, &op);
        let mut operand = Vec::new();
        let opcode = encode_apply_op(&op, &mut operand) as u8;
        let version = store.json_get(b"doc", NOW).expect("read").expect("document").version;
        let current = store.json_freeze(b"doc", NOW).expect("freeze").expect("document");
        all_full +=
            RecordView::DocFull { ns: NS, key: b"doc", lineage: LINEAGE, version, idoc: &current }
                .encoded_len() as u64;
        let delta_len = RecordView::DocDelta {
            ns: NS,
            key: b"doc",
            lineage: LINEAGE,
            base_version: version.wrapping_sub(1) & inf_log::DOC_VERSION_MASK,
            match_count: 1,
            post_len: current.len() as u32,
            opcode,
            program: program.as_bytes(),
            operand: &operand,
        }
        .encoded_len();
        match store
            .json_log_delta_decision(b"doc", delta_len, operand.len(), NOW)
            .expect("document remains live")
        {
            JsonLogDecision::Delta { .. } => actual += delta_len as u64,
            JsonLogDecision::Full { lineage, version, idoc, .. } => {
                actual += RecordView::DocFull { ns: NS, key: b"doc", lineage, version, idoc: &idoc }
                    .encoded_len() as u64;
            }
        }
    }
    (actual, all_full, actual as f64 / all_full as f64)
}

fn replay_once(documents: usize, histories: usize, idoc: &[u8]) -> f64 {
    let mut ks = keyspace(documents);
    for index in 0..documents {
        let key = key_of(index);
        let outcome = ks
            .apply_record(
                &RecordView::DocFull { ns: NS, key: &key, lineage: LINEAGE, version: 1, idoc },
                NOW,
                ANCHOR,
            )
            .expect("initial full");
        assert_eq!(outcome, ReplayOutcome::Applied);
    }
    let program = compile(b"$.n").expect("path");
    let op = ApplyOp::NumIncrBy(Number::I64(1));
    let mut operand = Vec::new();
    let opcode = encode_apply_op(&op, &mut operand) as u8;
    let start = Instant::now();
    for depth in 0..histories {
        for index in 0..documents {
            let key = key_of(index);
            let outcome = ks
                .apply_record(
                    &RecordView::DocDelta {
                        ns: NS,
                        key: &key,
                        lineage: LINEAGE,
                        base_version: depth as u32 + 1,
                        match_count: 1,
                        post_len: idoc.len() as u32,
                        opcode,
                        program: program.as_bytes(),
                        operand: &operand,
                    },
                    NOW,
                    ANCHOR,
                )
                .expect("valid sequential delta");
            assert_eq!(outcome, ReplayOutcome::Applied);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let sample = ks.ns_store_mut(NS).expect("store").json_get(&key_of(0), NOW).unwrap().unwrap();
    assert_eq!(sample.version, 1 + histories as u32);
    elapsed
}

fn main() {
    let documents = env_usize("INF_DOC_REPLAY_DOCS", 1_000_000);
    let histories = env_usize("INF_DOC_REPLAY_HISTORY", 64);
    let reps = env_usize("INF_DOC_REPLAY_REPS", 3);
    let idoc = gate_doc();
    let (delta_bytes, full_bytes, ratio) = volume_ratio(&idoc, histories);
    println!(
        "volume documents=normalized-1KiB history={histories} delta_cadence_bytes={delta_bytes} full_every_mutation_bytes={full_bytes} ratio={ratio:.6}"
    );
    for rep in 1..=reps {
        let seconds = replay_once(documents, histories, &idoc);
        let mutations = documents as f64 * histories as f64;
        let equivalent_gbps = mutations * idoc.len() as f64 / seconds / 1_000_000_000.0;
        println!(
            "replay rep={rep} documents={documents} history={histories} mutations={} seconds={seconds:.6} equivalent_gbps={equivalent_gbps:.6}",
            mutations as u64,
        );
    }
}
