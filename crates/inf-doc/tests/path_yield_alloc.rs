//! Yield cost is O(frames), not O(matches) (ADR-0040 D6) — the
//! `path_program` nightly OOM of 2026-09-12.
//!
//! `save` used to clone the whole match set on every yield, so a
//! budgeted run churned O(nodes / budget × matches) bytes: 139 MiB for
//! the third input below (1,872 matches, budget 3) on a 20-node
//! fixture — under the fuzz build's sanitizer allocator that was 368 MB
//! RSS per input and 1 GB within 150 s of fuzzing from these seeds,
//! 2 GB over a nightly. Measured with the same thread-local byte
//! counter, per yield: 9.5 KiB / 1.5 KiB / 33 KiB before the fix, ≤ 1.3
//! KiB after (frames vector + boxed state + rebuilt frames).
//!
//! Thread-local counters for the same reason as `scalar_patch_alloc.rs`:
//! the process-global one cannot attribute an allocation to this path.

use inf_alloc::CountingAllocator;
use inf_doc::model::{self, Value};
use inf_doc::path::{self, EvalLimits, EvalStep, PathProgram};
use inf_doc::{DocValue, TapeDoc};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// The three nightly artifacts (`fuzz/corpora/path_program/
/// regress-yield-clone-oom-{1,2,3}`), verbatim.
const NIGHTLY_INPUTS: [&[u8]; 3] = [
    b"..[:,:,:,:,:,0,:,:,:,0,:,:,:,0,0][:,6,:,:,:,1:,:,:,:,:,:,:,:,:,0]",
    b"..[:,:,:,:,:,:,:,:,:,:,:,\"a\",\"a\"]..[2,:,:,:,:,:,:,:,\"a\",\"a\",\"a\",:,:,:,:,:]..[\"a\"\
         ,\"a\"][4:,\"a\"]",
    b"..[\"b\",\"a\",\"a\",:,\"b\",:,\"a\",\"\",\"a\",:,\"b\",\"b\",:]..[::,:,:,:,:,:1:,:,:,:,:,:,\
         :,:,0,:,:][\"a\",\"a\",\"a\",:,:2,:,1:,::2,:,::,::,:,\"\"]",
];

/// The fuzz target's fixture (`fuzz_targets/path_program.rs`).
fn fuzz_fixture() -> Vec<u8> {
    let doc = Value::Obj(vec![
        (
            "a".into(),
            Value::Obj(vec![
                ("a".into(), Value::I64(1)),
                (
                    "b".into(),
                    Value::Arr(vec![
                        Value::I64(0),
                        Value::Str("s".into()),
                        Value::Obj(vec![("a".into(), Value::Null)]),
                    ]),
                ),
            ]),
        ),
        (
            "b".into(),
            Value::Arr(vec![
                Value::Arr(vec![Value::I64(7), Value::I64(8), Value::I64(9)]),
                Value::Arr(vec![]),
                Value::Bool(true),
            ]),
        ),
        ("k".into(), Value::F64(2.5)),
        ("empty".into(), Value::Obj(vec![])),
    ]);
    model::encode(&doc).expect("fixture encodes")
}

struct Churn {
    yields: u64,
    bytes: u64,
    allocations: u64,
}

/// Drive a budgeted run to completion and account this thread's
/// allocation churn across every yield/resume.
fn churn(program: &PathProgram, root: DocValue<'_>, budget: u64) -> Churn {
    let limits = EvalLimits::default();
    let bytes0 = ALLOC.thread_bytes();
    let allocs0 = ALLOC.thread_allocations();
    let mut state = None;
    let mut yields = 0u64;
    loop {
        match path::eval_budgeted(program, root, &limits, budget, state.take()).expect("in cap") {
            EvalStep::Done(_) => break,
            EvalStep::Yield(s) => {
                yields += 1;
                state = Some(s);
            }
        }
    }
    Churn {
        yields,
        bytes: ALLOC.thread_bytes() - bytes0,
        allocations: ALLOC.thread_allocations() - allocs0,
    }
}

/// Per-yield churn bound: frames + trail + boxed state for a spine a
/// dozen frames deep is ~1.3 KiB; a cloned match set is tens of KiB.
const PER_YIELD_BYTES_MAX: u64 = 4096;

#[test]
fn nightly_inputs_yield_in_constant_bytes_per_yield() {
    let bytes = fuzz_fixture();
    let doc = TapeDoc::from_validated_bytes(&bytes);
    let root = DocValue::from(doc.root());
    for text in NIGHTLY_INPUTS {
        let program = path::compile(text).expect("nightly input compiles");
        let matches = path::eval(&program, root, &EvalLimits::default()).expect("in cap");
        // The fuzz target's budget, then the worst ratio (one yield per node).
        for budget in [3u64, 1] {
            let c = churn(&program, root, budget);
            assert!(c.yields > 0, "the input must yield");
            let per_yield = c.bytes / c.yields;
            assert!(
                per_yield <= PER_YIELD_BYTES_MAX,
                "{} B/yield over {} yields ({} matches) at budget {budget} — the yield \
                 re-copies the match set: {}",
                per_yield,
                c.yields,
                matches.len(),
                String::from_utf8_lossy(text),
            );
        }
    }
}

/// The scaling law directly: `$..*` over a flat array of N scalars
/// yields once per visited item at budget 1 (≥ N) and records N matches. Churn must grow
/// linearly in N — a per-yield clone of the match set makes it
/// quadratic (4× the elements → ~16× the bytes).
#[test]
fn yield_churn_scales_linearly_with_the_match_set() {
    let program = path::compile(b"$..*").expect("compiles");
    let run = |n: i64| {
        let bytes = model::encode(&Value::Arr((0..n).map(Value::I64).collect())).expect("encodes");
        let doc = TapeDoc::from_validated_bytes(&bytes);
        let root = DocValue::from(doc.root());
        let c = churn(&program, root, 1);
        assert!(c.yields >= n as u64, "at least one yield per element at budget 1");
        c
    };
    let small = run(1024);
    let large = run(4096);
    let ratio = large.bytes as f64 / small.bytes as f64;
    assert!(
        ratio < 8.0,
        "churn grew {ratio:.1}× for 4× the matches ({} → {} B; {} → {} allocations)",
        small.bytes,
        large.bytes,
        small.allocations,
        large.allocations,
    );
    assert!(large.bytes / large.yields <= PER_YIELD_BYTES_MAX);
}
