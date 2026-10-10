//! The capped deque's allocation census (ADR-0151 D6; ADR-0171 D5): zero
//! allocator calls after `Live` across churn on both crossings, and a
//! refusal planted at each constructor call builds nothing. Counted here,
//! from an `inf-runtime` integration test, so `inf-foundation` stays
//! dependency-free: the counting allocator is `inf-alloc`'s. Counts are per
//! thread (a process-wide delta captures harness noise it cannot attribute).

use inf_alloc::CountingAllocator;
use inf_foundation::CellId;
use inf_foundation::bounded::{
    Cap, CapCensus, CapError, CapFill, CappedDeque, Coalesce, Lossy, Merge, Reserve, Reserved,
};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

const ENTRIES: Cap = Cap::entries::<1024>("alloc-entries", CapFill::Serving);
const MARKS: Cap = Cap::entries::<1024>("alloc-marks", CapFill::Serving);
const CHURN: u64 = 100_000;

#[derive(Clone, Copy)]
struct Mark(u64);

impl Lossy for Mark {}

struct Later;

impl Merge<Mark> for Later {
    fn merge(back: &mut Mark, new: Mark) {
        *back = new;
    }
}

#[test]
fn no_allocator_call_after_live_across_churn() {
    let census = CapCensus::new(CellId(0));
    let mut entries = CappedDeque::<u64, Reserve>::new(ENTRIES, &census).expect("built");
    let mut marks = CappedDeque::<Mark, Coalesce<Later>>::new(MARKS, &census).expect("built");
    let before = ALLOC.thread_allocations();
    let mut acc = 0u64;
    for i in 0..CHURN {
        // Fill past the cap, drain by a third, merge: every path of both
        // crossings, reaching Full and the merge arm on every pass.
        if let Reserved::Slot(slot) = entries.reserve() {
            slot.publish(i);
        }
        marks.push(Mark(i));
        if i % 3 == 0 {
            acc ^= entries.pop_front().unwrap_or(0);
            acc ^= marks.pop_front().map_or(0, |m| m.0);
        }
        if i % 4096 == 4095 {
            // Pairs merge; between passes the deque gains two entries per
            // three ops, so it reaches the cap and the merge arm each time.
            marks.merge_adjacent(|a, b| a.0 >> 1 == b.0 >> 1);
        }
        if let Some(back) = entries.back_mut() {
            *back ^= 1;
        }
    }
    assert_eq!(ALLOC.thread_allocations(), before, "an allocator call after Live");
    assert!(entries.len() <= 1024);
    assert!(marks.len() <= 1024);
    let row = census.row("alloc-entries").expect("row");
    assert_eq!(row.high_water, 1024);
    assert!(row.crossings > 0, "the churn reached Full");
    let row = census.row("alloc-marks").expect("row");
    assert!(row.crossings > 0, "the churn reached the merge arm");
    assert_ne!(acc, 0);
}

#[test]
fn a_refused_backing_builds_nothing() {
    let census = CapCensus::new(CellId(1));
    // The deque's construction makes exactly one allocation: the backing.
    let before = ALLOC.thread_allocations();
    let built = CappedDeque::<u64, Reserve>::new(ENTRIES, &census).expect("built");
    assert_eq!(ALLOC.thread_allocations() - before, 1, "one allocation per deque");
    drop(built);
    for refused_at in 0..2u8 {
        let guard = ALLOC.refuse_after(0).expect("no refusal armed");
        let outcome = match refused_at {
            0 => CappedDeque::<u64, Reserve>::new(ENTRIES, &census).map(drop),
            _ => CappedDeque::<Mark, Coalesce<Later>>::new(MARKS, &census).map(drop),
        };
        assert!(guard.was_triggered(), "the constructor reached the allocator");
        assert_eq!(outcome, Err(CapError::Alloc));
        drop(guard);
    }
    // The name keeps its row: a retry shares it, and nothing else was kept.
    assert_eq!(census.rows().count(), 2);
    let retry = CappedDeque::<u64, Reserve>::new(ENTRIES, &census).expect("the retry builds");
    assert_eq!(retry.entries_max(), 1024);
}
