//! The cold-read path's allocation census: once the host is built, its read
//! path makes no allocator call on the cell's thread across enqueue, drain,
//! receipt, delivery, cancellation and drop churn, nor at its full states:
//! both class queues at their cap and refusing one more, every pool buffer
//! held until a drain finds the pool dry, and maintain reads the device
//! budget defers (ADR-0181 R4: its stores are fixed when the host is built).
//! This binary's global allocator is inf-alloc's counting allocator, one per
//! test binary, so inf-runtime's other tests run on the system allocator;
//! counts are per thread, since a process-wide delta takes in whatever the
//! harness does. Beside it, the slot stride the capped store gives each of
//! the host's fixed entries, read from the bytes its one chunk requests.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use inf_alloc::{AlignedPool, CountingAllocator};
use inf_foundation::CellId;
use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapFill, CappedSlab, Retiring, WrapWithProof,
};
use inf_runtime::{
    ColdDone, ColdReadConfig, ColdReads, ColdReceipt, ColdRefused, ColdTurn, ColdWait, IoOp, Issue,
    ReadClass, TierFileId,
};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

/// The default `COLD-READ-QD`: 64 buffers.
const DEPTH: usize = 64;
/// A class queue's cap, four times the depth as the server sizes it: 256.
const CLASS_QUEUE: usize = 4 * DEPTH;
/// One pool buffer, the server's cold window.
const BUFFER_BYTES: usize = 4 * inf_alloc::TIER_READ_ALIGN;
const ROUNDS: u32 = 64;
/// A round's churn: one cohort of 31 adjacent requests on one file, 16
/// singletons on 16 files, 8 requests cancelled while queued, 4 cancelled
/// after their read issued, and 4 values kept to the next round.
const COHORT: usize = 31;
const SINGLES: usize = 16;
const CANCELLED_QUEUED: usize = 8;
const CANCELLED_ISSUED: usize = 4;
const KEPT: usize = 4;
/// A round's full state: one request a file on files of its own per class,
/// and the maintain requests that issue at its end (the class's others are
/// cancelled while queued).
const FOREGROUND_FILES: u32 = 10_000;
const MAINTAIN_FILES: u32 = 20_000;
const MAINTAIN_ISSUED: usize = 4;
/// The drains that empty the queues after the full state: each call purges
/// or issues at least one request, so twice a class queue bounds them.
const EMPTYING_DRAINS_MAX: usize = 2 * CLASS_QUEUE;

/// The harness's storage for one round, reserved before the count starts:
/// a push within capacity makes no allocator call.
struct Round {
    waits: Vec<ColdWait>,
    values: Vec<ColdDone>,
    kept: Vec<ColdDone>,
    ops: Vec<IoOp>,
    foreground: Vec<ColdWait>,
    maintain: Vec<ColdWait>,
    queued: Vec<ColdWait>,
}

/// The device budget of the full state's drains: the foreground issues now,
/// every maintain offer waits for a later slice.
fn defer_maintain(class: ReadClass, _bytes: u64) -> Issue {
    match class {
        ReadClass::Foreground => Issue::Now,
        ReadClass::Maintain => Issue::NotThisSlice,
    }
}

