//! A consuming foreign waker call, `wake` or `drop`, may let another thread free its header and
//! the remote stack the moment its swap of the lane word, or its push's compare-and-swap, lands:
//! the drain and the executor's drop, or a second thread's last drop of a closed header, may run
//! before the call returns. From that access on, the call reads nothing through its references to
//! them and copies neither, not even into a function that reads nothing, since a reference to a
//! freed thing is undefined behavior once it is formed (ADR-0003 A1.2.5).
//!
//! Only a schedule that preempts the call inside that window can see a breach, and Miri's default
//! schedule does not. So the CI's Miri job runs this target on its own step, at a pinned
//! preemption rate over a pinned seed range, each seed a schedule of its own, on one test thread:
//! `MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-preemption-rate=0.002 -Zmiri-many-seeds=0..32"`,
//! `cargo +nightly miri test -p inf-runtime --test waker_race -- --test-threads=1`. A reference
//! formed after the access is red there as a dangling reference or a use after free. The rate is
//! low because the freeing thread's path, from its own swap to the stack's free, is hundreds of
//! the interpreter's steps that it must run with the call preempted. Natively, and under Miri
//! too, the counting allocator reads each byte back, per thread.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Barrier, mpsc};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

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

/// Races of a closed header's two last drops: 10⁴ natively, 64 for each seed under Miri.
const CLOSED_RACES: usize = if cfg!(miri) { 64 } else { 10_000 };
/// Races of a consuming call against the drain and the executor's drop: 10⁴ natively, and 256
/// for each seed under Miri, since the owner's path from the drain to the stack's free is the
/// longer one, so fewer of its races fall inside the window.
const LIVE_RACES: usize = if cfg!(miri) { 256 } else { 10_000 };

/// Stays Pending; puts a clone of its waker in `out` on its first poll and nothing after, so a
/// poll after a wake names its header with no new waker.
struct ExportOnce {
    out: Option<Rc<Cell<Option<Waker>>>>,
}

impl Future for ExportOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(out) = self.out.take() {
            out.set(Some(cx.waker().clone()));
        }
        Poll::Pending
    }
}

/// Parks one `ExportOnce` task on `executor` and returns the waker it exported.
fn park(executor: &mut CellExecutor, out: &Rc<Cell<Option<Waker>>>) -> Waker {
    let outcome = executor.poll_immediate(ExportOnce { out: Some(Rc::clone(out)) });
    assert!(matches!(outcome, PollImmediate::Suspended(_)), "the task parks");
    out.take().expect("the first poll exports its waker")
}

/// This thread's allocated less freed bytes across `body`.
fn kept_across(body: impl FnOnce()) -> i128 {
    let (allocated, freed) = (ALLOC.thread_bytes(), ALLOC.thread_freed_bytes());
    body();
    let (allocated_after, freed_after) = (ALLOC.thread_bytes(), ALLOC.thread_freed_bytes());
    i128::from(allocated_after - allocated) - i128::from(freed_after - freed)
}

/// What the rival thread does with the waker it is handed.
#[derive(Clone, Copy, Debug)]
enum Consume {
    Wake,
    Drop,
}

/// A second thread that, for each race, takes a waker, meets the test's thread at the gate and
/// consumes the waker there, then reports the bytes it kept across that call. The hand-off and
/// the report are outside both threads' windows.
struct Rival {
    to: mpsc::SyncSender<(Waker, Consume)>,
    kept: mpsc::Receiver<i128>,
    gate: Arc<Barrier>,
    thread: JoinHandle<()>,
}

impl Rival {
    fn spawn() -> Rival {
        let (to, from) = mpsc::sync_channel::<(Waker, Consume)>(0);
        let (report, kept) = mpsc::sync_channel(0);
        let gate = Arc::new(Barrier::new(2));
        let at_gate = Arc::clone(&gate);
        let thread = thread::spawn(move || {
            for (waker, consume) in from {
                at_gate.wait();
                let kept = kept_across(|| match consume {
                    Consume::Wake => waker.wake(),
                    Consume::Drop => drop(waker),
                });
                report.send(kept).expect("the test's thread reads the report");
            }
        });
        Rival { to, kept, gate, thread }
    }

    /// Hands `waker` over and meets the rival at the gate: the race starts as this returns.
    fn start(&self, waker: Waker, consume: Consume) {
        self.to.send((waker, consume)).expect("the rival runs");
        self.gate.wait();
    }

    /// The bytes the rival kept across its call.
    fn kept(&self) -> i128 {
        self.kept.recv().expect("the rival reports")
    }

    fn stop(self) {
        drop(self.to);
        self.thread.join().expect("the rival thread");
    }
}

/// Two threads drop a closed header's last two wakers at once. The swap that leaves `c = 0`
/// frees the header and then, the header's count being the stack's last, the stack; the other
/// swap answered `Kept`, and its call may still be running while both are freed. Between them
/// the two drops free the header and the stack, each once.
#[test]
fn two_foreign_last_drops_of_a_closed_header_free_it_once() {
    let rival = Rival::spawn();
    let out = Rc::default();
    for _ in 0..CLOSED_RACES {
        let mut executor = CellExecutor::new(1);
        let first = park(&mut executor, &out);
        let second = first.clone();
        // The drop closes the header with both wakers folded into its foreign count: every later
        // operation on it, on either thread, takes the foreign lane.
        drop(executor);
        rival.start(second, Consume::Drop);
        let here = kept_across(|| drop(first));
        let there = rival.kept();
        assert_eq!(
            here + there,
            -i128::from(HEADER_BYTES + STACK_BYTES),
            "the two drops free the header and the stack, each once ({here} here, {there} there)"
        );
    }
    rival.stop();
}

/// A parked task's one waker, woken or dropped on another thread, against the owner thread's
/// `run_ready` and then the executor's drop. When the foreign call's push lands, the drain clears
/// `Q` and the drop frees the header open and the stack with the executor's own count, while the
/// call may still be running; when the push finds the head closed, the call frees both itself.
/// Allocated less freed over both threads, across the executor's life and the call, is 0.
#[test]
fn a_consuming_foreign_call_races_the_drain_and_the_executors_drop() {
    let rival = Rival::spawn();
    let out = Rc::default();
    for race in 0..LIVE_RACES {
        let consume = if race % 2 == 0 { Consume::Wake } else { Consume::Drop };
        let mut executor = None;
        let mut waker = None;
        let built = kept_across(|| {
            let mut parked = CellExecutor::new(1);
            waker = Some(park(&mut parked, &out));
            executor = Some(parked);
        });
        rival.start(waker.expect("parked"), consume);
        let dropped = kept_across(|| {
            let mut executor = executor.take().expect("built");
            executor.run_ready(1);
            drop(executor);
        });
        let there = rival.kept();
        assert_eq!(
            built + dropped + there,
            0,
            "allocated less freed over both threads, {consume:?} ({built} built, {dropped} \
             dropped, {there} there)"
        );
    }
    rival.stop();
}
