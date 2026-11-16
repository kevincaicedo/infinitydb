//! M0-S06 AC: executor fast-path overhead vs a direct function call.
//!
//! The gate (≤ 5 ns delta) is judged on the Linux reference box; macOS runs
//! are dev-tier sanity numbers. Also measures the full suspend→resume cycle
//! so the slow path has a tracked baseline from day one. The fast path is read at three
//! shapes: the bench's class, and a future padded so its slot takes 16 a chunk, its class at
//! 2,500 and at 10,000 slots (two directory levels); `poll_immediate_ready_twice` is the read's
//! liveness leg, two first polls an iteration. The padded future is written by hand on the
//! bench class's body, one `command_direct` call, so the shapes differ only in the slot's width
//! and depth. `poll_direct` and `poll_direct_padded` poll each future once in a stack slot under
//! a no-op waker, from the factory its executor legs use: read beside the executor's legs, never
//! subtracted from them (the padded one pays its stack slot's store and reload, which the
//! executor's slot does not). `padded_2500_after_own_read` is the planted re-lend: the padded fast
//! op after `class_tasks` of its own class, whose slab borrow forgets the kept lend, so the op
//! re-lends by `get_mut`; `padded_2500_after_other_read` is its control, the same op after
//! `class_tasks` of the executor's other class, which keeps the lend. The futures, factories and
//! fast step are `support/fast.rs`'s; this one binary is timed, counted (`perf stat`, instructions
//! per near call) and listed (`perf record`), so every reading of a shape reads one loop.

use std::cell::{Cell, RefCell};
use std::hint::black_box;
use std::rc::Rc;

use criterion::{Criterion, criterion_group, criterion_main};
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::{
    CellExecutor, FabricAdmission, FabricGate, GateUnits, GateWait, PollImmediate, TaskAdmission,
    TaskClass, TaskFactory,
};

#[path = "support/fast.rs"]
mod fast;

use fast::{
    PADDED_2500, PADDED_10000, TASKS, command_direct, command_task, executor_padded_and_other,
    executor_with, padded_task, poll_direct, ready_once,
};

/// An executor of one class, the future `factory` makes, on a census of its own.
fn executor_of<Args, Fac: TaskFactory<Args>>(factory: &Fac) -> (CellExecutor, TaskClass) {
    executor_with(factory, TASKS)
}

/// The slow path's task: it awaits its gate's value and keeps it.
async fn waiter_task(waiter: GateWait<'static, u64>, sink: Rc<Cell<u64>>) {
    sink.set(waiter.await);
}

fn bench_fast_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_fast_path");

    let counter = Cell::new(0u64);
    group.bench_function("direct_call", |b| {
        b.iter(|| {
            command_direct(black_box(&counter));
        });
    });

    let (mut ex, class) = executor_of(&command_task);
    let counter = Rc::new(Cell::new(0u64));
    group.bench_function("poll_immediate_ready", |b| {
        b.iter(|| {
            let counter = Rc::clone(&counter);
            ready_once(&mut ex, class, || command_task(counter));
        });
    });
    group.bench_function("poll_direct", |b| {
        b.iter(|| poll_direct(command_task(Rc::clone(&counter))));
    });
    group.bench_function("poll_direct_padded", |b| {
        b.iter(|| poll_direct(padded_task(Rc::clone(&counter))));
    });
    group.bench_function("poll_immediate_ready_twice", |b| {
        b.iter(|| {
            for _ in 0..2 {
                let counter = Rc::clone(&counter);
                ready_once(&mut ex, class, || command_task(counter));
            }
        });
    });
    assert_eq!(ex.live_tasks(), 0);

    for (name, cap) in [
        ("poll_immediate_ready_padded_2500", PADDED_2500),
        ("poll_immediate_ready_padded_10000", PADDED_10000),
    ] {
        let (mut ex, class) = executor_with(&padded_task, cap);
        group.bench_function(name, |b| {
            b.iter(|| {
                let counter = Rc::clone(&counter);
                ready_once(&mut ex, class, || padded_task(counter));
            });
        });
        assert_eq!(ex.live_tasks(), 0);
    }

    let (mut ex, padded, other) = executor_padded_and_other();
    for (name, read) in
        [("padded_2500_after_own_read", padded), ("padded_2500_after_other_read", other)]
    {
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(ex.class_tasks(read));
                let counter = Rc::clone(&counter);
                ready_once(&mut ex, padded, || padded_task(counter));
            });
        });
        assert_eq!(ex.class_tasks(padded), 0);
        assert_eq!(ex.live_tasks(), 0);
    }

    group.finish();
}

fn bench_suspend_resume(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_slow_path");

    let (mut ex, class) = executor_of(&waiter_task);
    // A task's wait borrows its gate and units, so the bench's live for the
    // run: the spawned future is `'static`.
    let gate: &'static RefCell<FabricGate<u64>> =
        Box::leak(Box::new(RefCell::new(one_holder_gate())));
    let units: &'static GateUnits = Box::leak(Box::new(gate.borrow().units(1).expect("a unit")));
    let sink = Rc::new(Cell::new(0u64));
    group.bench_function("suspend_complete_resume", |b| {
        b.iter(|| {
            let (token, waiter) = {
                let mut open = gate.borrow_mut();
                let FabricAdmission::Granted(permit) = open.reserve(units) else {
                    panic!("the bench's one wait is done");
                };
                let token = permit.key();
                (token, permit.register(gate))
            };
            let sink = Rc::clone(&sink);
            let TaskAdmission::Granted(permit) = ex.reserve(class) else {
                panic!("the bench's class has room");
            };
            let outcome = permit.poll_immediate(|| waiter_task(waiter, sink));
            assert!(matches!(outcome, PollImmediate::Suspended(_)));
            FabricGate::complete(gate, token, token, inf_foundation::time::Nanos(0));
            ex.run_ready(1);
        });
    });
    assert_eq!(ex.live_tasks(), 0);

    group.finish();
}

criterion_group!(benches, bench_fast_path, bench_suspend_resume);
criterion_main!(benches);

/// A gate of one unit over one holder, on a census of its own.
fn one_holder_gate() -> FabricGate<u64> {
    const HOLDERS: Cap = Cap::entries::<1>("bench-gate-holders", CapFill::Serving);
    let (census, _mark) = CapCensus::new(CellId(0));
    FabricGate::build(0, 1, Some(HOLDERS), &census).expect("the bench's gate")
}
