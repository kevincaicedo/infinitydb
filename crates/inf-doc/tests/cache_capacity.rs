//! ADR-0146: validate before allocating; reserve before installing; count actual storage.

use inf_alloc::CountingAllocator;
use inf_doc::ProgramCache;
use inf_doc::limits::{PROGRAM_CACHE_ENTRIES_MAX, ProgramCacheCapacity};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

#[test]
fn capacity_boundary_refuses_before_any_allocation() {
    for requested in [usize::from(PROGRAM_CACHE_ENTRIES_MAX) + 1, u32::MAX as usize, usize::MAX] {
        let before = ALLOC.thread_allocations();
        let result = ProgramCacheCapacity::try_from(requested);
        assert!(result.is_err(), "out-of-budget count must be refused before constructing");
        assert_eq!(ALLOC.thread_allocations(), before);
    }
    for requested in [0, 1024, usize::from(PROGRAM_CACHE_ENTRIES_MAX)] {
        let capacity = ProgramCacheCapacity::try_from(requested).unwrap();
        let before = ALLOC.thread_allocations();
        let cache = ProgramCache::try_new(capacity).unwrap();
        let allocations = ALLOC.thread_allocations() - before;
        assert_eq!(allocations, if requested == 0 { 0 } else { 2 });
        assert!(cache.is_empty());
    }
}

#[test]
fn either_metadata_reservation_can_refuse_without_replacing_the_cache() {
    let capacity = ProgramCacheCapacity::try_from(4).unwrap();
    let mut current = ProgramCache::try_new(capacity).unwrap();
    current.get_or_compile(b"$.kept", 4096).unwrap();
    let before = (current.len(), current.bytes(), current.hits(), current.misses());
    for skip in 0..2 {
        let guard = ALLOC.refuse_after(skip).unwrap();
        let replacement = ProgramCache::try_new(capacity);
        let triggered = guard.was_triggered();
        drop(guard);
        assert!(triggered, "reservation refusal was never reached");
        assert!(replacement.is_err(), "an allocation refusal must reach the constructor caller");
        assert_eq!((current.len(), current.bytes(), current.hits(), current.misses()), before);
    }
    assert_eq!(
        current.get_or_compile(b"$.kept", 4096).unwrap().as_bytes(),
        inf_doc::path::compile(b"$.kept").unwrap().as_bytes()
    );
}

#[test]
fn resident_byte_gauge_matches_independent_allocation_census() {
    let mut cache = ProgramCache::try_new(ProgramCacheCapacity::try_from(4).unwrap()).unwrap();
    for i in 0..32 {
        let text = format!("$.key{i}");
        let reported = cache.bytes();
        let requested = ALLOC.thread_bytes();
        let freed = ALLOC.thread_freed_bytes();
        cache.get_or_compile(std::hint::black_box(text.as_bytes()), 4096).unwrap();
        let retained = i128::from(ALLOC.thread_bytes() - requested)
            - i128::from(ALLOC.thread_freed_bytes() - freed);
        assert_eq!(cache.bytes() as i128 - reported as i128, retained);
    }
}
