//! Executor + gate semantics (M0-S06 AC): fast-path behavior, suspension
//! correctness under randomized interleavings vs a reference model, and
//! zero-leak slab accounting.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::gate::{KeyedGate, ListWait};
use inf_runtime::{
    CellExecutor, CompletionToken, ExecutorLayout, FabricGate, IoGate, PollImmediate,
    TaskAdmission, TaskClass, TokenClass, WaitList, WatermarkGate, Woken,
};
use proptest::prelude::*;

#[path = "support/tasks.rs"]
mod tasks;
use tasks::TestExecutor;

#[test]
fn ready_future_never_allocates_a_task_slot() {
    let mut ex = TestExecutor::new();
    let hits = Rc::new(RefCell::new(0));
    for _ in 0..100 {
        let hits = Rc::clone(&hits);
        let outcome = ex.poll_immediate(async move {
            *hits.borrow_mut() += 1;
        });
        assert_eq!(outcome, PollImmediate::Completed);
    }
    assert_eq!(*hits.borrow(), 100);
    assert_eq!(ex.live_tasks(), 0, "fast path must not occupy slots");
}

#[test]
fn suspended_future_promotes_resumes_and_frees() {
    let mut ex = TestExecutor::new();
    let gate: FabricGate<u64> = FabricGate::new();
    let seen = Rc::new(RefCell::new(Vec::new()));

    let waiter = gate.waiter(7);
    let sink = Rc::clone(&seen);
    let outcome = ex.poll_immediate(async move {
        let value = waiter.await;
        sink.borrow_mut().push(value);
    });
    let PollImmediate::Suspended(id) = outcome else {
        panic!("gate waiter must suspend, got {outcome:?}")
    };
    assert!(ex.is_live(id));
    assert_eq!(ex.live_tasks(), 1);
    assert_eq!(ex.run_ready(64), 0, "nothing ready before completion");

    assert!(gate.complete(7, 42));
    assert_eq!(ex.run_ready(64), 1);
    assert_eq!(*seen.borrow(), vec![42]);
    assert_eq!(ex.live_tasks(), 0, "completed task must free its slot");
    assert!(!ex.is_live(id));
}

#[test]
fn delivery_before_first_poll_keeps_the_fast_path() {
    let mut ex = TestExecutor::new();
    let gate: FabricGate<&'static str> = FabricGate::new();
    let waiter = gate.waiter(1);
    // Reply lands before the future ever polls (same-iteration delivery).
    assert!(gate.complete(1, "early"));
    let seen = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&seen);
    let outcome = ex.poll_immediate(async move {
        *sink.borrow_mut() = Some(waiter.await);
    });
    assert_eq!(outcome, PollImmediate::Completed, "parked value ⇒ no suspension");
    assert_eq!(*seen.borrow(), Some("early"));
    assert_eq!(gate.pending(), 0);
}

#[test]
fn spawn_local_runs_under_budget() {
    let mut ex = TestExecutor::new();
    let ran = Rc::new(RefCell::new(0));
    for _ in 0..10 {
        let ran = Rc::clone(&ran);
        ex.spawn_local(async move {
            *ran.borrow_mut() += 1;
        });
    }
    assert_eq!(ex.live_tasks(), 10);
    assert_eq!(ex.run_ready(3), 3, "budget bounds the slice");
    assert_eq!(*ran.borrow(), 3);
    assert_eq!(ex.run_ready(100), 7);
    assert_eq!(*ran.borrow(), 10);
    assert_eq!(ex.live_tasks(), 0);
}

/// Future that returns Pending once (waking itself immediately), then Ready
/// — exercises the wake-during-poll (`RunningWoken`) transition.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn wake_during_poll_requeues_without_losing_the_task() {
    let mut ex = TestExecutor::new();
    let done = Rc::new(RefCell::new(false));
    let flag = Rc::clone(&done);
    let outcome = ex.poll_immediate(async move {
        YieldOnce(false).await;
        *flag.borrow_mut() = true;
    });
    assert!(matches!(outcome, PollImmediate::Suspended(_)));
    // The self-wake must already have it queued — no external event needed.
    assert_eq!(ex.run_ready(64), 1);
    assert!(*done.borrow());
    assert_eq!(ex.live_tasks(), 0);
}

/// A future that leaks a waker clone and then completes immediately —
/// the executor must tolerate the escaped clone (a Retired task's wakes are
/// no-ops; the clone's drop links its reclaim, charged to the next slice).
struct WakerThief(Rc<RefCell<Option<Waker>>>);

impl Future for WakerThief {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        *self.0.borrow_mut() = Some(cx.waker().clone());
        Poll::Ready(())
    }
}

