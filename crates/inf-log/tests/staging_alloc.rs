//! M2-S03 AC: **zero heap allocations on the append path** (L5). The whole
//! steady-state cycle — stage → reserve → seal → commit → resolve LSNs →
//! release — runs under a counting global allocator and must perform zero
//! allocations after cell construction. This is the alloc-counter artifact
//! for the AC; it re-binds end-to-end under the real reactor write path in
//! M2-S05/S22.
//!
//! One test per binary: the counter is process-global, so no other test
//! may share this file.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use inf_log::fs::mem::MemFs;
use inf_log::{
    MutationEffect, NsId, SegmentConfig, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs,
};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Delegates every operation verbatim to [`System`], counting allocation
/// events. Test-binary-only instrumentation — the library itself stays
/// `#![forbid(unsafe_code)]`.
struct CountingAllocator;

// SAFETY: pure delegation to `System`, which upholds the `GlobalAlloc`
// contract; the added atomic counter has no effect on the returned memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged; caller upholds `dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; caller upholds `alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; caller upholds `realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[test]
fn append_path_performs_zero_heap_allocations() {
    // Construction: the one allowed allocation point (domain buffers,
    // segment preallocation, test scaffolding).
    let fs = MemFs::new();
    let dirs = create_cell_dirs(&fs, &PathBuf::from("data/shard-0")).expect("dirs");
    // Segment large enough that the measured loop never rotates —
    // rotation/prealloc is MAINTAIN work, not the append path.
    let cfg = SegmentConfig { segment_bytes: 32 << 20, seal_after_ms: None };
    let mut rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg).expect("rotor");
    let mut ring = StagingRing::new(StagingConfig { capacity_bytes: 64 << 10 });

    let value = [0x5A_u8; 64];
    let effects = [
        MutationEffect::StringSet { ns: NsId(1), key: b"user:0042", value: &value },
        MutationEffect::Delete { ns: NsId(1), key: b"user:0042" },
        MutationEffect::ExpireAt { ns: NsId(1), at_unix_ms: 1_780_000_000_000, key: b"user:0042" },
    ];

    let mut steady_iterations = || {
        for _ in 0..1_000 {
            for _ in 0..16 {
                for effect in &effects {
                    ring.stage(effect).expect("sized to fit");
                }
            }
            let lease = ring.flush_into(&mut rotor, 0).expect("flush").expect("frame");
            // LSN resolution is part of the per-iteration cycle (S06 gate
            // registration consumes it).
            let _ = lease.first_record_lsn();
            ring.release(lease);
        }
    };

    // Warm-up: first pass through every code path (lazy one-time work,
    // e.g. CRC dispatch caching, would otherwise show up as noise).
    steady_iterations();

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    steady_iterations();
    let after = ALLOCATIONS.load(Ordering::Relaxed);

    assert_eq!(
        after - before,
        0,
        "append path allocated: stage/seal/commit/release must be allocation-free (L5)"
    );
}
