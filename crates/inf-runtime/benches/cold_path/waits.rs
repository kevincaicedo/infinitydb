//! The wait legs: the fixed stores the gates and the wait lists take, one
//! operation or one round a step, beside the growable representations they
//! replace; reported and never judged. All but the wake steps run the rate
//! legs' loop (`rates::rate_loop`), so a leg's reading less `harness`'s is
//! its step's own instructions, and a round leg's is read a wait: that over
//! its waits.
//!
//! - `slab-holder-get`, `-get-mut`, `-cycle`: on a slab of the fabric gate's
//!   holders' generation and policy (a `u32` that wraps) and a 56 B holder,
//!   256 slots holding 255: a get or a `get_mut` of its first occupant, or a
//!   reserve, publish and take of its free slot (a holder returns its value
//!   at its take); and their `-2` twins on 16,448 slots holding 16,447, the
//!   fewest past one directory level, so every op walks a second level.
//! - `slab-holder-cycle-named`, `-2`: the holder cycle as the fabric gate
//!   runs it: a reserve named before its publish (its handle read), the
//!   publish and the take.
//! - `slab-holder-word`, `-2`: a reply's resolve, `get_mut_by_word` of the
//!   first occupant's word at the fabric key's layout (24 index bits, the
//!   low 23 of the generation).
//! - `slab-node-get`, `-get-mut`, `-cycle-release`: the same on a slab of
//!   48 B wait nodes, 256 slots holding 255, one level: a get or a `get_mut`
//!   of its first occupant, or a reserve, publish and release of its free
//!   slot (a node's life ends at a release, which drops the occupant where it
//!   lies and returns nothing).
//! - `map-admit-remove-counter-2496`, `-159744`: on the capped map of `u64`
//!   keys, that many live keys from one counter: the next counter key
//!   admitted, then a live key drawn uniformly removed, its place in the
//!   array of live keys taken by the admitted one (`draw_and_swap`, a
//!   SplitMix64 draw of a fixed seed). Eight times the live count of such
//!   ops run before the counted loop, so the live set is the draw's steady
//!   one; both stores one directory level at 2,496, two at 159,744.
//! - `map-admit-remove-counter-draw-2496`, `-159744`: the same draw and
//!   array at each count with no map, the removed key returned in the map
//!   calls' place: the instructions a counter leg spends outside its map,
//!   which its route term subtracts.
//! - `hash-gate-get-10`, `-16`, `hash-gate-insert-remove-10`, `-16`: std's
//!   map at the fabric gate's widths (a `u64` key and a 48 B slot) under the
//!   integer hasher, 2^d even keys: a probe of a live one, or an insert and
//!   a remove of the odd key beside one.
//! - `heap-hook-rising-962`, `-2825`, `-16384` and `heap-hook-falling-*`: a
//!   round on the indexed heap of 24 B wait entries (an LSN and a node's
//!   handle) over a pool of that many live wait nodes, each entry carrying
//!   its own node's handle, with the watermark gate's hook (each placed
//!   entry's node resolved by `get_mut` and its position word stored): that
//!   many waits pushed in rising or falling LSN order, then two advances,
//!   to the middle and to the last, each popping every entry at or below it.
//! - `heap-hook-one-962`, `-2825`: one wait on the same stores, a far entry
//!   parked throughout: an entry at the next LSN pushed under it and popped,
//!   three moves with the hook.
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
//! - `advance-hook-2825`, `-16384`, `-50000`, `wake-budget-*` and
//!   `advance-btree-*`: the wake steps, one call over that many waits one
//!   fsync covers, filled before it in rising LSN order: every covered wait
//!   popped with the hook, its node marked and a waker woken; the first
//!   1,024 of them, the durable cell's budget between two LOG steps; or the
//!   B-tree gate's `advance` over all of them. Each op runs one call right
//!   after its fill and one with 64 MB written after its fill. These legs
//!   are read by a count collected inside the step's call alone (callgrind's
//!   `--toggle-collect`) and by the call's wall time: they run no rate loop,
//!   and their fills dwarf the step.
//!
//! The gates before their fixed stores are copied below, their wait paths
//! as they stood, so these legs read the same code after the runtime's
//! gates change.
//!
//! Plants: the single-op legs' `-plus-one`, one `nop` at the loop's top; the
//! heap's rounds' `-plus-wait`, one `nop` a wait after its push's `expect`,
//! a call that may unwind, so no pass runs the `nop` ahead of it (at the top
//! of the push loop's body the optimizer runs it once a round); the hook's
//! wake steps' `-plus-wait`, one `nop` after each pop's result is read; the
//! B-tree's rounds and the list and watermark legs' `-plus-hundred`, 100
//! `nop`s a wait. Those representations allocate a waiter on every wait, and
//! the allocator's path moves with the plant's code by a few instructions,
//! so their plant is read at a hundred. The B-tree's wake step has no plant:
//! its `advance` is the copy's, unchanged, and pops nothing.
//!
//! Three readings outside the count:
//!
//! - a heap leg writes its round's moves, its dry pass's (a round, or on the
//!   one-wait legs an op, with a counting hook before the counted loop, the
//!   same at every op count), as `dry-pass-moves <moves>` on stderr, and
//!   asserts every round after the loop places as many;
//! - a wake step leg writes each regime's calls, their median and their
//!   worst wall time, as `step-ns <warm|evicted> calls <n> median <ns> worst
//!   <ns>` on stderr;
//! - `--walk-depth` on a counter leg replays its seed on its map for `--ops`
//!   ops after its warm-up, in a process the count never runs, prints the
//!   mean branch depth of the keys its ops admit and remove in the live
//!   set's tree, each branch a first-difference bit of the keys under it,
//!   and then gets every key its array holds from its map. Its `-window`
//!   variant (`map-admit-remove-counter-2496-window`, `-159744-window`)
//!   replays the sliding window, the oldest key removed, whose depths are
//!   shallower than the draw's: a reading of the window is the walk depth's
//!   planted red, and the window is read by its walk depth alone.

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
    Released, SlabReserved, SlotId, SlotWord, Taken, WordLookup, WrapWithProof,
};
use inf_foundation::limits::{
    GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS, MAP_CHUNK_SLOTS_MAX, directory_levels,
};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::{BuildIntHasher, CellId};
use inf_runtime::heap::{ArrayBuilt, FixedArray, HeapEntry, IndexedHeap};

use super::plant;
use super::rates::{MASK_14, ONE_LEVEL_SLOTS, SlabLevels, rate_loop};

/// A wait leg: the store and operation its step drives.
#[derive(Clone, Copy)]
pub enum WaitLeg {
    Holder { op: HolderOp, levels: SlabLevels },
    Node { op: NodeOp },
    RouteCounter { routes: Routes, shape: RouteShape },
    HashGate { depth: u32, op: HashOp },
    HeapHook { order: Order, waits: Waits },
    HeapHookOne { waits: Waits },
    WakeStep { step: WakeStep, waits: Waits },
    BtreeMark { order: Order, waits: Waits },
    ListWaitWake { key: ListKey },
    MarkWaitFor,
}

