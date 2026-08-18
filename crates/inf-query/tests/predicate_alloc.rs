//! M4.5-S08 AC: the predicate eval path performs **zero heap
//! allocations** — prepared pools are the cold path, per-eval state is
//! the fixed connective stack plus the walk's fixed-capacity frames.
//! The delta is taken on the thread-local counter (the M3-S16 lesson:
//! a process-global delta captures harness noise it cannot attribute).
//!
//! This is the standing regression gate; the profile artifact under
//! `.artifacts/m4.5/s08/` is the same claim made visually (no allocator
//! frames under perf).

use inf_alloc::CountingAllocator;
use inf_doc::{DocValue, JsonParser, TapeDoc, path};
use inf_query::predicate::{CmpOp, Constant, Predicate, PredicateVm, encode};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

fn vm_of(predicate: &Predicate) -> PredicateVm {
    PredicateVm::new(&encode(predicate).expect("fixture encodes"))
}

fn p(text: &str) -> path::PathProgram {
    path::compile(text.as_bytes()).expect("fixture path compiles")
}

#[test]
fn eval_path_allocates_nothing() {
    // The §4.1 residual shape plus the leaf variety the VM dispatches:
    // comparison, BETWEEN, IN, EXISTS, NOT, wildcard multi-match.
    let predicates = [
        Predicate::And(vec![
            Predicate::Cmp { op: CmpOp::Ge, path: p("$.price"), constant: Constant::I64(10) },
            Predicate::Cmp {
                op: CmpOp::Eq,
                path: p("$.status"),
                constant: Constant::Utf8("open".into()),
            },
        ]),
        Predicate::Between { path: p("$.price"), lo: Constant::I64(1), hi: Constant::F64(99.5) },
        Predicate::In {
            path: p("$.status"),
            members: vec![Constant::Utf8("open".into()), Constant::Utf8("held".into())],
        },
        Predicate::Not(Box::new(Predicate::Exists { path: p("$.gone") })),
        Predicate::Cmp { op: CmpOp::Gt, path: p("$.tags[*]"), constant: Constant::I64(1) },
    ];
    let vms: Vec<PredicateVm> = predicates.iter().map(vm_of).collect();
    let bytes = JsonParser::new()
        .parse(br#"{"price":20,"status":"open","tags":[1,2,3],"nested":{"a":1}}"#)
        .expect("fixture parses");
    let tape = TapeDoc::from_bytes(&bytes).expect("parser emits valid idoc");
    let root = DocValue::from(tape.root());

    // Warm every VM once before observing the counter.
    for vm in &vms {
        vm.eval(root, u64::MAX).expect("completes");
    }

    let before = ALLOC.thread_allocations();
    for _ in 0..10_000 {
        for vm in &vms {
            vm.eval(root, u64::MAX).expect("completes");
        }
    }
    let after = ALLOC.thread_allocations();
    assert_eq!(after - before, 0, "the predicate eval path allocated on this thread");
}
