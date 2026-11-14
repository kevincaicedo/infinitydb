#![allow(
    clippy::disallowed_types,
    reason = "test target: std containers in test code, outside cell code (ADR-0163 D2)"
)]
//! What an executor's drop leaves behind (ADR-0003 A1.3): it frees every future, its spare
//! header, the scratch block and its shared state inside the drop, and closes each header a waker
//! still names. A closed header is freed by its last waker's drop, on whichever thread drops it,
//! and the remote stack with the last closed header it counts: nothing is kept for the process,
//! however many executors it creates and drops. The counting allocator reads each byte back, per
//! thread; Miri runs these too, its leak check on.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

use inf_alloc::CountingAllocator;
use inf_foundation::wake::RemoteStack;
use inf_runtime::{CellExecutor, PollImmediate};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// A wake header's bytes: the bridge's four words and the owner part's five (the executor's
/// `const` assertion holds it).
const HEADER_BYTES: u64 = 72;
/// A remote stack's bytes.
const STACK_BYTES: u64 = size_of::<RemoteStack>() as u64;

/// Executors each repetition creates and drops: 10³ natively, 10² under Miri.
const EXECUTORS: usize = if cfg!(miri) { 100 } else { 1_000 };

/// Stays Pending and puts a clone of its waker in `out` on its first poll; allocates nothing.
struct Export {
    out: Rc<Cell<Option<Waker>>>,
}

impl Future for Export {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.out.take().is_none() {
            self.out.set(Some(cx.waker().clone()));
        }
        Poll::Pending
    }
}

/// Parks one `Export` task on `executor` and returns the waker it exported.
fn park(executor: &mut CellExecutor, out: &Rc<Cell<Option<Waker>>>) -> Waker {
    let outcome = executor.poll_immediate(Export { out: Rc::clone(out) });
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "the task parks");
    out.take().expect("the first poll exports its waker")
}

/// A thread holds a waker past its executor's drop. Its wake does nothing (no executor is left
/// to poll), and its drop frees the closed header and, the header being the stack's last count,
/// the stack, on that thread.
#[test]
fn a_waker_another_thread_holds_past_the_executor_is_freed_there() {
    let out = Rc::default();
    let mut executor = CellExecutor::new(1);
    let waker = park(&mut executor, &out);
    let (to, from) = mpsc::sync_channel::<()>(0);
    let holder = std::thread::spawn(move || {
        from.recv().expect("the executor is dropped first");
        waker.wake_by_ref();
        let before = ALLOC.thread_freed_bytes();
        drop(waker);
        ALLOC.thread_freed_bytes() - before
    });
    drop(executor);
    to.send(()).expect("the holder waits");
    let freed = holder.join().expect("the holding thread");
    assert_eq!(freed, HEADER_BYTES + STACK_BYTES, "the header and the stack, freed on that thread");
}

/// Builds an executor with two parked tasks, its two exported wakers, and the bytes the build
/// allocated on this thread (nothing else allocates in the window).
fn two_parked() -> (CellExecutor, Waker, Waker, u64) {
    let before = ALLOC.thread_bytes();
    // Inside the window: the futures hold its `Rc`, which their drop frees.
    let out = Rc::default();
    let mut executor = CellExecutor::new(2);
    let first = park(&mut executor, &out);
    let second = park(&mut executor, &out);
    (executor, first, second, ALLOC.thread_bytes() - before)
}

/// Two headers closed under wakers the owner thread keeps past the executor's drop, their last
/// wakers dropped in either order. The executor's drop returns all it allocated but the two
/// headers and the stack (the futures' blocks and the shared state among it: released at drop);
/// each later drop returns its header, and the second the stack too.
#[test]
fn an_executor_dropped_before_its_threads_last_wakers_frees_each_at_its_drop() {
    for first_dropped_first in [true, false] {
        let (executor, first, second, allocated) = two_parked();
        let before = ALLOC.thread_freed_bytes();
        drop(executor);
        assert_eq!(
            ALLOC.thread_freed_bytes() - before,
            allocated - 2 * HEADER_BYTES - STACK_BYTES,
            "released at drop: the futures and the shared state, all but the closed headers"
        );
        let (one, two) = if first_dropped_first { (first, second) } else { (second, first) };
        let before = ALLOC.thread_freed_bytes();
        drop(one);
        assert_eq!(ALLOC.thread_freed_bytes() - before, HEADER_BYTES, "its header, freed");
        let before = ALLOC.thread_freed_bytes();
        drop(two);
        assert_eq!(
            ALLOC.thread_freed_bytes() - before,
            HEADER_BYTES + STACK_BYTES,
            "its header and, the last count, the stack"
        );
    }
}

