#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
//! A standard `Waker` is `Send + Sync`: its vtable's four functions must be thread-safe, whatever
//! the future that cloned it. So safe code may move a cell executor's waker to another thread, or
//! wake one task from two threads at once. Each test here does that through the executor's safe
//! API, and each must be sound: they run natively and under Miri, the oracle they are written
//! for, whose data-race detector sees an unsynchronized access to the executor's cell-local
//! state and whose Stacked Borrows and leak checks see a header reached or kept wrongly.
//!
//! The executor answers a waker operation on two lanes (ADR-0003 A1): on its own thread the owner
//! lane, on any other the foreign lane, which counts `waker_foreign_ops` and queues a wake for the
//! next `run_ready`. A finished task's header is Held while a waker names it or a foreign wake for
//! it is queued; the counting allocator reads where each header's bytes return.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Poll, Waker};

use inf_alloc::CountingAllocator;
use inf_runtime::{CellExecutor, PollImmediate};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// A wake header's bytes: the bridge's four words and the owner part's five (the executor's
/// `const` assertion holds it).
const HEADER_BYTES: u64 = 72;

/// Clones and drops each churn leg makes across threads: 10⁵ natively, 10² under Miri.
const CHURN: usize = if cfg!(miri) { 100 } else { 100_000 };

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

/// A wake from another thread takes the foreign lane: it is counted, `has_ready` reports it
/// before the next `run_ready`, which polls the task. The lane compare is the oracle: a compare
/// that answered the owner part there would count nothing.
#[test]
fn a_wake_from_another_thread_takes_the_foreign_lane() {
    let (mut executor, polls, waker) = parked_task();
    assert_eq!(executor.waker_foreign_ops(), 0, "the owner thread's clone is the owner lane's");
    assert!(!executor.has_ready(), "nothing is queued yet");
    let waker = std::thread::spawn(move || {
        waker.wake_by_ref();
        waker
    })
    .join()
    .expect("the waking thread returns the waker");
    assert_eq!(executor.waker_foreign_ops(), 1, "one foreign wake, counted");
    assert!(executor.has_ready(), "the foreign wake is ready work before the next run_ready");
    assert_eq!(executor.run_ready(4), 1, "the drain delivers the wake and the task is polled");
    assert_eq!(polls.get(), 2, "its first poll and the woken one");
    assert!(!executor.has_ready(), "the drain left nothing");
    drop(waker);
    assert_eq!(executor.waker_foreign_ops(), 1, "the owner thread's drop is the owner lane's");
}

/// Completes on its first poll; during it, `during` runs with the polled waker.
struct ReadyAfter<F: FnMut(&Waker)> {
    during: F,
}

impl<F: FnMut(&Waker) + Unpin> Future for ReadyAfter<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        (self.during)(cx.waker());
        Poll::Ready(())
    }
}

/// A scoped thread clones the borrowed view of the polled waker and keeps the clone; the first
/// poll is Ready. The foreign clone names the header, so it is Held, not kept spare, and the
/// clone's drop frees it. The clone stays in a `ManuallyDrop` until the check: a header taken
/// spare would be reused under it.
#[test]
fn a_foreign_clone_keeps_a_finished_tasks_header() {
    let mut executor = CellExecutor::new(1);
    let kept: Rc<RefCell<Option<ManuallyDrop<Waker>>>> = Rc::default();
    let keep = Rc::clone(&kept);
    let outcome = executor.poll_immediate(ReadyAfter {
        during: move |waker: &Waker| {
            let clone = std::thread::scope(|scope| {
                scope.spawn(|| waker.clone()).join().expect("the scoped thread clones")
            });
            *keep.borrow_mut() = Some(ManuallyDrop::new(clone));
        },
    });
    assert_eq!(outcome, PollImmediate::Completed);
    assert_eq!(executor.waker_foreign_ops(), 1, "the scoped thread's clone is foreign");
    assert_eq!(executor.held_headers(), 1, "a foreign clone keeps the finished task's header");
    let clone = kept.borrow_mut().take().expect("the clone was kept");
    drop(ManuallyDrop::into_inner(clone));
    assert_eq!(executor.held_headers(), 0, "the clone's drop on the owner thread frees it");
}

