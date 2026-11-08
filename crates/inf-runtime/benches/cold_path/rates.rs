//! The rate legs: one container operation a step, on the containers each
//! leg's terms are billed at, reported and never judged. Every rate leg and
//! the harness leg run one loop, so a leg's reading less the harness's is
//! its step's own instructions:
//!
//! - `harness`: the loop and a step that stores its key and returns, the
//!   share every rate leg carries.
//! - `map-get-0`, `-6`, `-9`, `-14`: on the capped map the fixed stores
//!   build on, a get of one live key in a map of 2^d even keys, a perfect
//!   tree of depth d (one key at 0: the root leaf).
//! - `map-admit-remove-empty`, `-6`, `-9`, `-14`: an admit of a vacant odd
//!   key between two live ones (the split at its neighbour's leaf; at
//!   `empty`, the map's first key), its publish and its remove.
//! - `slab-cycle`, `slab-get`: on a capped slab of 256 queued-request-sized
//!   slots (120 B) holding 255, a reserve, publish and take, or a get
//!   through a held handle.
//! - `hash-get-6`, `-9`, `-14`, `hash-insert-remove-6`, `-9`, `-14`: on the
//!   per-file pin table of the host before its fixed stores (std's map under
//!   the host's integer hasher, the hash of every key it holds), a probe of
//!   one of 2^d live files, or an insert and a remove of a vacant one.
//! - `deque-push-or-pop`, `vec-push-or-pop`: on that host's request queue
//!   (256 queued) and per-read waiter list, a push at even ops and a pop at
//!   odd ones, each step testing its op's parity first.

use std::collections::{HashMap, VecDeque};
use std::hint::black_box;
use std::rc::Rc;

use inf_foundation::bounded::{
    Admission, Built, Cap, CapCensus, CapFill, CappedMap, CappedSlab, Lookup, MapBuilt,
    SlabReserved, SlotId, Taken, WrapWithProof,
};
use inf_foundation::{BuildIntHasher, CellId, FileOffset};
use inf_runtime::{CompletionToken, TokenClass};

use super::cold::TierFileId;
use super::plant;

/// A rate leg: the container and operation its step drives.
#[derive(Clone, Copy)]
pub enum RateLeg {
    Harness,
    MapGet { depth: u32 },
    MapAdmitRemove { depth: u32 },
    MapAdmitRemoveEmpty,
    SlabCycle,
    SlabGet,
    HashGet { depth: u32 },
    HashInsertRemove { depth: u32 },
    DequePushOrPop,
    VecPushOrPop,
}

/// The rate legs' loop: `ops` steps over the keys `2 · (op & mask)`, with
/// `NOPS` planted per op. Every rate leg and the harness run it, so the
/// harness reads the loop's own share of each.
#[inline(always)]
fn rate_loop<const NOPS: u8>(ops: u64, mask: u64, mut step: impl FnMut(u64) -> u64) -> u64 {
    let mut accumulator = 0u64;
    for op in 0..ops {
        plant::<NOPS>();
        accumulator = accumulator.wrapping_add(step(2 * (op & mask)));
    }
    accumulator
}

/// The keys the depth-14 legs walk: the harness walks the same.
const MASK_14: u64 = (1 << 14) - 1;

/// Drives `ops` steps of `leg` with `NOPS` planted per op, then checks what
/// they did.
#[inline(never)]
pub fn drive<const NOPS: u8>(leg: RateLeg, ops: u64) -> u64 {
    match leg {
        RateLeg::Harness => drive_harness::<NOPS>(ops),
        RateLeg::MapGet { depth } => drive_map::<NOPS>(Some(depth), false, ops),
        RateLeg::MapAdmitRemove { depth } => drive_map::<NOPS>(Some(depth), true, ops),
        RateLeg::MapAdmitRemoveEmpty => drive_map::<NOPS>(None, true, ops),
        RateLeg::SlabCycle => drive_slab::<NOPS>(true, ops),
        RateLeg::SlabGet => drive_slab::<NOPS>(false, ops),
        RateLeg::HashGet { depth } => drive_hash::<NOPS>(depth, false, ops),
        RateLeg::HashInsertRemove { depth } => drive_hash::<NOPS>(depth, true, ops),
        RateLeg::DequePushOrPop => drive_deque::<NOPS>(ops),
        RateLeg::VecPushOrPop => drive_vec::<NOPS>(ops),
    }
}

