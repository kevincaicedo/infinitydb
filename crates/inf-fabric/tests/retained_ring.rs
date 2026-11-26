//! The retained ring's allocations (ADR-0157 D2, D4), read by the counting
//! allocator on the test's thread: a build requests exactly the bytes the
//! ring bills, each refused allocation retires what was built, and every
//! byte requested is freed by the retirement.

// Loom replaces the ring's atomics; these tests read the real allocator.
#![cfg(not(loom))]

use inf_alloc::CountingAllocator;
use inf_fabric::{RetainedRing, RetireCause, RingPhase, RingSteps};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

#[repr(align(256))]
struct Wide {
    _bytes: [u8; 256],
}

/// This thread's (allocations, requested bytes, freed bytes).
fn counters() -> (u64, u64, u64) {
    (ALLOCATOR.thread_allocations(), ALLOCATOR.thread_bytes(), ALLOCATOR.thread_freed_bytes())
}

/// Builds while the ring is Building, one unit a call: at most three.
fn build(ring: &mut RetainedRing<impl Sized>) -> RingPhase {
    for _ in 0..3 {
        ring.build_step(&mut RingSteps::new(1));
    }
    ring.phase()
}

/// Retires to Terminal one unit a call.
fn retire(ring: &mut RetainedRing<impl Sized>) -> RingPhase {
    for _ in 0..4 {
        ring.retire_step(&mut RingSteps::new(1), drop);
    }
    ring.phase()
}

/// A full build requests `blocks` allocations of exactly the ring's billed
/// bytes and frees none; a cancel retires them all. Then each allocation
/// refused in turn: the ring enters Retiring with the refusal's cause and
/// frees every byte it was granted.
fn every_refusal_retires<T>(blocks: u64) {
    let before = counters();
    let mut ring = RetainedRing::<T>::admit().expect("within the bound");
    assert_eq!(counters(), before, "admission allocates nothing");
    assert_eq!(build(&mut ring), RingPhase::Ready);
    let built = counters();
    assert_eq!(built.0 - before.0, blocks, "one allocation a block");
    assert_eq!(built.1 - before.1, ring.requested_bytes() as u64, "the bill is what it requests");
    assert_eq!(built.2, before.2, "building frees nothing");
    ring.cancel();
    assert_eq!(retire(&mut ring), RingPhase::Terminal(RetireCause::Cancelled));
    assert_eq!(counters().2 - built.2, ring.requested_bytes() as u64, "every byte freed");

    for ordinal in 0..blocks {
        let before = counters();
        let mut ring = RetainedRing::<T>::admit().expect("within the bound");
        let refusal = ALLOCATOR.refuse_after(ordinal).expect("no refusal window open");
        let phase = build(&mut ring);
        assert!(refusal.was_triggered(), "refusal {ordinal} engaged");
        drop(refusal);
        assert_eq!(phase, RingPhase::Retiring(RetireCause::AllocationRefused), "{ordinal}");
        let refused = counters();
        assert_eq!(retire(&mut ring), RingPhase::Terminal(RetireCause::AllocationRefused));
        let after = counters();
        // The refused request is counted as requested and never granted.
        let granted_bytes = after.1 - before.1 - refused_bytes::<T>(ordinal);
        assert_eq!(refused.2, before.2, "{ordinal}: building frees nothing");
        assert_eq!(after.2 - before.2, granted_bytes, "{ordinal}: every granted byte freed");
    }
}

/// The bytes of allocation `ordinal`: the slot block first, unless the
/// payload is zero-sized and only the header allocates.
fn refused_bytes<T>(ordinal: u64) -> u64 {
    let slots = (8 * size_of::<T>()) as u64;
    let ring = RetainedRing::<T>::admit().expect("within the bound");
    let header = ring.requested_bytes() as u64 - slots;
    let mut ring = ring;
    ring.cancel();
    retire(&mut ring);
    match (slots, ordinal) {
        (0, _) | (_, 1) => header,
        _ => slots,
    }
}

#[test]
fn every_allocation_refusal_retires_what_was_built_u64() {
    every_refusal_retires::<u64>(2);
}

#[test]
fn every_allocation_refusal_retires_what_was_built_over_aligned() {
    every_refusal_retires::<Wide>(2);
}

#[test]
fn every_allocation_refusal_retires_what_was_built_zero_sized() {
    every_refusal_retires::<()>(1);
}

/// A payload over the byte bound is refused before any allocation.
#[test]
fn a_refused_layout_allocates_nothing() {
    let before = counters();
    let refused = RetainedRing::<[u8; 8_193]>::admit().map(drop);
    assert!(refused.is_err(), "8 × 8,193 bytes is over the bound");
    assert_eq!(counters().0, before.0, "no allocation before the refusal");
}