/// The operation a holder leg drives.
#[derive(Clone, Copy)]
pub enum HolderOp {
    Get,
    GetMut,
    Cycle,
    CycleNamed,
    Word,
}

/// The operation a wait node leg drives.
#[derive(Clone, Copy)]
pub enum NodeOp {
    Get,
    GetMut,
    CycleRelease,
}

/// A counter leg's live routes: the keyed gate's default and its ceiling.
#[derive(Clone, Copy)]
pub enum Routes {
    Default,
    Ceiling,
}

/// What a counter leg does with its draw.
#[derive(Clone, Copy)]
pub enum RouteShape {
    /// The next counter key admitted, a drawn live key removed: the route
    /// term with its draw.
    Draw,
    /// The draw alone, no map: the term a counter leg subtracts.
    Twin,
    /// The sliding window, the oldest key removed: read by its walk depth
    /// alone, the walk depth's planted red.
    Window,
}

/// The operation a hashed gate table's leg drives.
#[derive(Clone, Copy)]
pub enum HashOp {
    Get,
    InsertRemove,
}

/// What a wake step leg's one call runs over the waits one fsync covers.
#[derive(Clone, Copy)]
pub enum WakeStep {
    /// Every covered wait popped with the hook, in one call.
    AdvanceHook,
    /// The first `WAKE_BUDGET` of them: one budgeted call.
    WakeBudget,
    /// HEAD's `advance` on the bench's copy of its gate.
    AdvanceBtree,
}

/// A wait list's key: a list of one unit key, or of `u32` keys.
#[derive(Clone, Copy)]
pub enum ListKey {
    Unit,
    Word,
}

/// The LSN order a round pushes its waits in.
#[derive(Clone, Copy)]
pub enum Order {
    Rising,
    Falling,
}

/// A round's waits: the wait nodes at one cell (962) and at four (2,825), the
/// most one directory level of the heap's entries holds (16,384), and the
/// storm one fsync releases on the wake step legs (50,000).
#[derive(Clone, Copy)]
pub enum Waits {
    OneCell,
    FourCells,
    OneDirectory,
    Storm,
}

const _: () = assert!(ONE_LEVEL_SLOTS == 16_384, "the heap legs' names carry 16,384");

impl Waits {
    fn count(self) -> u64 {
        match self {
            Waits::OneCell => 962,
            Waits::FourCells => 2_825,
            Waits::OneDirectory => ONE_LEVEL_SLOTS as u64,
            Waits::Storm => 50_000,
        }
    }
}

/// Drives `ops` steps of `leg` with `nops` planted: per op on the single-op
/// legs, per wait on the round legs and the hook's wake steps, each of whose
/// ops is one call in each regime. A plant the leg does not take panics,
/// and so does the window, which is read by its walk depth alone: the table
/// of legs names each leg's plant once.
#[inline(never)]
pub fn drive(leg: WaitLeg, nops: u8, ops: u64) -> u64 {
    match (leg, nops) {
        (WaitLeg::Holder { op, levels }, 0) => drive_holder::<0>(op, levels, ops),
        (WaitLeg::Holder { op, levels }, 1) => drive_holder::<1>(op, levels, ops),
        (WaitLeg::Node { op }, 0) => drive_node::<0>(op, ops),
        (WaitLeg::Node { op }, 1) => drive_node::<1>(op, ops),
        (WaitLeg::RouteCounter { routes, shape: RouteShape::Draw }, 0) => {
            drive_route_counter::<0>(routes, ops)
        }
        (WaitLeg::RouteCounter { routes, shape: RouteShape::Draw }, 1) => {
            drive_route_counter::<1>(routes, ops)
        }
        (WaitLeg::RouteCounter { routes, shape: RouteShape::Twin }, 0) => {
            drive_route_twin::<0>(routes, ops)
        }
        (WaitLeg::RouteCounter { routes, shape: RouteShape::Twin }, 1) => {
            drive_route_twin::<1>(routes, ops)
        }
        (WaitLeg::RouteCounter { shape: RouteShape::Window, .. }, _) => {
            panic!("the window is read by its walk depth alone (--walk-depth)")
        }
        (WaitLeg::HashGate { depth, op }, 0) => drive_hash_gate::<0>(depth, op, ops),
        (WaitLeg::HashGate { depth, op }, 1) => drive_hash_gate::<1>(depth, op, ops),
        (WaitLeg::HeapHook { order, waits }, 0) => drive_heap_hook::<0>(order, waits, ops),
        (WaitLeg::HeapHook { order, waits }, 1) => drive_heap_hook::<1>(order, waits, ops),
        (WaitLeg::HeapHookOne { waits }, 0) => drive_heap_hook_one::<0>(waits, ops),
        (WaitLeg::HeapHookOne { waits }, 1) => drive_heap_hook_one::<1>(waits, ops),
        (WaitLeg::WakeStep { step: WakeStep::AdvanceHook, waits }, 0) => {
            drive_wake_hook::<0>(false, waits, ops)
        }
        (WaitLeg::WakeStep { step: WakeStep::AdvanceHook, waits }, 1) => {
            drive_wake_hook::<1>(false, waits, ops)
        }
        (WaitLeg::WakeStep { step: WakeStep::WakeBudget, waits }, 0) => {
            drive_wake_hook::<0>(true, waits, ops)
        }
        (WaitLeg::WakeStep { step: WakeStep::WakeBudget, waits }, 1) => {
            drive_wake_hook::<1>(true, waits, ops)
        }
        (WaitLeg::WakeStep { step: WakeStep::AdvanceBtree, waits }, 0) => {
            drive_wake_btree(waits, ops)
        }
        (WaitLeg::BtreeMark { order, waits }, 0) => drive_btree_mark::<0>(order, waits, ops),
        (WaitLeg::BtreeMark { order, waits }, 100) => drive_btree_mark::<100>(order, waits, ops),
        (WaitLeg::ListWaitWake { key: ListKey::Unit }, 0) => drive_list::<0, ()>((), ops),
        (WaitLeg::ListWaitWake { key: ListKey::Unit }, 100) => drive_list::<100, ()>((), ops),
        (WaitLeg::ListWaitWake { key: ListKey::Word }, 0) => drive_list::<0, u32>(7, ops),
        (WaitLeg::ListWaitWake { key: ListKey::Word }, 100) => drive_list::<100, u32>(7, ops),
        (WaitLeg::MarkWaitFor, 0) => drive_mark_wait_for::<0>(ops),
        (WaitLeg::MarkWaitFor, 100) => drive_mark_wait_for::<100>(ops),
        (_, other) => panic!("no wait leg plants {other} nops"),
    }
}

// ---- the fabric gate's holder slab and the wait node slab ---------------------------------------

/// A holder's slot: 56 B, the gate's holder of a reply.
type HolderSlot = [u64; 7];
type HolderSlab = CappedSlab<HolderSlot, u32, WrapWithProof>;
const HOLDERS_256: Cap = Cap::entries::<256>("bench-holders-256", CapFill::Serving);
/// 257 chunks of 64: the fewest slots past one directory level's.
const HOLDERS_16448: Cap = Cap::entries::<16_448>("bench-holders-16448", CapFill::Serving);

