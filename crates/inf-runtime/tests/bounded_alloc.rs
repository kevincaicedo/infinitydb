//! The capped containers' allocation census (ADR-0151 D6; ADR-0171 D5): zero
//! allocator calls after `Live` across churn, on both of the deque's
//! crossings, both of the slab's generation policies and the map; a refusal
//! planted at each constructor call, and at each of a slab's or a map's
//! build steps, builds nothing that is not retired by steps; a slab's and a
//! map's build and retirement allocate and free one chunk or directory per
//! step, across both of a map's stores. The slab's rows run at a narrow slot
//! and at a wide one, 2,992 B in chunks of 16 (ADR-0194 A1). Counted here,
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
    Admission, Built, Cap, CapCensus, CapError, CapFill, CappedDeque, CappedMap, CappedSlab,
    Coalesce, Generation, GenerationPolicy, Lookup, LookupMut, Lossy, MapBuilt, MapRetired, Merge,
    Reserve, Reserved, Retire, Retiring, SlabReserved, SlotId, Taken, WrapWithProof,
};
use inf_foundation::limits::{MAP_BACKING_ALLOCATION_BYTES_MAX, chunk_slots_for};

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

/// A wide slot's payload, 372 words: a slot of 2,992 B at a `u64`
/// generation, the widest of the executor's task classes as first measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Wide([u64; 372]);

/// The wide slot's chunk, the count the rule answers for its slot's bytes,
/// picked where the slab is declared as a site that builds one picks it.
const WIDE_CHUNK: usize = match chunk_slots_for(CappedSlab::<Wide, u64, Retire>::SLOT_BYTES) {
    Some(slots) => slots,
    None => panic!("no chunk holds the wide slot"),
};
const _: () = assert!(CappedSlab::<Wide, u64, Retire>::SLOT_BYTES == 2_992, "a 2,992 B slot");
const _: () = assert!(WIDE_CHUNK == 16, "a 2,992 B slot takes chunks of 16");
/// 1,024 wide slots: one directory and 64 chunks of 16.
const WIDE_ITEMS: u64 = 65;

/// A slab's payload in the census, carrying one `u64`.
trait Payload: PartialEq + std::fmt::Debug {
    fn of(value: u64) -> Self;
    fn value(&self) -> u64;
    fn flip(&mut self);
}

impl Payload for u64 {
    fn of(value: u64) -> u64 {
        value
    }

    fn value(&self) -> u64 {
        *self
    }

    fn flip(&mut self) {
        *self ^= 1;
    }
}

impl Payload for Wide {
    fn of(value: u64) -> Wide {
        let mut words = [0; 372];
        words[0] = value;
        words[371] = value;
        Wide(words)
    }

    fn value(&self) -> u64 {
        assert_eq!(self.0[0], self.0[371], "the payload's two words disagree");
        self.0[0]
    }

    fn flip(&mut self) {
        self.0[0] ^= 1;
        self.0[371] ^= 1;
    }
}

