//! M3-S13 §4.1 AC: arena slack for arrays stays ≤ 25% of array bytes on
//! corpus-shaped documents under array-op storms — the L5 attribution
//! assert, exercised on both execution shapes:
//!
//! - **Splice backend (v1, ADR-0041 D5):** every mutation re-tiers
//!   through `ArenaDoc::from_tape`, which packs exactly — slack is
//!   **zero** by construction after each storm step, and accounting
//!   reconciles against the arena's own books at every step.
//! - **Arena growth path (the S16 shape):** `arr_push` storms keep slack
//!   within the ADR-0036 D7 ×1.25 bound (+ the 4-slot floor) — the arm
//!   the in-place engine will stand on when S16 replaces the splice
//!   backend (the carry ADR-0042 D1 names).

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::apply::{ApplyOp, apply};
use inf_doc::limits::DOC_BYTES_MAX;
use inf_doc::model::{self, Value};
use inf_doc::path::{EvalLimits, compile};
use inf_doc::{ArenaDoc, JsonParser, TapeDoc};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

/// Splice-backend storm: append/insert/trim/pop cycles through `apply`
/// on the wide-array corpus shape, re-morphing after every step — slack
/// stays exactly zero (the v1 backend packs on re-tier) and accounting
/// reconciles to zero drift.
#[test]
fn splice_storm_keeps_arena_slack_at_zero() {
    let json = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "wide-array").json;
    let mut bytes = JsonParser::new().parse(json.as_bytes()).expect("reference corpus parses");
    let program = compile(b"$").expect("compiles");
    let batch: Vec<Vec<u8>> =
        (0..8).map(|i| model::encode_fragment(&Value::I64(i)).expect("encodes")).collect();
    let refs: Vec<&[u8]> = batch.iter().map(|f| &f[..]).collect();
    let operand = inf_doc::array_operand(&refs).expect("fits");
    let ops = [
        ApplyOp::ArrAppend { elements: &operand },
        ApplyOp::ArrInsert { index: 0, elements: &operand },
        ApplyOp::ArrTrim { start: 2, stop: -2 },
        ApplyOp::ArrPop { index: -1 },
    ];
    let mut arena = Arena::new(ArenaConfig::default());
    for round in 0..12 {
        let doc = TapeDoc::from_bytes(&bytes).expect("storm output revalidates");
        let limits = EvalLimits::default();
        let outcome = apply(&doc, &program, &ops[round % ops.len()], &limits, DOC_BYTES_MAX)
            .expect("storm ops stay in range");
        if let Some(new_bytes) = outcome.bytes {
            bytes = new_bytes;
        }
        let doc = TapeDoc::from_bytes(&bytes).expect("revalidates");
        let baseline = arena.report().live_bytes as usize;
        let adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
        let report = adoc.report();
        assert_eq!(report.slack_bytes, 0, "the splice backend re-tiers packed (round {round})");
        assert_eq!(
            arena.report().live_bytes as usize - baseline,
            report.node_bytes,
            "attribution reconciles against the arena's books (round {round})"
        );
        adoc.free(&mut arena);
        assert_eq!(arena.report().live_bytes as usize, baseline, "zero drift (round {round})");
    }
}

/// Growth-path storm (the S16 mechanism isolate): pushing elements one at
/// a time onto a corpus-scale array keeps slack within 25% of array slot bytes
/// plus the 4-slot floor — the AC bound with *real* slack, which the
/// in-place engine inherits.
#[test]
fn growth_storm_keeps_array_slack_bounded() {
    let seed = Value::Arr((0..500).map(Value::I64).collect());
    let bytes = model::encode(&seed).expect("encodes");
    let doc = TapeDoc::from_bytes(&bytes).expect("validates");
    let mut arena = Arena::new(ArenaConfig::default());
    let mut adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morphs");
    let mut arr = adoc.root_ref();
    let mut slot_bytes = 500usize * 8;
    for i in 0..1500i64 {
        let element = adoc.alloc_i64(&mut arena, i).expect("alloc");
        arr = adoc.arr_push(&mut arena, arr, element).expect("push");
        slot_bytes += 8;
        let report = adoc.report();
        let bound = slot_bytes / 4 + 4 * 8;
        assert!(
            report.slack_bytes <= bound,
            "slack {} exceeds 25% of array bytes {} + floor at push {i}",
            report.slack_bytes,
            slot_bytes,
        );
    }
    adoc.free(&mut arena);
}
