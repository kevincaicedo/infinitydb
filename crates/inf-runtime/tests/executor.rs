//! Executor + gate semantics (M0-S06 AC): fast-path behavior, suspension
//! correctness under randomized interleavings vs a reference model, and
//! zero-leak slab accounting.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::marker::PhantomPinned;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::gate::{KeyedGate, KeyedWait, ListWait};
use inf_runtime::{
    CellExecutor, CompletionToken, Delivery, ExecutorLayout, FabricAdmission, FabricGate, GateKey,
    IoGate, KeyedAdmission, PollImmediate, TaskAdmission, TaskClass, TaskId, TokenClass, WaitList,
    WatermarkGate, Woken, park,
};
use proptest::prelude::*;

#[path = "support/tasks.rs"]
mod tasks;
use tasks::TestExecutor;

/// A task's wait on a fabric gate: the future owns the gate's handle and its
/// one unit, which its wait borrows, and publishes the key it waits under,
/// which the test completes.
async fn fabric_wait<V>(gate: Rc<RefCell<FabricGate<V>>>, key: Rc<Cell<u64>>) -> V {
    let units = gate.borrow().units(1).expect("a unit");
    let wait = {
        let mut open = gate.borrow_mut();
        let FabricAdmission::Granted(permit) = open.reserve(&units) else {
            panic!("a gate of one free holder");
        };
        key.set(permit.key());
        permit.register(&gate)
    };
    wait.await
}

/// A fabric gate of `units` units over as many holders, on a census of its
/// own, and the cell its wait's key is published in.
fn fabric_gate<V>(units: u32) -> (Rc<RefCell<FabricGate<V>>>, Rc<Cell<u64>>) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let holders = Cap::derived(&TEST_GATE_HOLDERS, units).expect("a test gate's holders");
    let gate = FabricGate::build(0, units, Some(holders), &census).expect("a test gate");
    (Rc::new(RefCell::new(gate)), Rc::new(Cell::new(0)))
}

/// The tests' gate holders' ceiling.
const TEST_GATE_HOLDERS: Cap = Cap::entries::<64>("test-gate-holders", CapFill::Serving);

/// The tests' keyed gates' routes and holders: past any test's open waits.
const TEST_KEYED_HOLDERS: Cap = Cap::entries::<256>("test-keyed-holders", CapFill::Serving);

/// A keyed gate of [`TEST_KEYED_HOLDERS`] on a census of its own.
fn keyed_gate<K: GateKey, V>() -> KeyedGate<K, V> {
    let (census, _mark) = CapCensus::new(CellId(0));
    KeyedGate::build(TEST_KEYED_HOLDERS, &census).expect("the test gate")
}

/// A registered wait on a keyed gate.
fn keyed_wait<K: GateKey, V>(gate: &KeyedGate<K, V>, key: K) -> KeyedWait<K, V> {
    let KeyedAdmission::Granted(permit) = gate.reserve(key) else {
        panic!("a fresh key on a gate with room");
    };
    permit.register()
}

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
    let (gate, key) = fabric_gate::<u64>(1);
    let seen = Rc::new(RefCell::new(Vec::new()));

    let waiter = fabric_wait(Rc::clone(&gate), Rc::clone(&key));
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

    assert_eq!(FabricGate::complete(&gate, key.get(), 42), Delivery::Delivered);
    assert_eq!(ex.run_ready(64), 1);
    assert_eq!(*seen.borrow(), vec![42]);
    assert_eq!(ex.live_tasks(), 0, "completed task must free its slot");
    assert!(!ex.is_live(id));
}

