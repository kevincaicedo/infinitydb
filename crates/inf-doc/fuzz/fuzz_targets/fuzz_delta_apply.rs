//! M3-S17 delta-apply fuzz (ADR-0043 D4/D6): arbitrary program/op/operand
//! bytes are replayed against an arbitrary accepted document (or a diverse
//! canonical fallback). Every rejection is typed; errors and skips leave
//! the logical digest and document accounting unchanged; applied records
//! preserve canonical bytes and exact accounting.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_doc::{JsonParser, TapeDoc};
use inf_foundation::time::Nanos;
use inf_log::{DocLineage, FsyncClass, NsId, RecordView};
use inf_store::{Keyspace, NsMode, NsSpec, ReplayOutcome, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const NOW: Nanos = Nanos::from_millis(1);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

fn keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"fuzz".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
    })
    .expect("fixed namespace");
    ks
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }
    let mut at = 0usize;
    let doc_len = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    at += 2;
    let doc_len = doc_len.min(data.len() - at);
    let candidate = &data[at..at + doc_len];
    at += doc_len;
    if data.len() - at < 7 {
        return;
    }
    let program_len = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    at += 2;
    let program_len = program_len.min(data.len() - at - 4);
    let program = &data[at..at + program_len];
    at += program_len;
    let opcode = data[at];
    let base_version =
        u32::from_le_bytes([data[at + 1], data[at + 2], data[at + 3], 0]);
    let operand = &data[at + 4..];

    let fallback = JsonParser::new()
        .parse(br#"{"n":7,"b":false,"s":"x","a":[1,2],"o":{"k":1}}"#)
        .expect("fixed document");
    let idoc = TapeDoc::from_bytes(candidate)
        .ok()
        .filter(|_| candidate[3] == 0)
        .map_or(fallback.as_slice(), |_| candidate);

    let mut ks = keyspace();
    ks.apply_record(
        &RecordView::DocFull {
            ns: NS,
            key: b"doc",
            lineage: DocLineage::FIRST,
            version: 1,
            idoc,
        },
        NOW,
        ANCHOR,
    )
    .expect("accepted/fallback document loads");
    let before_digest = ks.state_digest(NOW);
    let before_domain = ks.ns_store(NS).expect("materialized").doc_domain();

    let result = ks.apply_record(
        &RecordView::DocDelta {
            ns: NS,
            key: b"doc",
            lineage: DocLineage::FIRST,
            base_version,
            match_count: u32::from(data[0]) + 1,
            post_len: idoc.len() as u32,
            opcode,
            program,
            operand,
        },
        NOW,
        ANCHOR,
    );
    let (domain, applied_is_canonical) = {
        let store = ks.ns_store_mut(NS).expect("materialized");
        let domain = store.doc_domain();
        assert_eq!(store.doc_live_bytes(), domain.tape_bytes + domain.arena_bytes);
        assert!(domain.slack_bytes <= domain.arena_bytes);
        let canonical = if matches!(result, Ok(ReplayOutcome::Applied)) {
            let frozen = store.json_freeze(b"doc", NOW).expect("document").expect("live");
            TapeDoc::from_bytes(&frozen).is_ok()
        } else {
            true
        };
        (domain, canonical)
    };
    match result {
        Ok(ReplayOutcome::Applied) => {
            assert!(applied_is_canonical, "replay preserves canonical idoc");
        }
        Ok(ReplayOutcome::SkippedDocDeltaStale) | Err(_) => {
            assert_eq!(ks.state_digest(NOW), before_digest, "rejection is atomic");
            assert_eq!(domain, before_domain, "rejection preserves accounting");
        }
        Ok(other) => panic!("live known document has no other replay outcome: {other:?}"),
    }
});