#[test]
fn escaped_waker_clone_on_fast_path_is_harmless() {
    let mut ex = TestExecutor::new();
    let stolen = Rc::new(RefCell::new(None));
    let outcome = ex.poll_immediate(WakerThief(Rc::clone(&stolen)));
    assert_eq!(outcome, PollImmediate::Completed);
    let waker = stolen.borrow_mut().take().expect("clone escaped");
    assert_eq!(ex.retired(), 1, "the escaped clone keeps the finished task Retired");
    waker.wake_by_ref(); // Retired: must be a silent no-op
    assert_eq!(ex.live_tasks(), 0);
    assert!(!ex.has_ready(), "a Retired task's wake must not queue anything");
    waker.wake(); // the last clone: its drop links the reclaim
    assert_eq!(ex.run_ready(64), 1, "one reclaim entry, no activation");
    assert_eq!(ex.retired(), 0);
}

#[test]
fn io_gate_routes_by_completion_token() {
    let mut ex = TestExecutor::new();
    let gate = IoGate::new();
    let token = CompletionToken::new(TokenClass::Recv, 3, 9);
    let waiter = gate.waiter(token);
    let seen = Rc::new(RefCell::new(false));
    let sink = Rc::clone(&seen);
    let outcome = ex.poll_immediate(async move {
        let result = waiter.await;
        assert!(matches!(result, inf_runtime::CompletionResult::Closed));
        *sink.borrow_mut() = true;
    });
    assert!(matches!(outcome, PollImmediate::Suspended(_)));
    gate.complete(token, inf_runtime::CompletionResult::Closed);
    ex.run_ready(8);
    assert!(*seen.borrow());
}

#[test]
fn waitlist_is_fifo_and_wake_one_wakes_exactly_one() {
    let mut ex = TestExecutor::new();
    let list: WaitList<u32> = WaitList::new();
    let order = Rc::new(RefCell::new(Vec::new()));
    for i in 0..3u32 {
        let wait = list.wait(5);
        let order = Rc::clone(&order);
        ex.spawn_local(async move {
            wait.await;
            order.borrow_mut().push(i);
        });
    }
    ex.run_ready(64); // all suspend
    assert_eq!(list.waiting(), 3);

    assert!(list.wake_one(5));
    ex.run_ready(64);
    assert_eq!(*order.borrow(), vec![0], "FIFO: first waiter first");

    assert_eq!(list.wake_all(5), 2);
    ex.run_ready(64);
    assert_eq!(*order.borrow(), vec![0, 1, 2]);
    assert_eq!(ex.live_tasks(), 0);
    assert_eq!(list.waiting(), 0);
}

#[test]
fn waitlist_drop_passes_the_baton() {
    let list: WaitList<u32> = WaitList::new();
    let first: ListWait<u32> = list.wait(1);
    let _second = list.wait(1);
    assert!(list.wake_one(1), "wakes `first`");
    // `first` is dropped without observing its wake (timeout path): the
    // baton must pass to `second`, not vanish.
    drop(first);
    assert_eq!(list.waiting(), 0, "second was woken by the baton pass");
}

/// `any_waiting` is `waiting() > 0` in every state of a queue: empty, a
/// live waiter behind a cancelled one, cancelled entries only, and a woken
/// waiter that has not run yet.
#[test]
fn waitlist_any_waiting_is_true_exactly_when_a_live_waiter_is_queued() {
    let list: WaitList<u32> = WaitList::new();
    assert!(!list.any_waiting(), "an empty list");
    let first = list.wait(1);
    let second = list.wait(1);
    let other_key = list.wait(2);
    assert!(list.any_waiting());
    // Cancelled in place: the entry stays queued until a wake skips it.
    drop(first);
    assert_eq!(list.waiting(), 2);
    assert!(list.any_waiting(), "a live waiter behind a cancelled entry");
    drop(second);
    drop(other_key);
    assert_eq!(list.waiting(), 0);
    assert!(!list.any_waiting(), "cancelled entries are not waiters");
    let woken = list.wait(3);
    assert!(list.wake_one(3));
    assert_eq!(list.waiting(), 0);
    assert!(!list.any_waiting(), "a woken waiter is no longer queued");
    drop(woken);
}