/// A task that counts its polls and stays Pending.
struct CountPolls {
    polls: Rc<Cell<u32>>,
}

impl Future for CountPolls {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        Poll::Pending
    }
}

/// A scoped thread wakes the task while it polls, and the poll is Ready: the queued wake keeps
/// the header Held (not spare), so the next first poll takes a new header and the drain delivers
/// the wake to the finished one, which it then frees. A header recycled with its wake queued
/// would hand the wake to the next task, which would then be polled twice.
#[test]
fn a_foreign_wake_queued_for_a_finished_task_never_polls_the_next() {
    let mut executor = CellExecutor::new(2);
    let outcome = executor.poll_immediate(ReadyAfter {
        during: |waker: &Waker| {
            std::thread::scope(|scope| {
                scope.spawn(|| waker.wake_by_ref()).join().expect("the scoped thread wakes")
            });
        },
    });
    assert_eq!(outcome, PollImmediate::Completed);
    let polls = Rc::new(Cell::new(0));
    let next = executor.poll_immediate(CountPolls { polls: Rc::clone(&polls) });
    assert!(matches!(next, PollImmediate::Suspended(_)), "the next task parks");
    assert!(executor.has_ready(), "the queued wake is ready work");
    let polled = executor.run_ready(4);
    assert_eq!(polls.get(), 1, "the next task was polled once, by its own first poll");
    assert_eq!(polled, 0, "the drain wakes no task: the woken one is finished");
    assert_eq!(executor.held_headers(), 0, "the drain freed the finished header");
}

/// A task that completes once `done` is set, else stays Pending; it counts its polls.
struct UntilDone {
    done: Rc<Cell<bool>>,
    polls: Rc<Cell<u32>>,
}

impl Future for UntilDone {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        if self.done.get() { Poll::Ready(()) } else { Poll::Pending }
    }
}

/// A parked `UntilDone` task, its flags and the waker its first poll's view cloned.
fn parked_until_done(executor: &mut CellExecutor) -> (Rc<Cell<bool>>, Rc<Cell<u32>>, Waker) {
    let done = Rc::new(Cell::new(false));
    let polls = Rc::new(Cell::new(0));
    let (to, from) = mpsc::channel();
    let task = ExportFirst {
        inner: UntilDone { done: Rc::clone(&done), polls: Rc::clone(&polls) },
        to: Some(to),
    };
    assert!(matches!(executor.poll_immediate(task), PollImmediate::Suspended(_)));
    (done, polls, from.recv().expect("the first poll exports its waker"))
}

/// `inner`, which hands a clone of its waker out on its first poll.
struct ExportFirst<F> {
    inner: F,
    to: Option<mpsc::Sender<Waker>>,
}

impl<F: Future<Output = ()> + Unpin> Future for ExportFirst<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(to) = self.to.take() {
            to.send(cx.waker().clone()).expect("the test holds the receiver");
        }
        Pin::new(&mut self.inner).poll(cx)
    }
}

/// Clones churn across threads: made on the owner thread and dropped on another, then made on
/// another thread and dropped on the owner's. `w = o + c` stays exact through each, so after the
/// last the header is named by the one exported waker: the task, woken and finished, frees it
/// at its release, on the owner thread, once.
#[test]
fn clone_and_drop_churn_across_threads_counts_exactly() {
    let mut executor = CellExecutor::new(1);
    let (done, polls, waker) = parked_until_done(&mut executor);
    let made_here: Vec<Waker> = (0..CHURN).map(|_| waker.clone()).collect();
    std::thread::spawn(move || drop(made_here)).join().expect("the other thread drops them");
    assert_eq!(executor.held_headers(), 0, "the task is live: no header is Held");
    let shared = waker.clone();
    let made_there = std::thread::spawn(move || {
        let made: Vec<Waker> = (0..CHURN).map(|_| shared.clone()).collect();
        drop(shared);
        made
    })
    .join()
    .expect("the other thread makes them");
    drop(made_there);
    let foreign = u64::try_from(CHURN * 2 + 1).expect("a count");
    assert_eq!(executor.waker_foreign_ops(), foreign, "every foreign clone and drop counted");
    done.set(true);
    waker.wake();
    assert_eq!(executor.run_ready(4), 1, "the woken task finishes");
    assert_eq!(polls.get(), 2);
    assert_eq!(executor.live_tasks(), 0);
    assert_eq!(executor.held_headers(), 0, "no waker names it: freed at its release, not Held");
    assert!(!executor.has_ready(), "the foreign drops' entry was drained");
}

