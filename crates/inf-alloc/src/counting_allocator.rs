//! Test-only allocation counter. The unsafe `GlobalAlloc` delegation lives
//! in the audited allocation leaf so engine-crate tests remain safe Rust.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    /// Per-thread allocation count. `const`-initialised so first access
    /// cannot itself allocate (a lazily-initialised TLS slot inside a
    /// global allocator would recurse).
    static THREAD_ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    /// Per-thread bytes requested (alloc/alloc_zeroed sizes plus realloc
    /// new sizes) — churn, not live memory: frees are not subtracted.
    static THREAD_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Successful deallocations, including the old block of a successful
    /// realloc. Paired snapshots measure retention in one thread's window.
    static THREAD_FREED_BYTES: Cell<u64> = const { Cell::new(0) };
    static REFUSAL: Cell<RefusalState> = const { Cell::new(RefusalState::Inactive) };
}

#[derive(Clone, Copy)]
enum RefusalState {
    Inactive,
    Armed(u64),
    Consumed,
}

fn refuse_request() -> bool {
    REFUSAL
        .try_with(|state| match state.get() {
            RefusalState::Inactive | RefusalState::Consumed => false,
            RefusalState::Armed(0) => {
                state.set(RefusalState::Consumed);
                true
            }
            RefusalState::Armed(remaining) => {
                state.set(RefusalState::Armed(remaining.saturating_sub(1)));
                false
            }
        })
        .unwrap_or(false)
}

fn record_free(bytes: usize) {
    let _ = THREAD_FREED_BYTES.try_with(|c| c.set(c.get().wrapping_add(bytes as u64)));
}

/// A refusal window is already active on the calling thread.
#[derive(Debug, PartialEq, Eq)]
pub struct RefusalActive;

/// Test-only, thread-bound owner of one injected allocation refusal.
/// Dropping it cancels an unused refusal. It cannot be nested or moved
/// to another thread; neither operation may clear someone else's window.
///
/// ```compile_fail
/// let allocator = inf_alloc::CountingAllocator::new();
/// let guard = allocator.refuse_after(0).unwrap();
/// std::thread::spawn(move || drop(guard));
/// ```
#[must_use]
pub struct AllocationRefusal {
    thread: PhantomData<Rc<()>>,
}

impl AllocationRefusal {
    pub fn was_triggered(&self) -> bool {
        REFUSAL.try_with(|state| matches!(state.get(), RefusalState::Consumed)).unwrap_or(false)
    }
}

impl Drop for AllocationRefusal {
    fn drop(&mut self) {
        let _ = REFUSAL.try_with(|state| state.set(RefusalState::Inactive));
    }
}

/// `try_with` because TLS is unavailable during thread teardown; an
/// allocation there is not attributable to any test window and is dropped
/// rather than panicking inside the allocator.
#[inline]
fn bump_thread(bytes: usize) {
    let _ = THREAD_ALLOCATIONS.try_with(|c| c.set(c.get().wrapping_add(1)));
    let _ = THREAD_BYTES.try_with(|c| c.set(c.get().wrapping_add(bytes as u64)));
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

    /// Bytes requested by the **calling thread** — a churn counter (frees
    /// are never subtracted), for "this path allocates O(x), not O(y)"
    /// assertions where the count alone cannot tell a 16-byte box from a
    /// cloned match set.
    #[inline]
    pub fn thread_bytes(&self) -> u64 {
        THREAD_BYTES.try_with(Cell::get).unwrap_or(0)
    }

    /// Requested bytes freed on this thread. For a window with no failed
    /// requests or ownership transfers between threads, requested minus
    /// freed bytes is the change in retained allocator-requested storage.
    #[inline]
    pub fn thread_freed_bytes(&self) -> u64 {
        THREAD_FREED_BYTES.try_with(Cell::get).unwrap_or(0)
    }

    /// Let `skip` requests reach System, then refuse exactly one request
    /// on this thread. Null is returned through the real GlobalAlloc seam:
    /// try_reserve observes an error, while an infallible allocation aborts.
    /// This does not alter deallocation or a refused realloc's old block.
    pub fn refuse_after(&self, skip: u64) -> Result<AllocationRefusal, RefusalActive> {
        REFUSAL.with(|state| match state.get() {
            RefusalState::Inactive => {
                state.set(RefusalState::Armed(skip));
                Ok(AllocationRefusal { thread: PhantomData })
            }
            RefusalState::Armed(_) | RefusalState::Consumed => Err(RefusalActive),
        })
    }
}

impl Default for CountingAllocator {
    fn default() -> CountingAllocator {
        CountingAllocator::new()
    }
}