/// A wait node's slot: 48 B, its list links, LSN and heap position.
type NodeSlot = [u64; 6];
type NodeSlab = CappedSlab<NodeSlot, u32, WrapWithProof>;
const NODES_256: Cap = Cap::entries::<256>("bench-nodes-256", CapFill::Serving);

// The stand-in's slot is the product pool's, byte for byte, so the node legs read the stride
// the pool builds; a node that grows or shrinks fails this build.
const _: () = assert!(
    NodeSlab::SLOT_BYTES == inf_runtime::WaitNodes::NODE_SLOT_BYTES,
    "the bench's node slot is the pool's"
);

// The store's own derivation of its root's level: one directory over the
// chunks for the one-level legs, two for the `-2` legs, so each op of a `-2`
// leg walks one link more than its twin's; another cap does not compile.
const _: () = assert!(directory_levels(HOLDERS_256.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 1);
const _: () =
    assert!(directory_levels(HOLDERS_16448.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 2);
const _: () = assert!(directory_levels(NODES_256.entries_max() as u64, MAP_CHUNK_SLOTS_MAX) == 1);

/// A wrapping `u32` slab of `cap` holding all but one slot, each occupant
/// `fill` of its ordinal, built by steps before the loop, and the handle of
/// its first occupant.
fn occupied<T>(
    cap: Cap,
    fill: impl Fn(u64) -> T,
) -> (CappedSlab<T, u32, WrapWithProof>, SlotId<u32>, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder =
        CappedSlab::<T, u32, WrapWithProof>::builder(cap, &census).expect("registered");
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
        let id = slot.publish(fill(value));
        first.get_or_insert(id);
    }
    (slab, first.expect("an occupant"), census)
}

#[inline(never)]
fn step_holder_get(slab: &mut HolderSlab, id: &SlotId<u32>) -> u64 {
    match slab.get(*id) {
        Lookup::Live(value) => value[6] + 1,
        Lookup::Stale | Lookup::Vacant => panic!("a live occupant"),
    }
}

#[inline(never)]
fn step_holder_get_mut(slab: &mut HolderSlab, id: &SlotId<u32>) -> u64 {
    match slab.get_mut(*id) {
        LookupMut::Live(value) => value[6] + 1,
        LookupMut::Stale | LookupMut::Vacant => panic!("a live occupant"),
    }
}

#[inline(never)]
fn step_holder_cycle_named(slab: &mut HolderSlab, key: u64) -> u64 {
    let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
    let named = slot.named();
    let named_id = named.id();
    let id = named.publish([key; 7]);
    match slab.take_inlined(id) {
        Taken::Value(value) => u64::from(value[6] == key && id == named_id),
        Taken::Stale | Taken::Vacant => panic!("the slot just published"),
    }
}

/// The fabric key's adapter (ADR-0177 D4; ADR-0194 D6 with A2).
type HolderKey = SlotWord<u32, WrapWithProof, GATE_KEY_INDEX_BITS, GATE_KEY_GENERATION_BITS>;

#[inline(never)]
fn step_holder_word(slab: &mut HolderSlab, adapter: &HolderKey, bits: &u64) -> u64 {
    match slab.get_mut_by_word(adapter, *bits) {
        WordLookup::Live(_, value) => value[6] + 1,
        WordLookup::Stale | WordLookup::Vacant => panic!("a live occupant"),
    }
}

#[inline(never)]
fn step_holder_cycle(slab: &mut HolderSlab, key: u64) -> u64 {
    let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
    let id = slot.publish([key; 7]);
    match slab.take_inlined(id) {
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
    let (mut slab, first, _census) = occupied(cap, |value| [value; 7]);
    let slab = black_box(&mut slab);
    // The handle by reference: a by-value handle (12 B) reaches the step
    // through a stack copy, which the loop may rebuild every op (two stores
    // the harness's loop does not carry, as when the builder inlines); by
    // reference the loop passes one address, and the step reads the handle
    // through a pointer either way.
    let first = &first;
    // The adapter and the first occupant's word, made before the loop, as the
    // gate makes its adapter at its build and a reply carries its word.
    let adapter = HolderKey::for_slab(slab).expect("the holder slab numbers in 24 bits");
    let bits = &adapter.encode(*first).expect("this slab's handle");
    let adapter = &adapter;
    let accumulator = match op {
        HolderOp::Get => rate_loop::<NOPS>(ops, MASK_14, |_| step_holder_get(slab, first)),
        HolderOp::GetMut => rate_loop::<NOPS>(ops, MASK_14, |_| step_holder_get_mut(slab, first)),
        HolderOp::Cycle => rate_loop::<NOPS>(ops, MASK_14, |key| step_holder_cycle(slab, key)),
        HolderOp::CycleNamed => {
            rate_loop::<NOPS>(ops, MASK_14, |key| step_holder_cycle_named(slab, key))
        }
        HolderOp::Word => {
            rate_loop::<NOPS>(ops, MASK_14, |_| step_holder_word(slab, adapter, bits))
        }
    };
    // Engagement, after the counted loop: every op reached its slot and the
    // slab holds its occupants alone.
    assert_eq!(accumulator, ops, "every op reached its slot");
    assert_eq!(slab.len(), cap.entries_max() - 1, "the slab holds its occupants");
    accumulator
}

#[inline(never)]
fn step_node_get(slab: &mut NodeSlab, id: &SlotId<u32>) -> u64 {
    match slab.get(*id) {
        Lookup::Live(value) => value[5] + 1,
        Lookup::Stale | Lookup::Vacant => panic!("a live node"),
    }
}

#[inline(never)]
fn step_node_get_mut(slab: &mut NodeSlab, id: &SlotId<u32>) -> u64 {
    match slab.get_mut(*id) {
        LookupMut::Live(value) => value[5] + 1,
        LookupMut::Stale | LookupMut::Vacant => panic!("a live node"),
    }
}

#[inline(never)]
fn step_node_cycle_release(slab: &mut NodeSlab, key: u64) -> u64 {
    let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
    let id = slot.publish([key; 6]);
    match slab.release(id) {
        Released::Dropped => 1,
        Released::Stale | Released::Vacant => panic!("the node just published"),
    }
}

/// A get or a `get_mut` of the node slab's first occupant: a poll's read of
/// its node, and a wake's or a hook's write; or a node's life on its one
/// free slot: reserved and published at a wait's first poll, released at
/// its wake or its drop.
fn drive_node<const NOPS: u8>(op: NodeOp, ops: u64) -> u64 {
    let (mut slab, first, _census) = occupied(NODES_256, |value| [value; 6]);
    let slab = black_box(&mut slab);
    // The handle by reference, as the holder legs pass theirs.
    let first = &first;
    let accumulator = match op {
        NodeOp::Get => rate_loop::<NOPS>(ops, MASK_14, |_| step_node_get(slab, first)),
        NodeOp::GetMut => rate_loop::<NOPS>(ops, MASK_14, |_| step_node_get_mut(slab, first)),
        NodeOp::CycleRelease => {
            rate_loop::<NOPS>(ops, MASK_14, |key| step_node_cycle_release(slab, key))
        }
    };
    // Engagement, after the counted loop: every op reached its node and the
    // slab holds its occupants alone.
    assert_eq!(accumulator, ops, "every op reached its node");
    assert_eq!(slab.len(), NODES_256.entries_max() - 1, "the slab holds its occupants");
    accumulator
}
// ---- the keyed gate's route map, from one counter ---------------------------------------------

/// The live routes and the one an op admits before it removes a drawn one.
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

/// The draw's fixed seed: every counter leg, its twin and its walk depth
/// replay one stream.
const ROUTE_SEED: u64 = 11;
/// The ops run before the counted loop, in live counts: a live key outlives
/// them with probability e⁻⁸, so the live set is the draw's steady one.
const ROUTE_WARM_UP: u64 = 8;

impl Routes {
    fn cap(self) -> Cap {
        match self {
            Routes::Default => ROUTES_2496,
            Routes::Ceiling => ROUTES_159744,
        }
    }

    /// The live routes: all but the slot an op admits into before its
    /// remove.
    fn live(self) -> u64 {
        u64::from(self.cap().entries_max() - 1)
    }
}

/// The live keys in an array, the stream that draws the next one removed,
/// and the next counter key.
struct RouteDraw {
    keys: Box<[u64]>,
    rng: SplitMix64,
    next: u64,
}

impl RouteDraw {
    /// The keys `0..live`, live, and the stream at its seed.
    fn new(live: u64) -> RouteDraw {
        RouteDraw { keys: (0..live).collect(), rng: SplitMix64::new(ROUTE_SEED), next: live }
    }

    /// The live keys' sum, a scan of the array whose instructions do not
    /// depend on the keys.
    fn sum(&self) -> u64 {
        self.keys.iter().fold(0, |sum, key| sum.wrapping_add(*key))
    }
}

/// The draw every counter leg and its twin run, one body (inlined into each
/// step, so the twin's subtraction removes the same instructions): an index
/// `(x × N) >> 64` of the stream's next output over the `N` live keys, the
/// key there returned and `next` written in its place.
#[inline(always)]
fn draw_and_swap(keys: &mut [u64], rng: &mut SplitMix64, next: u64) -> u64 {
    let live = keys.len() as u128;
    let index = ((u128::from(rng.next_u64()) * live) >> 64) as usize;
    std::mem::replace(&mut keys[index], next)
}

/// A counter leg's state: the map of live routes and its draw.
struct RouteCounter {
    map: CappedMap<u64, u32>,
    draw: RouteDraw,
}

/// The routes `0..live` of `routes`, live in the map and the draw's array,
/// built by steps before the loop.
fn route_counter(routes: Routes) -> (RouteCounter, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = CappedMap::<u64, u32>::builder(routes.cap(), &census).expect("registered");
    let mut map = loop {
        match builder.step() {
            MapBuilt::More(next) => builder = next,
            MapBuilt::Live(map) => break map,
            MapBuilt::Refused(_, refused) => panic!("{refused}"),
        }
    };
    for key in 0..routes.live() {
        let Admission::Vacant(slot) = map.admit(key) else { panic!("a fresh key") };
        slot.publish(1);
    }
    (RouteCounter { map, draw: RouteDraw::new(routes.live()) }, census)
}

#[inline(never)]
fn step_route_counter(routes: &mut RouteCounter) -> u64 {
    let next = routes.draw.next;
    routes.draw.next = next + 1;
    let Admission::Vacant(slot) = routes.map.admit(next) else { panic!("the next key is vacant") };
    slot.publish(1);
    let removed = draw_and_swap(&mut routes.draw.keys, &mut routes.draw.rng, next);
    1 + routes.map.remove(removed).map_or(0, u64::from)
}

/// The next counter key admitted and a drawn live key removed, each op,
/// after the warm-up.
fn drive_route_counter<const NOPS: u8>(routes: Routes, ops: u64) -> u64 {
    let (mut counter, _census) = route_counter(routes);
    let live = routes.live();
    // The warm-up, the same at every op count: so its instructions leave the
    // count.
    let warm_ops = ROUTE_WARM_UP * live;
    let warmed = (0..warm_ops).fold(0u64, |sum, _| sum + step_route_counter(&mut counter));
    assert_eq!(warmed, 2 * warm_ops, "every warm-up op admitted and removed a key");
    let counter = black_box(&mut counter);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_route_counter(counter));
    // Engagement, after the counted loop: every op admitted its key and
    // removed a live one, and the map holds the live count. A get of every
    // key the array holds runs after the walk depth's replay of these ops
    // instead: its instructions follow the tree's depth, which differs
    // between I(0)'s tree and I(n)'s (+0.57 an op at 159,744 at n = 10⁶).
    assert_eq!(accumulator, 2 * ops, "every op admitted and removed a key");
    assert_eq!(u64::from(counter.map.len()), live, "the map holds the live routes");
    accumulator
}

#[inline(never)]
fn step_route_twin(draw: &mut RouteDraw) -> u64 {
    let next = draw.next;
    draw.next = next + 1;
    draw_and_swap(&mut draw.keys, &mut draw.rng, next)
}

/// The counter legs' draw alone, each op, after the same warm-up: the
/// removed key returned to the loop's accumulator in the map calls' place.
fn drive_route_twin<const NOPS: u8>(routes: Routes, ops: u64) -> u64 {
    let live = routes.live();
    let mut draw = RouteDraw::new(live);
    for _ in 0..ROUTE_WARM_UP * live {
        step_route_twin(&mut draw);
    }
    let (sum_before, first) = (draw.sum(), draw.next);
    let draw = black_box(&mut draw);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_route_twin(draw));
    // Engagement, after the counted loop: the keys removed are the keys live
    // before it and those it admitted, `first..first + ops`, less the keys
    // live after it, so every op drew a live key and wrote its own.
    let admitted = (first..first + ops).fold(0u64, u64::wrapping_add);
    let removed = sum_before.wrapping_add(admitted).wrapping_sub(draw.sum());
    assert_eq!(accumulator, removed, "every op removed a live key and admitted its own");
    assert_eq!(draw.next, first + ops, "every op admitted the next key");
    accumulator
}

