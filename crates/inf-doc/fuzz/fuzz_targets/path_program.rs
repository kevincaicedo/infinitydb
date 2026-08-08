//! Path-program fuzz (M3-S08/S09, L9 — same-PR rule for both new
//! decoders): the input drives the **program-byte** decoder and the
//! **path-text** parser in one target.
//!
//! Program arm: arbitrary bytes never panic; accepted programs are
//! canonically stable (decode → re-encode is byte-identity — programs
//! ship inside `DocDelta` records forever, ADR-0040 D2) and evaluate
//! against a fixed document without panicking, with deterministic
//! matches and a borrow-free yield/resume path that converges to the
//! straight run.
//!
//! Text arm: arbitrary bytes never panic the parser; accepted text
//! prints canonically, reparses to the identical AST, and compiles to
//! the identical program (the full S08 round-trip under fuzz).

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

use inf_doc::model::{self, Value};
use inf_doc::path::{self, EvalLimits, EvalStep, PathProgram};
use inf_doc::{DocValue, TapeDoc};

/// A fixed evaluation substrate with every container shape a selector
/// can touch (objects, nested arrays, scalars, empties).
fn doc_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let doc = Value::Obj(vec![
            ("a".into(), Value::Obj(vec![
                ("a".into(), Value::I64(1)),
                ("b".into(), Value::Arr(vec![
                    Value::I64(0),
                    Value::Str("s".into()),
                    Value::Obj(vec![("a".into(), Value::Null)]),
                ])),
            ])),
            ("b".into(), Value::Arr(vec![
                Value::Arr(vec![Value::I64(7), Value::I64(8), Value::I64(9)]),
                Value::Arr(vec![]),
                Value::Bool(true),
            ])),
            ("k".into(), Value::F64(2.5)),
            ("empty".into(), Value::Obj(vec![])),
        ]);
        model::encode(&doc).expect("fixture encodes")
    })
}

fn exercise(program: &PathProgram) {
    let bytes = doc_bytes();
    let doc = TapeDoc::from_validated_bytes(bytes);
    let root = DocValue::from(doc.root());
    let limits = EvalLimits::default();
    let matches = path::eval(program, root, &limits).expect("fixture is far below the cap");
    // Determinism (L7) + the canonical view never panics.
    let again = path::eval(program, root, &limits).expect("deterministic");
    assert_eq!(matches, again, "evaluation must be deterministic");
    let canonical = matches.canonical();
    assert!(canonical.ids.len() <= matches.len());
    // Every match resolves back to a node (location paths are real).
    for steps in matches.iter() {
        assert!(path::resolve(root, steps).is_some(), "match path must resolve");
    }
    // Budgeted run converges to the straight run through owned yields.
    let mut state = None;
    let resumed = loop {
        match path::eval_budgeted(program, root, &limits, 3, state.take()).expect("in cap") {
            EvalStep::Done(m) => break m,
            EvalStep::Yield(s) => state = Some(s),
        }
    };
    assert_eq!(resumed, matches, "yield/resume must converge to the straight run");
}

fuzz_target!(|data: &[u8]| {
    // Program-byte arm (the replay trust boundary).
    if let Ok(program) = PathProgram::from_bytes(data) {
        let ast = program.decode();
        let re = path::encode_ast(&ast);
        assert_eq!(
            re.as_bytes(),
            data,
            "canonical stability: accepted program bytes re-encode identically"
        );
        exercise(&program);
    }
    // Text arm (the S08 parser + printer + compiler loop).
    if let Ok(ast) = path::parse_ast(data) {
        let printed = path::ast::print(&ast);
        let reparsed = path::parse_ast(printed.as_bytes()).expect("canonical print reparses");
        assert_eq!(reparsed, ast, "print(parse(text)) must reparse identically");
        let program = path::compile(data).expect("parsed text compiles");
        assert_eq!(
            path::encode_ast(&ast).as_bytes(),
            program.as_bytes(),
            "compile ≡ parse+encode"
        );
        exercise(&program);
    }
});
