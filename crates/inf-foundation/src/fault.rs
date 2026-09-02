//! Named fault points + deterministic injection (M2-S16, master plan
//! §8.4/§24 — the Vortex crash-testing pattern, rebuilt): every dangerous
//! durability step names a fault point; tests arm points with a
//! deterministic trigger, and the site's `fire()` decides whether the
//! documented failure is injected. Every crash test is reproducible (L7):
//! triggers are occurrence counts or seeded draws, never wall-clock races.
//!
//! **Threading model:** the registry is thread-local. Cells are
//! single-threaded (L1); arming happens on the thread that will fire —
//! test bodies for the sync tier, cell-boot plans for node tests. No
//! shared state, no atomics, nothing for the cell denylist to see.
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
            _ => 0,
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
}

pub use imp::{arm, disarm, disarm_all, fire, fired, occurrences};

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