/// A scoped thread wakes the task while it polls, and the poll is Pending: the next `run_ready`
/// drains the wake and polls the task, so the wake is not lost.
#[test]
fn a_foreign_wake_during_a_pending_poll_is_polled_at_the_next_run_ready() {
    let mut executor = CellExecutor::new(1);
    let polls = Rc::new(Cell::new(0));
    let counted = Rc::clone(&polls);
    let outcome = executor.poll_immediate(core::future::poll_fn(move |cx| {
        counted.set(counted.get() + 1);
        if counted.get() == 1 {
            let waker = cx.waker();
            std::thread::scope(|scope| {
                scope.spawn(|| waker.wake_by_ref()).join().expect("the scoped thread wakes")
            });
        }
        Poll::<()>::Pending
    }));
    assert!(matches!(outcome, PollImmediate::Suspended(_)));
    assert!(executor.has_ready(), "the wake made during the poll is ready work");
    assert_eq!(executor.run_ready(4), 1, "the next run_ready polls the task");
    assert_eq!(polls.get(), 2, "its first poll and the woken one");
    assert_eq!(executor.run_ready(4), 0, "one wake, one poll");
}

/// Keeps a clone of its own waker in its state; completes on its second poll (`finish`) or
/// never. Its drop reads the thread's freed bytes around the drop of that clone: the clone is
/// counted out while the header is Dead and undecided, which frees nothing.
struct KeepsOwnClone {
    own: Option<Waker>,
    finish: bool,
    polls: u32,
}

