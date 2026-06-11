//! Executor + gate semantics (M0-S06 AC): fast-path behavior, suspension
//! correctness under randomized interleavings vs a reference model, and
//! zero-leak slab accounting.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_runtime::gate::{KeyedGate, ListWait};
use inf_runtime::{
    CellExecutor, CompletionToken, FabricGate, IoGate, PollImmediate, TokenClass, WaitList,
    WatermarkGate,
};
use proptest::prelude::*;

#[test]
fn ready_future_never_allocates_a_task_slot() {
    let mut ex = CellExecutor::new(8);
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
    let mut ex = CellExecutor::new(8);
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
    let mut ex = CellExecutor::new(8);
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
    let mut ex = CellExecutor::new(8);
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
    let mut ex = CellExecutor::new(8);
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
/// the executor must tolerate the escaped clone (dead wakes are no-ops).
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
    let mut ex = CellExecutor::new(8);
    let stolen = Rc::new(RefCell::new(None));
    let outcome = ex.poll_immediate(WakerThief(Rc::clone(&stolen)));
    assert_eq!(outcome, PollImmediate::Completed);
    let waker = stolen.borrow_mut().take().expect("clone escaped");
    waker.wake(); // dead header: must be a silent no-op
    assert_eq!(ex.live_tasks(), 0);
    assert_eq!(ex.run_ready(64), 0, "dead wake must not queue anything");
}

#[test]
fn io_gate_routes_by_completion_token() {
    let mut ex = CellExecutor::new(8);
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
    let mut ex = CellExecutor::new(8);
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

#[test]
fn watermark_gate_wakes_at_or_below() {
    let mut ex = CellExecutor::new(8);
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

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
        let mut ex = CellExecutor::new(32);
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
        let mut ex = CellExecutor::new(32);
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
