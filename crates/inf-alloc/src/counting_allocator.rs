//! Test-only allocation counter. The unsafe `GlobalAlloc` delegation lives
//! in the audited allocation leaf so engine-crate tests remain safe Rust.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    /// Per-thread allocation count. `const`-initialised so first access
    /// cannot itself allocate (a lazily-initialised TLS slot inside a
    /// global allocator would recurse).
    static THREAD_ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// `try_with` because TLS is unavailable during thread teardown; an
/// allocation there is not attributable to any test window and is dropped
/// rather than panicking inside the allocator.
#[inline]
fn bump_thread() {
    let _ = THREAD_ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
}

pub struct CountingAllocator {
    allocations: AtomicU64,
}

impl CountingAllocator {
    pub const fn new() -> CountingAllocator {
        CountingAllocator { allocations: AtomicU64::new(0) }
    }

    /// Process-global count: **every allocation on every thread**,
    /// including the test harness and any background work. Use it for
    /// whole-process budgets, never to attribute allocations to a code
    /// path under test — it cannot tell the two apart.
    #[inline]
    pub fn allocations(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
    }

    /// Allocations made by the **calling thread** only.
    ///
    /// This is the counter an "allocates nothing" assertion wants. The
    /// global counter above is process-wide, so a delta taken around a
    /// tight loop also captures anything the harness or another thread
    /// did in that window — which produced a CI failure on 2026-08-17
    /// (4 allocations across 20,000 patch calls, a path since shown to be
    /// allocation-free on both representations).
    #[inline]
    pub fn thread_allocations(&self) -> u64 {
        THREAD_ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
    }
}

impl Default for CountingAllocator {
    fn default() -> CountingAllocator {
        CountingAllocator::new()
    }
}

// SAFETY: every allocation operation delegates its pointer/layout contract
// unchanged to `System`; the relaxed counter does not inspect or alter the
// allocation, pointer, size, alignment, or lifetime.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread();
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged under the caller's deallocation contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread();
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread();
        // SAFETY: forwarded unchanged under the caller's reallocation contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_and_counts_allocations() {
        let alloc = CountingAllocator::new();
        let layout = Layout::from_size_align(64, 8).expect("layout");
        // SAFETY: the test deallocates the returned pointer exactly once
        // with the same allocator and layout, after checking non-null.
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(alloc.allocations(), 1);
        // SAFETY: `ptr` came from `alloc` with `layout` above and is live.
        unsafe { alloc.dealloc(ptr, layout) };
    }

    #[test]
    fn thread_counter_ignores_other_threads() {
        // The property the M3-S16 "allocates nothing" assertions rely on:
        // the global counter cannot attribute an allocation to a code path,
        // the thread-local one can.
        static ALLOC: CountingAllocator = CountingAllocator::new();
        let layout = Layout::from_size_align(64, 8).expect("layout");

        let before_thread = ALLOC.thread_allocations();
        let before_global = ALLOC.allocations();

        std::thread::scope(|s| {
            s.spawn(|| {
                // SAFETY: allocated and freed here with the same layout.
                let ptr = unsafe { ALLOC.alloc(layout) };
                assert!(!ptr.is_null());
                // SAFETY: `ptr` came from the `alloc` directly above.
                unsafe { ALLOC.dealloc(ptr, layout) };
            });
        });

        assert_eq!(
            ALLOC.thread_allocations(),
            before_thread,
            "another thread's allocation leaked into this thread's count"
        );
        assert!(
            ALLOC.allocations() > before_global,
            "the global counter should have observed the other thread"
        );
    }
}
