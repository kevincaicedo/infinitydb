//! M0-S06 AC: executor fast-path overhead vs a direct function call.
//!
//! The gate (≤ 5 ns delta) is judged on the Linux reference box; macOS runs
//! are dev-tier sanity numbers. Also measures the full suspend→resume cycle
//! so the slow path has a tracked baseline from day one. The fast path is read at three
//! shapes: the bench's class, and a future padded so its slot takes 16 a chunk, its class at
//! 2,500 and at 10,000 slots (two directory levels); `poll_immediate_ready_twice` is the read's
//! liveness leg, two first polls an iteration.

use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::rc::Rc;

use criterion::{Criterion, criterion_group, criterion_main};
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::{
    CellExecutor, ExecutorLayout, FabricGate, GateWait, PollImmediate, TaskAdmission, TaskClass,
    TaskFactory,
};

/// Tasks each bench's executor holds: at most one is alive at a time.
const TASKS: Cap = Cap::entries::<64>("bench-tasks", CapFill::Serving);

/// The pad that puts a padded class's slot at 16 slots a chunk: a future of about 2,900 B.
const PAD_BYTES: usize = 2_900;
/// The padded classes, at one directory level and at two.
const PADDED_2500: Cap = Cap::entries::<2_500>("bench-padded-2500", CapFill::Serving);
const PADDED_10000: Cap = Cap::entries::<10_000>("bench-padded-10000", CapFill::Serving);

/// An executor of one class, the future `factory` makes, on a census of its own.
fn executor_of<Args, Fac: TaskFactory<Args>>(factory: &Fac) -> (CellExecutor, TaskClass) {
    executor_with(factory, TASKS)
}

/// An executor of one class of `cap` tasks, the future `factory` makes.
fn executor_with<Args, Fac: TaskFactory<Args>>(
    factory: &Fac,
    cap: Cap,
) -> (CellExecutor, TaskClass) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(factory, cap).expect("one class");
    (layout.build().expect("no class refuses"), class)
}

/// One task of `class`, made by `make`, Ready on its first poll.
#[inline(always)]
fn ready_once<F: Future<Output = ()> + 'static>(
    ex: &mut CellExecutor,
    class: TaskClass,
    make: impl FnOnce() -> F,
) {
    let TaskAdmission::Granted(permit) = ex.reserve::<F>(class) else {
        panic!("the bench's class has room");
    };
    let outcome = permit.poll_immediate(make);
    assert!(matches!(outcome, PollImmediate::Completed));
}

/// The fast path's command as a task.
async fn command_task(counter: Rc<Cell<u64>>) {
    command_direct(black_box(&counter));
}

/// The fast path's command as a padded task: its pad lives across a ready await.
async fn padded_task(counter: Rc<Cell<u64>>) {
    let pad = MaybeUninit::<[u8; PAD_BYTES]>::uninit();
    core::future::ready(()).await;
    black_box(&pad);
    command_direct(black_box(&counter));
}

/// The slow path's task: it awaits its gate's value and keeps it.
async fn waiter_task(waiter: GateWait<u64, u64>, sink: Rc<Cell<u64>>) {
    sink.set(waiter.await);
}

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

    let (mut ex, class) = executor_of(&command_task);
    let counter = Rc::new(Cell::new(0u64));
    group.bench_function("poll_immediate_ready", |b| {
        b.iter(|| {
            let counter = Rc::clone(&counter);
            let TaskAdmission::Granted(permit) = ex.reserve(class) else {
                panic!("the bench's class has room");
            };
            let outcome = permit.poll_immediate(|| command_task(counter));
            assert!(matches!(outcome, PollImmediate::Completed));
        });
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

    group.finish();
}

fn bench_suspend_resume(c: &mut Criterion) {
    let mut group = c.benchmark_group("executor_slow_path");

    let (mut ex, class) = executor_of(&waiter_task);
    let gate: FabricGate<u64> = FabricGate::new();
    let sink = Rc::new(Cell::new(0u64));
    let mut token = 0u64;
    group.bench_function("suspend_complete_resume", |b| {
        b.iter(|| {
            token += 1;
            let waiter = gate.waiter(token);
            let sink = Rc::clone(&sink);
            let TaskAdmission::Granted(permit) = ex.reserve(class) else {
                panic!("the bench's class has room");
            };
            let outcome = permit.poll_immediate(|| waiter_task(waiter, sink));
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