#[test]
fn delivery_before_first_poll_keeps_the_fast_path() {
    let mut ex = TestExecutor::new();
    let (gate, _) = fabric_gate::<&'static str>(1);
    let seen = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&seen);
    let own = Rc::clone(&gate);
    let outcome = ex.poll_immediate(async move {
        let gate = own;
        let units = gate.borrow().units(1).expect("a unit");
        let waiter = {
            let mut open = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = open.reserve(&units) else {
                panic!("a gate of one free holder");
            };
            let key = permit.key();
            let waiter = permit.register(&gate);
            drop(open);
            // Reply lands before the waiter ever polls (same-iteration delivery).
            assert_eq!(FabricGate::complete(&gate, key, "early"), Delivery::Delivered);
            waiter
        };
        *sink.borrow_mut() = Some(waiter.await);
    });
    assert_eq!(outcome, PollImmediate::Completed, "parked value ⇒ no suspension");
    assert_eq!(*seen.borrow(), Some("early"));
    assert_eq!(gate.borrow().holders(), 0);
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
    let gate: IoGate = keyed_gate();
    let token = CompletionToken::new(TokenClass::Recv, 3, 9);
    let waiter = keyed_wait(&gate, token);
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
        let gate: KeyedGate<u64, u64> = keyed_gate();
        let done: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));

        for key in 0..n_tasks as u64 {
            let waiter = keyed_wait(&gate, key);
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
                    let delivery = gate.complete(idx as u64, idx as u64 * 100);
                    prop_assert_eq!(delivery, Delivery::Delivered);
                }
                _ => { ex.run_ready(budget); }
            }
        }
        // Drain the tail: complete the rest, run to quiescence.
        for (idx, was_done) in completed.iter().enumerate() {
            if !was_done {
                let delivery = gate.complete(idx as u64, idx as u64 * 100);
                prop_assert_eq!(delivery, Delivery::Delivered);
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
        let gate: KeyedGate<u64, ()> = keyed_gate();
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
                    let waiter = keyed_wait(&gate, key);
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

/// Ready at its `ready_at`-th poll; before it, wakes a clone of its waker and is Pending. It
/// records its own address at each poll and at its drop. `Unpin`, so a move of it is sound and
/// seen.
struct Recorder {
    polls: Rc<Cell<u32>>,
    at: Rc<RefCell<Vec<usize>>>,
    ready_at: u32,
}

impl Recorder {
    /// Pending once, woken by its own clone, then Ready.
    fn twice(polls: &Rc<Cell<u32>>, at: &Rc<RefCell<Vec<usize>>>) -> Recorder {
        Recorder { polls: Rc::clone(polls), at: Rc::clone(at), ready_at: 2 }
    }

    /// Ready on its first poll.
    fn once(at: &Rc<RefCell<Vec<usize>>>) -> Recorder {
        Recorder { polls: Rc::default(), at: Rc::clone(at), ready_at: 1 }
    }
}

impl Future for Recorder {
    type Output = ();

    #[allow(clippy::waker_clone_wake, reason = "the input is a clone woken, counted in and out")]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.at.borrow_mut().push(core::ptr::from_ref::<Self>(&self) as usize);
        self.polls.set(self.polls.get() + 1);
        if self.polls.get() < self.ready_at {
            cx.waker().clone().wake();
            return Poll::Pending;
        }
        Poll::Ready(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.at.borrow_mut().push(core::ptr::from_ref::<Self>(self) as usize);
    }
}

const RECORDERS: Cap = Cap::entries::<2>("test-recorders", CapFill::Serving);

/// An executor of one class of recorders, of two slots (a box would hide a move), and its
/// census.
fn recorders() -> (CellExecutor, TaskClass, Rc<CapCensus>) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(&|recorder: Recorder| recorder, RECORDERS).expect("one class");
    (layout.build().expect("no class refuses"), class, census)
}

/// One recorder made a task of `class` and polled once in place.
fn poll_recorder(
    executor: &mut CellExecutor,
    class: TaskClass,
    recorder: Recorder,
) -> PollImmediate {
    let TaskAdmission::Granted(permit) = executor.reserve::<Recorder>(class) else {
        panic!("the class has room");
    };
    permit.poll_immediate(|| recorder)
}

/// The first poll runs in its class's spare, in its slot (ADR-0194 D1's note): one address from
/// the first poll to the drop, and the wake it raised kept, so the Pending task is queued once.
#[test]
fn the_first_poll_runs_in_its_slot_and_keeps_its_wake() {
    let (mut executor, class, _census) = recorders();
    let (polls, at) = (Rc::new(Cell::new(0)), Rc::new(RefCell::new(Vec::new())));
    let outcome = poll_recorder(&mut executor, class, Recorder::twice(&polls, &at));
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "the first poll is Pending");
    assert_eq!(executor.run_ready(1), 1, "first-poll-wake-lost: the wake in the first poll");
    assert_eq!(polls.get(), 2, "polled twice");
    let at = at.borrow();
    assert_eq!(at.len(), 3, "two polls and the drop");
    assert!(at.iter().all(|address| *address == at[0]), "first-poll-moved: {at:x?}");
    assert_eq!(executor.live_tasks(), 0);
    assert_eq!(executor.class_tasks(class), 0);
}

