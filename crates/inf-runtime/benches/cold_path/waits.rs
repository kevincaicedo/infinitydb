//! The wait legs: the fixed stores the gates and the wait lists take, one
//! operation or one round a step, beside the growable representations they
//! replace; reported and never judged. They run the rate legs' loop
//! (`rates::rate_loop`), so a leg's reading less `harness`'s is its step's
//! own instructions, and a round leg's is read a wait: that over its waits.
//!
//! - `slab-holder-get`, `-get-mut`, `-cycle`: on a slab of the fabric gate's
//!   holders' generation and policy (a `u32` that wraps) and a 56 B holder,
//!   256 slots holding 255: a get or a `get_mut` of its first occupant, or a
//!   reserve, publish and take of its free slot (a holder returns its value
//!   at its take); and their `-2` twins on 16,448 slots holding 16,447, the
//!   fewest past one directory level, so every op walks a second level.
//! - `map-admit-remove-counter-2496`, `-159744`: on the capped map of `u64`
//!   keys, that many live keys from one counter: the next counter key
//!   admitted and the oldest removed, so the window keeps its count and each
//!   op splits and unlinks at the tree's depth; both stores one directory
//!   level at 2,496, two at 159,744.
//! - `hash-gate-get-10`, `-16`, `hash-gate-insert-remove-10`, `-16`: std's
//!   map at the fabric gate's widths (a `u64` key and a 48 B slot) under the
//!   integer hasher, 2^d even keys: a probe of a live one, or an insert and
//!   a remove of the odd key beside one.
//! - `heap-mark-rising-962`, `-2825`, `-16384` and `heap-mark-falling-*`: a
//!   round on the indexed heap of 24 B wait entries (an LSN and a node's
//!   handle), its hook empty: that many waits pushed in rising or falling
//!   LSN order, then two advances, to the middle and to the last, each
//!   popping every entry at or below it.
//! - `btree-mark-rising-*`, `btree-mark-falling-*`: the same round on the
//!   watermark gate before its fixed stores, each wait polled once before
//!   the advances and once after.
//! - `list-wait-wake-unit`, `-key`: the wait list before its fixed stores,
//!   one wait parked on its key, unit or `u32`: a second wait and its poll,
//!   `wake_one`, the parked one's poll and drop, the second parked in its
//!   place.
//! - `mark-wait-for`: the watermark gate before its fixed stores, one far
//!   wait parked: a wait at the next LSN and its poll, an advance to it, and
//!   the poll that reads it ready.
//!
//! The gates before their fixed stores are copied below, their wait paths
//! as they stood, so these legs read the same code after the runtime's
//! gates change.
//!
//! Plants: the single-op legs' `-plus-one`, one `nop` at the loop's top; the
//! heap's rounds' `-plus-wait`, one `nop` a wait at the top of the push
//! loop's body; the B-tree's rounds and the list and watermark legs'
//! `-plus-hundred`, 100 `nop`s a wait. Those representations allocate a
//! waiter on every wait, and the allocator's path moves with the plant's
//! code by a few instructions, so their plant is read at a hundred.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::hint::black_box;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::bounded::{
    Admission, Built, Cap, CapCensus, CapFill, CappedMap, CappedSlab, Lookup, LookupMut, MapBuilt,
    SlabReserved, SlotId, Taken, WrapWithProof,
};
use inf_foundation::limits::{MAP_CHUNK_SLOTS_MAX, directory_levels};
use inf_foundation::{BuildIntHasher, CellId};
use inf_runtime::heap::{ArrayBuilt, FixedArray, HeapEntry, IndexedHeap};

use super::plant;
use super::rates::{MASK_14, SlabLevels, rate_loop};

/// A wait leg: the store and operation its step drives.
#[derive(Clone, Copy)]
pub enum WaitLeg {
    Holder { op: HolderOp, levels: SlabLevels },
    RouteCounter { routes: Routes },
    HashGate { depth: u32, insert: bool },
    HeapMark { order: Order, waits: Waits },
    BtreeMark { order: Order, waits: Waits },
    ListWaitWake { keyed: bool },
    MarkWaitFor,
}