/// The live keys counted in each aligned block of every power-of-two size,
/// the sorted copy the walk depth reads: a key's branch depth in the map's
/// tree is the number of bits `b` at which the block of 2^b keys beside it
/// (the key's bits above `b`, bit `b` flipped) holds a live key, each such
/// bit a branch on its path, the first-difference bit of the keys under it.
struct BlockCounts {
    levels: Vec<Vec<u32>>,
}

impl BlockCounts {
    /// Every count zero, for keys below `end`.
    fn new(end: u64) -> BlockCounts {
        let bits = u64::BITS - end.leading_zeros();
        let blocks = |bit: u32| usize::try_from((end >> bit) + 2).expect("a level's blocks");
        BlockCounts { levels: (0..=bits).map(|bit| vec![0; blocks(bit)]).collect() }
    }

    fn insert(&mut self, key: u64) {
        for (bit, level) in self.levels.iter_mut().enumerate() {
            level[usize::try_from(key >> bit).expect("a block")] += 1;
        }
    }

    fn remove(&mut self, key: u64) {
        for (bit, level) in self.levels.iter_mut().enumerate() {
            level[usize::try_from(key >> bit).expect("a block")] -= 1;
        }
    }

    fn depth(&self, key: u64) -> u64 {
        let beside = |bit: usize| usize::try_from((key >> bit) ^ 1).expect("a block");
        let held =
            |(bit, level): (usize, &Vec<u32>)| level.get(beside(bit)).is_some_and(|c| *c > 0);
        self.levels.iter().enumerate().filter(|entry| held(*entry)).count() as u64
    }
}