/// A Pending first poll takes the spare as its task's slot before its id exists, so the next
/// first poll runs in another slot: the two addresses differ (read before any poll of the first),
/// the first's id stays live, and the class counts one task, then none once it completes.
#[test]
fn the_spare_serves_no_task_an_id_names() {
    let (mut executor, class, _census) = recorders();
    let (polls, first) = (Rc::new(Cell::new(0)), Rc::new(RefCell::new(Vec::new())));
    let outcome = poll_recorder(&mut executor, class, Recorder::twice(&polls, &first));
    let PollImmediate::Suspended(id) = outcome else { panic!("the first is Pending: {outcome:?}") };
    let second = Rc::new(RefCell::new(Vec::new()));
    let outcome = poll_recorder(&mut executor, class, Recorder::once(&second));
    assert_eq!(outcome, PollImmediate::Completed);
    let (first_at, second_at) = (first.borrow()[0], second.borrow()[0]);
    assert_ne!(first_at, second_at, "spare-under-id: a suspended task's slot was the spare");
    assert!(executor.is_live(id), "the first task stays live");
    assert_eq!(executor.class_tasks(class), 1, "one task; the spare is none");
    assert_eq!(executor.run_ready(1), 1, "the first task's own wake polls it");
    assert_eq!(polls.get(), 2);
    assert!(!executor.is_live(id));
    assert_eq!(executor.class_tasks(class), 0);
    assert_eq!(executor.live_tasks(), 0);
}

/// Each other use of the class between fast first polls: a Pending first poll, an id's lookups
/// (`is_live`, `wake`, which read the header alone), a run that polls a task, a spawn and its
/// run, a Retired task's reclaim, and the Pending task's completion; each that borrows the slab
/// forgets the kept lend. Each fast op is counted by its future; Miri reads the whole sequence
/// under both aliasing models.
#[test]
fn the_spare_lend_survives_every_other_borrow() {
    let mut ex = TestExecutor::new();
    let (gate, key) = fabric_gate::<u64>(1);
    let hits = Rc::new(Cell::new(0u64));
    let fast = |ex: &mut TestExecutor| {
        let hits = Rc::clone(&hits);
        let outcome = ex.poll_immediate(async move { hits.set(hits.get() + 1) });
        assert_eq!(outcome, PollImmediate::Completed);
    };
    fast(&mut ex);
    fast(&mut ex);
    let waiter = fabric_wait(Rc::clone(&gate), Rc::clone(&key));
    let sink = Rc::clone(&hits);
    let outcome = ex.poll_immediate(async move {
        let value = waiter.await;
        sink.set(sink.get() + value);
    });
    let PollImmediate::Suspended(id) = outcome else { panic!("the waiter suspends") };
    fast(&mut ex);
    assert!(ex.is_live(id));
    fast(&mut ex);
    assert_eq!(ex.wake(id), Woken::Queued);
    fast(&mut ex);
    assert_eq!(ex.run_ready(8), 1, "the woken task polls and parks again");
    fast(&mut ex);
    let spawned = Rc::clone(&hits);
    ex.spawn_local(async move { spawned.set(spawned.get() + 1_000) });
    fast(&mut ex);
    assert_eq!(ex.run_ready(8), 1);
    fast(&mut ex);
    let stolen = Rc::new(RefCell::new(None));
    assert_eq!(ex.poll_immediate(WakerThief(Rc::clone(&stolen))), PollImmediate::Completed);
    assert_eq!(ex.retired(), 1);
    fast(&mut ex);
    drop(stolen.borrow_mut().take());
    assert_eq!(ex.run_ready(8), 1, "the Retired task's reclaim");
    fast(&mut ex);
    assert_eq!(FabricGate::complete(&gate, key.get(), 1_000_000), Delivery::Delivered);
    fast(&mut ex);
    assert_eq!(ex.run_ready(8), 1, "the waiter completes");
    fast(&mut ex);
    fast(&mut ex);
    assert_eq!(hits.get(), 13 + 1_000 + 1_000_000, "thirteen fast ops, the spawn and the waiter");
    assert_eq!(ex.live_tasks(), 0);
    assert_eq!(ex.retired(), 0);
    assert_eq!(ex.class_tasks(ex.class()), 0);
}

