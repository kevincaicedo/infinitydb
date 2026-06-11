//! Injected time (L7): all cell code reads time through `Clock`, never
//! through `Instant::now()` directly, so the simulator can own the clock.

use core::cell::Cell;
use core::fmt;
use core::ops::{Add, Sub};
use std::rc::Rc;
use std::time::Instant;

/// Monotonic nanoseconds since the owning clock's origin.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const ZERO: Nanos = Nanos(0);

    #[inline]
    pub const fn from_secs(s: u64) -> Nanos {
        Nanos(s * 1_000_000_000)
    }

    #[inline]
    pub const fn from_millis(ms: u64) -> Nanos {
        Nanos(ms * 1_000_000)
    }

    #[inline]
    pub const fn from_micros(us: u64) -> Nanos {
        Nanos(us * 1_000)
    }

    #[inline]
    pub const fn as_secs(self) -> u64 {
        self.0 / 1_000_000_000
    }

    #[inline]
    pub const fn as_millis(self) -> u64 {
        self.0 / 1_000_000
    }

    #[inline]
    pub const fn as_micros(self) -> u64 {
        self.0 / 1_000
    }

    #[inline]
    pub const fn saturating_sub(self, rhs: Nanos) -> Nanos {
        Nanos(self.0.saturating_sub(rhs.0))
    }

    #[inline]
    pub const fn saturating_add(self, rhs: Nanos) -> Nanos {
        Nanos(self.0.saturating_add(rhs.0))
    }
}

impl Add for Nanos {
    type Output = Nanos;
    #[inline]
    fn add(self, rhs: Nanos) -> Nanos {
        Nanos(self.0 + rhs.0)
    }
}

impl Sub for Nanos {
    type Output = Nanos;
    #[inline]
    fn sub(self, rhs: Nanos) -> Nanos {
        Nanos(self.0 - rhs.0)
    }
}

impl fmt::Display for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ns", self.0)
    }
}

pub trait Clock {
    fn now(&self) -> Nanos;
}

/// Production clock: monotonic, origin at construction.
#[derive(Clone, Debug)]
pub struct StdClock {
    origin: Instant,
}

impl StdClock {
    pub fn new() -> StdClock {
        StdClock { origin: Instant::now() }
    }
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for StdClock {
    #[inline]
    fn now(&self) -> Nanos {
        Nanos(u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }
}

/// Simulator/test clock: single-threaded by design (no atomics — L1).
/// Share within a cell via `Rc<VirtualClock>`.
#[derive(Debug, Default)]
pub struct VirtualClock {
    now: Cell<u64>,
}

impl VirtualClock {
    pub fn new(start: Nanos) -> VirtualClock {
        VirtualClock { now: Cell::new(start.0) }
    }

    pub fn set(&self, t: Nanos) {
        debug_assert!(t.0 >= self.now.get(), "virtual time must be monotonic");
        self.now.set(t.0);
    }

    pub fn advance(&self, delta: Nanos) {
        self.now.set(self.now.get() + delta.0);
    }
}

impl Clock for VirtualClock {
    #[inline]
    fn now(&self) -> Nanos {
        Nanos(self.now.get())
    }
}

impl<C: Clock + ?Sized> Clock for &C {
    #[inline]
    fn now(&self) -> Nanos {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for Rc<C> {
    #[inline]
    fn now(&self) -> Nanos {
        (**self).now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_advances() {
        let clock = VirtualClock::new(Nanos::ZERO);
        assert_eq!(clock.now(), Nanos::ZERO);
        clock.advance(Nanos::from_millis(5));
        assert_eq!(clock.now().as_millis(), 5);
        clock.set(Nanos::from_secs(1));
        assert_eq!(clock.now().as_secs(), 1);
    }

    #[test]
    fn std_clock_is_monotonic() {
        let clock = StdClock::new();
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }
}