/// A slab of `SLAB` in chunks of `C` built by steps, each step one
/// allocation, `items` steps in all.
fn slab<T, G: Generation, P: GenerationPolicy, const C: usize>(
    census: &std::rc::Rc<CapCensus>,
    items: u64,
) -> CappedSlab<T, G, P, C> {
    let mut builder = CappedSlab::<T, G, P, C>::builder(SLAB, census).expect("registered");
    let mut steps = 0;
    loop {
        let before = ALLOC.thread_allocations();
        let step = builder.step();
        assert_eq!(ALLOC.thread_allocations() - before, 1, "one allocation per build step");
        steps += 1;
        match step {
            Built::More(next) => builder = next,
            Built::Live(slab) => {
                assert_eq!(steps, items);
                return slab;
            }
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    }
}

/// Reserve, publish, read, write and take across 100,000 ops, reaching
/// `Full` on every pass: no allocator call once the slab is live.
fn churn<T: Payload, G: Generation, P: GenerationPolicy, const C: usize>(items: u64) {
    let (census, _serve_mark) = CapCensus::new(CellId(2));
    let mut slab = slab::<T, G, P, C>(&census, items);
    let mut held: VecDeque<SlotId<G>> = VecDeque::with_capacity(2048);
    let before = ALLOC.thread_allocations();
    let mut accumulator = 0u64;
    for i in 0..CHURN {
        match slab.reserve() {
            SlabReserved::Slot(slot) => held.push_back(slot.publish(T::of(i))),
            SlabReserved::Full => {}
            SlabReserved::Exhausted => panic!("no slot retires in this churn"),
        }
        if let Some(&id) = held.back() {
            if let LookupMut::Live(value) = slab.get_mut(id) {
                value.flip();
            }
            if let Lookup::Live(value) = slab.get(id) {
                accumulator ^= value.value();
            }
        }
        if i % 3 == 0
            && let Some(id) = held.pop_front()
        {
            if let Taken::Value(value) = slab.take(id) {
                accumulator ^= value.value();
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
    churn::<u64, u32, Retire, 64>(SLAB_ITEMS);
    churn::<u64, u64, WrapWithProof, 64>(SLAB_ITEMS);
}

#[test]
fn no_wide_slab_allocator_call_after_live_across_churn_under_either_policy() {
    churn::<Wide, u64, Retire, WIDE_CHUNK>(WIDE_ITEMS);
    churn::<Wide, u64, WrapWithProof, WIDE_CHUNK>(WIDE_ITEMS);
}

#[test]
fn a_slab_retires_one_backing_allocation_per_step() {
    retires_one_backing_allocation_per_step::<u64, u32, 64>(CellId(3), SLAB_ITEMS);
}

#[test]
fn a_wide_slab_retires_one_backing_allocation_per_step() {
    retires_one_backing_allocation_per_step::<Wide, u64, WIDE_CHUNK>(CellId(8), WIDE_ITEMS);
}

fn retires_one_backing_allocation_per_step<T, G: Generation, const C: usize>(
    cell: CellId,
    items: u64,
) {
    let (census, _serve_mark) = CapCensus::new(cell);
    let mut retiring = slab::<T, G, Retire, C>(&census, items).retire();
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
    assert_eq!(steps, items);
    assert_eq!(census.unsliced_drops(), 0);
}

/// A refusal at each build step: the step answers `Refused` with `Alloc`,
/// and the prefix built before it retires by steps, one per item it holds.
#[test]
fn a_refused_slab_build_step_retires_its_prefix_by_steps() {
    refused_build_steps_retire_their_prefix::<u64, u32, 64>(CellId(4), SLAB_ITEMS);
}

#[test]
fn a_refused_wide_slab_build_step_retires_its_prefix_by_steps() {
    refused_build_steps_retire_their_prefix::<Wide, u64, WIDE_CHUNK>(CellId(9), WIDE_ITEMS);
}

fn refused_build_steps_retire_their_prefix<T, G: Generation, const C: usize>(
    cell: CellId,
    items: u64,
) {
    let (census, _serve_mark) = CapCensus::new(cell);
    for refused_at in 0..items {
        let mut builder = CappedSlab::<T, G, Retire, C>::builder(SLAB, &census).expect("row");
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

const MAP: Cap = Cap::entries::<1024>("alloc-map", CapFill::Serving);
/// 1,024 entries: in each store a directory and 16 chunks of 64 (the
/// branch store's 1,023 slots fill its last chunk but one).
const MAP_ITEMS: u64 = 34;

/// A map of `MAP` built by steps, each step one allocation, across both
/// stores.
fn map(census: &std::rc::Rc<CapCensus>) -> CappedMap<u64, u64> {
    let mut builder = CappedMap::<u64, u64>::builder(MAP, census).expect("registered");
    let mut steps = 0;
    loop {
        let before = ALLOC.thread_allocations();
        let step = builder.step();
        assert_eq!(ALLOC.thread_allocations() - before, 1, "one allocation per build step");
        steps += 1;
        match step {
            MapBuilt::More(next) => builder = next,
            MapBuilt::Live(map) => {
                assert_eq!(steps, MAP_ITEMS);
                return map;
            }
            MapBuilt::Refused(_, refused) => panic!("{refused}"),
        }
    }
}

/// Admit and publish, replace, read, write and remove across 100,000 ops
/// on sparse keys, reaching `Full` on every pass: no allocator call once
/// the map is live.
#[test]
fn no_map_allocator_call_after_live_across_churn() {
    let (census, _serve_mark) = CapCensus::new(CellId(5));
    let mut map = map(&census);
    let mut held: VecDeque<u64> = VecDeque::with_capacity(2048);
    let before = ALLOC.thread_allocations();
    let mut accumulator = 0u64;
    for i in 0..CHURN {
        // An odd multiple modulo 2^64 is a bijection: every key is fresh.
        let key = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        match map.admit(key) {
            Admission::Vacant(slot) => {
                slot.publish(i);
                held.push_back(key);
            }
            Admission::Occupied(_) => panic!("a fresh key is absent"),
            Admission::Full => {}
        }
        if let Some(&newest) = held.back() {
            if let Some(value) = map.get_mut(newest) {
                *value ^= 1;
            }
            accumulator ^= map.get(newest).copied().unwrap_or(0);
        }
        if let Some(&oldest) = held.front()
            && let Admission::Occupied(slot) = map.admit(oldest)
        {
            accumulator ^= slot.replace(i);
        }
        if i % 3 == 0
            && let Some(key) = held.pop_front()
        {
            accumulator ^= map.remove(key).unwrap_or(0);
            assert_eq!(map.remove(key), None, "a removed key is absent");
        }
    }
    assert_eq!(ALLOC.thread_allocations(), before, "an allocator call after Live");
    let row = census.row("alloc-map").expect("row");
    assert_eq!(row.high_water, 1024);
    assert!(row.crossings > 0, "the churn reached Full");
    assert_ne!(accumulator, 0);
}

#[test]
fn a_map_retires_one_backing_allocation_per_step() {
    let (census, _serve_mark) = CapCensus::new(CellId(6));
    let mut retiring = map(&census).retire();
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
            MapRetired::More(next) => retiring = next,
            MapRetired::Done => break,
        }
    }
    assert_eq!(steps, MAP_ITEMS);
    assert_eq!(census.unsliced_drops(), 0);
}

/// A refusal at each build step, in either store: the step answers
/// `Refused` with `Alloc`, and both stores' prefix retires by steps, one
/// per item built.
#[test]
fn a_refused_map_build_step_retires_its_prefix_by_steps() {
    let (census, _serve_mark) = CapCensus::new(CellId(7));
    for refused_at in 0..MAP_ITEMS {
        let mut builder = CappedMap::<u64, u64>::builder(MAP, &census).expect("row");
        for _ in 0..refused_at {
            builder = match builder.step() {
                MapBuilt::More(next) => next,
                MapBuilt::Live(_) | MapBuilt::Refused(..) => panic!("{refused_at}: built early"),
            };
        }
        let guard = ALLOC.refuse_after(0).expect("no refusal armed");
        let step = builder.step();
        assert!(guard.was_triggered(), "the step reached the allocator");
        drop(guard);
        let MapBuilt::Refused(mut prefix, refused) = step else {
            panic!("{refused_at}: not refused")
        };
        assert_eq!(refused, CapError::Alloc);
        let mut steps = 0;
        loop {
            steps += 1;
            match prefix.step() {
                MapRetired::More(next) => prefix = next,
                MapRetired::Done => break,
            }
        }
        assert_eq!(steps, refused_at.max(1), "{refused_at}: one step per item built");
    }
    // The name keeps its row; every refused build shared it.
    assert_eq!(census.rows().count(), 1);
}

const WAIT_POOL: Cap = Cap::entries::<64>("alloc-wait-nodes", CapFill::Serving);
const WAIT_KEYS: Cap = Cap::entries::<3>("alloc-wait-keys", CapFill::Assembly);

/// The wait lists on the cell's pool (ADR-0177 D5): zero allocator calls after the pool's and
/// the keyed list's build across waits made and polled, woken one at a time and whole, read,
/// dropped while queued and dropped after an unread wake (the baton), on a unit list and a
/// keyed one. A wait's node is the pool's, never an allocation of its own.
#[test]
fn no_allocator_call_after_build_across_waits_wakes_and_drops() {
    let (census, _serve_mark) = CapCensus::new(CellId(2));
    let nodes = inf_runtime::WaitNodes::build(WAIT_POOL, &census).expect("the pool").nodes;
    let unit = inf_runtime::WaitList::unit(&nodes);
    let keyed = inf_runtime::WaitList::keyed(&nodes, Some(WAIT_KEYS), &census, [1u32, 2, 3])
        .expect("the keyed list");
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let before = ALLOC.thread_allocations();
    let mut ready = 0u64;
    for i in 0..CHURN {
        let key = 1 + (i % 3) as u32;
        let mut first = unit.wait(());
        let mut second = keyed.wait(key);
        let mut third = keyed.wait(key);
        assert!(std::pin::Pin::new(&mut first).poll(&mut cx).is_pending());
        assert!(std::pin::Pin::new(&mut second).poll(&mut cx).is_pending());
        assert!(std::pin::Pin::new(&mut third).poll(&mut cx).is_pending());
        assert!(unit.wake_one(()));
        ready += u64::from(std::pin::Pin::new(&mut first).poll(&mut cx).is_ready());
        // Woken and dropped unread: the baton goes to the key's next wait.
        assert!(keyed.wake_one(key));
        drop(second);
        ready += u64::from(std::pin::Pin::new(&mut third).poll(&mut cx).is_ready());
        // Dropped while queued: it unlinks.
        let mut fourth = keyed.wait(key);
        assert!(std::pin::Pin::new(&mut fourth).poll(&mut cx).is_pending());
        drop(fourth);
        assert_eq!(keyed.wake_all(key), 0, "no wait is left on the key");
    }
    assert_eq!(ALLOC.thread_allocations(), before, "an allocator call after the build");
    assert_eq!(ready, 2 * CHURN, "every woken wait read its wake");
    assert_eq!(nodes.len(), 0, "every node back in the pool");
}

/// The default node's pool, `T` = 2,825 (ADR-0177 D5): its slab in chunks of 64 nodes under one
/// directory, then its watermark gate's entry array in two chunks under one directory.
const WAIT_POOL_DEFAULT: Cap = Cap::entries::<2_825>("alloc-wait-nodes-t", CapFill::Serving);

/// A refusal planted at each allocation of the pool's build, its slab's steps and then its
/// gate's entry array's: the build answers `Alloc`, and every allocation before the refusal is
/// freed, a refused array retiring the built slab by steps. A refused request is counted as
/// requested and never freed, so the window of the refusal at allocation `k + 1` frees exactly
/// what the window of the refusal at `k` requested, the first frees nothing, and the chain
/// closes on the slab's and the array's bytes, counted here from their layouts: the slab's
/// chunks of `c` slots and its directory of 256 child pointers, the array's entries exact and
/// its directory of one 16 B pointer a chunk. The build's one allocation past them, the pool's
/// shared handle, is infallible, so a refusal there would abort: it is counted, not refused.
#[test]
fn a_wait_pool_refused_at_any_build_step_leaves_nothing_behind() {
    let nodes = u64::from(WAIT_POOL_DEFAULT.entries_max());
    let slot = inf_runtime::WaitNodes::NODE_SLOT_BYTES;
    let chunk = chunk_slots_for(slot).expect("a node's chunk");
    let slab_chunks = nodes.div_ceil(chunk as u64);
    let directory_children = inf_foundation::limits::MAP_DIRECTORY_CHILDREN_MAX;
    assert!(slab_chunks > 1, "the slab is past one chunk");
    assert!(slab_chunks <= directory_children as u64, "under one directory");
    let slab_directory = directory_children * size_of::<Option<Box<u8>>>();
    let slab_bytes = slab_chunks * (chunk * slot) as u64 + slab_directory as u64;
    let entry = inf_runtime::WatermarkGate::ENTRY_BYTES;
    let array_chunks = nodes.div_ceil((MAP_BACKING_ALLOCATION_BYTES_MAX / entry) as u64);
    assert_eq!(array_chunks, 2, "the array is two chunks under one directory");
    let array_bytes = nodes * entry as u64 + array_chunks * size_of::<Box<[u8]>>() as u64;
    let steps = slab_chunks + 1 + array_chunks + 1;
    let (census, _serve_mark) = CapCensus::new(CellId(3));
    let (requests, requested) = (ALLOC.thread_allocations(), ALLOC.thread_bytes());
    let built = inf_runtime::WaitNodes::build(WAIT_POOL_DEFAULT, &census).expect("the pool");
    let build = (ALLOC.thread_allocations() - requests, ALLOC.thread_bytes() - requested);
    drop(built);
    assert_eq!(build.0, steps + 1, "the slab's and the array's steps, then the pool's handle");
    let mut requested_before = 0;
    for refused_at in 0..steps {
        let (requested, freed) = (ALLOC.thread_bytes(), ALLOC.thread_freed_bytes());
        let guard = ALLOC.refuse_after(refused_at).expect("no refusal armed");
        let outcome = inf_runtime::WaitNodes::build(WAIT_POOL_DEFAULT, &census).map(drop);
        assert!(guard.was_triggered(), "allocation {refused_at} reached the allocator");
        drop(guard);
        assert_eq!(outcome, Err(CapError::Alloc), "refused at allocation {refused_at}");
        let freed = ALLOC.thread_freed_bytes() - freed;
        assert_eq!(
            freed, requested_before,
            "refused at allocation {refused_at}: an allocation before the refusal was kept"
        );
        requested_before = ALLOC.thread_bytes() - requested;
    }
    assert_eq!(requested_before, slab_bytes + array_bytes, "the chain closes on the layouts");
    assert!(build.1 > requested_before, "the pool's handle is the build's one more allocation");
}