/// Fast ops on one class with an id's lookup and a shared borrow of the slab between them: the
/// lookup reads the task's header alone, so the lend is kept across it; the count passes the
/// slab's shared door, which forgets the lend, so the next fast op lends the spare again. The
/// count reads no chunk, so this test cannot see the door's forget: the unit test
/// `executor::tests::the_shared_door_forgets_the_lend` borrows the spare's chunk through it.
#[test]
fn the_kept_lend_across_an_id_lookup_and_a_shared_borrow() {
    let mut ex = TestExecutor::new();
    let (gate, key) = fabric_gate::<u64>(1);
    let hits = Rc::new(Cell::new(0u64));
    let waiter = fabric_wait(Rc::clone(&gate), Rc::clone(&key));
    let outcome = ex.poll_immediate(async move {
        let _ = waiter.await;
    });
    let PollImmediate::Suspended(id) = outcome else { panic!("the waiter suspends") };
    let fast = |ex: &mut TestExecutor| {
        let hits = Rc::clone(&hits);
        let outcome = ex.poll_immediate(async move { hits.set(hits.get() + 1) });
        assert_eq!(outcome, PollImmediate::Completed);
    };
    for _ in 0..3 {
        fast(&mut ex);
        assert!(ex.is_live(id));
        fast(&mut ex);
        assert_eq!(ex.class_tasks(ex.class()), 1, "the waiter; the spare is none");
    }
    assert_eq!(FabricGate::complete(&gate, key.get(), 1), Delivery::Delivered);
    assert_eq!(ex.run_ready(8), 1);
    assert_eq!(hits.get(), 6);
    assert_eq!(ex.live_tasks(), 0);
}

/// A permit dropped unused leaves its slot the class's spare, which the next first poll runs in:
/// one address for every fast op, with a dropped permit before each, and no task counted.
#[test]
fn a_dropped_permit_leaves_the_spare() {
    let (mut executor, class, _census) = recorders();
    let at = Rc::new(RefCell::new(Vec::new()));
    for _ in 0..3 {
        let admission = executor.reserve::<Recorder>(class);
        assert!(matches!(admission, TaskAdmission::Granted(_)), "the class has room");
        drop(admission);
        assert_eq!(executor.class_tasks(class), 0, "a dropped permit holds no task");
        let outcome = poll_recorder(&mut executor, class, Recorder::once(&at));
        assert_eq!(outcome, PollImmediate::Completed);
        assert_eq!(executor.class_tasks(class), 0);
    }
    let at = at.borrow();
    assert_eq!(at.len(), 6, "three polls and three drops");
    assert!(at.iter().all(|address| *address == at[0]), "one slot, the spare: {at:x?}");
}

/// A class at its cap with its spare: `T - 1` suspended tasks and the spare, so a permit is
/// granted, and its Pending task takes the last slot; then `T` suspended and no spare, so the
/// next `reserve` is `Full`, counted on the class's row, nothing taken or published.
#[test]
fn the_class_at_its_cap_with_its_spare() {
    let (mut executor, class, census) = recorders();
    let (first, second) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let at = Rc::new(RefCell::new(Vec::new()));
    let outcome = poll_recorder(&mut executor, class, Recorder::twice(&first, &at));
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "T - 1 = 1 suspended");
    let outcome = poll_recorder(&mut executor, class, Recorder::once(&at));
    assert_eq!(outcome, PollImmediate::Completed, "the spare made from the last free slot");
    assert_eq!(executor.class_tasks(class), 1);
    let outcome = poll_recorder(&mut executor, class, Recorder::twice(&second, &at));
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "the spare granted: T suspended");
    assert_eq!(executor.class_tasks(class), 2);
    let row = census.row(RECORDERS.name()).expect("the class's row");
    assert_eq!((row.high_water, row.crossings), (2, 0));
    let admission = executor.reserve::<Recorder>(class);
    assert!(matches!(admission, TaskAdmission::Full), "no spare and no free slot");
    drop(admission);
    let row = census.row(RECORDERS.name()).expect("the class's row");
    assert_eq!((row.high_water, row.crossings), (2, 1), "the crossing counted, nothing taken");
    assert_eq!(executor.class_tasks(class), 2);
    assert_eq!(executor.run_ready(4), 2, "each task's own wake polls it");
    assert_eq!(executor.live_tasks(), 0);
}

