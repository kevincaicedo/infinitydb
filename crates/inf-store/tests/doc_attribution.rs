//! M3-S19 document-domain attribution: resident partitions reconcile,
//! diagnostic overlays are not double-counted, and keyspace aggregation is
//! the exact field-wise sum of per-namespace reports.
#![cfg(feature = "doc")]

use inf_doc::JsonParser;
use inf_foundation::time::Nanos;
use inf_log::FsyncClass;
use inf_store::{
    CellStore, JsonSetOptions, Keyspace, MemoryReport, NsId, NsMode, NsSpec, StoreConfig,
};

const NOW: Nanos = Nanos(1);
const NS: NsId = NsId(16);

#[test]
fn constructed_report_sums_only_disjoint_resident_domains() {
    let report = MemoryReport {
        records_live_bytes: 1,
        records_slack_bytes: 2,
        records_resident_bytes: 3,
        index_bytes: 5,
        wheel_bytes: 7,
        evict_bytes: 11,
        doc_tape_bytes: 13,
        doc_arena_bytes: 17,
        doc_resident_bytes: 19,
        doc_intern_bytes: 23,
        doc_slack_bytes: 29,
        doc_scratch_bytes: 31,
        doc_path_cache_bytes: 37,
        live_records: 41,
        docs_live: 43,
    };
    assert_eq!(report.attributed_bytes(), 3 + 5 + 7 + 11 + 19 + 31 + 37);
}

#[test]
fn keyspace_report_is_the_per_namespace_field_sum() {
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

    let mut parser = JsonParser::new();
    let inline = parser.parse(br#"{"n":1}"#).expect("inline fixture");
    let tape = parser
        .parse(format!(r#"{{"pad":"{}"}}"#, "x".repeat(1_024)).as_bytes())
        .expect("tape fixture");
    let tree = parser
        .parse(format!(r#"{{"pad":"{}"}}"#, "x".repeat(5_000)).as_bytes())
        .expect("tree fixture");

    ks.db_mut(0).json_set(b"inline", &inline, JsonSetOptions::default(), NOW).unwrap();
    let named = ks.ns_store_mut(NS).expect("namespace store");
    named.json_set(b"tape", &tape, JsonSetOptions::default(), NOW).unwrap();
    named.json_set(b"tree", &tree, JsonSetOptions::default(), NOW).unwrap();
    let _ = named.json_freeze(b"tree", NOW).expect("freeze").expect("document");

    let db = ks.db(0).expect("db0").report();
    let named = ks.ns_store(NS).expect("namespace store").report();
    let total = ks.report();
    macro_rules! summed {
        ($field:ident) => {
            assert_eq!(total.$field, db.$field + named.$field, stringify!($field));
        };
    }
    summed!(records_live_bytes);
    summed!(records_slack_bytes);
    summed!(records_resident_bytes);
    summed!(index_bytes);
    summed!(wheel_bytes);
    summed!(evict_bytes);
    summed!(doc_tape_bytes);
    summed!(doc_arena_bytes);
    summed!(doc_resident_bytes);
    summed!(doc_intern_bytes);
    summed!(doc_slack_bytes);
    summed!(doc_scratch_bytes);
    summed!(doc_path_cache_bytes);
    summed!(live_records);
    summed!(docs_live);

    assert_eq!(total.docs_live, 3);
    assert_eq!(
        total.doc_tape_bytes + total.doc_arena_bytes,
        named.doc_tape_bytes + named.doc_arena_bytes
    );
    assert!(total.doc_resident_bytes >= total.doc_tape_bytes + total.doc_arena_bytes);
    assert!(total.doc_scratch_bytes > 0, "tree freeze retains bounded per-store scratch");
    assert_eq!(total.doc_path_cache_bytes, 0, "the cache is added once by the cell report");
}

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn key_of(mut value: usize) -> [u8; 12] {
    let mut key = *b"d:0000000000";
    for byte in key[2..].iter_mut().rev() {
        *byte = b'0' + (value % 10) as u8;
        value /= 10;
    }
    key
}

fn per_document(bytes: u64, documents: usize) -> f64 {
    bytes as f64 / documents as f64
}

#[test]
fn corpus_shape_bytes_per_document_table() {
    // At least 32 MiB of source idoc per row amortizes the product's
    // default 2 MiB arena chunk tail without changing production config.
    const TARGET_LIVE_BYTES: usize = 32 << 20;
    eprintln!(
        "shape          idoc_B   docs  records_B/doc  doc_live_B/doc  doc_res_B/doc  slack_B/doc  index_B/doc  attributed_B/doc  attributed/idoc"
    );
    let mut parser = JsonParser::new();
    for doc in doc_corpus::generate(doc_corpus::CANONICAL_SEED) {
        let name = doc.name;
        let idoc = parser.parse(doc.json.as_bytes()).expect("reference corpus parses");
        let documents = (TARGET_LIVE_BYTES / idoc.len()).clamp(16, 65_536);
        let mut store =
            CellStore::new(StoreConfig { initial_keys: documents, ..StoreConfig::default() });
        for index in 0..documents {
            store
                .json_set(&key_of(index), &idoc, JsonSetOptions::default(), NOW)
                .expect("corpus document stores");
        }
        let report = store.report();
        assert_eq!(report.docs_live, documents as u64);
        assert_eq!(report.live_records, report.docs_live);
        assert!(report.doc_resident_bytes >= report.doc_tape_bytes + report.doc_arena_bytes);
        let attributed_per_doc = per_document(report.attributed_bytes(), documents);
        eprintln!(
            "{name:<14} {:>7} {:>6} {:>14.1} {:>15.1} {:>14.1} {:>12.1} {:>12.1} {:>17.1} {:>16.3}x",
            idoc.len(),
            documents,
            per_document(report.records_resident_bytes, documents),
            per_document(report.doc_tape_bytes + report.doc_arena_bytes, documents),
            per_document(report.doc_resident_bytes, documents),
            per_document(report.doc_slack_bytes, documents),
            per_document(report.index_bytes, documents),
            attributed_per_doc,
            attributed_per_doc / idoc.len() as f64,
        );
    }
}
