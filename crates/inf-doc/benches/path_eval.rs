//! Direct materialized and streaming path workloads for ARCH-W0.3 T2.
//! Fixed scalar answers check engagement before measurement. Setting
//! INF_BENCH_PATH_CANARY plants a wrong answer and must fail that check.

use core::ops::ControlFlow;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use inf_doc::path::{self, EvalLimits, PathProgram, VisitEnd};
use inf_doc::{DocValue, JsonParser, TapeDoc};

struct Row {
    name: &'static str,
    program: PathProgram,
    expected: Vec<i64>,
}

fn rows() -> Vec<Row> {
    [
        ("child", "$.child.child.child.score", vec![42]),
        ("wildcard", "$.items[*].qty", (0..16).collect()),
        ("slice", "$.items[0:16:2].qty", (0..16).step_by(2).collect()),
        ("descend", "$..qty", (0..16).collect()),
        ("union", "$['items','items'][0].qty", vec![0, 0]),
    ]
    .into_iter()
    .map(|(name, text, expected)| Row {
        name,
        program: path::compile(text.as_bytes()).expect("fixed path compiles"),
        expected,
    })
    .collect()
}

fn fixture() -> String {
    let items: Vec<_> = (0..16).map(|i| format!("{{\"qty\":{i}}}")).collect();
    format!(
        "{{\"child\":{{\"child\":{{\"child\":{{\"score\":42}}}}}},\"items\":[{}]}}",
        items.join(",")
    )
}

fn scalar(value: DocValue<'_>) -> i64 {
    let DocValue::I64(value) = value else {
        panic!("fixture answers are integers");
    };
    value
}

fn verify(row: &Row, root: DocValue<'_>) {
    let matches = path::eval(&row.program, root, &EvalLimits::default()).expect("complete walk");
    let actual: Vec<_> = matches
        .iter()
        .map(|steps| scalar(path::resolve(root, steps).expect("match resolves")))
        .collect();
    assert_eq!(actual, row.expected, "{} materialized answers", row.name);
    let mut visited = Vec::new();
    let outcome = path::eval_visit(&row.program, root, u64::MAX, |value| {
        visited.push(scalar(value));
        ControlFlow::Continue(())
    });
    assert_eq!(outcome.end, VisitEnd::Complete);
    assert!(outcome.nodes_visited > 0, "walk must engage");
    assert_eq!(visited, row.expected, "{} streaming answers", row.name);
}

fn bench_path_eval(c: &mut Criterion) {
    let bytes = JsonParser::new().parse(fixture().as_bytes()).expect("fixture parses");
    let doc = TapeDoc::from_bytes(&bytes).expect("fixture validates");
    let root = DocValue::from(doc.root());
    let mut rows = rows();
    if std::env::var_os("INF_BENCH_PATH_CANARY").is_some() {
        rows[0].expected[0] = -1;
    }
    for row in &rows {
        verify(row, root);
    }
    let mut group = c.benchmark_group("path_eval");
    for row in &rows {
        group.bench_function(row.name, |b| {
            b.iter(|| {
                black_box(path::eval(&row.program, black_box(root), &EvalLimits::default()))
                    .expect("complete walk")
            });
        });
    }
    group.finish();
    let mut group = c.benchmark_group("path_visit");
    for row in &rows {
        group.bench_function(row.name, |b| {
            b.iter(|| {
                black_box(path::eval_visit(&row.program, black_box(root), u64::MAX, |value| {
                    black_box(value);
                    ControlFlow::Continue(())
                }))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_path_eval);
criterion_main!(benches);