/// The operation a holder leg drives.
#[derive(Clone, Copy)]
pub enum HolderOp {
    Get,
    GetMut,
    Cycle,
}

/// A counter leg's live routes: the keyed gate's default and its ceiling.
#[derive(Clone, Copy)]
pub enum Routes {
    Default,
    Ceiling,
}

/// The LSN order a round pushes its waits in.
#[derive(Clone, Copy)]
pub enum Order {
    Rising,
    Falling,
}

/// A round's waits: the wait nodes at one cell, at four, and the most.
#[derive(Clone, Copy)]
pub enum Waits {
    OneCell,
    FourCells,
    Most,
}

impl Waits {
    fn count(self) -> u64 {
        match self {
            Waits::OneCell => 962,
            Waits::FourCells => 2_825,
            Waits::Most => 16_384,
        }
    }
}

/// Drives `ops` steps of `leg` with `nops` planted: per op on the single-op
/// legs, per wait on the round legs. A plant the leg does not take panics:
/// the table of legs names each leg's plant once.
#[inline(never)]
pub fn drive(leg: WaitLeg, nops: u8, ops: u64) -> u64 {
    match (leg, nops) {
        (WaitLeg::Holder { op, levels }, 0) => drive_holder::<0>(op, levels, ops),
        (WaitLeg::Holder { op, levels }, 1) => drive_holder::<1>(op, levels, ops),
        (WaitLeg::RouteCounter { routes }, 0) => drive_route_counter::<0>(routes, ops),
        (WaitLeg::RouteCounter { routes }, 1) => drive_route_counter::<1>(routes, ops),
        (WaitLeg::HashGate { depth, insert }, 0) => drive_hash_gate::<0>(depth, insert, ops),
        (WaitLeg::HashGate { depth, insert }, 1) => drive_hash_gate::<1>(depth, insert, ops),
        (WaitLeg::HeapMark { order, waits }, 0) => drive_heap_mark::<0>(order, waits, ops),
        (WaitLeg::HeapMark { order, waits }, 1) => drive_heap_mark::<1>(order, waits, ops),
        (WaitLeg::BtreeMark { order, waits }, 0) => drive_btree_mark::<0>(order, waits, ops),
        (WaitLeg::BtreeMark { order, waits }, 100) => drive_btree_mark::<100>(order, waits, ops),
        (WaitLeg::ListWaitWake { keyed: false }, 0) => drive_list::<0, ()>((), ops),
        (WaitLeg::ListWaitWake { keyed: false }, 100) => drive_list::<100, ()>((), ops),
        (WaitLeg::ListWaitWake { keyed: true }, 0) => drive_list::<0, u32>(7, ops),
        (WaitLeg::ListWaitWake { keyed: true }, 100) => drive_list::<100, u32>(7, ops),
        (WaitLeg::MarkWaitFor, 0) => drive_mark_wait_for::<0>(ops),
        (WaitLeg::MarkWaitFor, 100) => drive_mark_wait_for::<100>(ops),
        (_, other) => panic!("no wait leg plants {other} nops"),
    }
}

// ---- the fabric gate's holder slab --------------------------------------------------------

/// A holder's slot: 56 B, the gate's holder of a reply.
type HolderSlot = [u64; 7];
type HolderSlab = CappedSlab<HolderSlot, u32, WrapWithProof>;
const HOLDERS_256: Cap = Cap::entries::<256>("bench-holders-256", CapFill::Serving);
/// 257 chunks of 64: the fewest slots past one directory level's.
const HOLDERS_16448: Cap = Cap::entries::<16_448>("bench-holders-16448", CapFill::Serving);