// SAFETY: successful requests delegate the pointer/layout contract unchanged
// to System. An injected refusal returns null before calling System; a refused
// realloc therefore leaves its original allocation live and unchanged.
// Counters and the const-initialized TLS state do not allocate or unwind.
unsafe impl GlobalAlloc for CountingAllocator {
    /// # Safety
    /// As `GlobalAlloc::alloc`: `layout` has a non-zero size; the request is forwarded to `System`
    /// unchanged.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread(layout.size());
        if refuse_request() {
            return std::ptr::null_mut();
        }
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc(layout) }
    }

    /// # Safety
    /// As `GlobalAlloc::dealloc`: `ptr` came from this allocator with `layout`; forwarded to
    /// `System` unchanged.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_free(layout.size());
        // SAFETY: forwarded unchanged under the caller's deallocation contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    /// # Safety
    /// As `GlobalAlloc::alloc_zeroed`: `layout` has a non-zero size; forwarded to `System`
    /// unchanged.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread(layout.size());
        if refuse_request() {
            return std::ptr::null_mut();
        }
        // SAFETY: forwarded unchanged under the caller's allocation contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    /// # Safety
    /// As `GlobalAlloc::realloc`: `ptr` came from this allocator with `layout` and `new_size` is
    /// non-zero; forwarded to `System` unchanged.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        bump_thread(new_size);
        if refuse_request() {
            return std::ptr::null_mut();
        }
        // SAFETY: forwarded unchanged under the caller's reallocation contract.
        let result = unsafe { System.realloc(ptr, layout, new_size) };
        if !result.is_null() {
            record_free(layout.size());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_is_one_shot_and_thread_owned() {
        let alloc = CountingAllocator::new();
        let layout = Layout::from_size_align(64, 8).expect("layout");
        let guard = alloc.refuse_after(0).expect("no prior refusal");
        assert!(matches!(alloc.refuse_after(0), Err(RefusalActive)));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // SAFETY: the returned live block is freed here with the same layout.
                let ptr = unsafe { alloc.alloc(layout) };
                assert!(!ptr.is_null(), "a different thread inherited the refusal");
                // SAFETY: ptr was allocated directly above with layout.
                unsafe { alloc.dealloc(ptr, layout) };
            });
        });
        // SAFETY: a valid nonzero layout; null is expected and never dereferenced.
        assert!(unsafe { alloc.alloc_zeroed(layout) }.is_null());
        assert!(guard.was_triggered());
        assert!(matches!(alloc.refuse_after(0), Err(RefusalActive)));
        // SAFETY: the single refusal was consumed; the block is freed below.
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated directly above with layout.
        unsafe { alloc.dealloc(ptr, layout) };
        drop(guard);
        let unused = alloc.refuse_after(0).expect("prior guard released");
        drop(unused);
        // SAFETY: dropping an unused guard cancels it; block freed below.
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        // SAFETY: ptr was allocated directly above with layout.
        unsafe { alloc.dealloc(ptr, layout) };
    }

    #[test]
    fn refused_realloc_preserves_original() {
        let alloc = CountingAllocator::new();
        let layout = Layout::from_size_align(64, 8).expect("layout");
        let guard = alloc.refuse_after(1).expect("no prior refusal");
        // SAFETY: valid layout; the first request succeeds and is freed below.
        let ptr = unsafe { alloc.alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        // SAFETY: the live 64-byte block contains this first byte.
        unsafe { ptr.write(0xA5) };
        let freed = alloc.thread_freed_bytes();
        // SAFETY: ptr is the live block allocated above; new size is nonzero.
        assert!(unsafe { alloc.realloc(ptr, layout, 128) }.is_null());
        assert!(guard.was_triggered());
        assert_eq!(alloc.thread_freed_bytes(), freed);
        // SAFETY: failed realloc leaves the original allocation live and unchanged.
        assert_eq!(unsafe { ptr.read() }, 0xA5);
        // SAFETY: the original pointer and layout remain the allocation's owner.
        unsafe { alloc.dealloc(ptr, layout) };
    }

    #[test]
    fn freed_bytes_include_successful_realloc() {
        let alloc = CountingAllocator::new();
        let small = Layout::from_size_align(64, 8).expect("layout");
        let large = Layout::from_size_align(128, 8).expect("layout");
        let requested = alloc.thread_bytes();
        let freed = alloc.thread_freed_bytes();
        // SAFETY: valid layout, grown then freed here.
        let ptr = unsafe { alloc.alloc(small) };
        assert!(!ptr.is_null());
        // SAFETY: ptr is the live small allocation; size is nonzero with the same alignment.
        let grown = unsafe { alloc.realloc(ptr, small, large.size()) };
        assert!(!grown.is_null());
        assert_eq!(alloc.thread_bytes() - requested, 192);
        assert_eq!(alloc.thread_freed_bytes() - freed, 64);
        // SAFETY: successful realloc transferred ownership to grown with the large layout.
        unsafe { alloc.dealloc(grown, large) };
        assert_eq!(alloc.thread_freed_bytes() - freed, 192);
    }

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
    fn thread_bytes_sum_requested_sizes() {
        static ALLOC: CountingAllocator = CountingAllocator::new();
        let layout = Layout::from_size_align(96, 8).expect("layout");
        let before = ALLOC.thread_bytes();
        // SAFETY: allocated and freed here with the same layout.
        let ptr = unsafe { ALLOC.alloc(layout) };
        assert!(!ptr.is_null());
        // SAFETY: `ptr` came from the `alloc` directly above.
        unsafe { ALLOC.dealloc(ptr, layout) };
        assert_eq!(ALLOC.thread_bytes() - before, 96, "frees are not subtracted");
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