/// The mean branch depths of the keys a counter leg's ops admit and remove.
pub struct WalkDepth {
    pub admit: f64,
    pub remove: f64,
}

/// A counter leg's walk depth over `ops` ops after its warm-up, its own keys
/// replayed from its seed, or its window's: read outside the count, in a
/// process the count never runs.
pub fn walk_depth(leg: WaitLeg, ops: u64) -> Result<WalkDepth, String> {
    let WaitLeg::RouteCounter { routes, shape } = leg else {
        return Err("--walk-depth reads a counter leg or its window".to_owned());
    };
    if ops == 0 {
        return Err("--walk-depth reads a mean over --ops above 0".to_owned());
    }
    let (admit, remove) = match shape {
        RouteShape::Draw => walk_depth_draw(routes, ops),
        RouteShape::Window => walk_depth_window(routes.live(), ops),
        RouteShape::Twin => {
            return Err("the draw's twin holds no map; its counter leg reads the walk depth".into());
        }
    };
    let mean = |total: u64| total as f64 / ops as f64;
    Ok(WalkDepth { admit: mean(admit), remove: mean(remove) })
}

/// The counter leg's ops replayed on its own map, the same warm-up and
/// draws, beside a second draw of the seed that names each op's keys: the
/// depth of each admitted key once it is live and of each removed key
/// before its remove, summed over the `ops` after the warm-up. Then the
/// leg's engagement that reads the tree: the two draws hold one array, and
/// every key in it is live in the map.
fn walk_depth_draw(routes: Routes, ops: u64) -> (u64, u64) {
    let (live, warm_ops) = (routes.live(), ROUTE_WARM_UP * routes.live());
    let (mut counter, _census) = route_counter(routes);
    let mut keys = RouteDraw::new(live);
    let mut blocks = BlockCounts::new(live + warm_ops + ops);
    (0..live).for_each(|key| blocks.insert(key));
    let (mut admit, mut remove) = (0, 0);
    for op in 0..warm_ops + ops {
        let next = keys.next;
        keys.next = next + 1;
        blocks.insert(next);
        let removed = draw_and_swap(&mut keys.keys, &mut keys.rng, next);
        assert_eq!(step_route_counter(&mut counter), 2, "the leg's op admitted and removed");
        if op >= warm_ops {
            admit += blocks.depth(next);
            remove += blocks.depth(removed);
        }
        blocks.remove(removed);
    }
    assert!(keys.keys == counter.draw.keys, "the replay drew the leg's keys");
    assert_eq!(u64::from(counter.map.len()), live, "the map holds the live routes");
    let gone = counter.draw.keys.iter().filter(|key| counter.map.get(**key).is_none()).count();
    assert_eq!(gone, 0, "every key the array holds is live");
    (admit, remove)
}

