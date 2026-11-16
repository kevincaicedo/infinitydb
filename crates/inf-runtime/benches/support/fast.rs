//! The fast path's bench support, one module for `executor.rs` (criterion's time an iteration)
//! and `executor_counts.rs` (instructions an op by `perf stat`), so the two read one future, one
//! factory and one step: the bench class's future and the padded one on the same body, their
//! classes, and the fast op, `reserve` then `permit.poll_immediate` of a future Ready on its first
//! poll.

use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::{
    CellExecutor, ExecutorLayout, PollImmediate, TaskAdmission, TaskClass, TaskFactory,
};

/// The bench class's tasks: at most one is alive at a time.
pub const TASKS: Cap = Cap::entries::<64>("bench-tasks", CapFill::Serving);

/// The pad that puts a padded class's slot at 16 slots a chunk, the `conn` class's: a future of
/// about 2,900 B.
pub const PAD_BYTES: usize = 2_900;
/// The padded classes, at one directory level and at two.
pub const PADDED_2500: Cap = Cap::entries::<2_500>("bench-padded-2500", CapFill::Serving);
pub const PADDED_10000: Cap = Cap::entries::<10_000>("bench-padded-10000", CapFill::Serving);

/// An executor of one class of `cap` tasks, the future `factory` makes, on a census of its own.
pub fn executor_with<Args, Fac: TaskFactory<Args>>(
    factory: &Fac,
    cap: Cap,
) -> (CellExecutor, TaskClass) {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(factory, cap).expect("one class");
    (layout.build().expect("no class refuses"), class)
}

/// The fast op: one task of `class`, made by `make`, Ready on its first poll.
#[inline(always)]
pub fn ready_once<F: Future<Output = ()> + 'static>(
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

/// The future polled once in a stack slot under a no-op waker, with no executor: the control
/// read beside a fast op, never subtracted from its time (a padded future pays its stack slot's
/// store and reload, which the executor's slot does not).
#[inline(always)]
pub fn poll_direct<F: Future<Output = ()>>(future: F) {
    let mut cx = Context::from_waker(Waker::noop());
    let future = pin!(future);
    assert!(future.poll(&mut cx).is_ready());
}

/// The "command" every leg runs: a tiny read-modify-write, the shape of a fast-path GET hitting
/// cell-local state.
#[inline(never)]
pub fn command_direct(counter: &Cell<u64>) {
    counter.set(counter.get().wrapping_add(1));
}

/// The fast path's command as a task: the bench class's future.
pub async fn command_task(counter: Rc<Cell<u64>>) {
    command_direct(black_box(&counter));
}

/// The fast path's command as a padded task, written by hand so its body is the bench class's
/// own (one `command_direct` call) and only its size differs: the pad, never written, takes its
/// slot to 16 a chunk.
pub struct PaddedTask {
    counter: Option<Rc<Cell<u64>>>,
    pad: MaybeUninit<[u8; PAD_BYTES]>,
}

/// The padded class's factory, for its executor legs and its direct poll alike.
pub fn padded_task(counter: Rc<Cell<u64>>) -> PaddedTask {
    PaddedTask { counter: Some(counter), pad: MaybeUninit::uninit() }
}

impl Future for PaddedTask {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        black_box(&this.pad);
        if let Some(counter) = this.counter.take() {
            command_direct(black_box(&counter));
        }
        Poll::Ready(())
    }
}
