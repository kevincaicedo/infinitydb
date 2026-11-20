//! Named fault points + deterministic injection (M2-S16, master plan
//! §8.4/§24 — the Vortex crash-testing pattern, rebuilt): every dangerous
//! durability step names a fault point; tests arm points with a
//! deterministic trigger, and the site's `fire()` decides whether the
//! documented failure is injected. Every crash test is reproducible (L7):
//! triggers are occurrence counts or seeded draws, never wall-clock races.
//!
//! **Threading model:** the registry is thread-local. Cells are
//! single-threaded (L1); arming happens on the thread that will fire —
//! test bodies for the sync tier, cell-boot plans for node tests. The one
//! value two threads share is a [`HoldGate`]: the thread that waits arms
//! it on itself, and the test that releases it keeps a clone. With the
//! feature off nothing is shared, and nothing for the cell denylist to see.
//!
//! **Cost model:** compiled out entirely unless the `fault-points`
//! feature is on (test builds enable it through dev-dependency feature
//! unification; release binaries never do) — `fire()` is a `const false`
//! the optimizer erases along with each site's injection arm. With the
//! feature on but nothing armed, `fire()` is one thread-local read and
//! one branch. The A/B artifact for the compiled-out claim rides the
//! recovery/commit benches (M2-S16 AC).
//!
//! Point *names* are declared by the crate that owns the mechanism
//! (`inf-log` for M2), each listed in that crate's `ALL` inventory —
//! `scripts/check-fault-points.sh` fails CI when a declared point has no
//! exercising test, so coverage cannot rot.

// Only the `fault-points` `imp` module (below) consumes these — the
// default build compiles neither, so gate the import to match its use.
#[cfg(feature = "fault-points")]
use crate::rng::{Entropy, SplitMix64};

/// Whether this build carries the registry at all (ADR-0107). A DST
/// binary asserts `true` at startup — a simulator built without the
/// feature would arm points into a void and report clean runs; a
/// shipping binary is `false`, and `scripts/check-shipping-features.sh`
/// keeps the workspace graph from ever turning it on for one.
pub const COMPILED_IN: bool = cfg!(feature = "fault-points");

/// When an armed point fires (all variants deterministic — L7).
/// Occurrence counts start at 1 with the arming call: "the nth time this
/// point is passed *after arming*".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FaultSpec {
    /// Fire on every occurrence.
    Always,
    /// Fire on exactly the nth occurrence (1-based), once.
    Nth(u64),
    /// Fire on every occurrence from the nth onward (1-based) — persistent
    /// exhaustion shapes (e.g. disk stays full).
    FromNth(u64),
    /// Fire with probability `num/den` per occurrence, drawn from a
    /// [`SplitMix64`] stream seeded with `seed` at arming.
    Probability { num: u32, den: u32, seed: u64 },
}

#[cfg(feature = "fault-points")]
mod imp {
    use super::{Entropy, FaultSpec, SplitMix64};
    use core::cell::RefCell;

    struct Armed {
        point: &'static str,
        spec: FaultSpec,
        occurrences: u64,
        fired: u64,
        rng: SplitMix64,
    }

    thread_local! {
        static ARMED: RefCell<Vec<Armed>> = const { RefCell::new(Vec::new()) };
    }

    /// Arms `point` with `spec` on this thread (re-arming resets counts).
    pub fn arm(point: &'static str, spec: FaultSpec) {
        let seed = match spec {
            FaultSpec::Probability { seed, .. } => seed,
            FaultSpec::Always | FaultSpec::Nth(_) | FaultSpec::FromNth(_) => 0,
        };
        ARMED.with(|armed| {
            let mut armed = armed.borrow_mut();
            armed.retain(|a| a.point != point);
            armed.push(Armed { point, spec, occurrences: 0, fired: 0, rng: SplitMix64::new(seed) });
        });
    }

    /// Disarms `point` on this thread (unknown points are a no-op).
    pub fn disarm(point: &'static str) {
        ARMED.with(|armed| armed.borrow_mut().retain(|a| a.point != point));
    }

    /// Disarms everything on this thread (test teardown).
    pub fn disarm_all() {
        ARMED.with(|armed| armed.borrow_mut().clear());
    }

