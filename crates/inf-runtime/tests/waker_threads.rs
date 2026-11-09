//! A standard `Waker` is `Send + Sync`: its vtable's four functions must be thread-safe, whatever
//! the future that cloned it. So safe code may move a cell executor's waker to another thread, or
//! wake one task from two threads at once. Each test here does that through the executor's safe
//! API, and each must be sound: Miri, whose data-race detector sees an unsynchronized access to
//! the executor's cell-local state, runs them.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Poll, Waker};

use inf_runtime::{CellExecutor, PollImmediate};

/// A task that stays Pending, counts its polls and hands a clone of its waker out on its first.
struct ExportWaker {
    to: Option<mpsc::Sender<Waker>>,
    polls: Rc<Cell<u32>>,
}

impl Future for ExportWaker {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        if let Some(to) = self.to.take() {
            to.send(cx.waker().clone()).expect("the test holds the receiver");
        }
        Poll::Pending
    }
}

/// An executor holding one parked `ExportWaker` task, its poll count and the waker it exported.
fn parked_task() -> (CellExecutor, Rc<Cell<u32>>, Waker) {
    let (to, from) = mpsc::channel();
    let polls = Rc::new(Cell::new(0));
    let mut executor = CellExecutor::new(1);
    let outcome = executor.poll_immediate(ExportWaker { to: Some(to), polls: Rc::clone(&polls) });
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "the task parks on its first poll");
    let waker = from.recv().expect("the first poll exports its waker");
    (executor, polls, waker)
}

/// The waker crosses to another thread, is cloned and dropped there, and comes back; a wake on
/// the executor's thread then schedules the task, which the next `run_ready` polls.
#[test]
fn a_waker_moved_to_another_thread_and_back_is_sound() {
    let (mut executor, polls, waker) = parked_task();
    let waker = std::thread::spawn(move || {
        drop(waker.clone());
        waker
    })
    .join()
    .expect("the other thread returns the waker");
    waker.wake_by_ref();
    assert_eq!(executor.run_ready(4), 1, "the woken task is polled once");
    assert_eq!(polls.get(), 2, "its first poll and the woken one");
    drop(waker);
    assert_eq!(executor.live_tasks(), 1, "the task stays parked");
}

/// Two threads wake one parked task at once, then drop their wakers there. The wakes coalesce:
/// the next `run_ready` polls the task once and leaves nothing queued.
#[test]
#[cfg_attr(
    not(miri),
    ignore = "on this executor two threads waking one task race on its cell-local state (a \
              RefCell borrow can panic natively); Miri reports the race"
)]
fn two_threads_wake_one_task() {
    let (mut executor, polls, first) = parked_task();
    let second = first.clone();
    let barrier = Arc::new(Barrier::new(3));
    let wake_from_a_thread = |waker: Waker| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            waker.wake_by_ref();
        })
    };
    let one = wake_from_a_thread(first);
    let two = wake_from_a_thread(second);
    barrier.wait();
    one.join().expect("the first waking thread");
    two.join().expect("the second waking thread");
    assert_eq!(executor.run_ready(4), 1, "two wakes of one task are one poll");
    assert_eq!(polls.get(), 2, "its first poll and the woken one");
    assert_eq!(executor.run_ready(4), 0, "nothing is left queued");
}
