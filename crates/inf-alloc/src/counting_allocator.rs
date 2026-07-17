//! Test-only allocation counter. The unsafe `GlobalAlloc` delegation lives
//! in the audited allocation leaf so engine-crate tests remain safe Rust.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct CountingAllocator {
    allocations: AtomicU64,
}

impl CountingAllocator {
    pub const fn new() -> CountingAllocator {
        CountingAllocator { allocations: AtomicU64::new(0) }
    }

    #[inline]
    pub fn allocations(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
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
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged under the caller's deallocation contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
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
}
