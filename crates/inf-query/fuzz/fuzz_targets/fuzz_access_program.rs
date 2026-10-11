//! Access-program decoder fuzz (M4.5-S09, L9 — the same-PR rule for
//! the new trust boundary): arbitrary bytes never panic `from_bytes`;
//! accepted bytes decode, re-encode **byte-identically** (one program,
//! one encoding — programs ride `QueryOp` frames and sit behind
//! cursors, ADR-0080 D2), and EXPLAIN renders total — including the
//! nested revalidation of embedded residual predicate programs.

#![no_main]

use inf_query::access::{AccessProgram, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(program) = AccessProgram::from_bytes(data) else {
        return;
    };
    let access = program.decode();
    let re_encoded = encode(&access).expect("validated programs re-encode");
    assert_eq!(
        re_encoded.as_bytes(),
        program.as_bytes(),
        "decode → encode is byte identity (canonical form)"
    );
    let explained = program.explain();
    assert_eq!(explained, re_encoded.explain(), "EXPLAIN is deterministic");
});