#[test]
fn watermark_gate_wakes_at_or_below() {
    let mut ex = TestExecutor::new();
    let gate = WatermarkGate::new();
    let acked = Rc::new(RefCell::new(Vec::new()));
    for lsn in [5u64, 10, 15] {
        let waiter = gate.waiter(lsn);
        let acked = Rc::clone(&acked);
        ex.spawn_local(async move {
            waiter.await;
            acked.borrow_mut().push(lsn);
        });
    }
    ex.run_ready(64);
    assert_eq!(gate.waiting(), 3);

    assert_eq!(gate.advance(10), 2);
    ex.run_ready(64);
    assert_eq!(*acked.borrow(), vec![5, 10]);

    assert_eq!(gate.advance(7), 0, "watermark is monotonic");
    assert_eq!(gate.advance(100), 1);
    ex.run_ready(64);
    assert_eq!(*acked.borrow(), vec![5, 10, 15]);
    assert_eq!(ex.live_tasks(), 0);

    // At-or-below the current watermark: ready without suspending.
    let mut ready = gate.waiter(50);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(Pin::new(&mut ready).poll(&mut cx).is_ready());
}

// ---- Randomized interleaving proptest (M0-S06 AC) ---------------------------

/// 256 cases natively; under Miri, 8 cases and no failure file, which isolation forbids (the
/// interpreter's checks, not the case count, are what that run is for).
fn interleavings() -> ProptestConfig {
    if cfg!(miri) {
        ProptestConfig { cases: 8, failure_persistence: None, ..ProptestConfig::default() }
    } else {
        ProptestConfig::with_cases(256)
    }
}

proptest! {
    #![proptest_config(interleavings())]

    /// N tasks each await a unique key; completions and executor slices are
    /// interleaved in random order. Reference model: every task completes
    /// exactly once, observing its own payload; slab and gate end empty.
    #[test]
    fn random_suspend_resume_interleavings(
        n_tasks in 1usize..24,
        // Each schedule step: Some(task idx to complete) or None (run a slice).
        schedule in proptest::collection::vec(proptest::option::of(0usize..24), 1..96),
        budget in 1usize..8,
    ) {
        let mut ex = TestExecutor::new();
        let gate: KeyedGate<u64, u64> = KeyedGate::new();
        let done: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

        for key in 0..n_tasks as u64 {
            let waiter = gate.waiter(key);
            let done = Rc::clone(&done);
            let outcome = ex.poll_immediate(async move {
                let payload = waiter.await;
                done.borrow_mut().push(payload);
            });
            prop_assert!(matches!(outcome, PollImmediate::Suspended(_)));
        }

        let mut completed = vec![false; n_tasks];
        for step in schedule {
            match step {
                Some(idx) if idx < n_tasks && !completed[idx] => {
                    completed[idx] = true;
                    prop_assert!(gate.complete(idx as u64, idx as u64 * 100));
                }
                _ => { ex.run_ready(budget); }
            }
        }
        // Drain the tail: complete the rest, run to quiescence.
        for (idx, was_done) in completed.iter().enumerate() {
            if !was_done {
                prop_assert!(gate.complete(idx as u64, idx as u64 * 100));
            }
        }
        while ex.run_ready(16) > 0 {}

        let mut observed = done.borrow().clone();
        observed.sort_unstable();
        let expected: Vec<u64> = (0..n_tasks as u64).map(|k| k * 100).collect();
        prop_assert_eq!(observed, expected, "every task exactly once, right payload");
        prop_assert_eq!(ex.live_tasks(), 0, "slab leak");
        prop_assert_eq!(gate.pending(), 0, "gate leak");
    }

    /// Fast path storm mixed with suspensions: slab occupancy returns to
    /// zero and scratch reuse never corrupts a live future.
    #[test]
    fn mixed_fast_and_slow_paths_reconcile(ops in proptest::collection::vec(0u8..4, 1..200)) {
        let mut ex = TestExecutor::new();
        let gate: KeyedGate<u64, ()> = KeyedGate::new();
        let mut next_key = 0u64;
        let mut open = Vec::new();
        let counter = Rc::new(RefCell::new(0u64));

        for op in ops {
            match op {
                // Fast-path command.
                0 | 1 => {
                    let c = Rc::clone(&counter);
                    let r = ex.poll_immediate(async move { *c.borrow_mut() += 1; });
                    prop_assert_eq!(r, PollImmediate::Completed);
                }
                // Suspend on a fresh key.
                2 => {
                    let key = next_key;
                    next_key += 1;
                    let waiter = gate.waiter(key);
                    let c = Rc::clone(&counter);
                    let r = ex.poll_immediate(async move { waiter.await; *c.borrow_mut() += 1; });
                    prop_assert!(matches!(r, PollImmediate::Suspended(_)));
                    open.push(key);
                }
                // Complete the oldest open key.
                _ => {
                    if let Some(key) = open.first().copied() {
                        open.remove(0);
                        gate.complete(key, ());
                        ex.run_ready(4);
                    }
                }
            }
        }
        for key in open {
            gate.complete(key, ());
        }
        while ex.run_ready(16) > 0 {}
        prop_assert_eq!(ex.live_tasks(), 0);
        prop_assert_eq!(gate.pending(), 0);
    }
}