/// The sliding window's ops: the next counter key admitted and the oldest
/// removed, its live set always `live` keys in a row, so it needs no
/// warm-up.
fn walk_depth_window(live: u64, ops: u64) -> (u64, u64) {
    let mut blocks = BlockCounts::new(live + ops);
    (0..live).for_each(|key| blocks.insert(key));
    let (mut admit, mut remove) = (0, 0);
    for oldest in 0..ops {
        let next = oldest + live;
        blocks.insert(next);
        admit += blocks.depth(next);
        remove += blocks.depth(oldest);
        blocks.remove(oldest);
    }
    (admit, remove)
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
fn drive_hash_gate<const NOPS: u8>(depth: u32, op: HashOp, ops: u64) -> u64 {
    let mut table = even_tokens(depth);
    let live = table.len();
    let mask = (1u64 << depth) - 1;
    let table = black_box(&mut table);
    let accumulator = match op {
        HashOp::Get => rate_loop::<NOPS>(ops, mask, |key| step_hash_gate_get(table, key)),
        HashOp::InsertRemove => {
            rate_loop::<NOPS>(ops, mask, |key| step_hash_gate_insert_remove(table, key + 1))
        }
    };
    // Engagement, after the counted loop: every op found or placed its token
    // and the table holds its live tokens alone.
    let per_op = match op {
        HashOp::Get => 1,
        HashOp::InsertRemove => 2,
    };
    assert_eq!(accumulator, ops * per_op, "every op reached its token");
    assert_eq!(table.len(), live, "the table holds its live tokens");
    accumulator
}

// ---- the wait nodes' heap, with the gate's hook -----------------------------------------------

/// A wait's heap entry: its LSN and its node's handle, 24 B.
#[derive(Clone, Copy)]
struct WaitEntry {
    lsn: u64,
    node: SlotId<u32>,
}

impl HeapEntry for WaitEntry {
    type Key = u64;

    fn key(&self) -> u64 {
        self.lsn
    }
}

const _: () = assert!(size_of::<WaitEntry>() == 24, "a wait entry is 24 B");

// The stand-in's entry is the product gate's, byte for byte, so the hook legs read the stride
// the gate's entry array is built at; an entry that grows or shrinks fails this build.
const _: () = assert!(
    size_of::<WaitEntry>() == inf_runtime::WatermarkGate::ENTRY_BYTES,
    "the bench's wait entry is the gate's"
);

/// The node words the hook and a wake write: the heap position the hook
/// stores, and the state a wake marks.
const NODE_POSITION: usize = 5;
const NODE_STATE: usize = 4;

/// A pool of each round's waits, every node live: the nodes at one cell
/// (962), at four (2,825), one directory of entries (16,384) and the storm
/// one fsync releases on the wake step legs (50,000).
const NODES_962: Cap = Cap::entries::<962>("bench-nodes-962", CapFill::Serving);
const NODES_2825: Cap = Cap::entries::<2_825>("bench-nodes-2825", CapFill::Serving);
const NODES_16384: Cap = Cap::entries::<16_384>("bench-nodes-16384", CapFill::Serving);
const NODES_50000: Cap = Cap::entries::<50_000>("bench-nodes-50000", CapFill::Serving);

impl Waits {
    fn node_cap(self) -> Cap {
        match self {
            Waits::OneCell => NODES_962,
            Waits::FourCells => NODES_2825,
            Waits::OneDirectory => NODES_16384,
            Waits::Storm => NODES_50000,
        }
    }
}

/// The gate's hook: an entry a sift places at `index` has its node's
/// position word stored, through the pool's `get_mut`. An entry exists only
/// with its live node, so any other answer is a broken invariant, as the
/// gate's resolve stops on it.
#[inline(always)]
fn place(nodes: &mut NodeSlab, entry: WaitEntry, index: u32) {
    match nodes.get_mut(entry.node) {
        LookupMut::Live(node) => node[NODE_POSITION] = u64::from(index),
        LookupMut::Stale | LookupMut::Vacant => unreachable!("an entry's node is live"),
    }
}

/// The hook's rounds: the heap, the pool every entry's hook resolves, each
/// wait's own node, the LSN the next round starts above, its waits and their
/// order.
struct HookRounds {
    heap: IndexedHeap<WaitEntry>,
    nodes: NodeSlab,
    handles: Vec<SlotId<u32>>,
    base: u64,
    waits: u64,
    order: Order,
}

/// A pool of `waits` nodes at the pool's slot, every one live, and a heap of
/// as many entries, both built by steps before the loop.
fn hook_rounds(order: Order, waits: Waits) -> (HookRounds, Rc<CapCensus>) {
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let mut builder = NodeSlab::builder(waits.node_cap(), &census).expect("registered");
    let mut nodes = loop {
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    };
    let count = waits.count();
    let mut handles = Vec::with_capacity(usize::try_from(count).expect("a round's waits"));
    for _ in 0..count {
        let SlabReserved::Slot(slot) = nodes.reserve() else { panic!("a free slot") };
        handles.push(slot.publish([0; 6]));
    }
    let fill = WaitEntry { lsn: 0, node: handles[0] };
    let capacity = u32::try_from(count).expect("a round's waits fit a u32");
    let mut array = FixedArray::builder(capacity, fill).expect("a heap's layout");
    let entries = loop {
        match array.step() {
            ArrayBuilt::More(next) => array = next,
            ArrayBuilt::Done(entries) => break entries,
            ArrayBuilt::Refused(refused) => panic!("{refused}"),
        }
    };
    let heap = IndexedHeap::new(entries);
    (HookRounds { heap, nodes, handles, base: 0, waits: count, order }, census)
}

/// One round: the waits pushed in the round's order, each entry carrying its
/// own node's handle and `WAIT_NOPS` planted after its push's `expect` (a
/// call that may unwind, so no pass runs the plant ahead of it), then the
/// advances to the middle and to the last, each popping every entry at or
/// below it; the hook writes each placed entry's position. Returns the waits
/// woken.
#[inline(always)]
fn hook_round<const WAIT_NOPS: u8>(rounds: &mut HookRounds) -> u64 {
    let (base, waits, order) = (rounds.base, rounds.waits, rounds.order);
    let HookRounds { heap, nodes, handles, .. } = rounds;
    for index in 0..waits {
        let lsn = match order {
            Order::Rising => base + 1 + index,
            Order::Falling => base + waits - index,
        };
        let entry = WaitEntry { lsn, node: handles[index as usize] };
        heap.push(
            entry,
            #[inline(always)]
            |entry, at| place(nodes, entry, at),
        )
        .expect("room for every wait");
        plant::<WAIT_NOPS>();
    }
    let mut woken = 0;
    for to in [base + waits / 2, base + waits] {
        while heap.peek().is_some_and(|entry| entry.lsn <= to) {
            let popped = heap.pop(
                #[inline(always)]
                |entry, at| place(nodes, entry, at),
            );
            woken += u64::from(popped.is_some());
        }
    }
    rounds.base = base + waits;
    woken
}

// The round's step and its plant's, each one body (a generic step would be
// two bodies under one name, which the site's-shape control refuses to read).
#[inline(never)]
fn step_heap_hook(rounds: &mut HookRounds) -> u64 {
    hook_round::<0>(rounds)
}

#[inline(never)]
fn step_heap_hook_plus_wait(rounds: &mut HookRounds) -> u64 {
    hook_round::<1>(rounds)
}

/// A round's moves: the entries its sifts place, heard by a counting hook
/// that writes each position as the gate's does.
fn hook_round_moves(rounds: &mut HookRounds) -> u64 {
    let (base, waits, order) = (rounds.base, rounds.waits, rounds.order);
    let HookRounds { heap, nodes, handles, .. } = rounds;
    let mut moves = 0u64;
    let mut counted = |entry: WaitEntry, at: u32| {
        place(nodes, entry, at);
        moves += 1;
    };
    for index in 0..waits {
        let lsn = match order {
            Order::Rising => base + 1 + index,
            Order::Falling => base + waits - index,
        };
        let entry = WaitEntry { lsn, node: handles[index as usize] };
        heap.push(entry, &mut counted).expect("room for every wait");
    }
    let mut woken = 0u64;
    for to in [base + waits / 2, base + waits] {
        while heap.peek().is_some_and(|entry| entry.lsn <= to) {
            woken += u64::from(heap.pop(&mut counted).is_some());
        }
    }
    assert_eq!(woken, waits, "a counted round woke every wait");
    rounds.base = base + waits;
    moves
}

/// A dropped wait's removal at the position its node holds, the last entry
/// sifted into its place with the hook: the heap's `remove` at the wait
/// entry's instance, its own step.
#[inline(never)]
fn step_heap_hook_remove(heap: &mut IndexedHeap<WaitEntry>, nodes: &mut NodeSlab, at: u32) -> u64 {
    let removed = heap.remove(
        at,
        #[inline(always)]
        |entry, at| place(nodes, entry, at),
    );
    u64::from(removed.is_some())
}

/// A round of drops: the round's waits pushed with the hook, then the wait
/// in the middle of the heap removed at the position its node holds, until
/// none is left. Every position read is the entry's own, so the hook wrote
/// each one. Returns the waits removed.
fn hook_drop_round(rounds: &mut HookRounds) -> u64 {
    let (base, waits) = (rounds.base, rounds.waits);
    let HookRounds { heap, nodes, handles, .. } = rounds;
    for index in 0..waits {
        let entry = WaitEntry { lsn: base + 1 + index, node: handles[index as usize] };
        heap.push(entry, |entry, at| place(nodes, entry, at)).expect("room for every wait");
    }
    let mut removed = 0;
    while !heap.is_empty() {
        let middle = heap.get(heap.len() / 2).expect("an entry in the middle");
        let at = match nodes.get(middle.node) {
            Lookup::Live(node) => u32::try_from(node[NODE_POSITION]).expect("a position"),
            Lookup::Stale | Lookup::Vacant => panic!("an entry's node is live"),
        };
        assert_eq!(at, heap.len() / 2, "the node holds its entry's position");
        removed += step_heap_hook_remove(heap, nodes, at);
    }
    rounds.base = base + waits;
    removed
}

/// One round a step, `WAIT_NOPS` planted a wait.
fn drive_heap_hook<const WAIT_NOPS: u8>(order: Order, waits: Waits, ops: u64) -> u64 {
    let (mut rounds, _census) = hook_rounds(order, waits);
    // The dry pass: a round with a counting hook, before the counted loop.
    // Its moves go to the run's log, where the rows' hook term reads them.
    let dry_moves = hook_round_moves(&mut rounds);
    eprintln!("dry-pass-moves {dry_moves}");
    let rounds = black_box(&mut rounds);
    let accumulator = match WAIT_NOPS {
        0 => rate_loop::<0>(ops, MASK_14, |_| step_heap_hook(rounds)),
        _ => rate_loop::<0>(ops, MASK_14, |_| step_heap_hook_plus_wait(rounds)),
    };
    // Engagement, after the counted loop, the same at every op count: every
    // wait woken, the heap empty and every node live, a round from here
    // places the entries the dry pass placed, and a round of drops removes
    // every wait at the position the hook wrote.
    assert_eq!(accumulator, ops * rounds.waits, "every wait woken");
    assert!(rounds.heap.is_empty(), "every entry popped");
    assert_eq!(u64::from(rounds.nodes.len()), rounds.waits, "every node live");
    assert_eq!(hook_round_moves(rounds), dry_moves, "a round's moves are its dry pass's");
    assert_eq!(hook_drop_round(rounds), rounds.waits, "every dropped wait removed");
    accumulator
}

/// The watermark's one wait with the gate's hook: a pool and a heap of a
/// round's size, a far entry parked throughout, the op's own node, and the
/// LSN of its last wait.
struct HookOne {
    heap: IndexedHeap<WaitEntry>,
    nodes: NodeSlab,
    node: SlotId<u32>,
    lsn: u64,
}

/// One wait: an entry at the next LSN pushed under the far one and popped
/// while the minimum is at or below it, three moves, each with the hook.
#[inline(never)]
fn step_heap_hook_one(one: &mut HookOne) -> u64 {
    let HookOne { heap, nodes, node, lsn } = one;
    *lsn += 1;
    let to = *lsn;
    let entry = WaitEntry { lsn: to, node: *node };
    heap.push(
        entry,
        #[inline(always)]
        |entry, at| place(nodes, entry, at),
    )
    .expect("room under the far entry");
    let mut woken = 0;
    while heap.peek().is_some_and(|entry| entry.lsn <= to) {
        let popped = heap.pop(
            #[inline(always)]
            |entry, at| place(nodes, entry, at),
        );
        woken += u64::from(popped.is_some());
    }
    woken
}

/// One op's moves, heard by a counting hook outside the count.
fn hook_one_moves(one: &mut HookOne) -> u64 {
    let HookOne { heap, nodes, node, lsn } = one;
    let mut moves = 0u64;
    let mut counted = |entry: WaitEntry, at: u32| {
        place(nodes, entry, at);
        moves += 1;
    };
    *lsn += 1;
    let to = *lsn;
    heap.push(WaitEntry { lsn: to, node: *node }, &mut counted).expect("room under the far entry");
    while heap.peek().is_some_and(|entry| entry.lsn <= to) {
        assert!(heap.pop(&mut counted).is_some(), "a counted op popped its entry");
    }
    moves
}

/// A wait for the next LSN a step, a far entry parked.
fn drive_heap_hook_one<const NOPS: u8>(waits: Waits, ops: u64) -> u64 {
    let (rounds, _census) = hook_rounds(Order::Rising, waits);
    let HookRounds { mut heap, mut nodes, handles, .. } = rounds;
    let far = WaitEntry { lsn: u64::MAX / 2, node: handles[1] };
    heap.push(far, |entry, at| place(&mut nodes, entry, at)).expect("room for the far entry");
    let mut one = HookOne { heap, nodes, node: handles[0], lsn: 0 };
    // The dry pass: one op with a counting hook, before the counted loop.
    let dry_moves = hook_one_moves(&mut one);
    eprintln!("dry-pass-moves {dry_moves}");
    let one = black_box(&mut one);
    let accumulator = rate_loop::<NOPS>(ops, MASK_14, |_| step_heap_hook_one(one));
    // Engagement, after the counted loop: each op's entry popped, the far
    // entry alone left at the root with its position written, every node
    // live, and an op from here places the dry pass's three moves.
    assert_eq!(dry_moves, 3, "a push under the far entry and its pop place three");
    assert_eq!(accumulator, ops, "every op's entry popped");
    assert_eq!(one.heap.len(), 1, "the far entry alone is left");
    match one.nodes.get(far.node) {
        Lookup::Live(node) => assert_eq!(node[NODE_POSITION], 0, "the far entry at the root"),
        Lookup::Stale | Lookup::Vacant => panic!("the far entry's node is live"),
    }
    assert_eq!(u64::from(one.nodes.len()), waits.count(), "every node live");
    assert_eq!(hook_one_moves(one), dry_moves, "an op's moves are its dry pass's");
    accumulator
}

// ---- the wake step: one call over the waits one fsync covers ----------------------------------

/// The durable cell's wakes between two LOG steps (ADR-0177 A2.1): the
/// budgeted step wakes this many of a storm's covered waits.
const WAKE_BUDGET: u64 = 1_024;
/// Bytes written after a fill, so the evicted call finds the fill's stores
/// out of the near caches.
const EVICT_BYTES: usize = 64 << 20;

/// A wake step's waits, filled before every call in rising LSN order with
/// the hook, all covered by the call's watermark.
fn fill_rising(rounds: &mut HookRounds) {
    let (base, waits) = (rounds.base, rounds.waits);
    let HookRounds { heap, nodes, handles, .. } = rounds;
    for index in 0..waits {
        let entry = WaitEntry { lsn: base + 1 + index, node: handles[index as usize] };
        heap.push(entry, |entry, at| place(nodes, entry, at)).expect("room for every wait");
    }
    rounds.base = base + waits;
}

/// The gate's wake loop over the covered waits, at most `budget`: a peek, a
/// pop with the hook, the popped node marked with `to` and a waker woken,
/// `WAIT_NOPS` planted after each pop's result is read. `BUDGETED` gives
/// each step a closure type of its own, as each gate body has, so no two
/// steps share a sift's instance.
#[inline(always)]
fn wake_covered<const BUDGETED: bool, const WAIT_NOPS: u8>(
    rounds: &mut HookRounds,
    to: u64,
    budget: u64,
) -> u64 {
    let HookRounds { heap, nodes, .. } = rounds;
    let waker = Waker::noop();
    let mut woken = 0;
    while woken < budget && heap.peek().is_some_and(|entry| entry.lsn <= to) {
        let popped = heap.pop(
            #[inline(always)]
            |entry, at| place(nodes, entry, at),
        );
        let Some(entry) = popped else { break };
        plant::<WAIT_NOPS>();
        match nodes.get_mut(entry.node) {
            LookupMut::Live(node) => node[NODE_STATE] = to,
            LookupMut::Stale | LookupMut::Vacant => unreachable!("a popped entry's node is live"),
        }
        waker.wake_by_ref();
        woken += 1;
    }
    woken
}

// Each step one body: every covered wait, or the budget's first; and HEAD's
// `advance` on the bench's copy of its gate.
#[inline(never)]
fn step_advance_hook(rounds: &mut HookRounds, to: u64) -> u64 {
    wake_covered::<false, 0>(rounds, to, u64::MAX)
}

#[inline(never)]
fn step_advance_hook_plus_wait(rounds: &mut HookRounds, to: u64) -> u64 {
    wake_covered::<false, 1>(rounds, to, u64::MAX)
}

#[inline(never)]
fn step_wake_budget(rounds: &mut HookRounds, to: u64) -> u64 {
    wake_covered::<true, 0>(rounds, to, WAKE_BUDGET)
}

#[inline(never)]
fn step_wake_budget_plus_wait(rounds: &mut HookRounds, to: u64) -> u64 {
    wake_covered::<true, 1>(rounds, to, WAKE_BUDGET)
}

#[inline(never)]
fn step_advance_btree(gate: &WatermarkGate, to: u64) -> u64 {
    gate.advance(to) as u64
}

/// The waits a budgeted step left covered, woken outside the step.
fn drain_covered(rounds: &mut HookRounds, to: u64) -> u64 {
    let HookRounds { heap, nodes, .. } = rounds;
    let mut woken = 0;
    while heap.peek().is_some_and(|entry| entry.lsn <= to) {
        let entry = heap.pop(|entry, at| place(nodes, entry, at)).expect("a covered entry");
        match nodes.get_mut(entry.node) {
            LookupMut::Live(node) => node[NODE_STATE] = to,
            LookupMut::Stale | LookupMut::Vacant => unreachable!("a popped entry's node is live"),
        }
        woken += 1;
    }
    woken
}

/// Writes every cache line of `scratch`, so the stores a fill touched leave
/// the near caches before the evicted call.
#[inline(never)]
fn evict(scratch: &mut [u8], salt: u8) -> u64 {
    let mut sum = 0u64;
    for (index, byte) in scratch.iter_mut().enumerate().step_by(64) {
        *byte = salt.wrapping_add(index as u8);
        sum += u64::from(*byte);
    }
    black_box(sum)
}

/// The cache state a wake step's call starts in: right after its fill, or
/// with `EVICT_BYTES` written after the fill.
#[derive(Clone, Copy)]
enum Regime {
    Warm,
    Evicted,
}

/// One call of `step`, its wall time read around it by the monotonic clock.
#[allow(
    clippy::disallowed_methods,
    reason = "clock: the wake step legs' time is a wall clock read around one call, \
              outside any cell"
)]
#[inline(always)]
fn timed(step: impl FnOnce() -> u64) -> (u64, u64) {
    let began = std::time::Instant::now();
    let woken = step();
    let nanos = began.elapsed().as_nanos();
    (woken, u64::try_from(nanos).expect("a call under 584 years"))
}

