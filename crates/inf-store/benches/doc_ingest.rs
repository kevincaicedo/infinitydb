//! M3-S05 gate-shape projection row (§4.1 arithmetic): the 1 KiB parse
//! cost against same-run plain-SET cost, plus the parse+store e2e —
//! the early warning for `JSON.SET ≥ 70% SET`. Store-level SET excludes
//! wire + log staging, so the projection is conservative in the gate's
//! favor is NOT assumed: the ledger entry does the arithmetic against
//! the M2.5 server-level SET rows explicitly.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use inf_doc::JsonParser;
use inf_foundation::time::Nanos;
use inf_store::{CellStore, JsonSetOptions, SetOptions, StoreConfig};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn bench_doc_ingest(c: &mut Criterion) {
    let text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let now = Nanos::from_millis(1);
    let mut parser = JsonParser::new();
    let idoc = parser.parse(text.as_bytes()).expect("gate shape parses");
    let value_1kib = vec![0xABu8; text.len()]; // equal-size plain value

    let mut group = c.benchmark_group("doc_ingest_1kib");
    group.throughput(Throughput::Bytes(text.len() as u64));
    group.bench_function("parse", |b| {
        b.iter(|| black_box(parser.parse(black_box(text.as_bytes()))).expect("parses"))
    });
    group.bench_function("store_set", |b| {
        let mut store = CellStore::new(StoreConfig::default());
        let mut i = 0u64;
        b.iter(|| {
            // Rotate a small key set so the row measures steady-state
            // overwrite, matching the SET gate's pipelined shape.
            let key = [b'k', (i % 251) as u8];
            i += 1;
            store.set(&key, &value_1kib, SetOptions::default(), now).expect("set")
        })
    });
    group.bench_function("json_set_prebuilt", |b| {
        let mut store = CellStore::new(StoreConfig::default());
        let mut i = 0u64;
        b.iter(|| {
            let key = [b'j', (i % 251) as u8];
            i += 1;
            store.json_set(&key, &idoc, JsonSetOptions::default(), now).expect("json_set")
        })
    });
    group.bench_function("parse_plus_json_set", |b| {
        let mut store = CellStore::new(StoreConfig::default());
        let mut i = 0u64;
        b.iter(|| {
            let bytes = parser.parse(black_box(text.as_bytes())).expect("parses");
            let key = [b'e', (i % 251) as u8];
            i += 1;
            store.json_set(&key, &bytes, JsonSetOptions::default(), now).expect("json_set")
        })
    });
    // The S05-slice-2 reuse arm: one recycled parse buffer per cell — the
    // shape S11's command path will actually run.
    group.bench_function("parse_into_plus_json_set", |b| {
        let mut store = CellStore::new(StoreConfig::default());
        let mut buf = Vec::new();
        let mut i = 0u64;
        b.iter(|| {
            parser.parse_into(black_box(text.as_bytes()), &mut buf).expect("parses");
            let key = [b'e', (i % 251) as u8];
            i += 1;
            store.json_set(&key, &buf, JsonSetOptions::default(), now).expect("json_set")
        })
    });
    group.finish();
}

criterion_group!(benches, bench_doc_ingest);
criterion_main!(benches);