/// A handle another executor's class minted, whose slot index lies past this executor's class
/// of one slot, is `Stale` to `wake` and not live to `is_live`, its index never used. Two test
/// executors on censuses of one cell mint one instance (ADR-0194 D5), so the index's bound
/// against the class's slots refuses it here, and another cell's id is refused by the instance.
/// With the bound skipped, the lookup reads a header past the array, which the array's own
/// check (and Miri) sees.
#[test]
fn an_id_another_class_minted_past_this_class_is_stale() {
    let mut wide = TestExecutor::new();
    let ids: Vec<_> = (0..3).map(|_| wide.spawn_local(core::future::pending::<()>())).collect();
    assert_eq!(wide.run_ready(8), 3, "each task parks");
    let mut narrow = TestExecutor::one();
    let mut other_cell = TestExecutor::at(1);
    for id in ids {
        assert!(wide.is_live(id));
        assert!(!narrow.is_live(id), "another class's id: not live here");
        assert_eq!(narrow.wake(id), Woken::Stale);
        assert!(!other_cell.is_live(id), "another cell's id");
        assert_eq!(other_cell.wake(id), Woken::Stale);
    }
    assert!(!narrow.has_ready() && !other_cell.has_ready());
}

/// A stale id, its slot's next occupant live, is `Stale` and wakes nothing: the header's handle
/// names the occupant's generation, not the id's.
#[test]
fn a_stale_id_wakes_nothing_of_its_slots_next_occupant() {
    let mut ex = TestExecutor::one();
    let first = ex.spawn_local(async {});
    assert_eq!(ex.run_ready(4), 1, "the first task completes");
    let next = ex.spawn_local(core::future::pending::<()>());
    assert_eq!(ex.run_ready(4), 1, "the next task parks in the same slot");
    assert_eq!(first.generation() + 1, next.generation());
    assert!(!ex.is_live(first));
    assert_eq!(ex.wake(first), Woken::Stale);
    assert!(!ex.has_ready(), "nothing queued");
    assert_eq!(ex.wake(next), Woken::Queued);
}

// ---- Self-referential futures in their slots ---------------------------------------------------

/// Holds `&mut` into its caller's local across a park, as an async fn the plane runs does.
async fn bump(local: &mut [u64; 4]) {
    park().await;
    local[0] += 1;
    local[3] += 7;
}

/// A `!Unpin` future placed unboxed in its slot, suspended holding `&mut` into itself.
async fn self_referential(seen: Rc<Cell<u64>>) {
    let mut local = [0u64; 4];
    bump(&mut local).await;
    seen.set(local[0] + local[3]);
}

const SELF_REFERENTIAL: Cap = Cap::entries::<4>("test-self-referential", CapFill::Serving);

/// An executor of one class of `self_referential` futures, unboxed.
fn self_referential_class() -> (CellExecutor, TaskClass) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(&self_referential, SELF_REFERENTIAL).expect("one class");
    (layout.build().expect("no class refuses"), class)
}

/// One `self_referential` task of `class`, polled once in place: it parks.
fn park_self_referential(ex: &mut CellExecutor, class: TaskClass, seen: &Rc<Cell<u64>>) -> TaskId {
    let TaskAdmission::Granted(permit) = ex.reserve(class) else { panic!("the class has room") };
    let seen = Rc::clone(seen);
    let outcome = permit.poll_immediate(move || self_referential(seen));
    let PollImmediate::Suspended(id) = outcome else { panic!("it parks: {outcome:?}") };
    id
}

/// Unboxed self-referential tasks in one chunk, suspended holding `&mut` into themselves, while
/// the executor reaches their slots and their neighbours': each one's own `wake(id)` and its
/// poll, a neighbour's `wake(id)`, `is_live` and the class's count, and a fast op's first poll
/// in the class's spare between them. Each resumes and reads its own `&mut` (8). Miri reads the
/// sequence under Stacked and Tree Borrows: a lookup's shared reborrow of the chunk leaves the
/// futures, which are in an `UnsafeCell`, as they were.
#[test]
fn self_referential_futures_resume_in_their_slots() {
    let (mut ex, class) = self_referential_class();
    let (a, b) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let id_a = park_self_referential(&mut ex, class, &a);
    let id_b = park_self_referential(&mut ex, class, &b);
    assert!(ex.is_live(id_a) && ex.is_live(id_b));
    assert_eq!(ex.class_tasks(class), 2);
    assert_eq!(ex.wake(id_b), Woken::Queued, "a neighbour's wake");
    assert_eq!(ex.run_ready(4), 1);
    assert_eq!(b.get(), 8, "b resumed through its own &mut");
    let c = Rc::new(Cell::new(0));
    let id_c = park_self_referential(&mut ex, class, &c);
    assert_eq!(ex.wake(id_a), Woken::Queued, "its own wake, after a neighbour's poll");
    assert_eq!(ex.run_ready(4), 1);
    assert_eq!(a.get(), 8, "a resumed through its own &mut");
    assert_eq!(ex.wake(id_c), Woken::Queued);
    assert_eq!(ex.run_ready(4), 1);
    assert_eq!(c.get(), 8);
    assert_eq!(ex.live_tasks(), 0);
}

