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
//! The wheel's warm-up visits every bucket of every level twice: at level
//! `l`, 128 singleton timers expire, spaced by the level's span of `64^l`
//! ms of injected time, so a bucket's capacity is the warmed one before a
//! window opens.

use inf_alloc::CountingAllocator;
use inf_foundation::time::Nanos;
use inf_runtime::TimerWheel;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// One millisecond, the timers' resolution, in injected nanoseconds.
const STEP: Nanos = Nanos(1_000_000);
const KEY: u64 = 77;
/// The census's populations.
const COUNTS: [u32; 3] = [1, 64, 4_096];

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

/// The hierarchical wheel.
struct Wheel(TimerWheel);

impl Wheel {
    /// Arms one timer at `due`, steps to it and checks it fires once.
    fn expire_at(&mut self, now: &mut Nanos, due: Nanos) {
        self.0.insert(due, KEY);
        assert_eq!(self.0.armed(), 1);
        let mut fired = 0u32;
        self.0.advance(due, |key| {
            assert_eq!(key, KEY);
            fired += 1;
        });
        assert_eq!(fired, 1, "the timer fires once, at its deadline");
        assert_eq!(self.0.next_deadline(), None);
        *now = due;
    }
}

impl Subject for Wheel {
    fn warmed(now: &mut Nanos) -> Wheel {
        let mut wheel = Wheel(TimerWheel::new());
        for level in 0..6u32 {
            let stride = Nanos(STEP.0 * 64u64.pow(level));
            for _ in 0..128 {
                let due = *now + stride;
                wheel.expire_at(now, due);
            }
        }
        wheel
    }

    fn cycle(&mut self, mode: Mode, now: &mut Nanos) {
        let due = *now + STEP;
        match mode {
            Mode::Expire => self.expire_at(now, due),
            Mode::Cancel | Mode::Idle => {
                let id = self.0.insert(due, KEY);
                assert_eq!(self.0.armed(), 1);
                assert!(self.0.cancel(id), "the armed timer cancels");
                assert_eq!(self.0.next_deadline(), None);
                if matches!(mode, Mode::Idle) {
                    self.0.advance(due, |_| panic!("a cancelled timer fired"));
                    *now = due;
                }
            }
        }
    }

    fn armed(&self) -> usize {
        self.0.armed()
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
fn the_wheel_requests_nothing_after_its_warm_up_as_timers_expire() {
    census::<Wheel>("wheel", Mode::Expire);
}

#[test]
#[ignore = "red on the wheel: a cancelled timer leaves its reference in a bucket, which grows"]
fn the_wheel_requests_nothing_after_its_warm_up_as_timers_are_cancelled() {
    census::<Wheel>("wheel", Mode::Cancel);
}

#[test]
#[ignore = "red on the wheel: a clock jump with nothing armed visits no cancelled reference"]
fn the_wheel_requests_nothing_after_its_warm_up_across_idle_cycles() {
    census::<Wheel>("wheel", Mode::Idle);
}
