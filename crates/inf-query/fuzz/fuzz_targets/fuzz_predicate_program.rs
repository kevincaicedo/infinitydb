//! Predicate-program fuzz (M4.5-S07/S08, L9 — the same-PR rule for the
//! new decoder and its evaluator): arbitrary bytes never panic the
//! validator; accepted programs decode deterministically and are
//! canonically stable — decode → re-encode is byte identity, because
//! programs ride access programs, `QueryOp` fabric frames, and
//! cursor-adjacent state (ADR-0079 D2), where a second encoding of the
//! same meaning would be a silent divergence class. Accepted programs
//! are then **evaluated** (S08) against a small document corpus under a
//! finite fuel budget: no panic, termination structural, the only error
//! `FuelExhausted`, and the outcome deterministic (L7).

#![no_main]

use std::sync::OnceLock;

use inf_doc::{DocValue, JsonParser, TapeDoc};
use inf_query::predicate::{PredicateEvalError, PredicateProgram, PredicateVm, encode};
use libfuzzer_sys::fuzz_target;

/// Small corpus hitting what fuzz-discovered paths reach: single-char
/// keys, arrays (multi-match), nesting, null, every scalar type.
fn corpus() -> &'static [Vec<u8>] {
    static DOCS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    DOCS.get_or_init(|| {
        [
            br#"{"a":1,"b":"ab","c":true,"d":null,"e":2.5}"#.as_slice(),
            br#"{"a":[1,"x",2.5,null,[3],{"a":4}],"b":{"a":{"a":7}}}"#.as_slice(),
            br#"[0,1,2,3]"#.as_slice(),
            br#""lone""#.as_slice(),
        ]
        .iter()
        .map(|json| JsonParser::new().parse(json).expect("fuzz corpus parses"))
        .collect()
    })
}

fuzz_target!(|data: &[u8]| {
    if let Ok(program) = PredicateProgram::from_bytes(data) {
        let tree = program.decode();
        let again = program.decode();
        assert_eq!(tree, again, "decode must be deterministic");
        let re = encode(&tree).expect("decoded tree re-encodes");
        assert_eq!(
            re.as_bytes(),
            data,
            "canonical stability: accepted bytes re-encode identically"
        );
        let vm = PredicateVm::new(&program);
        for bytes in corpus() {
            let tape = TapeDoc::from_validated_bytes(bytes);
            let root = DocValue::from(tape.root());
            let first = vm.eval(root, 10_000);
            let second = vm.eval(root, 10_000);
            assert_eq!(first, second, "evaluation must be deterministic (L7)");
            if let Err(error) = first {
                assert_eq!(error, PredicateEvalError::FuelExhausted, "the one typed error");
            }
        }
    }
});