/// Leaves its waker in `slot` and is Pending once.
struct Export {
    slot: Rc<RefCell<Option<Waker>>>,
    done: bool,
}

impl Future for Export {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.done {
            return Poll::Ready(());
        }
        self.done = true;
        *self.slot.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// `bump`, suspended on its own waker rather than a park.
async fn bump_on_waker(local: &mut [u64; 4], slot: Rc<RefCell<Option<Waker>>>) {
    Export { slot, done: false }.await;
    local[0] += 1;
    local[3] += 7;
}

/// `self_referential`, woken by the waker it left.
async fn self_referential_on_waker(seen: Rc<Cell<u64>>, slot: Rc<RefCell<Option<Waker>>>) {
    let mut local = [0u64; 4];
    bump_on_waker(&mut local, slot).await;
    seen.set(local[0] + local[3]);
}

/// The same, woken by their own wakers, so only the polls reach the slots, while a neighbour is
/// placed, polled and freed in the same chunk.
#[test]
fn self_referential_futures_woken_by_their_wakers_resume_in_their_slots() {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(&self_referential_on_waker, SELF_REFERENTIAL).expect("one class");
    let mut ex = layout.build().expect("no class refuses");
    let mut parked = Vec::new();
    for _ in 0..2 {
        let (seen, slot) = (Rc::new(Cell::new(0)), Rc::new(RefCell::new(None)));
        let TaskAdmission::Granted(permit) = ex.reserve(class) else { panic!("room") };
        let (s, w) = (Rc::clone(&seen), Rc::clone(&slot));
        let _ = permit.spawn_local(move || self_referential_on_waker(s, w));
        assert_eq!(ex.run_ready(4), 1, "it parks on its waker");
        parked.push((seen, slot));
    }
    for (seen, slot) in parked.iter().rev() {
        slot.borrow_mut().take().expect("the waker it left").wake();
        assert_eq!(ex.run_ready(4), 1);
        assert_eq!(seen.get(), 8);
    }
    assert_eq!(ex.live_tasks(), 0);
}

/// What registering futures leave beside them for the test to read: the addresses their polls
/// registered and their drops unregistered, every address a poll ran at, the polls that found
/// their own address registered already (a future placed over a live one), their drops, and a
/// waker one of them kept.
#[derive(Default)]
struct Registry {
    registered: RefCell<Vec<usize>>,
    polled_at: RefCell<Vec<usize>>,
    written_over: Cell<u32>,
    drops: Cell<u32>,
    kept: RefCell<Option<Waker>>,
}

/// What a registering future does at its first poll, and at its drop.
#[derive(Clone, Copy)]
enum Act {
    Ready,
    Panic,
    /// Keeps a clone of its waker in the registry, then panics.
    PanicKeepingWaker,
    /// Keeps a clone of its waker in the registry and is Pending.
    PendingKeepingWaker,
    /// Pending; its drop panics.
    PendingPanickingInDrop,
    /// Ready; its drop panics.
    ReadyPanickingInDrop,
}

/// A `!Unpin` future that registers its own address at its poll and unregisters it at its drop,
/// as an intrusive waiter does, which only Pin's drop guarantee makes sound: a registration left
/// after the executor's drop, or a poll that finds its address registered, is that guarantee
/// broken. Unboxed, so its address is its slot's.
struct Registering {
    act: Act,
    registry: Rc<Registry>,
    _pinned: PhantomPinned,
}

impl Future for Registering {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let at = core::ptr::from_ref::<Self>(self.as_ref().get_ref()) as usize;
        let registry = &self.registry;
        if registry.registered.borrow().contains(&at) {
            registry.written_over.set(registry.written_over.get() + 1);
        }
        registry.registered.borrow_mut().push(at);
        registry.polled_at.borrow_mut().push(at);
        match self.act {
            Act::Ready | Act::ReadyPanickingInDrop => Poll::Ready(()),
            Act::Panic => panic!("the future's first poll panics"),
            Act::PanicKeepingWaker => {
                *registry.kept.borrow_mut() = Some(cx.waker().clone());
                panic!("the future's first poll panics, a clone of its waker kept")
            }
            Act::PendingKeepingWaker => {
                *registry.kept.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            Act::PendingPanickingInDrop => Poll::Pending,
        }
    }
}

impl Drop for Registering {
    fn drop(&mut self) {
        let at = core::ptr::from_ref::<Self>(self) as usize;
        self.registry.registered.borrow_mut().retain(|registered| *registered != at);
        self.registry.drops.set(self.registry.drops.get() + 1);
        if matches!(self.act, Act::PendingPanickingInDrop | Act::ReadyPanickingInDrop) {
            panic!("the future's drop panics");
        }
    }
}

const REGISTERING: Cap = Cap::entries::<2>("test-registering", CapFill::Serving);

/// An executor of one class of registering futures, unboxed, of two slots.
fn registering() -> (CellExecutor, TaskClass) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(&|future: Registering| future, REGISTERING).expect("one class");
    (layout.build().expect("no class refuses"), class)
}

