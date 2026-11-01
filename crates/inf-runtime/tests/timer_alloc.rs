//! The timers' allocation census (ADR-0154 D5): once a timer structure is
//! built and warmed, a cell that arms timers and lets them expire, arms and
//! cancels them, or arms, cancels and steps past them requests no storage
//! from the allocator, at 1, 64 and 4,096 operations. One live timer is no
//! bound on backing that a cancelled reference can still hold.
//!
//! The oracle is the calling thread's count of allocator requests, with
//! the bytes requested and freed beside it (inf-alloc's counting
//! allocator), so it shares no code with the timer it reads. A window holds
//! no fixture vector, no print and no refusal. Its liveness: the expire
//! mode reaches every operation's arm and delivery and the drop frees every
//! byte the window retained, so a window that read nothing is red too.
//!
//! The fixed timer set's warm-up is its build and its owners' reserves:
//! nothing after it may request a byte. Its build is one allocation of at
//! most 64 KiB a step, and a refusal at any step leaves nothing behind.

use std::rc::Rc;

use inf_alloc::CountingAllocator;
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapError, CapFill};
use inf_foundation::limits::MAP_BACKING_ALLOCATION_BYTES_MAX;
use inf_foundation::time::Nanos;
use inf_runtime::heap::{ArrayBuilt, FixedArray};
use inf_runtime::{Cancelled, Consumed, Reserved, TimerOutcome, TimerOwner, TimerSet};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// One millisecond, the timers' resolution, in injected nanoseconds.
const STEP: Nanos = Nanos(1_000_000);
/// The census's populations.
const COUNTS: [u32; 3] = [1, 64, 4_096];
/// The set under census: one chunk of owners, each reserved at warm-up.
const OWNERS: Cap = Cap::entries::<64>("census-owners", CapFill::Serving);
/// The set whose build steps are counted: a directory of slab chunks, and
/// heap and handle arrays of 16 chunks each.
const BUILT: Cap = Cap::entries::<65_536>("census-built", CapFill::Serving);

/// What one cycle of the census does.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Arm one millisecond out, step to the deadline, deliver it.
    Expire,
    /// Arm one millisecond out and cancel, the clock held.
    Cancel,
    /// Arm one millisecond out, cancel, step past the deadline.
    Idle,
}

/// The allocator's counters on the calling thread.
#[derive(Clone, Copy, Debug)]
struct Counters {
    requests: u64,
    requested_bytes: u64,
    freed_bytes: u64,
}

impl Counters {
    fn read() -> Counters {
        Counters {
            requests: ALLOC.thread_allocations(),
            requested_bytes: ALLOC.thread_bytes(),
            freed_bytes: ALLOC.thread_freed_bytes(),
        }
    }

    fn since(self, before: Counters) -> Counters {
        Counters {
            requests: self.requests - before.requests,
            requested_bytes: self.requested_bytes - before.requested_bytes,
            freed_bytes: self.freed_bytes - before.freed_bytes,
        }
    }

    /// Requested bytes the window kept.
    fn retained(self) -> u64 {
        self.requested_bytes - self.freed_bytes
    }
}

/// A timer structure under census.
trait Subject {
    /// Builds and warms it, from injected time `now`.
    fn warmed(now: &mut Nanos) -> Self;
    /// One cycle of `mode`, from injected time `now`.
    fn cycle(&mut self, mode: Mode, now: &mut Nanos);
    /// Timers armed now.
    fn armed(&self) -> usize;
}

/// The fixed timer set: its census, kept so its drop frees it too, and its
/// owners, cycled in turn.
struct Set {
    set: TimerSet,
    owners: Vec<TimerOwner>,
    next: usize,
    _census: Rc<CapCensus>,
}

impl Subject for Set {
    fn warmed(_now: &mut Nanos) -> Set {
        let (census, _mark) = CapCensus::new(CellId(0));
        let mut set = TimerSet::build(OWNERS, &census).expect("built");
        let owners = (0..OWNERS.entries_max())
            .map(|route| match set.reserve(u64::from(route)) {
                Reserved::Owner(owner) => owner,
                other => panic!("a free position: {other:?}"),
            })
            .collect();
        Set { set, owners, next: 0, _census: census }
    }

    fn cycle(&mut self, mode: Mode, now: &mut Nanos) {
        let owner = &self.owners[self.next];
        self.next = (self.next + 1) % self.owners.len();
        let due = *now + STEP;
        let arm = self.set.arm(owner, due, *now).expect("armed");
        assert_eq!(self.set.armed(), 1);
        match mode {
            Mode::Expire => {
                *now = due;
                let notice = self.set.begin(*now).expect("due at its deadline");
                assert_eq!(notice.arm(), arm, "the timer fires once, at its deadline");
                assert_eq!(self.set.consume(&notice, TimerOutcome::Complete), Consumed::Idle);
            }
            Mode::Cancel | Mode::Idle => {
                assert_eq!(self.set.cancel(&arm), Cancelled::Removed, "the armed timer cancels");
                if matches!(mode, Mode::Idle) {
                    *now = due;
                    assert!(self.set.begin(*now).is_none(), "a cancelled timer fired");
                }
            }
        }
        assert_eq!(self.set.next_deadline(), None);
    }

