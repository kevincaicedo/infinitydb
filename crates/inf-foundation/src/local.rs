//! Cell-local building blocks: cache-line padding and atomics-free counters.

use core::cell::Cell;
use core::ops::{Deref, DerefMut};

/// Pads/aligns `T` to 128 bytes — one cache-line pair on x86-64 (spatial
/// prefetcher) and the line size on Apple/ARM big cores. Used for SPSC ring
/// indices and any cross-thread-visible field (false-sharing discipline,
/// master plan §6.1).
#[derive(Debug, Default, Clone, Copy)]
#[repr(align(128))]
pub struct CachePadded<T>(pub T);

impl<T> Deref for CachePadded<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for CachePadded<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Cell-local counter: interior-mutable, **no atomics** (L1). The control
/// thread never reads these directly — cells publish snapshots at MAINTAIN.
#[derive(Debug, Default)]
pub struct LocalCounter(Cell<u64>);

impl LocalCounter {
    pub const fn new() -> LocalCounter {
        LocalCounter(Cell::new(0))
    }

    #[inline]
    pub fn add(&self, n: u64) {
        self.0.set(self.0.get().wrapping_add(n));
    }

    #[inline]
    pub fn incr(&self) {
        self.add(1);
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.0.get()
    }

    #[inline]
    pub fn take(&self) -> u64 {
        self.0.replace(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_basics() {
        let c = LocalCounter::new();
        c.incr();
        c.add(4);
        assert_eq!(c.get(), 5);
        assert_eq!(c.take(), 5);
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn padding_is_128() {
        assert_eq!(core::mem::align_of::<CachePadded<u64>>(), 128);
        assert_eq!(core::mem::size_of::<CachePadded<u64>>(), 128);
    }
}