    /// Records one occurrence of `point`; true = inject the documented
    /// failure. Unarmed points cost one thread-local read + an is-empty
    /// branch.
    #[inline]
    pub fn fire(point: &'static str) -> bool {
        ARMED.with(|armed| {
            let mut armed = armed.borrow_mut();
            if armed.is_empty() {
                return false;
            }
            let Some(a) = armed.iter_mut().find(|a| a.point == point) else {
                return false;
            };
            a.occurrences += 1;
            let hit = match a.spec {
                FaultSpec::Always => true,
                FaultSpec::Nth(n) => a.occurrences == n,
                FaultSpec::FromNth(n) => a.occurrences >= n,
                FaultSpec::Probability { num, den, .. } => {
                    a.rng.next_below(u64::from(den.max(1))) < u64::from(num)
                }
            };
            if hit {
                a.fired += 1;
            }
            hit
        })
    }

    /// Occurrences of `point` since arming (0 when unarmed) — test
    /// observability, e.g. "the site was reached but did not fire".
    pub fn occurrences(point: &'static str) -> u64 {
        ARMED.with(|armed| {
            armed.borrow().iter().find(|a| a.point == point).map_or(0, |a| a.occurrences)
        })
    }

    /// Times `point` actually fired since arming (0 when unarmed).
    pub fn fired(point: &'static str) -> u64 {
        ARMED.with(|armed| armed.borrow().iter().find(|a| a.point == point).map_or(0, |a| a.fired))
    }

    /// No thread has reached the gate's point yet.
    const GATE_ARMED: u8 = 0;
    /// A thread waits at the gate's point.
    const GATE_HELD: u8 = 1;
    /// The holder released the gate: a thread at its point proceeds.
    const GATE_RELEASED: u8 = 2;

    /// One cross-thread hold: the order of two threads a test needs
    /// decided, where no injected clock reaches (the catalog writer's
    /// senders, a peer's fan leg). The thread that will wait arms the gate
    /// on itself ([`arm_hold`]), as every point is armed on the thread
    /// that fires it; the test keeps a clone and releases it. A thread
    /// whose point fires with no gate armed does not wait.
    #[derive(Clone, Debug, Default)]
    pub struct HoldGate(std::sync::Arc<core::sync::atomic::AtomicU8>);

    impl HoldGate {
        /// A gate no thread has reached.
        #[must_use]
        pub fn new() -> HoldGate {
            HoldGate::default()
        }

        /// A thread waits at the gate's point now.
        #[must_use]
        pub fn is_held(&self) -> bool {
            self.0.load(core::sync::atomic::Ordering::Acquire) == GATE_HELD
        }

        /// The waiting thread, or the next one to reach the point, goes on.
        pub fn release(&self) {
            self.0.store(GATE_RELEASED, core::sync::atomic::Ordering::Release);
        }
    }

    thread_local! {
        static HOLDS: RefCell<Vec<(&'static str, HoldGate)>> = const { RefCell::new(Vec::new()) };
    }

    /// Arms `gate` for this thread's next [`hold`] of `point` (one use;
    /// re-arming replaces the gate). Arm the point itself with [`arm`].
    pub fn arm_hold(point: &'static str, gate: &HoldGate) {
        HOLDS.with(|holds| {
            let mut holds = holds.borrow_mut();
            holds.retain(|(p, _)| *p != point);
            holds.push((point, gate.clone()));
        });
    }

    /// Waits at `point` until this thread's armed gate is released, or
    /// until no other handle of it lives (a test that ended without
    /// releasing it); returns at once with no gate armed. The site calls
    /// it only when its point fired, so an unarmed build never waits.
    pub fn hold(point: &'static str) {
        let gate = HOLDS.with(|holds| {
            let mut holds = holds.borrow_mut();
            let at = holds.iter().position(|(p, _)| *p == point)?;
            Some(holds.swap_remove(at).1)
        });
        let Some(HoldGate(state)) = gate else { return };
        let _ = state.compare_exchange(
            GATE_ARMED,
            GATE_HELD,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        );
        while state.load(core::sync::atomic::Ordering::Acquire) != GATE_RELEASED
            && std::sync::Arc::strong_count(&state) > 1
        {
            core::hint::spin_loop();
            std::thread::yield_now();
        }
    }
}

#[cfg(not(feature = "fault-points"))]
mod imp {
    use super::FaultSpec;