#[inline(never)]
fn step_harness(last: &mut u64, key: u64) -> u64 {
    *last = key;
    1
}

fn drive_harness<const NOPS: u8>(ops: u64) -> u64 {
    let mut last = 0u64;
    let last_ref = black_box(&mut last);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |key| step_harness(last_ref, key));
    // Engagement, after the counted loop: every op ran its step.
    assert_eq!(accumulator, ops, "every op ran its step");
    accumulator.wrapping_add(last)
}

/// The map legs' caps: their live keys and the one the admit leg adds.
const MAP_0: Cap = Cap::entries::<2>("bench-map-0", CapFill::Serving);
const MAP_6: Cap = Cap::entries::<65>("bench-map-6", CapFill::Serving);
const MAP_9: Cap = Cap::entries::<513>("bench-map-9", CapFill::Serving);
const MAP_14: Cap = Cap::entries::<16_385>("bench-map-14", CapFill::Serving);

/// A map of the 2^`depth` even keys below 2^(`depth` + 1), a perfect tree of
/// depth `depth`, or an empty map at `None`, built by steps before the loop.
fn even_keys(depth: Option<u32>) -> (CappedMap<u64, u32>, Rc<CapCensus>) {
    let cap = match depth {
        None | Some(0) => MAP_0,
        Some(6) => MAP_6,
        Some(9) => MAP_9,
        Some(14) => MAP_14,
        Some(other) => unreachable!("no map leg at depth {other}"),
    };
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = CappedMap::<u64, u32>::builder(cap, &census).expect("registered");
    let mut map = loop {
        match builder.step() {
            MapBuilt::More(next) => builder = next,
            MapBuilt::Live(map) => break map,
            MapBuilt::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let live = depth.map_or(0, |depth| 1u64 << depth);
    for key in (0..2 * live).step_by(2) {
        let Admission::Vacant(slot) = map.admit(key) else { panic!("a fresh key") };
        slot.publish(1);
    }
    (map, census)
}

#[inline(never)]
fn step_map_get(map: &mut CappedMap<u64, u32>, key: u64) -> u64 {
    map.get(key).copied().map_or(0, u64::from)
}

#[inline(never)]
fn step_map_admit_remove(map: &mut CappedMap<u64, u32>, key: u64) -> u64 {
    let Admission::Vacant(slot) = map.admit(key) else { panic!("a vacant key") };
    slot.publish(2);
    map.remove(key).map_or(0, u64::from)
}

/// A get of a live even key, or an admit, publish and remove of the odd key
/// beside one (the map's first key on the empty map), the keys walked in
/// turn.
fn drive_map<const NOPS: u8>(depth: Option<u32>, admit: bool, ops: u64) -> u64 {
    let (mut map, _census) = even_keys(depth);
    let live = map.len();
    let mask = depth.map_or(0, |depth| (1u64 << depth) - 1);
    let map = black_box(&mut map);
    let accumulator = if admit {
        rate_loop::<NOPS>(ops, mask, |key| step_map_admit_remove(map, key + 1))
    } else {
        rate_loop::<NOPS>(ops, mask, |key| step_map_get(map, key))
    };
    // Engagement, after the counted loop: every op found or placed its key
    // and the map holds its live keys alone.
    let per_op = if admit { 2 } else { 1 };
    assert_eq!(accumulator, ops * per_op, "every op reached its key");
    assert_eq!(map.len(), live, "the map holds its live keys");
    accumulator
}

/// A queued request's slot: 120 B, the fixed stores' intent slot's size.
type IntentSlot = [u64; 15];
type IntentSlab = CappedSlab<IntentSlot, u32, WrapWithProof>;
const SLAB_256: Cap = Cap::entries::<256>("bench-slab-256", CapFill::Serving);

/// A slab of 256 slots holding 255, built by steps before the loop, and the
/// handle of its first occupant.
fn intent_slab() -> (IntentSlab, SlotId<u32>, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = IntentSlab::builder(SLAB_256, &census).expect("registered");
    let mut slab = loop {
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let mut first = None;
    for value in 0..255u64 {
        let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
        let id = slot.publish([value; 15]);
        first.get_or_insert(id);
    }
    (slab, first.expect("an occupant"), census)
}

#[inline(never)]
fn step_slab_cycle(slab: &mut IntentSlab, key: u64) -> u64 {
    let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
    let id = slot.publish([key; 15]);
    match slab.take(id) {
        Taken::Value(value) => u64::from(value[14] == key),
        Taken::Stale | Taken::Vacant => panic!("the slot just published"),
    }
}

#[inline(never)]
fn step_slab_get(slab: &mut IntentSlab, id: SlotId<u32>) -> u64 {
    match slab.get(id) {
        Lookup::Live(value) => value[14] + 1,
        Lookup::Stale | Lookup::Vacant => panic!("a live occupant"),
    }
}

/// A reserve, publish and take of the slab's one free slot, or a get of its
/// first occupant.
fn drive_slab<const NOPS: u8>(cycle: bool, ops: u64) -> u64 {
    let (mut slab, first, _census) = intent_slab();
    let slab = black_box(&mut slab);
    let accumulator = if cycle {
        rate_loop::<NOPS>(ops, MASK_14, |key| step_slab_cycle(slab, key))
    } else {
        rate_loop::<NOPS>(ops, MASK_14, |_| step_slab_get(slab, first))
    };
    // Engagement, after the counted loop: every op reached its slot and the
    // slab holds its occupants alone.
    assert_eq!(accumulator, ops, "every op reached its slot");
    assert_eq!(slab.len(), 255, "the slab holds its occupants");
    accumulator
}

/// The host's per-file pin table before its fixed stores.
type PinTable = HashMap<TierFileId, u32, BuildIntHasher>;

/// A table of the 2^`depth` even files below 2^(`depth` + 1).
fn even_files(depth: u32) -> PinTable {
    let mut table = PinTable::default();
    for id in (0..2u32 << depth).step_by(2) {
        table.insert(TierFileId::new(id), 1);
    }
    table
}

/// A file's id from a rate key: every key is below 2^15.
fn file_of(key: u64) -> TierFileId {
    TierFileId::new(key as u32)
}

#[inline(never)]
fn step_hash_get(table: &mut PinTable, key: u64) -> u64 {
    table.get(&file_of(key)).copied().map_or(0, u64::from)
}

#[inline(never)]
fn step_hash_insert_remove(table: &mut PinTable, key: u64) -> u64 {
    let file = file_of(key);
    if table.insert(file, 2).is_some() {
        panic!("a vacant file");
    }
    table.remove(&file).map_or(0, u64::from)
}

/// A probe of a live even file, or an insert and a remove of the odd file
/// beside one, the files walked in turn.
fn drive_hash<const NOPS: u8>(depth: u32, insert: bool, ops: u64) -> u64 {
    let mut table = even_files(depth);
    let live = table.len();
    let mask = (1u64 << depth) - 1;
    let table = black_box(&mut table);
    let accumulator = if insert {
        rate_loop::<NOPS>(ops, mask, |key| step_hash_insert_remove(table, key + 1))
    } else {
        rate_loop::<NOPS>(ops, mask, |key| step_hash_get(table, key))
    };
    // Engagement, after the counted loop: every op found or placed its file
    // and the table holds its live files alone.
    let per_op = if insert { 2 } else { 1 };
    assert_eq!(accumulator, ops * per_op, "every op reached its file");
    assert_eq!(table.len(), live, "the table holds its live files");
    accumulator
}

/// The host's queued request before its fixed stores, field for field, so
/// a push moves its bytes; the legs read its length.
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "the fields give the record its size; a pop reads its length")]
struct QueuedIntent {
    token: CompletionToken,
    fd: i32,
    file: TierFileId,
    offset: FileOffset,
    len: u32,
    enqueued_us: u64,
}

/// The host's per-read waiter before its fixed stores, field for field.
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "the fields give the record its size; a pop reads its length")]
struct QueuedWaiter {
    token: CompletionToken,
    skip: u32,
    len: u32,
    enqueued_us: u64,
}