/// One registering future made a task of `class` and polled once in place.
fn poll_registering(
    executor: &mut CellExecutor,
    class: TaskClass,
    act: Act,
    registry: &Rc<Registry>,
) -> PollImmediate {
    let TaskAdmission::Granted(permit) = executor.reserve::<Registering>(class) else {
        panic!("the class has room");
    };
    let registry = Rc::clone(registry);
    permit.poll_immediate(move || Registering { act, registry, _pinned: PhantomPinned })
}

/// The same, its first poll's call panicking, caught past the permit's call.
fn poll_registering_unwinds(
    executor: &mut CellExecutor,
    class: TaskClass,
    act: Act,
    registry: &Rc<Registry>,
) {
    let unwound = catch_unwind(AssertUnwindSafe(|| {
        poll_registering(executor, class, act, registry);
    }));
    assert!(unwound.is_err(), "the first poll's call panics");
}

/// A first poll that unwinds leaves its future live in its slot, the slot its task's: the class
/// counts the task and nothing is dropped until the executor's drop drops the future in place,
/// which unregisters it. With the spare left holding the slot, nothing counts the task and the
/// executor's drop never reaches its future.
#[test]
fn a_first_poll_that_unwinds_holds_its_slot_until_the_drop() {
    let (mut executor, class) = registering();
    let registry = Rc::new(Registry::default());
    poll_registering_unwinds(&mut executor, class, Act::Panic, &registry);
    assert_eq!(executor.live_tasks(), 1, "first-poll-unwind-unowned: the task is live");
    assert_eq!(executor.class_tasks(class), 1, "first-poll-unwind-unowned: no slot held");
    assert_eq!(registry.drops.get(), 0, "the unwind drops nothing");
    drop(executor);
    assert_eq!(registry.drops.get(), 1, "first-poll-unwind-unowned: the future never dropped");
    let left = registry.registered.borrow();
    assert!(left.is_empty(), "first-poll-unwind-unowned: a registration outlived it: {left:x?}");
}

/// After a first poll that unwinds, the next first poll runs in another slot, which the class
/// makes its spare: no future is placed over the live one. The executor's drop then drops the
/// unwound future: each future dropped once, no registration left.
#[test]
fn a_first_poll_that_unwinds_is_not_written_over() {
    let (mut executor, class) = registering();
    let registry = Rc::new(Registry::default());
    poll_registering_unwinds(&mut executor, class, Act::Panic, &registry);
    let outcome = poll_registering(&mut executor, class, Act::Ready, &registry);
    assert_eq!(outcome, PollImmediate::Completed);
    let over = registry.written_over.get();
    assert_eq!(over, 0, "first-poll-unwind-unowned: a future placed over a live one");
    assert_eq!(registry.drops.get(), 1, "the fast op's future, dropped in place");
    assert_eq!(executor.class_tasks(class), 1, "the unwound task's slot; the spare is none");
    drop(executor);
    assert_eq!(registry.drops.get(), 2, "first-poll-unwind-unowned: a future never dropped");
    let left = registry.registered.borrow();
    assert!(left.is_empty(), "first-poll-unwind-unowned: a registration outlived it: {left:x?}");
}

