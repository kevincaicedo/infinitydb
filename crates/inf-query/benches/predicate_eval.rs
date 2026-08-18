//! M4.5-S08 §4.1 row (dev-tier on this box; campaign re-runs are
//! gate-grade): **≥ 5M predicate evals/s/core on the 1 KiB corpus shape
//! ⇒ ≤ 200 ns/eval**, allocation-free (the profile artifact and the
//! `predicate_alloc` gate test prove the latter).
//!
//! - `residual_two_leaf`: the plan's typical residual shape
//!   (`score >= 0.5 AND kind = 'gate'`) — the budget row.
//! - `residual_short_circuit`: first conjunct false — skip-by-decode
//!   pays op decodes only, no path work for the sibling.
//! - `residual_deep_path`: a depth-4 member chain (the S02
//!   `depth4_leaf_fetch` analog, plus VM overhead).
//! - `residual_in_list`: membership, hit on the last of three members.
//! - `residual_multi_match`: `items[*]` existential over 12 array
//!   elements on the 2 KiB shape — the multi-match cost row.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use inf_doc::{DocValue, JsonParser, TapeDoc};
use inf_query::predicate::{CmpOp, Constant, Predicate, PredicateVm, encode};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

fn path(text: &str) -> inf_doc::PathProgram {
    inf_doc::path::compile(text.as_bytes()).expect("bench path compiles")
}

fn vm(predicate: &Predicate) -> PredicateVm {
    PredicateVm::new(&encode(predicate).expect("bench predicate encodes"))
}

fn bench_predicate_eval(c: &mut Criterion) {
    let gate_text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let gate_bytes = JsonParser::new().parse(gate_text.as_bytes()).expect("corpus parses");
    let gate = TapeDoc::from_bytes(&gate_bytes).expect("validates");
    let medium_text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "medium-2KiB").json;
    let medium_bytes = JsonParser::new().parse(medium_text.as_bytes()).expect("corpus parses");
    let medium = TapeDoc::from_bytes(&medium_bytes).expect("validates");

    let two_leaf = vm(&Predicate::And(vec![
        Predicate::Cmp { op: CmpOp::Ge, path: path("$.score"), constant: Constant::F64(0.5) },
        Predicate::Cmp {
            op: CmpOp::Eq,
            path: path("$.kind"),
            constant: Constant::Utf8("gate".into()),
        },
    ]));
    let short_circuit = vm(&Predicate::And(vec![
        Predicate::Cmp { op: CmpOp::Lt, path: path("$.id"), constant: Constant::I64(-1) },
        Predicate::Cmp {
            op: CmpOp::Eq,
            path: path("$.kind"),
            constant: Constant::Utf8("gate".into()),
        },
    ]));
    let deep_path = vm(&Predicate::Cmp {
        op: CmpOp::Gt,
        path: path("$.child.child.child.score"),
        constant: Constant::F64(0.5),
    });
    let in_list = vm(&Predicate::In {
        path: path("$.kind"),
        members: vec![
            Constant::Utf8("alpha".into()),
            Constant::Utf8("beta".into()),
            Constant::Utf8("gate".into()),
        ],
    });
    let multi_match = vm(&Predicate::Cmp {
        op: CmpOp::Gt,
        path: path("$.items[*].qty"),
        constant: Constant::I64(90),
    });

    let mut group = c.benchmark_group("predicate_eval_1kib");
    let rows: &[(&str, &PredicateVm, bool)] = &[
        ("residual_two_leaf", &two_leaf, true),
        ("residual_short_circuit", &short_circuit, false),
        ("residual_deep_path", &deep_path, true),
        ("residual_in_list", &in_list, true),
    ];
    for (name, vm, expected) in rows {
        let root = DocValue::from(gate.root());
        let outcome = vm.eval(root, u64::MAX).expect("bench eval completes");
        assert_eq!(outcome.verdict, *expected, "{name} verdict pinned");
        group.bench_function(*name, |b| {
            b.iter(|| {
                black_box(vm.eval(black_box(DocValue::from(gate.root())), u64::MAX))
                    .expect("completes")
            })
        });
    }
    group.finish();

    c.bench_function("predicate_eval_2kib/residual_multi_match", |b| {
        b.iter(|| {
            black_box(multi_match.eval(black_box(DocValue::from(medium.root())), u64::MAX))
                .expect("completes")
        })
    });
}

criterion_group!(benches, bench_predicate_eval);
criterion_main!(benches);
