#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "benchmark: fixture files outside cell code (ADR-0144 D5)"
)]
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

use inf_doc::{DocValue, JsonParser, TapeDoc, serialize_canonical_into};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn verify_document(case: &str, bytes: &[u8], expected: &serde_json::Value) {
    let doc = TapeDoc::from_bytes(bytes).expect("benchmark parse produced valid tape");
    let mut text = Vec::new();
    serialize_canonical_into(DocValue::from(doc.root()), &mut text);
    let mut actual: serde_json::Value = serde_json::from_slice(&text).expect("serialized JSON");
    if std::env::var("INF_BENCH_JSON_PARSE_CANARY").as_deref() == Ok(case) {
        actual = serde_json::Value::Null;
    }
    assert_eq!(&actual, expected, "{case} document answers");
}

/// The generated fixtures are valid JSON; this lexer is independent of
/// the scanner's bit masks and includes opening and closing string quotes.
fn expected_structurals(text: &[u8]) -> Vec<u32> {
    let mut offsets = Vec::new();
    let mut at = 0;
    while at < text.len() {
        if text[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        offsets.push(u32::try_from(at).expect("fixture offset fits u32"));
        match text[at] {
            b'"' => {
                at += 1;
                while text[at] != b'"' {
                    at += if text[at] == b'\\' { 2 } else { 1 };
                }
                offsets.push(u32::try_from(at).expect("fixture offset fits u32"));
                at += 1;
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => at += 1,
            _ => {
                at += 1;
                while at < text.len()
                    && !text[at].is_ascii_whitespace()
                    && !matches!(text[at], b'{' | b'}' | b'[' | b']' | b':' | b',')
                {
                    at += 1;
                }
            }
        }
    }
    offsets
}

fn verify_scan(case: &str, text: &[u8], scan: fn(&[u8], &mut Vec<u32>) -> usize) {
    let expected = expected_structurals(text);
    let mut actual = Vec::new();
    let count = scan(text, &mut actual);
    actual.truncate(count);
    if std::env::var("INF_BENCH_JSON_PARSE_CANARY").as_deref() == Ok(case) {
        let _ = actual.pop();
    }
    assert_eq!(actual, expected, "{case} structural offsets");
}

fn verify_corpus(corpus: &[(&str, String)]) {
    let mut parser = JsonParser::new();
    let mut out = Vec::new();
    for (name, text) in corpus {
        let expected: serde_json::Value = serde_json::from_str(text).expect("reference JSON");
        let bytes = parser.parse(text.as_bytes()).expect("fixture parses");
        verify_document(&format!("parse/{name}"), &bytes, &expected);
        parser.parse_into(text.as_bytes(), &mut out).expect("fixture parses into");
        verify_document(&format!("parse_into/{name}"), &out, &expected);
        let bytes = parser.parse_scalar_stage1(text.as_bytes()).expect("scalar fixture parses");
        verify_document(&format!("parse_scalar_stage1/{name}"), &bytes, &expected);
    }
    let medium = &corpus.iter().find(|(name, _)| *name == "medium-2KiB").expect("medium").1;
    verify_scan("scan/simd", medium.as_bytes(), inf_simd::json_scan_structurals);
    verify_scan("scan/scalar", medium.as_bytes(), inf_simd::scalar_json_scan_structurals);
}

fn bench_parse(c: &mut Criterion) {
    let corpus: Vec<(&str, String)> = doc_corpus::generate(doc_corpus::CANONICAL_SEED)
        .into_iter()
        .map(|doc| (doc.name, doc.json))
        .collect();
    verify_corpus(&corpus);

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