/// The median and the worst of a regime's calls, on stderr: `step-ns
/// <regime> calls <n> median <ns> worst <ns>`.
fn report(regime: Regime, mut nanos: Vec<u64>) {
    if nanos.is_empty() {
        return;
    }
    let label = match regime {
        Regime::Warm => "warm",
        Regime::Evicted => "evicted",
    };
    nanos.sort_unstable();
    let (calls, median, worst) = (nanos.len(), nanos[nanos.len() / 2], nanos[nanos.len() - 1]);
    eprintln!("step-ns {label} calls {calls} median {median} worst {worst}");
}

/// Each op, in each regime: `waits` waits filled and covered, one call of
/// the step timed, the rest woken outside it; `WAIT_NOPS` planted after each
/// of the step's pops.
fn drive_wake_hook<const WAIT_NOPS: u8>(budgeted: bool, waits: Waits, ops: u64) -> u64 {
    let step: fn(&mut HookRounds, u64) -> u64 = match (budgeted, WAIT_NOPS) {
        (false, 0) => step_advance_hook,
        (false, _) => step_advance_hook_plus_wait,
        (true, 0) => step_wake_budget,
        (true, _) => step_wake_budget_plus_wait,
    };
    let (mut rounds, _census) = hook_rounds(Order::Rising, waits);
    let mut scratch = vec![0u8; EVICT_BYTES];
    let calls = usize::try_from(ops).expect("a call count");
    let (mut warm, mut evicted) = (Vec::with_capacity(calls), Vec::with_capacity(calls));
    let first = if budgeted { WAKE_BUDGET.min(rounds.waits) } else { rounds.waits };
    let mut total = 0;
    for op in 0..ops {
        for regime in [Regime::Warm, Regime::Evicted] {
            fill_rising(&mut rounds);
            if let Regime::Evicted = regime {
                evict(&mut scratch, op as u8);
            }
            let to = rounds.base;
            let rounds = black_box(&mut rounds);
            let (woken, nanos) = timed(|| step(rounds, to));
            // Engagement, each call: the step woke its waits, the rest are
            // woken outside it, the heap is empty and every node is marked.
            assert_eq!(woken, first, "the step woke its waits");
            assert_eq!(drain_covered(rounds, to), rounds.waits - first, "the rest woken after");
            assert!(rounds.heap.is_empty(), "every entry popped");
            let marked = |handle: &SlotId<u32>| match rounds.nodes.get(*handle) {
                Lookup::Live(node) => node[NODE_STATE] == to,
                Lookup::Stale | Lookup::Vacant => false,
            };
            assert!(rounds.handles.iter().all(marked), "every node live and marked");
            total += woken;
            match regime {
                Regime::Warm => warm.push(nanos),
                Regime::Evicted => evicted.push(nanos),
            }
        }
    }
    report(Regime::Warm, warm);
    report(Regime::Evicted, evicted);
    total
}