impl Future for KeepsOwnClone {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls += 1;
        if self.polls == 1 {
            self.own = Some(cx.waker().clone());
            if self.finish {
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        Poll::Ready(())
    }
}

impl Drop for KeepsOwnClone {
    fn drop(&mut self) {
        let before = ALLOC.thread_freed_bytes();
        drop(self.own.take());
        assert_eq!(
            ALLOC.thread_freed_bytes(),
            before,
            "the future's own clone dropped while its header is undecided freed nothing"
        );
    }
}

/// A future keeps a clone of its own waker. Released by `run_ready`, its drop counts the clone
/// out first, and the header's bytes return once, after it; dropped by the executor's drop, the
/// same, and the executor's drop returns everything it allocated.
#[test]
fn a_future_that_keeps_its_own_waker_clone_is_freed_once() {
    let block = u64::try_from(size_of::<KeepsOwnClone>()).expect("a size");
    // Leg 1: released by run_ready.
    let mut executor = CellExecutor::new(1);
    executor.spawn_local(KeepsOwnClone { own: None, finish: true, polls: 0 });
    assert_eq!(executor.run_ready(1), 1, "the first poll keeps the clone and requeues");
    let before = ALLOC.thread_freed_bytes();
    assert_eq!(executor.run_ready(1), 1, "the second poll completes");
    assert_eq!(
        ALLOC.thread_freed_bytes() - before,
        HEADER_BYTES + block,
        "the release frees the future's block and, after its drop, the header, once"
    );
    assert_eq!(executor.held_headers(), 0);
    drop(executor);
    // Leg 2: dropped by the executor's drop, still pending.
    let allocated = ALLOC.thread_bytes();
    let freed = ALLOC.thread_freed_bytes();
    let mut executor = CellExecutor::new(1);
    executor.spawn_local(KeepsOwnClone { own: None, finish: false, polls: 0 });
    assert_eq!(executor.run_ready(1), 1, "the first poll keeps the clone and parks");
    let made = ALLOC.thread_bytes() - allocated;
    drop(executor);
    assert_eq!(
        ALLOC.thread_freed_bytes() - freed,
        made,
        "the executor's drop returns every byte it allocated, the header once"
    );
}

/// Two executors on one thread share its mark: each takes the owner lane for the other's
/// wakers too, as the simulator's cells, all on one thread, do.
#[test]
fn two_executors_on_one_thread_both_take_the_owner_lane() {
    let (mut first, first_polls, first_waker) = parked_task();
    let (mut second, second_polls, second_waker) = parked_task();
    first_waker.wake_by_ref();
    // A consuming wake: the owner lane's `wake`, then its drop.
    let consumed = second_waker.clone();
    consumed.wake();
    assert_eq!(first.run_ready(4), 1);
    assert_eq!(second.run_ready(4), 1);
    assert_eq!((first_polls.get(), second_polls.get()), (2, 2));
    assert_eq!(first.waker_foreign_ops(), 0, "one thread, one mark: no foreign operation");
    assert_eq!(second.waker_foreign_ops(), 0, "one thread, one mark: no foreign operation");
}

/// Exports clones of its waker into `keeper` and completes on its first poll: its header is Held.
fn finish_keeping_a_clone(executor: &mut CellExecutor, keeper: &Rc<RefCell<Vec<Waker>>>) {
    let keep = Rc::clone(keeper);
    let outcome = executor.poll_immediate(ReadyAfter {
        during: move |waker: &Waker| keep.borrow_mut().push(waker.clone()),
    });
    assert_eq!(outcome, PollImmediate::Completed);
}

/// Drops the last clone `keeper` holds inside its poll and reads the freed bytes around it.
fn drop_the_head_inside_a_poll(
    keeper: &Rc<RefCell<Vec<Waker>>>,
    freed_inside: &Rc<Cell<u64>>,
) -> impl Future<Output = ()> + 'static {
    let keeper = Rc::clone(keeper);
    let freed_inside = Rc::clone(freed_inside);
    core::future::poll_fn(move |_cx| {
        let before = ALLOC.thread_freed_bytes();
        drop(keeper.borrow_mut().pop());
        freed_inside.set(ALLOC.thread_freed_bytes() - before);
        Poll::Ready(())
    })
}

/// Two finished tasks' headers are Held, each named by a clone a third task keeps. The third
/// drops the clone of the held list's head inside its own poll, polled by `run_ready` and by
/// `poll_immediate`: the header is unlinked and freed inside that poll, through its own `Rc` of
/// the shared state, never through the executor the poll borrows. The executor's drop then
/// closes the remaining node, which the last clone's drop frees.
#[test]
fn a_waker_dropped_during_another_tasks_poll_frees_the_first_held_header() {
    for by_run_ready in [true, false] {
        let mut executor = CellExecutor::new(4);
        let keeper: Rc<RefCell<Vec<Waker>>> = Rc::default();
        finish_keeping_a_clone(&mut executor, &keeper);
        finish_keeping_a_clone(&mut executor, &keeper);
        assert_eq!(executor.held_headers(), 2, "both finished headers are Held");
        let freed_inside = Rc::new(Cell::new(0));
        let third = drop_the_head_inside_a_poll(&keeper, &freed_inside);
        if by_run_ready {
            executor.spawn_local(third);
            assert_eq!(executor.run_ready(1), 1);
        } else {
            assert_eq!(executor.poll_immediate(third), PollImmediate::Completed);
        }
        assert_eq!(
            freed_inside.get(),
            HEADER_BYTES,
            "by_run_ready {by_run_ready}: the header's bytes were freed inside the poll"
        );
        assert_eq!(executor.held_headers(), 1, "the other header stays Held");
        drop(executor);
        assert_eq!(keeper.borrow().len(), 1);
        drop(keeper);
    }
}