/// A first poll Ready whose future's drop unwinds: the drop ran, so the cell holds no live future
/// and the slot stays the class's spare. No slot is held, and the next fast op runs in the same
/// slot; were the spare handed to the header as for a live future, the slot would stay held for
/// good and the next fast op would run in another.
#[test]
fn a_ready_first_poll_whose_drop_unwinds_keeps_the_spare() {
    let (mut executor, class) = registering();
    let registry = Rc::new(Registry::default());
    poll_registering_unwinds(&mut executor, class, Act::ReadyPanickingInDrop, &registry);
    assert_eq!(registry.drops.get(), 1, "the Ready arm dropped the future");
    assert_eq!(executor.class_tasks(class), 0, "the Ready arm's unwind holds no slot");
    let outcome = poll_registering(&mut executor, class, Act::Ready, &registry);
    assert_eq!(outcome, PollImmediate::Completed);
    assert_eq!(executor.class_tasks(class), 0, "the spare is no task's");
    let polled_at = registry.polled_at.borrow();
    assert_eq!(polled_at.len(), 2, "two first polls");
    assert_eq!(polled_at[0], polled_at[1], "the next fast op in the spare's slot: {polled_at:x?}");
    drop(polled_at);
    drop(executor);
    assert_eq!(registry.drops.get(), 2, "each future dropped once");
    assert!(registry.registered.borrow().is_empty(), "no registration left");
}

/// A first poll that unwinds after cloning its waker: a wake of the clone marks the task woken
/// and links nothing, since no poll of it returns, so `run_ready` polls nothing. The executor's
/// drop drops the future in place and closes the header the clone names; the clone's drop, after
/// the executor's, releases it, which Miri's leak check reads.
#[test]
fn a_first_poll_that_unwinds_with_a_waker_kept() {
    let (mut executor, class) = registering();
    let registry = Rc::new(Registry::default());
    poll_registering_unwinds(&mut executor, class, Act::PanicKeepingWaker, &registry);
    let kept = registry.kept.borrow_mut().take().expect("the waker the first poll kept");
    kept.wake_by_ref();
    assert_eq!(executor.run_ready(4), 0, "a wake of the unwound task activates nothing");
    assert!(!executor.has_ready());
    assert_eq!(executor.class_tasks(class), 1, "first-poll-unwind-unowned: no slot held");
    assert_eq!(registry.drops.get(), 0, "the unwind drops nothing");
    drop(executor);
    assert_eq!(registry.drops.get(), 1, "first-poll-unwind-unowned: the future never dropped");
    let left = registry.registered.borrow();
    assert!(left.is_empty(), "first-poll-unwind-unowned: a registration outlived it: {left:x?}");
    drop(kept);
}

/// The executor's drop with two suspended tasks, the first's drop panicking: the drop resumes
/// past it, drops the second in place, which unregisters it, and closes the header the second's
/// kept waker names; then the unwind leaves the drop. The waker, dropped after, releases that
/// header, which Miri's leak check reads; a drop that skipped the close and freed the array
/// would make that release a use after free. Stopped at the panic, the drop never reaches the
/// second future, and frees its slab under it.
#[test]
fn the_executors_drop_resumes_past_an_unwinding_drop() {
    let (mut executor, class) = registering();
    let registry = Rc::new(Registry::default());
    let first = poll_registering(&mut executor, class, Act::PendingPanickingInDrop, &registry);
    assert!(matches!(first, PollImmediate::Suspended(_)), "the first suspends");
    let second = poll_registering(&mut executor, class, Act::PendingKeepingWaker, &registry);
    assert!(matches!(second, PollImmediate::Suspended(_)), "the second suspends");
    let kept = registry.kept.borrow_mut().take().expect("the waker the second kept");
    let unwound = catch_unwind(AssertUnwindSafe(move || drop(executor)));
    assert!(unwound.is_err(), "the first future's drop panics out of the executor's drop");
    assert_eq!(registry.drops.get(), 2, "drop-unwind-stopped: a future never dropped");
    let left = registry.registered.borrow();
    assert!(left.is_empty(), "drop-unwind-stopped: a registration outlived it: {left:x?}");
    drop(kept);
}