/// What the holding thread is told.
enum Hold {
    /// Keep this waker until told to drop it.
    Keep(Waker),
    /// Drop the waker held, then answer.
    Drop,
    /// Answer the bytes this thread allocated and freed inside its drops, then stop.
    Report,
}

/// This thread's allocated and freed bytes.
fn counts() -> (u64, u64) {
    (ALLOC.thread_bytes(), ALLOC.thread_freed_bytes())
}

/// Allocated less freed across `body`, on this thread.
fn kept_across(body: impl FnOnce()) -> i128 {
    let (allocated, freed) = counts();
    body();
    let (allocated_after, freed_after) = counts();
    i128::from(allocated_after - allocated) - i128::from(freed_after - freed)
}

/// One executor's life under a waker the holding thread keeps past its drop and then drops: the
/// bytes this thread kept across the executor's build and its drop. The hand-off between them
/// is the channel's, outside both windows.
fn held_past_drop(
    out: &Rc<Cell<Option<Waker>>>,
    to: &mpsc::SyncSender<Hold>,
    done: &mpsc::Receiver<(i128, i128)>,
) -> i128 {
    let mut executor = None;
    let mut waker = None;
    let built = kept_across(|| {
        let mut built = CellExecutor::new(1);
        waker = Some(park(&mut built, out));
        executor = Some(built);
    });
    to.send(Hold::Keep(waker.expect("parked"))).expect("the holder runs");
    let dropped = kept_across(|| drop(executor));
    to.send(Hold::Drop).expect("the holder runs");
    done.recv().expect("the holder dropped it");
    built + dropped
}

/// One executor's life with no waker left past its drop: the bytes it kept. After the parked
/// task, a first poll that completes leaves the executor a spare header, which its drop frees
/// open: the spare is in the window too.
fn none_left(out: &Rc<Cell<Option<Waker>>>) -> i128 {
    kept_across(|| {
        let mut executor = CellExecutor::new(1);
        drop(park(&mut executor, out));
        let outcome = executor.poll_immediate(async {});
        assert_eq!(outcome, PollImmediate::Completed, "a first poll that completes keeps a spare");
        drop(executor);
    })
}

/// Executors created and dropped in one process keep nothing: each dropped under a waker another
/// thread holds and then drops, and again with no waker left and a spare header. Allocated less
/// freed, summed over both threads across each executor's build and drop and each held waker's
/// drop, is 0.
#[test]
fn executors_created_and_dropped_keep_nothing() {
    let (to, from) = mpsc::sync_channel::<Hold>(0);
    let (answer, done) = mpsc::sync_channel::<(i128, i128)>(0);
    let holder = std::thread::spawn(move || {
        let mut kept = 0;
        let mut held = None;
        loop {
            match from.recv().expect("the owner thread sends") {
                Hold::Keep(waker) => held = Some(waker),
                Hold::Drop => {
                    kept += kept_across(|| drop(held.take()));
                    answer.send((0, 0)).expect("the owner waits");
                }
                Hold::Report => {
                    answer.send((kept, 0)).expect("the owner waits");
                    return;
                }
            }
        }
    });
    // Outlives every executor, so no window frees it: the futures hold clones of its `Rc`.
    let out = Rc::default();
    let mut here = 0;
    for _ in 0..EXECUTORS {
        here += held_past_drop(&out, &to, &done);
    }
    for _ in 0..EXECUTORS {
        here += none_left(&out);
    }
    to.send(Hold::Report).expect("the holder runs");
    let (there, _) = done.recv().expect("the holder reports");
    holder.join().expect("the holding thread");
    assert_eq!(
        here + there,
        0,
        "allocated less freed over both threads ({here} here, {there} there)"
    );
}
