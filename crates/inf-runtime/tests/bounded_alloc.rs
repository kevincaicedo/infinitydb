//! The capped containers' allocation census (ADR-0151 D6; ADR-0171 D5): zero
//! allocator calls after `Live` across churn, on both of the deque's
//! crossings and both of the slab's generation policies; a refusal planted
//! at each constructor call, and at each of a slab's build steps, builds
//! nothing that is not retired by steps; a slab's build and retirement
//! allocate and free one chunk or directory per step. Counted here,
//! from an `inf-runtime` integration test, so `inf-foundation` stays
//! dependency-free: the counting allocator is `inf-alloc`'s. Counts are per
//! thread (a process-wide delta captures harness noise it cannot attribute).
#![allow(
    clippy::disallowed_types,
    reason = "test target: the slab churn's handle queue, reserved before the count starts"
)]

use std::collections::VecDeque;

use inf_alloc::CountingAllocator;
use inf_foundation::CellId;
use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapError, CapFill, CappedDeque, CappedSlab, Coalesce, Generation,
    GenerationPolicy, Lookup, LookupMut, Lossy, Merge, Reserve, Reserved, Retire, Retiring,
    SlabReserved, SlotId, Taken, WrapWithProof,
};
use inf_foundation::limits::MAP_BACKING_ALLOCATION_BYTES_MAX;

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
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut entries = CappedDeque::<u64, Reserve>::new(ENTRIES, &census).expect("built");
    let mut marks = CappedDeque::<Mark, Coalesce<Later>>::new(MARKS, &census).expect("built");
    let before = ALLOC.thread_allocations();
    let mut accumulator = 0u64;
    for i in 0..CHURN {
        // Fill past the cap, drain by a third, merge: every path of both
        // crossings, reaching Full and the merge arm on every pass.
        if let Reserved::Slot(slot) = entries.reserve() {
            slot.publish(i);
        }
        marks.push(Mark(i));
        if i % 3 == 0 {
            accumulator ^= entries.pop_front().unwrap_or(0);
            accumulator ^= marks.pop_front().map_or(0, |m| m.0);
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
    assert_ne!(accumulator, 0);
}

#[test]
fn a_refused_backing_builds_nothing() {
    let (census, _serve_mark) = CapCensus::new(CellId(1));
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

const SLAB: Cap = Cap::entries::<1024>("alloc-slab", CapFill::Serving);
/// 1,024 slots: one directory and 16 chunks of 64.
const SLAB_ITEMS: u64 = 17;

/// A slab of `SLAB` built by steps, each step one allocation.
fn slab<G: Generation, P: GenerationPolicy>(
    census: &std::rc::Rc<CapCensus>,
) -> CappedSlab<u64, G, P> {
    let mut builder = CappedSlab::<u64, G, P>::builder(SLAB, census).expect("registered");
    let mut steps = 0;
    loop {
        let before = ALLOC.thread_allocations();
        let step = builder.step();
        assert_eq!(ALLOC.thread_allocations() - before, 1, "one allocation per build step");
        steps += 1;
        match step {
            Built::More(next) => builder = next,
            Built::Live(slab) => {
                assert_eq!(steps, SLAB_ITEMS);
                return slab;
            }
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    }
}

/// Reserve, publish, read, write and take across 100,000 ops, reaching
/// `Full` on every pass: no allocator call once the slab is live.
fn churn<G: Generation, P: GenerationPolicy>() {
    let (census, _serve_mark) = CapCensus::new(CellId(2));
    let mut slab = slab::<G, P>(&census);
    let mut held: VecDeque<SlotId<G>> = VecDeque::with_capacity(2048);
    let before = ALLOC.thread_allocations();
    let mut accumulator = 0u64;
    for i in 0..CHURN {
        match slab.reserve() {
            SlabReserved::Slot(slot) => held.push_back(slot.publish(i)),
            SlabReserved::Full => {}
            SlabReserved::Exhausted => panic!("no slot retires in this churn"),
        }
        if let Some(&id) = held.back() {
            if let LookupMut::Live(value) = slab.get_mut(id) {
                *value ^= 1;
            }
            if let Lookup::Live(value) = slab.get(id) {
                accumulator ^= *value;
            }
        }
        if i % 3 == 0
            && let Some(id) = held.pop_front()
        {
            if let Taken::Value(value) = slab.take(id) {
                accumulator ^= value;
            }
            assert_eq!(slab.take(id), Taken::Stale, "a taken handle is stale");
        }
    }
    assert_eq!(ALLOC.thread_allocations(), before, "an allocator call after Live");
    let row = census.row("alloc-slab").expect("row");
    assert_eq!(row.high_water, 1024);
    assert!(row.crossings > 0, "the churn reached Full");
    assert_ne!(accumulator, 0);
}

#[test]
fn no_slab_allocator_call_after_live_across_churn_under_either_policy() {
    churn::<u32, Retire>();
    churn::<u64, WrapWithProof>();
}

#[test]
fn a_slab_retires_one_backing_allocation_per_step() {
    let (census, _serve_mark) = CapCensus::new(CellId(3));
    let mut retiring = slab::<u32, Retire>(&census).retire();
    let mut steps = 0;
    loop {
        let before = ALLOC.thread_freed_bytes();
        let step = retiring.step();
        let freed = ALLOC.thread_freed_bytes() - before;
        assert!(freed > 0, "a step frees its item");
        assert!(
            freed <= MAP_BACKING_ALLOCATION_BYTES_MAX as u64,
            "one backing allocation per step"
        );
        steps += 1;
        match step {
            Retiring::More(next) => retiring = next,
            Retiring::Done => break,
        }
    }
    assert_eq!(steps, SLAB_ITEMS);
    assert_eq!(census.unsliced_drops(), 0);
}

/// A refusal at each build step: the step answers `Refused` with `Alloc`,
/// and the prefix built before it retires by steps, one per item it holds.
#[test]
fn a_refused_slab_build_step_retires_its_prefix_by_steps() {
    let (census, _serve_mark) = CapCensus::new(CellId(4));
    for refused_at in 0..SLAB_ITEMS {
        let mut builder = CappedSlab::<u64, u32, Retire>::builder(SLAB, &census).expect("row");
        for _ in 0..refused_at {
            builder = match builder.step() {
                Built::More(next) => next,
                Built::Live(_) | Built::Refused(..) => panic!("{refused_at}: built early"),
            };
        }
        let guard = ALLOC.refuse_after(0).expect("no refusal armed");
        let step = builder.step();
        assert!(guard.was_triggered(), "the step reached the allocator");
        drop(guard);
        let Built::Refused(mut prefix, refused) = step else { panic!("{refused_at}: not refused") };
        assert_eq!(refused, CapError::Alloc);
        let mut steps = 0;
        loop {
            steps += 1;
            match prefix.step() {
                Retiring::More(next) => prefix = next,
                Retiring::Done => break,
            }
        }
        assert_eq!(steps, refused_at.max(1), "{refused_at}: one step per item built");
    }
    // The name keeps its row; every refused build shared it.
    assert_eq!(census.rows().count(), 1);
}