    fn armed(&self) -> usize {
        self.set.armed() as usize
    }
}

/// One census case: the warmed subject, `count` cycles of `mode` in the
/// window, then its drop, which must free everything and request nothing.
/// Returns the window's counters.
fn census_case<S: Subject>(mode: Mode, count: u32) -> Counters {
    let before = Counters::read();
    let mut now = Nanos(0);
    let mut subject = S::warmed(&mut now);
    let warm = Counters::read().since(before);
    let before = Counters::read();
    for _ in 0..count {
        subject.cycle(mode, &mut now);
    }
    let window = Counters::read().since(before);
    assert_eq!(subject.armed(), 0, "{mode:?} at {count}: every timer ends delivered or cancelled");
    let before = Counters::read();
    drop(subject);
    let dropped = Counters::read().since(before);
    assert_eq!(dropped.requests, 0, "the drop requests nothing");
    assert_eq!(
        dropped.freed_bytes,
        warm.retained() + window.retained(),
        "{mode:?} at {count}: the drop frees the warm backing and the window's growth"
    );
    window
}

/// Every census case of `mode` on `S`: no request in any window. Each
/// case's counters are printed after its window closes.
fn census<S: Subject>(subject: &str, mode: Mode) {
    let readings: Vec<(u32, Counters)> =
        COUNTS.iter().map(|&count| (count, census_case::<S>(mode, count))).collect();
    for (count, window) in &readings {
        println!(
            "census {subject} {mode:?} operations={count} requests={} requested_bytes={} \
             freed_bytes={} retained_bytes={}",
            window.requests,
            window.requested_bytes,
            window.freed_bytes,
            window.retained()
        );
    }
    for (count, window) in &readings {
        assert_eq!(
            (window.requests, window.retained()),
            (0, 0),
            "{subject} {mode:?} at {count} operations: an allocator request after the warm-up"
        );
    }
}

#[test]
fn the_set_requests_nothing_after_its_build_as_timers_expire() {
    census::<Set>("set", Mode::Expire);
}

#[test]
fn the_set_requests_nothing_after_its_build_as_timers_are_cancelled() {
    census::<Set>("set", Mode::Cancel);
}

#[test]
fn the_set_requests_nothing_after_its_build_across_idle_cycles() {
    census::<Set>("set", Mode::Idle);
}

/// A 16-byte element, the heap entry's and the handle's width.
#[derive(Clone, Copy)]
struct Element([u64; 2]);

/// The set's arrays build one allocation of at most one backing
/// allocation's bytes a step: a directory, then its chunks.
#[test]
fn an_array_builds_one_allocation_of_at_most_64_kib_a_step() {
    let mut builder = FixedArray::builder(65_536, Element([7, 9])).expect("a layout that fits");
    let mut steps = 0u32;
    loop {
        let before = Counters::read();
        let step = builder.step();
        let grown = Counters::read().since(before);
        steps += 1;
        assert_eq!(grown.requests, 1, "step {steps}: one allocation");
        assert!(
            grown.requested_bytes <= MAP_BACKING_ALLOCATION_BYTES_MAX as u64,
            "step {steps}: {} bytes in one allocation",
            grown.requested_bytes
        );
        builder = match step {
            ArrayBuilt::More(next) => next,
            ArrayBuilt::Done(array) => {
                assert_eq!(array.capacity(), 65_536);
                assert_eq!(array.get(65_535).0, [7, 9], "every element holds the fill");
                break;
            }
            ArrayBuilt::Refused(refused) => panic!("{refused}"),
        };
    }
    assert_eq!(steps, 17, "a directory and 16 chunks of 4,096");
}

/// A refusal planted at each of a set's build allocations: the build
/// answers `Alloc`, and every allocation before the refusal is freed. A
/// refused request is counted as requested and never freed, so the window
/// of the refusal at allocation `k + 1` frees exactly what the window of
/// the refusal at `k` requested, the first frees nothing, and the chain
/// closes on the whole build's request.
#[test]
fn a_set_refused_at_any_build_step_leaves_nothing_behind() {
    let (census, _mark) = CapCensus::new(CellId(1));
    let before = Counters::read();
    let built = TimerSet::build(BUILT, &census).expect("built");
    let build = Counters::read().since(before);
    drop(built);
    assert!(build.requests > 30, "the arrays' 34 steps and the slab's: {}", build.requests);
    let mut requested_before = 0;
    for refused_at in 0..build.requests {
        let before = Counters::read();
        let guard = ALLOC.refuse_after(refused_at).expect("no refusal armed");
        let outcome = TimerSet::build(BUILT, &census).map(drop);
        assert!(guard.was_triggered(), "allocation {refused_at} reached the allocator");
        drop(guard);
        assert_eq!(outcome, Err(CapError::Alloc), "refused at allocation {refused_at}");
        let window = Counters::read().since(before);
        assert_eq!(
            window.freed_bytes, requested_before,
            "refused at allocation {refused_at}: an allocation before the refusal was kept"
        );
        requested_before = window.requested_bytes;
    }
    assert_eq!(requested_before, build.requested_bytes, "the chain closes on the whole build");
}