    /// Compiled out: arming without the `fault-points` feature is a no-op
    /// (tests get the feature through dev-dependency unification).
    #[inline(always)]
    pub fn arm(_point: &'static str, _spec: FaultSpec) {}

    #[inline(always)]
    pub fn disarm(_point: &'static str) {}

    #[inline(always)]
    pub fn disarm_all() {}

    /// Compiled out: constant false — the optimizer erases the site's
    /// injection arm entirely (the M2-S16 zero-cost contract).
    #[inline(always)]
    pub fn fire(_point: &'static str) -> bool {
        false
    }

    #[inline(always)]
    pub fn occurrences(_point: &'static str) -> u64 {
        0
    }

    #[inline(always)]
    pub fn fired(_point: &'static str) -> u64 {
        0
    }

    /// Compiled out: a gate no thread can wait on.
    #[derive(Clone, Debug, Default)]
    pub struct HoldGate;

    impl HoldGate {
        #[must_use]
        pub fn new() -> HoldGate {
            HoldGate
        }

        #[must_use]
        pub fn is_held(&self) -> bool {
            false
        }

        pub fn release(&self) {}
    }

    #[inline(always)]
    pub fn arm_hold(_point: &'static str, _gate: &HoldGate) {}

    /// Compiled out: never waits (no site reaches it: `fire` is false).
    #[inline(always)]
    pub fn hold(_point: &'static str) {}
}

pub use imp::{HoldGate, arm, arm_hold, disarm, disarm_all, fire, fired, hold, occurrences};

#[cfg(all(test, feature = "fault-points"))]
mod tests {
    use super::*;

    #[test]
    fn nth_fires_exactly_once() {
        disarm_all();
        arm("p", FaultSpec::Nth(3));
        let hits: Vec<bool> = (0..5).map(|_| fire("p")).collect();
        assert_eq!(hits, [false, false, true, false, false]);
        assert_eq!(occurrences("p"), 5);
        assert_eq!(fired("p"), 1);
    }

    #[test]
    fn from_nth_fires_persistently() {
        disarm_all();
        arm("p", FaultSpec::FromNth(2));
        let hits: Vec<bool> = (0..4).map(|_| fire("p")).collect();
        assert_eq!(hits, [false, true, true, true]);
    }

    #[test]
    fn probability_is_seed_deterministic() {
        disarm_all();
        arm("p", FaultSpec::Probability { num: 1, den: 4, seed: 0xC0FFEE });
        let a: Vec<bool> = (0..64).map(|_| fire("p")).collect();
        arm("p", FaultSpec::Probability { num: 1, den: 4, seed: 0xC0FFEE });
        let b: Vec<bool> = (0..64).map(|_| fire("p")).collect();
        assert_eq!(a, b, "same seed, same firing schedule (L7)");
        assert!(a.iter().any(|&h| h) && !a.iter().all(|&h| h), "p=1/4 over 64 draws");
    }

    /// A thread that armed a gate waits at its point until the holder
    /// releases it; an unarmed thread, a second reach and a gate whose
    /// holder is gone never wait.
    #[test]
    fn a_hold_waits_until_released_and_only_where_armed() {
        disarm_all();
        hold("h");
        let gate = HoldGate::new();
        let waiter = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                arm("h", FaultSpec::Always);
                arm_hold("h", &gate);
                if fire("h") {
                    hold("h");
                }
                hold("h");
            })
        };
        while !gate.is_held() {
            std::thread::yield_now();
        }
        assert!(!waiter.is_finished(), "the armed thread waits at its point");
        gate.release();
        waiter.join().expect("the released thread finishes");
        assert!(!gate.is_held());
        let orphan = HoldGate::new();
        let abandoned = {
            let orphan = orphan.clone();
            std::thread::spawn(move || {
                arm_hold("h", &orphan);
                drop(orphan);
                hold("h");
            })
        };
        while !orphan.is_held() {
            std::thread::yield_now();
        }
        drop(orphan);
        abandoned.join().expect("a gate with no holder left releases its thread");
    }

    #[test]
    fn unarmed_points_never_fire_and_disarm_works() {
        disarm_all();
        assert!(!fire("q"));
        arm("q", FaultSpec::Always);
        assert!(fire("q"));
        disarm("q");
        assert!(!fire("q"));
        assert_eq!(occurrences("q"), 0);
    }
}
