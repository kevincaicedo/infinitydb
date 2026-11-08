//! The cold-read path's allocation census: once the host is built, its read
//! path makes no allocator call on the cell's thread across enqueue, drain,
//! receipt, delivery, cancellation and drop churn (ADR-0181 R4: its stores
//! are fixed when the host is built). This binary's global allocator is
//! inf-alloc's counting allocator, one per test binary, so inf-runtime's
//! other tests run on the system allocator; counts are per thread, since a
//! process-wide delta takes in whatever the harness does. Beside it, the
//! slot stride the capped store gives each of the host's fixed entries, read
//! from the bytes its one chunk requests.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use inf_alloc::{AlignedPool, CountingAllocator};
use inf_foundation::CellId;
use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapFill, CappedSlab, Retiring, WrapWithProof,
};
use inf_runtime::{
    ColdDone, ColdReadConfig, ColdReads, ColdWait, CompletionResult, IoOp, ReadClass, TierFileId,
};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// The default `COLD-READ-QD`: 64 buffers, class queues of 256.
const DEPTH: usize = 64;
/// One pool buffer, the server's cold window.
const BUFFER_BYTES: usize = 4 * inf_alloc::TIER_READ_ALIGN;
const ROUNDS: u32 = 64;
/// A round's shape: one cohort of 31 adjacent requests on one file, 16
/// singletons on 16 files, 8 requests cancelled while queued, 4 cancelled
/// after their read issued, and 4 values kept to the next round.
const COHORT: usize = 31;
const SINGLES: usize = 16;
const CANCELLED_QUEUED: usize = 8;
const CANCELLED_ISSUED: usize = 4;
const KEPT: usize = 4;

/// The harness's storage for one round, reserved before the count starts:
/// a push within capacity makes no allocator call.
struct Round {
    waits: Vec<ColdWait>,
    values: Vec<ColdDone>,
    kept: Vec<ColdDone>,
    ops: Vec<IoOp>,
}

/// One round of churn on `cold`: enqueue, cancel while queued, drain,
/// cancel after issue, every receipt, every value polled, most dropped at
/// once and `KEPT` held to the next round.
fn churn(cold: &ColdReads, round: u32, storage: &mut Round) {
    let mut cx = Context::from_waker(Waker::noop());
    let file = |id: u32| TierFileId::new(id);
    let class = ReadClass::Foreground;
    for member in 0..COHORT {
        let offset = (member * 512) as u64;
        let wait = cold.enqueue(-1, file(1_000 + round), offset, 512, class, 0).expect("room");
        storage.waits.push(wait);
    }
    for single in 0..SINGLES {
        let id = u32::try_from(single).expect("a few files");
        storage.waits.push(cold.enqueue(-1, file(id), 0, 64, class, 0).expect("room"));
    }
    for cancelled in 0..CANCELLED_QUEUED {
        let id = 100 + u32::try_from(cancelled).expect("a few files");
        drop(cold.enqueue(-1, file(id), 0, 64, class, 0).expect("room"));
    }
    let ops = &mut storage.ops;
    cold.drain(|op| ops.push(op));
    for _ in 0..CANCELLED_ISSUED {
        drop(storage.waits.pop());
    }
    for op in storage.ops.drain(..) {
        let IoOp::TierRead { token, .. } = op else { panic!("the host issues reads") };
        cold.on_completion(token, CompletionResult::TierRead, u64::from(round));
    }
    storage.kept.clear();
    for mut wait in storage.waits.drain(..) {
        let Poll::Ready(done) = Pin::new(&mut wait).poll(&mut cx) else {
            panic!("a delivered value is ready at its first poll");
        };
        storage.values.push(done);
    }
    for (index, done) in storage.values.drain(..).enumerate() {
        if index < KEPT {
            storage.kept.push(done);
        }
    }
}

/// Zero allocator calls on this thread from the host's last build step to
/// the end of `ROUNDS` rounds of churn. The engagement half reads that the
/// churn merged, issued, delivered, purged and orphaned, and that custody
/// is back.
#[test]
#[ignore = "the cold-read path allocates while it serves: its queues and tables grow on demand \
            and each device read takes a waiter list; red until the host's stores are fixed when \
            it is built and nothing on its read path allocates"]
fn no_allocator_call_after_the_cold_host_is_live() {
    let config = ColdReadConfig { qd_cap: DEPTH, overflow_cap: 4 * DEPTH, ..Default::default() };
    let cold = ColdReads::with_config(AlignedPool::new(DEPTH, BUFFER_BYTES), config);
    let width = COHORT + SINGLES;
    let mut storage = Round {
        waits: Vec::with_capacity(width),
        values: Vec::with_capacity(width),
        kept: Vec::with_capacity(KEPT),
        ops: Vec::with_capacity(DEPTH),
    };
    let before = ALLOC.thread_allocations();
    for round in 0..ROUNDS {
        churn(&cold, round, &mut storage);
    }
    let calls = ALLOC.thread_allocations() - before;
    storage.kept.clear();
    let counts = cold.counters();
    assert_eq!(cold.reconcile(), Ok(()), "engagement: custody is back after the churn");
    assert!(counts.merged_waiters > 0, "engagement: the cohorts merged");
    assert!(counts.cancelled_queued > 0, "engagement: queued requests were purged");
    assert!(counts.unclaimed > 0, "engagement: issued requests were orphaned");
    assert!(counts.delivered > 0, "engagement: values were delivered");
    assert_eq!(
        calls, 0,
        "{calls} allocator call(s) on the cold-read path after the host was live, over \
         {ROUNDS} rounds of churn"
    );
}

/// One store chunk: 64 slots.
const CHUNK: Cap = Cap::entries::<64>("cold-stride", CapFill::Serving);

/// The store's slot stride for `T`: the bytes a 64-slot slab of `T`
/// requests, its one chunk, over 64. The slab is retired by steps after.
fn store_stride<T>() -> usize {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder =
        CappedSlab::<T, u32, WrapWithProof>::builder(CHUNK, &census).expect("registered");
    let before = ALLOC.thread_bytes();
    let slab = loop {
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let bytes = ALLOC.thread_bytes() - before;
    let mut retiring = slab.retire();
    while let Retiring::More(next) = retiring.step() {
        retiring = next;
    }
    usize::try_from(bytes).expect("one chunk") / 64
}

/// The store adds at most 8 B a slot (its generation and tag) to each of
/// the host's fixed entries at its stride's bound: a waiter block 744 B, a
/// window descriptor 104 B, a queued request 120 B, each 8-aligned. The
/// payloads are word arrays of those sizes with no niche, which bounds the
/// entries' strides from above: an entry's niche can only absorb the tag.
#[test]
fn the_store_adds_at_most_eight_bytes_to_each_fixed_entry() {
    let strides = [
        ("WaiterBlock", 744, store_stride::<[u64; 93]>()),
        ("WindowDescriptor", 104, store_stride::<[u64; 13]>()),
        ("IntentSlot", 120, store_stride::<[u64; 15]>()),
    ];
    for (entry, size, stride) in strides {
        eprintln!("store stride {entry}: {size} B entry, {stride} B slot");
        assert!(stride >= size, "{entry}: a slot holds its entry");
        assert!(stride <= size + 8, "{entry}: the store adds {} B a slot", stride - size);
    }
}