/// The queue the deque leg keeps, as the host's class queue under a steady
/// load: 256 queued.
const QUEUED: usize = 256;
/// The waiter list the vec leg keeps: one cohort's members less one.
const WAITERS: usize = 30;
const RECORD_LEN: u32 = 64;

#[inline(never)]
fn step_deque_push_or_pop(queue: &mut VecDeque<QueuedIntent>, key: u64) -> u64 {
    if key & 2 == 0 {
        let token = CompletionToken::new(TokenClass::TierRead, 1, 1);
        let offset = FileOffset::from_u32_bytes(0);
        let file = file_of(key);
        let record =
            QueuedIntent { token, fd: -1, file, offset, len: RECORD_LEN, enqueued_us: key };
        queue.push_back(record);
        0
    } else {
        queue.pop_front().map_or(0, |head| u64::from(head.len))
    }
}

#[inline(never)]
fn step_vec_push_or_pop(waiters: &mut Vec<QueuedWaiter>, key: u64) -> u64 {
    if key & 2 == 0 {
        let token = CompletionToken::new(TokenClass::TierRead, 1, 1);
        waiters.push(QueuedWaiter { token, skip: 0, len: RECORD_LEN, enqueued_us: key });
        0
    } else {
        waiters.pop().map_or(0, |waiter| u64::from(waiter.len))
    }
}