/// Every kept device read's receipt, success, then turns until every
/// member is delivered.
fn receipts(cold: &ColdReads, ops: &mut Vec<IoOp>, now_us: u64) {
    for op in ops.drain(..) {
        let IoOp::TierRead { token, .. } = op else { panic!("the host issues reads") };
        let receipt = cold.record_completion(token, Ok(()), now_us);
        assert_eq!(receipt, ColdReceipt::Recorded, "a receipt names an issued read");
    }
    while cold.has_ready_delivery() {
        let mut turn = ColdTurn::begin();
        let _ = cold.deliver_ready(turn.delivery(), now_us);
    }
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
    receipts(cold, &mut storage.ops, u64::from(round));
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

/// Fills `class`'s queue to its cap with one request a file from `first`,
/// then offers one more, which the full queue refuses.
fn fill(cold: &ColdReads, class: ReadClass, first: u32, waits: &mut Vec<ColdWait>, now_us: u64) {
    let cap = u32::try_from(CLASS_QUEUE).expect("a class queue's files");
    for id in first..first + cap {
        let wait = cold.enqueue(-1, TierFileId::new(id), 0, 64, class, now_us).expect("room");
        waits.push(wait);
    }
    let refused = cold.enqueue(-1, TierFileId::new(first + cap), 0, 64, class, now_us);
    assert!(matches!(refused, Err(ColdRefused::QueueFull)), "a full class queue refuses");
}

/// The full state, once a round: both class queues filled to their cap and
/// offered one more each; drains under `defer_maintain`, each call's
/// receipts recorded and no value dropped, until one finds every pool
/// buffer held (at most one call a buffer and the one that finds none);
/// then the values dropped, every queued foreground request and all but
/// `MAINTAIN_ISSUED` maintain requests cancelled, and drains until the
/// queues are empty, which purge the cancelled and issue the rest.
fn full_state(cold: &ColdReads, round: u32, storage: &mut Round) {
    let now_us = u64::from(round);
    fill(cold, ReadClass::Foreground, FOREGROUND_FILES, &mut storage.foreground, now_us);
    fill(cold, ReadClass::Maintain, MAINTAIN_FILES, &mut storage.maintain, now_us);
    let mut dry = false;
    for _ in 0..=DEPTH {
        let before = cold.counters().pool_dry;
        let ops = &mut storage.ops;
        cold.drain_budgeted(defer_maintain, |_, _, _| {}, |op| ops.push(op));
        receipts(cold, &mut storage.ops, now_us);
        if cold.counters().pool_dry > before {
            dry = true;
            break;
        }
    }
    assert!(dry, "a drain finds every pool buffer held");
    let mut cx = Context::from_waker(Waker::noop());
    for mut wait in storage.foreground.drain(..) {
        let polled = Pin::new(&mut wait).poll(&mut cx);
        match polled {
            Poll::Ready(done) => storage.values.push(done),
            Poll::Pending => storage.queued.push(wait),
        }
    }
    storage.values.clear();
    storage.queued.clear();
    storage.maintain.truncate(MAINTAIN_ISSUED);
    for _ in 0..EMPTYING_DRAINS_MAX {
        if cold.queue_depth() == 0 {
            break;
        }
        let ops = &mut storage.ops;
        cold.drain(|op| ops.push(op));
        receipts(cold, &mut storage.ops, now_us);
    }
    assert_eq!(cold.queue_depth(), 0, "the drains empty the queues");
    for mut wait in storage.maintain.drain(..) {
        let Poll::Ready(done) = Pin::new(&mut wait).poll(&mut cx) else {
            panic!("an issued maintain read is ready at its first poll");
        };
        drop(done);
    }
}

/// Zero allocator calls on this thread from the host's last build step to
/// the end of `ROUNDS` rounds of churn and full states. The engagement half
/// reads that the churn merged, issued, delivered, purged and orphaned, that
/// each full state refused both classes at their cap, found the pool dry
/// and deferred a maintain offer, and that custody is back.
#[test]
#[ignore = "the cold-read path allocates while it serves: its queues and tables grow on demand \
            and each device read takes a waiter list; red until the host's stores are fixed when \
            it is built and nothing on its read path allocates"]
fn no_allocator_call_after_the_cold_host_is_live() {
    let config = ColdReadConfig { qd_cap: DEPTH, overflow_cap: CLASS_QUEUE, ..Default::default() };
    let cold = ColdReads::with_config(AlignedPool::new(DEPTH, BUFFER_BYTES), config);
    let width = COHORT + SINGLES;
    let mut storage = Round {
        waits: Vec::with_capacity(width),
        values: Vec::with_capacity(CLASS_QUEUE),
        kept: Vec::with_capacity(KEPT),
        ops: Vec::with_capacity(DEPTH),
        foreground: Vec::with_capacity(CLASS_QUEUE),
        maintain: Vec::with_capacity(CLASS_QUEUE),
        queued: Vec::with_capacity(CLASS_QUEUE),
    };
    let before = ALLOC.thread_allocations();
    for round in 0..ROUNDS {
        churn(&cold, round, &mut storage);
        full_state(&cold, round, &mut storage);
    }
    let calls = ALLOC.thread_allocations() - before;
    storage.kept.clear();
    let counts = cold.counters();
    let rounds = u64::from(ROUNDS);
    assert_eq!(cold.reconcile(), Ok(()), "engagement: custody is back after the churn");
    assert!(counts.merged_waiters > 0, "engagement: the cohorts merged");
    assert!(counts.cancelled_queued > 0, "engagement: queued requests were purged");
    assert!(counts.unclaimed > 0, "engagement: issued requests were orphaned");
    assert!(counts.delivered > 0, "engagement: values were delivered");
    assert_eq!(counts.queue_full, 2 * rounds, "engagement: both full classes refused a round");
    assert!(counts.pool_dry >= rounds, "engagement: a drain found the pool dry a round");
    assert!(counts.maintain_deferred >= rounds, "engagement: a maintain offer deferred a round");
    let both_full = u32::try_from(2 * CLASS_QUEUE).expect("two class queues");
    assert_eq!(counts.queue_depth_high_water, both_full, "engagement: both classes at their cap");
    assert_eq!(
        calls, 0,
        "{calls} allocator call(s) on the cold-read path after the host was live, over \
         {ROUNDS} rounds of churn and full states"
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
