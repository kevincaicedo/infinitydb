//! M0-S06 AC: executor fast-path overhead vs a direct function call.
//!
//! The gate (≤ 5 ns delta) is judged on the Linux reference box; macOS runs
//! are dev-tier sanity numbers. Also measures the full suspend→resume cycle
//! so the slow path has a tracked baseline from day one.

use std::cell::Cell;
use std::hint::black_box;
use std::rc::Rc;

use criterion::{Criterion, criterion_group, criterion_main};
use inf_runtime::{CellExecutor, FabricGate, PollImmediate};

/// The "command" both sides run: a tiny read-modify-write, the shape of a
/// fast-path GET hitting cell-local state.
#[inline(never)]
fn command_direct(counter: &Cell<u64>) {
    counter.set(counter.get().wrapping_add(1));
}

fn bench_fast_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_fast_path");

    let counter = Cell::new(0u64);
    group.bench_function("direct_call", |b| {
        b.iter(|| {
            command_direct(black_box(&counter));
        });
    });

    let mut ex = CellExecutor::new(64);
    let counter = Rc::new(Cell::new(0u64));
    group.bench_function("poll_immediate_ready", |b| {
        b.iter(|| {
            let counter = Rc::clone(&counter);
            let outcome = ex.poll_immediate(async move {
                command_direct(black_box(&counter));
            });
            assert!(matches!(outcome, PollImmediate::Completed));
        });
    });
    assert_eq!(ex.live_tasks(), 0);

    group.finish();
}

fn bench_suspend_resume(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_slow_path");

    let mut ex = CellExecutor::new(64);
    let gate: FabricGate<u64> = FabricGate::new();
    let sink = Rc::new(Cell::new(0u64));
    let mut token = 0u64;
    group.bench_function("suspend_complete_resume", |b| {
        b.iter(|| {
            token += 1;
            let waiter = gate.waiter(token);
            let sink = Rc::clone(&sink);
            let outcome = ex.poll_immediate(async move {
                sink.set(waiter.await);
            });
            assert!(matches!(outcome, PollImmediate::Suspended(_)));
            gate.complete(token, token);
            ex.run_ready(1);
        });
    });
    assert_eq!(ex.live_tasks(), 0);

    group.finish();
}

criterion_group!(benches, bench_fast_path, bench_suspend_resume);
criterion_main!(benches);