/// Two executors built alike, each layout on its own cell's census, mint the same class index
/// and `TypeId`: a class of one reserved on the other is `Foreign`, and a `TaskId` of one given
/// to the other's `wake` is `Stale`; nothing is taken or woken on either (ADR-0171 A2.1).
#[test]
fn a_class_and_a_task_of_one_executor_are_foreign_to_another_built_alike() {
    let mut one = TestExecutor::at(0);
    let mut other = TestExecutor::at(1);
    let id = one.spawn_local(core::future::pending::<()>());
    assert_eq!(one.run_ready(1), 1, "the task parks");
    let admission = other.reserve::<tasks::Boxed>(one.class());
    assert!(
        matches!(admission, TaskAdmission::Foreign),
        "a class of one executor: Foreign, not Granted on the other, built alike"
    );
    drop(admission);
    assert_eq!(other.wake(id), Woken::Stale, "a task of one executor is Stale on the other");
    assert!(!one.has_ready() && !other.has_ready(), "nothing woken on either");
    assert_eq!(other.class_tasks(other.class()), 0, "nothing taken on the other");
    assert_eq!(other.class_tasks(one.class()), 0, "the other reads no count for one's class");
    assert_eq!(one.class_tasks(one.class()), 1, "one's task stays");
}

/// A future of no bytes: it completes on its second poll, counted in a thread-local, since it
/// holds nothing.
struct ZeroSized;

thread_local! {
    static ZERO_SIZED_POLLS: Cell<u32> = const { Cell::new(0) };
}

impl Future for ZeroSized {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let polls = ZERO_SIZED_POLLS.with(|polls| {
            polls.set(polls.get() + 1);
            polls.get()
        });
        if polls == 1 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(())
    }
}

/// A future aligned past any allocator's default: it reads its own address at each poll and
/// completes on its second.
#[repr(align(4096))]
struct OverAligned {
    seen: Rc<RefCell<Vec<usize>>>,
}

impl Future for OverAligned {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let at = core::ptr::from_ref::<Self>(&self) as usize;
        let mut seen = self.seen.borrow_mut();
        seen.push(at);
        if seen.len() == 1 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(())
    }
}

const ODD_TASKS: Cap = Cap::entries::<2>("test-odd-futures", CapFill::Serving);

/// An executor of one class of `F`, which `factory` makes.
fn of_one<F: Future<Output = ()> + 'static>(
    factory: &impl Fn(F) -> F,
) -> (CellExecutor, TaskClass) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(factory, ODD_TASKS).expect("one class");
    (layout.build().expect("no class refuses"), class)
}

/// A zero-sized future and one aligned to 4 KiB, each its class's one type, unboxed: each is
/// placed in its slot before its first poll, woken by its own waker, polled at the address it was
/// placed at (the aligned one at its alignment), and dropped in place, by `poll_immediate` and by
/// `spawn_local`.
#[test]
fn a_zero_sized_and_an_over_aligned_future_run_in_their_slots() {
    let (mut executor, class) = of_one(&|future: ZeroSized| future);
    let TaskAdmission::Granted(permit) = executor.reserve::<ZeroSized>(class) else {
        panic!("the class has room");
    };
    assert!(matches!(permit.poll_immediate(|| ZeroSized), PollImmediate::Suspended(_)));
    assert_eq!(executor.run_ready(4), 1, "the zero-sized future's own wake polls it");
    assert_eq!(ZERO_SIZED_POLLS.with(Cell::get), 2);
    assert_eq!(executor.live_tasks(), 0);

    let (mut executor, class) = of_one(&|future: OverAligned| future);
    for immediate in [true, false] {
        let seen: Rc<RefCell<Vec<usize>>> = Rc::default();
        let TaskAdmission::Granted(permit) = executor.reserve::<OverAligned>(class) else {
            panic!("the class has room");
        };
        let make = || OverAligned { seen: Rc::clone(&seen) };
        if immediate {
            assert!(matches!(permit.poll_immediate(make), PollImmediate::Suspended(_)));
        } else {
            permit.spawn_local(make);
            assert_eq!(executor.run_ready(1), 1, "the first poll");
        }
        assert_eq!(executor.run_ready(4), 1, "the aligned future's own wake polls it");
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2, "immediate {immediate}: two polls");
        assert_eq!(seen[0], seen[1], "immediate {immediate}: polled where it was placed");
        assert_eq!(seen[0] % 4096, 0, "immediate {immediate}: at its alignment");
        assert_eq!(executor.live_tasks(), 0);
    }
}