// The store's own derivation of its root's level: one directory over the
// chunks for the one-level legs, two for the `-2` legs, so each op of a `-2`
// leg walks one link more than its twin's; another cap does not compile.
const _: () = assert!(directory_levels(HOLDERS_256.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 1);
const _: () =
    assert!(directory_levels(HOLDERS_16448.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 2);

/// A holder slab of `cap` holding all but one slot, built by steps before the
/// loop, and the handle of its first occupant.
fn holder_slab(cap: Cap) -> (HolderSlab, SlotId<u32>, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = HolderSlab::builder(cap, &census).expect("registered");
    let mut slab = loop {
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let mut first = None;
    for value in 0..u64::from(cap.entries_max() - 1) {
        let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
        let id = slot.publish([value; 7]);
        first.get_or_insert(id);
    }
    (slab, first.expect("an occupant"), census)
}

#[inline(never)]
fn step_holder_get(slab: &mut HolderSlab, id: SlotId<u32>) -> u64 {
    match slab.get(id) {
        Lookup::Live(value) => value[6] + 1,
        Lookup::Stale | Lookup::Vacant => panic!("a live occupant"),
    }
}

#[inline(never)]
fn step_holder_get_mut(slab: &mut HolderSlab, id: SlotId<u32>) -> u64 {
    match slab.get_mut(id) {
        LookupMut::Live(value) => value[6] + 1,
        LookupMut::Stale | LookupMut::Vacant => panic!("a live occupant"),
    }
}

#[inline(never)]
fn step_holder_cycle(slab: &mut HolderSlab, key: u64) -> u64 {
    let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
    let id = slot.publish([key; 7]);
    match slab.take(id) {
        Taken::Value(value) => u64::from(value[6] == key),
        Taken::Stale | Taken::Vacant => panic!("the slot just published"),
    }
}

/// A get or a `get_mut` of the holder slab's first occupant, or a reserve,
/// publish and take of its one free slot, at one or two levels.
fn drive_holder<const NOPS: u8>(op: HolderOp, levels: SlabLevels, ops: u64) -> u64 {
    let cap = match levels {
        SlabLevels::One => HOLDERS_256,
        SlabLevels::Two => HOLDERS_16448,
    };
    let (mut slab, first, _census) = holder_slab(cap);
    let slab = black_box(&mut slab);
    let accumulator = match op {
        HolderOp::Get => rate_loop::<NOPS>(ops, MASK_14, |_| step_holder_get(slab, first)),
        HolderOp::GetMut => rate_loop::<NOPS>(ops, MASK_14, |_| step_holder_get_mut(slab, first)),
        HolderOp::Cycle => rate_loop::<NOPS>(ops, MASK_14, |key| step_holder_cycle(slab, key)),
    };
    // Engagement, after the counted loop: every op reached its slot and the
    // slab holds its occupants alone.
    assert_eq!(accumulator, ops, "every op reached its slot");
    assert_eq!(slab.len(), cap.entries_max() - 1, "the slab holds its occupants");
    accumulator
}

// ---- the keyed gate's route map, from one counter ---------------------------------------------

/// The live routes and the one an op admits before it removes the oldest.
const ROUTES_2496: Cap = Cap::entries::<2_497>("bench-routes-2496", CapFill::Serving);
const ROUTES_159744: Cap = Cap::entries::<159_745>("bench-routes-159744", CapFill::Serving);

// The map's leaves take its cap and its branches one fewer: both stores at
// one directory level at the default, two at the ceiling, by the store's own
// derivation of its root's level.
const _: () = assert!(directory_levels(ROUTES_2496.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 1);
const _: () =
    assert!(directory_levels(ROUTES_2496.entries_max() as u64 - 1, MAP_CHUNK_SLOTS_MAX) == 1);
const _: () =
    assert!(directory_levels(ROUTES_159744.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 2);
const _: () =
    assert!(directory_levels(ROUTES_159744.entries_max() as u64 - 1, MAP_CHUNK_SLOTS_MAX) == 2);

/// A window of live routes from one counter: `[oldest, oldest + live)`.
struct RouteWindow {
    map: CappedMap<u64, u32>,
    oldest: u64,
    live: u64,
}

/// The window of `cap` less one live keys, `0..cap - 1`, built by steps
/// before the loop.
fn route_window(cap: Cap) -> (RouteWindow, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = CappedMap::<u64, u32>::builder(cap, &census).expect("registered");
    let mut map = loop {
        match builder.step() {
            MapBuilt::More(next) => builder = next,
            MapBuilt::Live(map) => break map,
            MapBuilt::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let live = u64::from(cap.entries_max() - 1);
    for key in 0..live {
        let Admission::Vacant(slot) = map.admit(key) else { panic!("a fresh key") };
        slot.publish(1);
    }
    (RouteWindow { map, oldest: 0, live }, census)
}

#[inline(never)]
fn step_route_counter(window: &mut RouteWindow) -> u64 {
    let next = window.oldest + window.live;
    let Admission::Vacant(slot) = window.map.admit(next) else { panic!("the next key is vacant") };
    slot.publish(1);
    let removed = window.map.remove(window.oldest).map_or(0, u64::from);
    window.oldest += 1;
    1 + removed
}

/// The next counter key admitted and the oldest removed, each op.
fn drive_route_counter<const NOPS: u8>(routes: Routes, ops: u64) -> u64 {
    let cap = match routes {
        Routes::Default => ROUTES_2496,
        Routes::Ceiling => ROUTES_159744,
    };
    let (mut window, _census) = route_window(cap);
    let window = black_box(&mut window);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_route_counter(window));
    // Engagement, after the counted loop: every op placed its key and
    // removed the oldest, and the map holds the window from the counter.
    let (oldest, live) = (window.oldest, window.live);
    assert_eq!(accumulator, 2 * ops, "every op admitted and removed a key");
    assert_eq!(u64::from(window.map.len()), live, "the map holds its window");
    assert!(window.map.get(oldest).is_some(), "the window's oldest key is live");
    assert!(window.map.get(oldest + live - 1).is_some(), "the window's newest key is live");
    assert!(oldest == 0 || window.map.get(oldest - 1).is_none(), "the oldest left is gone");
    accumulator
}

// ---- the fabric gate's table before its fixed stores, at its widths --------------------------

/// The gate's table: a `u64` token to a 48 B slot (its state and a reply).
type GateTable = HashMap<u64, [u64; 6], BuildIntHasher>;

/// A table of the 2^`depth` even keys below 2^(`depth` + 1).
fn even_tokens(depth: u32) -> GateTable {
    let mut table = GateTable::default();
    for key in (0..2u64 << depth).step_by(2) {
        table.insert(key, [1; 6]);
    }
    table
}

#[inline(never)]
fn step_hash_gate_get(table: &mut GateTable, key: u64) -> u64 {
    table.get(&key).map_or(0, |slot| slot[5])
}

#[inline(never)]
fn step_hash_gate_insert_remove(table: &mut GateTable, key: u64) -> u64 {
    if table.insert(key, [2; 6]).is_some() {
        panic!("a vacant token");
    }
    table.remove(&key).map_or(0, |slot| slot[5])
}

/// A probe of a live even token, or an insert and a remove of the odd token
/// beside one, the tokens walked in turn.
fn drive_hash_gate<const NOPS: u8>(depth: u32, insert: bool, ops: u64) -> u64 {
    let mut table = even_tokens(depth);
    let live = table.len();
    let mask = (1u64 << depth) - 1;
    let table = black_box(&mut table);
    let accumulator = if insert {
        rate_loop::<NOPS>(ops, mask, |key| step_hash_gate_insert_remove(table, key + 1))
    } else {
        rate_loop::<NOPS>(ops, mask, |key| step_hash_gate_get(table, key))
    };
    // Engagement, after the counted loop: every op found or placed its token
    // and the table holds its live tokens alone.
    let per_op = if insert { 2 } else { 1 };
    assert_eq!(accumulator, ops * per_op, "every op reached its token");
    assert_eq!(table.len(), live, "the table holds its live tokens");
    accumulator
}

// ---- the wait nodes' heap ---------------------------------------------------------------------

/// A wait's heap entry: its LSN and its node's handle, 24 B.
#[derive(Clone, Copy)]
struct WaitEntry {
    lsn: u64,
    #[allow(dead_code, reason = "the node's handle gives the entry its size; no hook reads it")]
    node: SlotId<u32>,
}

impl HeapEntry for WaitEntry {
    type Key = u64;

    fn key(&self) -> u64 {
        self.lsn
    }
}

const _: () = assert!(size_of::<WaitEntry>() == 24, "a wait entry is 24 B");

/// The wait node slab's slot, whose handle every entry carries.
type NodeSlab = CappedSlab<[u64; 6], u32, WrapWithProof>;
const NODES_1: Cap = Cap::entries::<1>("bench-nodes-1", CapFill::Serving);

/// The heap's rounds: the heap, the handle each entry carries, the LSN the
/// next round starts above, its waits and their order.
struct HeapRounds {
    heap: IndexedHeap<WaitEntry>,
    node: SlotId<u32>,
    base: u64,
    waits: u64,
    order: Order,
}

/// A heap of `waits` entries' capacity, built by steps before the loop, and
/// one node's handle.
fn heap_rounds(order: Order, waits: Waits) -> (HeapRounds, NodeSlab, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = NodeSlab::builder(NODES_1, &census).expect("registered");
    let mut nodes = loop {
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let SlabReserved::Slot(slot) = nodes.reserve() else { panic!("a free slot") };
    let node = slot.publish([0; 6]);
    let count = waits.count();
    let fill = WaitEntry { lsn: 0, node };
    let capacity = u32::try_from(count).expect("a round's waits fit a u32");
    let mut array = FixedArray::builder(capacity, fill).expect("a heap's layout");
    let entries = loop {
        match array.step() {
            ArrayBuilt::More(next) => array = next,
            ArrayBuilt::Done(entries) => break entries,
            ArrayBuilt::Refused(refused) => panic!("{refused}"),
        }
    };
    let rounds = HeapRounds { heap: IndexedHeap::new(entries), node, base: 0, waits: count, order };
    (rounds, nodes, census)
}

/// One round: the waits pushed in the round's order, `WAIT_NOPS` planted
/// each, then the advances to the middle and to the last, each popping every
/// entry at or below it; `moved` hears every entry a sift places. Returns
/// the waits woken.
#[inline(always)]
fn heap_round<const WAIT_NOPS: u8>(
    rounds: &mut HeapRounds,
    mut moved: impl FnMut(WaitEntry, u32),
) -> u64 {
    let (base, waits, node) = (rounds.base, rounds.waits, rounds.node);
    for index in 0..waits {
        plant::<WAIT_NOPS>();
        let lsn = match rounds.order {
            Order::Rising => base + 1 + index,
            Order::Falling => base + waits - index,
        };
        rounds.heap.push(WaitEntry { lsn, node }, &mut moved).expect("room for every wait");
    }
    let mut woken = 0;
    for to in [base + waits / 2, base + waits] {
        while rounds.heap.peek().is_some_and(|entry| entry.lsn <= to) {
            woken += u64::from(rounds.heap.pop(&mut moved).is_some());
        }
    }
    rounds.base = base + waits;
    woken
}

#[inline(never)]
fn step_heap_mark<const WAIT_NOPS: u8>(rounds: &mut HeapRounds) -> u64 {
    heap_round::<WAIT_NOPS>(rounds, |_, _| {})
}

/// A round's moves: the entries its sifts place, heard by a counting hook.
fn heap_round_moves(rounds: &mut HeapRounds) -> u64 {
    let mut moves = 0u64;
    let woken = heap_round::<0>(rounds, |_, _| moves += 1);
    assert_eq!(woken, rounds.waits, "a counted round woke every wait");
    moves
}

/// One round a step, `WAIT_NOPS` planted a wait.
fn drive_heap_mark<const WAIT_NOPS: u8>(order: Order, waits: Waits, ops: u64) -> u64 {
    let (mut rounds, _nodes, _census) = heap_rounds(order, waits);
    // The dry pass: a round with a counting hook, before the counted loop.
    let dry_moves = heap_round_moves(&mut rounds);
    let rounds = black_box(&mut rounds);
    let accumulator = rate_loop::<0>(ops, MASK_14, |_| step_heap_mark::<WAIT_NOPS>(rounds));
    // Engagement, after the counted loop: every wait woken, the heap empty,
    // and a round from here places the entries the dry pass placed.
    assert_eq!(accumulator, ops * rounds.waits, "every wait woken");
    assert!(rounds.heap.is_empty(), "every entry popped");
    assert_eq!(heap_round_moves(rounds), dry_moves, "a round's moves are its dry pass's");
    accumulator
}

// ---- the watermark gate before its fixed stores: rounds and one wait --------------------------

/// The B-tree's rounds: the gate, the LSN the next round starts above, its
/// waits and their order, and the round's futures, reserved before the loop.
struct BtreeRounds {
    gate: WatermarkGate,
    base: u64,
    waits: u64,
    order: Order,
    pending: Vec<WatermarkWait>,
}

#[inline(never)]
fn step_btree_mark<const WAIT_NOPS: u8>(rounds: &mut BtreeRounds) -> u64 {
    let mut cx = Context::from_waker(Waker::noop());
    let (base, waits) = (rounds.base, rounds.waits);
    let mut woken = 0u64;
    for index in 0..waits {
        plant::<WAIT_NOPS>();
        let lsn = match rounds.order {
            Order::Rising => base + 1 + index,
            Order::Falling => base + waits - index,
        };
        let mut wait = rounds.gate.waiter(lsn);
        // Its first poll stores its waker; a wait ready here would not be
        // counted by an advance, so the round's count would fall short.
        let _ = Pin::new(&mut wait).poll(&mut cx);
        rounds.pending.push(wait);
    }
    woken += rounds.gate.advance(base + waits / 2) as u64;
    woken += rounds.gate.advance(base + waits) as u64;
    for mut wait in rounds.pending.drain(..) {
        woken += u64::from(Pin::new(&mut wait).poll(&mut cx).is_ready());
    }
    rounds.base = base + waits;
    woken
}

/// One round a step, `WAIT_NOPS` planted a wait.
fn drive_btree_mark<const WAIT_NOPS: u8>(order: Order, waits: Waits, ops: u64) -> u64 {
    let count = waits.count();
    let pending = Vec::with_capacity(usize::try_from(count).expect("a round's waits"));
    let mut rounds =
        BtreeRounds { gate: WatermarkGate::new(), base: 0, waits: count, order, pending };
    let rounds = black_box(&mut rounds);
    let accumulator = rate_loop::<0>(ops, MASK_14, |_| step_btree_mark::<WAIT_NOPS>(rounds));
    // Engagement, after the counted loop: every wait woken by an advance (so
    // none was ready at its first poll) and read ready by its second; the
    // gate holds no waiter and its watermark is the last round's.
    assert_eq!(accumulator, 2 * ops * rounds.waits, "every wait woken and read ready");
    assert_eq!(rounds.gate.waiting(), 0, "no waiter left");
    assert_eq!(rounds.gate.watermark(), rounds.base, "the watermark is the last round's");
    accumulator
}

/// The watermark's one wait: the gate, the LSN of the last wait, and a far
/// wait parked throughout.
struct MarkOne {
    gate: WatermarkGate,
    lsn: u64,
    _far: WatermarkWait,
}

#[inline(never)]
fn step_mark_wait_for(mark: &mut MarkOne) -> u64 {
    let mut cx = Context::from_waker(Waker::noop());
    mark.lsn += 1;
    let mut wait = mark.gate.waiter(mark.lsn);
    let pending = Pin::new(&mut wait).poll(&mut cx).is_pending();
    let woken = mark.gate.advance(mark.lsn) as u64;
    let ready = Pin::new(&mut wait).poll(&mut cx).is_ready();
    drop(wait);
    u64::from(pending) + woken + u64::from(ready)
}

/// A wait for the next LSN a step, a far wait parked.
fn drive_mark_wait_for<const NOPS: u8>(ops: u64) -> u64 {
    let gate = WatermarkGate::new();
    let mut far = gate.waiter(u64::MAX / 2);
    let parked = Pin::new(&mut far).poll(&mut Context::from_waker(Waker::noop())).is_pending();
    assert!(parked, "the far wait parks");
    let mut mark = MarkOne { gate, lsn: 0, _far: far };
    let mark = black_box(&mut mark);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_mark_wait_for(mark));
    // Engagement, after the counted loop: each op's wait parked, was woken
    // and read ready; the far wait alone is left.
    assert_eq!(accumulator, 3 * ops, "every wait parked, woken and read ready");
    assert_eq!(mark.gate.waiting(), 1, "the far wait alone is left");
    assert_eq!(mark.gate.watermark(), ops, "the watermark is the last wait's");
    accumulator
}

// ---- the wait list before its fixed stores ----------------------------------------------------

/// The list's one key, a wait parked on it throughout.
struct ListOne<K: Eq + Hash + Copy> {
    list: WaitList<K>,
    key: K,
    parked: Option<ListWait<K>>,
}

#[inline(never)]
fn step_list_wait_wake<K: Eq + Hash + Copy>(one: &mut ListOne<K>) -> u64 {
    let mut cx = Context::from_waker(Waker::noop());
    let mut wait = one.list.wait(one.key);
    let pending = Pin::new(&mut wait).poll(&mut cx).is_pending();
    let woken = one.list.wake_one(one.key);
    let mut parked = one.parked.replace(wait).expect("one wait parked");
    let ready = Pin::new(&mut parked).poll(&mut cx).is_ready();
    drop(parked);
    u64::from(pending) + u64::from(woken) + u64::from(ready)
}

/// A wait, a wake and the woken one's poll a step, one wait parked.
fn drive_list<const NOPS: u8, K: Eq + Hash + Copy>(key: K, ops: u64) -> u64 {
    let list = WaitList::new();
    let mut parked = list.wait(key);
    let queued = Pin::new(&mut parked).poll(&mut Context::from_waker(Waker::noop())).is_pending();
    assert!(queued, "the first wait parks");
    let mut one = ListOne { list, key, parked: Some(parked) };
    let one = black_box(&mut one);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_list_wait_wake(one));
    // Engagement, after the counted loop: each op's wait parked, the parked
    // one was woken and read ready; one wait is left parked.
    assert_eq!(accumulator, 3 * ops, "every wait parked, woken and read ready");
    assert_eq!(one.list.waiting(), 1, "one wait left parked");
    accumulator
}

// ---- the wait list and the watermark gate as they stood before their fixed stores -------------
//
// Copied from the runtime's gates, their wait, wake, advance, poll and drop
// paths unchanged, with the counts the legs' engagement reads.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WaiterState {
    Queued,
    Woken,
    Cancelled,
}

struct Waiter {
    state: Cell<WaiterState>,
    waker: RefCell<Option<Waker>>,
}

/// Key-keyed FIFO wait list: `wake_one` hands the key to the longest-waiting
/// live task.
#[allow(clippy::type_complexity, reason = "the representation as it stood, copied")]
struct WaitList<K: Eq + Hash + Copy> {
    queues: Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
}

impl<K: Eq + Hash + Copy> WaitList<K> {
    fn new() -> WaitList<K> {
        WaitList { queues: Rc::new(RefCell::new(HashMap::default())) }
    }

    fn wait(&self, key: K) -> ListWait<K> {
        let waiter =
            Rc::new(Waiter { state: Cell::new(WaiterState::Queued), waker: RefCell::new(None) });
        self.queues.borrow_mut().entry(key).or_default().push_back(Rc::clone(&waiter));
        ListWait { queues: Rc::clone(&self.queues), key, waiter, done: false }
    }

    fn wake_one(&self, key: K) -> bool {
        wake_one_in(&self.queues, key)
    }

    fn waiting(&self) -> usize {
        self.queues
            .borrow()
            .values()
            .flat_map(|q| q.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

#[allow(clippy::type_complexity, reason = "the representation as it stood, copied")]
fn wake_one_in<K: Eq + Hash + Copy>(
    queues: &Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
    key: K,
) -> bool {
    let mut map = queues.borrow_mut();
    if map.is_empty() {
        return false;
    }
    let Some(queue) = map.get_mut(&key) else { return false };
    let woken = loop {
        match queue.pop_front() {
            None => break None,
            Some(w) if w.state.get() == WaiterState::Cancelled => continue,
            Some(w) => break Some(w),
        }
    };
    if queue.is_empty() {
        map.remove(&key);
    }
    drop(map);
    match woken {
        None => false,
        Some(waiter) => {
            waiter.state.set(WaiterState::Woken);
            if let Some(w) = waiter.waker.borrow_mut().take() {
                w.wake();
            }
            true
        }
    }
}

#[allow(clippy::type_complexity, reason = "the representation as it stood, copied")]
struct ListWait<K: Eq + Hash + Copy> {
    queues: Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
    key: K,
    waiter: Rc<Waiter>,
    done: bool,
}

impl<K: Eq + Hash + Copy> Unpin for ListWait<K> {}

impl<K: Eq + Hash + Copy> Future for ListWait<K> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.waiter.state.get() {
            WaiterState::Woken => {
                this.done = true;
                Poll::Ready(())
            }
            WaiterState::Queued => {
                *this.waiter.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Cancelled => panic!("ListWait polled after cancellation"),
        }
    }
}

impl<K: Eq + Hash + Copy> Drop for ListWait<K> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        match self.waiter.state.get() {
            WaiterState::Queued => self.waiter.state.set(WaiterState::Cancelled),
            WaiterState::Woken => {
                wake_one_in(&self.queues, self.key);
            }
            WaiterState::Cancelled => {}
        }
    }
}

struct WatermarkInner {
    watermark: Cell<u64>,
    waiters: RefCell<BTreeMap<u64, Vec<Rc<Waiter>>>>,
}

/// LSN-keyed gate: `advance` wakes every task at or below the new watermark.
struct WatermarkGate {
    inner: Rc<WatermarkInner>,
}

impl WatermarkGate {
    fn new() -> WatermarkGate {
        WatermarkGate {
            inner: Rc::new(WatermarkInner {
                watermark: Cell::new(0),
                waiters: RefCell::new(BTreeMap::new()),
            }),
        }
    }

    fn watermark(&self) -> u64 {
        self.inner.watermark.get()
    }

    fn waiter(&self, lsn: u64) -> WatermarkWait {
        let waiter = Rc::new(Waiter {
            state: Cell::new(if self.inner.watermark.get() >= lsn {
                WaiterState::Woken
            } else {
                WaiterState::Queued
            }),
            waker: RefCell::new(None),
        });
        if waiter.state.get() == WaiterState::Queued {
            self.inner.waiters.borrow_mut().entry(lsn).or_default().push(Rc::clone(&waiter));
        }
        WatermarkWait { waiter, done: false }
    }

    fn advance(&self, to: u64) -> usize {
        if to <= self.inner.watermark.get() {
            return 0;
        }
        self.inner.watermark.set(to);
        let due = {
            let mut waiters = self.inner.waiters.borrow_mut();
            let still_waiting = waiters.split_off(&(to + 1));
            core::mem::replace(&mut *waiters, still_waiting)
        };
        let mut woken = 0;
        for waiter in due.into_values().flatten() {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
                woken += 1;
            }
        }
        woken
    }

    fn waiting(&self) -> usize {
        self.inner
            .waiters
            .borrow()
            .values()
            .flat_map(|v| v.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

struct WatermarkWait {
    waiter: Rc<Waiter>,
    done: bool,
}

impl Future for WatermarkWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.waiter.state.get() {
            WaiterState::Woken => {
                this.done = true;
                Poll::Ready(())
            }
            WaiterState::Queued => {
                *this.waiter.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Cancelled => unreachable!("watermark waiters are never cancelled"),
        }
    }
}

impl Drop for WatermarkWait {
    fn drop(&mut self) {
        if !self.done && self.waiter.state.get() == WaiterState::Queued {
            self.waiter.state.set(WaiterState::Cancelled);
        }
    }
}