/// Each op, in each regime: `waits` waits made on the bench's copy of the
/// B-tree gate and polled once, one `advance` over all of them timed, then
/// each polled ready and dropped.
fn drive_wake_btree(waits: Waits, ops: u64) -> u64 {
    let count = waits.count();
    let gate = WatermarkGate::new();
    let mut pending = Vec::with_capacity(usize::try_from(count).expect("a round's waits"));
    let mut cx = Context::from_waker(Waker::noop());
    let mut scratch = vec![0u8; EVICT_BYTES];
    let calls = usize::try_from(ops).expect("a call count");
    let (mut warm, mut evicted) = (Vec::with_capacity(calls), Vec::with_capacity(calls));
    let (mut base, mut total) = (0u64, 0u64);
    for op in 0..ops {
        for regime in [Regime::Warm, Regime::Evicted] {
            for index in 0..count {
                let mut wait = gate.waiter(base + 1 + index);
                let parked = Pin::new(&mut wait).poll(&mut cx).is_pending();
                assert!(parked, "a wait above the watermark parks");
                pending.push(wait);
            }
            base += count;
            if let Regime::Evicted = regime {
                evict(&mut scratch, op as u8);
            }
            let gate = black_box(&gate);
            let (woken, nanos) = timed(|| step_advance_btree(gate, base));
            // Engagement, each call: the advance woke every wait, each reads
            // ready, and the gate holds none.
            assert_eq!(woken, count, "the advance woke every wait");
            for mut wait in pending.drain(..) {
                assert!(Pin::new(&mut wait).poll(&mut cx).is_ready(), "a woken wait is ready");
            }
            assert_eq!(gate.waiting(), 0, "no waiter left");
            total += woken;
            match regime {
                Regime::Warm => warm.push(nanos),
                Regime::Evicted => evicted.push(nanos),
            }
        }
    }
    report(Regime::Warm, warm);
    report(Regime::Evicted, evicted);
    total
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
