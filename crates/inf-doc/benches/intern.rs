//! M3-S04 interning A/B rows (ADR-0038 D6/D7; dev-tier on this box — the
//! RSS half of the decision rule re-runs on the reference box at S25).
//!
//! - A one-shot **size table**: plain vs interned bytes per corpus shape
//!   (the dev-tier RSS proxy — stored bytes are what the RSS gate loads).
//! - `depth4_leaf_fetch_intern/{plain,interned}`: the read-regression row
//!   on the 1 KiB gate shape (its keys repeat across levels, so it
//!   interns) — same value, both physical forms.
//! - `wide_probe/{plain,interned}`: element-key probe on the wide-array
//!   shape (the shape interning exists for).
//!
//! The third A/B leg — feature-compiled-in vs feature-off on PLAIN
//! documents — is `cargo bench -p inf-doc --bench traverse` run with and
//! without `--features doc-intern-keys` (same bench IDs, two builds).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use inf_doc::intern;
use inf_doc::{DocValue, JsonParser, TapeDoc};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn fetch_depth4<'a>(root: DocValue<'a>) -> DocValue<'a> {
    let mut v = root;
    for _ in 0..4 {
        let DocValue::Obj(o) = v else { panic!("gate shape is nested objects") };
        v = o.get(b"child").expect("gate shape has a child at every level");
    }
    let DocValue::Obj(leaf) = v else { panic!("leaf level is an object") };
    leaf.get(b"score").expect("leaf has a score")
}

fn size_table(parser: &mut JsonParser) {
    println!(
        "{:<12} {:>8} {:>10} {:>8}  (intern on-vs-off stored bytes)",
        "shape", "plain", "interned", "delta"
    );
    for doc in doc_corpus::generate(doc_corpus::CANONICAL_SEED) {
        let name = doc.name;
        let plain = parser.parse(doc.json.as_bytes()).expect("reference corpus parses");
        match intern::intern(&plain) {
            Some(interned) => {
                let delta =
                    100.0 * (interned.len() as f64 - plain.len() as f64) / plain.len() as f64;
                println!("{:<12} {:>8} {:>10} {:>7.1}%", name, plain.len(), interned.len(), delta);
            }
            None => {
                println!("{:<12} {:>8} {:>10} {:>7}", name, plain.len(), "(plain)", "0.0%");
            }
        }
    }
}

fn bench_intern(c: &mut Criterion) {
    let mut parser = JsonParser::new();
    size_table(&mut parser);

    let gate = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let gate_plain = parser.parse(gate.as_bytes()).expect("gate shape parses");
    let gate_interned = intern::intern(&gate_plain).expect("gate shape interns");
    let gate_pdoc = TapeDoc::from_bytes(&gate_plain).expect("validates");
    let gate_idoc = TapeDoc::from_bytes(&gate_interned).expect("validates");

    let mut group = c.benchmark_group("depth4_leaf_fetch_intern");
    group.bench_function("plain", |b| {
        b.iter(|| black_box(fetch_depth4(DocValue::from(black_box(gate_pdoc.root())))))
    });
    group.bench_function("interned", |b| {
        b.iter(|| black_box(fetch_depth4(DocValue::from(black_box(gate_idoc.root())))))
    });
    group.finish();

    let wide = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "wide-array").json;
    let wide_plain = parser.parse(wide.as_bytes()).expect("wide shape parses");
    let wide_interned = intern::intern(&wide_plain).expect("wide shape interns");
    let wide_pdoc = TapeDoc::from_bytes(&wide_plain).expect("validates");
    let wide_idoc = TapeDoc::from_bytes(&wide_interned).expect("validates");

    fn probe<'a>(doc: &TapeDoc<'a>) -> DocValue<'a> {
        let DocValue::Arr(arr) = DocValue::from(doc.root()) else { panic!("array root") };
        let DocValue::Obj(element) = arr.index(5_000).expect("element") else {
            panic!("object element")
        };
        element.get(b"qty").expect("qty present")
    }
    let mut group = c.benchmark_group("wide_probe");
    group.bench_function("plain", |b| b.iter(|| black_box(probe(black_box(&wide_pdoc)))));
    group.bench_function("interned", |b| b.iter(|| black_box(probe(black_box(&wide_idoc)))));
    group.finish();

    c.bench_function("intern_transform_1kib", |b| {
        b.iter(|| black_box(intern::intern(black_box(&gate_plain)).expect("interns")))
    });
}

criterion_group!(benches, bench_intern);
criterion_main!(benches);
