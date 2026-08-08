//! S02 traversal + morph criterion rows (dev-tier on this box; the S25
//! campaign re-runs gate-grade on the reference box).
//!
//! - `depth4_leaf_fetch/{tape,arena}`: the §4.1 budget row — fetch a leaf
//!   at depth 4 in the 1 KiB gate shape, ≤ 200 ns on tape.
//! - `morph`/`freeze`/`build`: the read/build side of the S02
//!   morph-threshold A/B (the mutation side lands with S16).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::{ArenaDoc, DocValue, JsonParser, TapeDoc};

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

fn bench_traverse(c: &mut Criterion) {
    let text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let mut parser = JsonParser::new();
    let bytes = parser.parse(text.as_bytes()).expect("reference corpus parses");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let mut arena = Arena::new(ArenaConfig::default());
    let adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");

    let mut group = c.benchmark_group("depth4_leaf_fetch");
    group.bench_function("tape", |b| {
        b.iter(|| black_box(fetch_depth4(DocValue::from(black_box(doc.root())))))
    });
    group.bench_function("arena", |b| {
        b.iter(|| black_box(fetch_depth4(black_box(adoc.root_value(&arena)))))
    });
    group.finish();

    c.bench_function("build_from_json_1kib", |b| {
        b.iter(|| black_box(parser.parse(black_box(text.as_bytes())).expect("parses")))
    });
    c.bench_function("morph_1kib", |b| {
        b.iter(|| {
            let d = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
            let r = black_box(d.report());
            d.free(&mut arena);
            r
        })
    });
    c.bench_function("freeze_1kib", |b| {
        b.iter(|| black_box(adoc.freeze(&arena).expect("freezes")))
    });
}

criterion_group!(benches, bench_traverse);
criterion_main!(benches);
