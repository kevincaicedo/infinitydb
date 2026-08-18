//! Predicate-program fuzz (M4.5-S07, L9 — the same-PR rule for the new
//! decoder): arbitrary bytes never panic the validator; accepted
//! programs decode deterministically and are canonically stable —
//! decode → re-encode is byte identity, because programs ride access
//! programs, `QueryOp` fabric frames, and cursor-adjacent state
//! (ADR-0079 D2), where a second encoding of the same meaning would be
//! a silent divergence class.

#![no_main]

use inf_query::predicate::{PredicateProgram, encode};
use libfuzzer_sys::fuzz_target;

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
    }
});
