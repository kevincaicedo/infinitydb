//! M3-S05 parse-throughput rows (dev-tier; §4.1 budgets):
//!
//! - `parse/{shape}`: GB/s per corpus shape — the budget rows are
//!   `medium-2KiB` (≥ 1 GB/s floor) and `gate-1KiB` (≥ 2.5 GB/s target,
//!   the arithmetic behind `JSON.SET ≥ 70% SET`).
//! - `parse_scalar_stage1/{shape}`: the same parse over the scalar
//!   stage-1 tier — the L4 SIMD-vs-scalar A/B's off arm.
//! - `scan/{simd,scalar}`: stage 1 in isolation on the medium shape.
//!
//! Inputs come from the dependency-free S20 generator. The measurement
//! instrument shares no document parser or serializer with the system
//! under test.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use inf_doc::JsonParser;

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn bench_parse(c: &mut Criterion) {
    let corpus: Vec<(&str, String)> = doc_corpus::generate(doc_corpus::CANONICAL_SEED)
        .into_iter()
        .map(|doc| (doc.name, doc.json))
        .collect();

    let mut parser = JsonParser::new();

    let mut group = c.benchmark_group("parse");
    for (name, text) in &corpus {
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| black_box(parser.parse(black_box(text.as_bytes()))).expect("parses"))
        });
    }
    group.finish();

    // The ingest-seam arm: one recycled output buffer (json_set's shape
    // after S03/S11 wire-up) — the delta vs `parse/` is the allocation.
    let mut out = Vec::new();
    let mut group = c.benchmark_group("parse_into");
    for (name, text) in &corpus {
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                parser.parse_into(black_box(text.as_bytes()), &mut out).expect("parses");
                black_box(out.len())
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("parse_scalar_stage1");
    for (name, text) in &corpus {
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                black_box(parser.parse_scalar_stage1(black_box(text.as_bytes()))).expect("parses")
            })
        });
    }
    group.finish();

    let medium = &corpus.iter().find(|(n, _)| *n == "medium-2KiB").expect("medium shape").1;
    let mut indices = Vec::new();
    let mut group = c.benchmark_group("scan");
    group.throughput(Throughput::Bytes(medium.len() as u64));
    group.bench_function("simd", |b| {
        b.iter(|| inf_simd::json_scan_structurals(black_box(medium.as_bytes()), &mut indices))
    });
    group.bench_function("scalar", |b| {
        b.iter(|| {
            inf_simd::scalar_json_scan_structurals(black_box(medium.as_bytes()), &mut indices)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