/// A push at even ops and a pop at odd ones, so the queue alternates
/// between 256 and 257 queued.
fn drive_deque<const NOPS: u8>(ops: u64) -> u64 {
    let token = CompletionToken::new(TokenClass::TierRead, 1, 1);
    let offset = FileOffset::from_u32_bytes(0);
    let mut queue = VecDeque::with_capacity(QUEUED + 1);
    for index in 0..QUEUED {
        let file = TierFileId::new(u32::try_from(index).expect("a queue position"));
        let record = QueuedIntent { token, fd: -1, file, offset, len: RECORD_LEN, enqueued_us: 0 };
        queue.push_back(record);
    }
    let capacity = queue.capacity();
    let queue = black_box(&mut queue);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |key| step_deque_push_or_pop(queue, key));
    // Engagement, after the counted loop: every pop read a record and the
    // queue holds its steady load, within its reserved capacity.
    assert_eq!(accumulator, ops / 2 * u64::from(RECORD_LEN), "every pop read a record");
    assert_eq!(queue.len(), QUEUED + usize::from(ops % 2 == 1), "the queue's steady load");
    assert_eq!(queue.capacity(), capacity, "no push grew the queue");
    accumulator
}

/// The same on a waiter list of 30, between 30 and 31.
fn drive_vec<const NOPS: u8>(ops: u64) -> u64 {
    let token = CompletionToken::new(TokenClass::TierRead, 1, 1);
    let mut waiters = Vec::with_capacity(WAITERS + 1);
    for _ in 0..WAITERS {
        waiters.push(QueuedWaiter { token, skip: 0, len: RECORD_LEN, enqueued_us: 0 });
    }
    let capacity = waiters.capacity();
    let waiters = black_box(&mut waiters);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |key| step_vec_push_or_pop(waiters, key));
    // Engagement, after the counted loop: every pop read a record and the
    // list holds its steady length, within its reserved capacity.
    assert_eq!(accumulator, ops / 2 * u64::from(RECORD_LEN), "every pop read a record");
    assert_eq!(waiters.len(), WAITERS + usize::from(ops % 2 == 1), "the list's steady length");
    assert_eq!(waiters.capacity(), capacity, "no push grew the list");
    accumulator
}
