#![allow(
    clippy::disallowed_types,
    reason = "bench target: the std containers are the A legs the capped types are measured against"
)]
//! The capped deque's A/B by instructions (L4): one push and one pop per op
//! on `VecDeque::with_capacity` (A) and on `CappedDeque` (B, both
//! crossings), at caps 16 and 1,024, from one binary. Beside it, the capped
//! slab's row, reported and not judged: one op publishes one entry, reads it
//! back and takes the oldest, on a `HashMap` keyed by sequential `u64` ids
//! under the integer hasher (A, the driver's operation-id table) and on a
//! `CappedSlab<_, u32, Retire>` (B: its instance compare and fill test), at
//! caps 1,024, 16,384 and 65,536 — one directory link up to 16,384 slots,
//! two at 65,536 — with the oldest id from a `VecDeque` of ids in both legs.
//! And the capped map's row, reported and not judged: one op admits and
//! publishes one fresh key, reads it back and removes the oldest, on a
//! `HashMap` of foreign keys under the integer hasher (A) and on a
//! `CappedMap<u64, _>` (B: its admission compare and its fill test), at cap
//! 15,360, with the oldest key from a `VecDeque` of keys in both legs.
//!
//! The verdict rule: B − A ≤ 10 instructions per push-and-pop pair at the
//! `call` shape, for either crossing at either cap; the `loop` shape is
//! reported, never judged. The bound counts what the type can add to the
//! pair at that shape, read on its disassembly: the two compares, the count
//! they re-read, and what they and their cold arms cost the pop after them.
//! `call` is judged because it is every site's shape: a site holds its deque
//! in a long-lived owner and performs one op per command, frame or parked op
//! through `&mut`, so every op loads and stores the deque's fields. `loop`,
//! the ops in one tight loop over a local deque, lets the compiler keep
//! those fields in registers, and an arm that calls out costs the whole loop
//! those registers; a verdict there would judge the register allocation of
//! a loop no site runs.
//!
//! The instrument: exact user-space instructions per op, `(I(n) − I(0)) / n`
//! at `n` = 10⁷, read outside this binary (`perf stat -e instructions:u`, or
//! an instruction-counting tool when the PMU is closed) on one pinned core,
//! legs A B B A twice at each cap and shape. A run judges only when its
//! controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: A against A and B against B within
//!   0.05 per op, at a resolution of 0.01 per op at `n` = 10⁷;
//! - liveness: `vecdeque-plus-one` and `reserve-plus-one`, one `nop` per op
//!   at the top of the loop body, read A + 1.00 and B + 1.00, ± 0.05;
//! - the rule's own red: `reserve-plus-four`, four `nop`s, reads B − A + 4:
//!   over the bound while B − A ≥ 7. A cheaper B leaves it at the bound or
//!   under, and the run is invalid until the red is repaired.
//!
//! `vecdeque-bounded` (A′) is A with the site's own `len < cap` test, the
//! cap a third argument of the step: the control that attributes the
//! bound's own cost, reported beside A.
//!
//! One leg, then one leg counted: `perf` wraps the binary that `--no-run`
//! prints, never `cargo`, whose own instructions would join the count; on a
//! hybrid CPU `<core>` is a performance core and the `cpu_core` line counts.
//!
//!   cargo bench -p inf-foundation --bench bounded -- \
//!       --variant <v> --cap <c> --ops <n> [--shape <s>]
//!   cargo bench -p inf-foundation --bench bounded --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <v> --cap <c> --ops <n> [--shape <s>]
//!
//! `v` is a leg of `LEGS`; `c` ∈ 16 | 1024 for a deque leg, 1024 | 16384 |
//! 65536 for a slab leg, 15360 for a map leg; `n` ≥ 0 (0 is the I(0) leg);
//! `s` ∈ call (the
//! default: one op per `#[inline(never)]` call through `&mut`) | loop, a
//! deque leg's alone. The deque is half full throughout, so every op pays
//! the cap compare and no op crosses it; the slab and its `HashMap` hold
//! `N / 2` entries throughout (`N` the cap), so no op crosses either and
//! every op walks the directory depth of its `N`. The slab row's controls:
//! A against A and B against B within 0.05 per op; `hashmap-plus-one` and
//! `slab-plus-one` read A + 1.00 and B + 1.00, ± 0.05; `ids-only-u64` and
//! `ids-only-slot`, the id queue alone (push one, pop the oldest) at each
//! handle width, attribute the queue's own cost. The map and its `HashMap`
//! hold `N / 2` entries throughout, 7,680 sparse 48-bit keys at `N` =
//! 15,360, so no op crosses and every walk reads a tree of that population.
//! The map row's controls: A against A and B against B within 0.05 per op;
//! `hashmap-keys-plus-one` and `map-plus-one` read A + 1.00 and B + 1.00,
//! ± 0.05; `keys-only`, the key queue and the keys' arithmetic alone,
//! attributes their cost.

use std::collections::{HashMap, VecDeque};
use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::bounded::{
    Admission, Built, Cap, CapCensus, CapFill, CappedDeque, CappedMap, CappedSlab, Coalesce,
    Lookup, Lossy, MapBuilt, Merge, Reserve, Reserved, Retire, SlabReserved, SlotId, Taken,
};
use inf_foundation::{BuildIntHasher, CellId};

const CAP_16: Cap = Cap::entries::<16>("bench-16", CapFill::Serving);
const CAP_1024: Cap = Cap::entries::<1024>("bench-1024", CapFill::Serving);
const CAP_16384: Cap = Cap::entries::<16384>("bench-16384", CapFill::Serving);
const CAP_65536: Cap = Cap::entries::<65536>("bench-65536", CapFill::Serving);
const CAP_15360: Cap = Cap::entries::<15360>("bench-15360", CapFill::Serving);

#[derive(Clone, Copy)]
struct Entry(u64);

impl Lossy for Entry {}

struct Later;

impl Merge<Entry> for Later {
    fn merge(back: &mut Entry, new: Entry) {
        *back = new;
    }
}

/// A leg: the deque it drives and the plant it carries.
#[derive(Clone, Copy)]
enum Leg {
    /// A.
    VecDeque,
    /// A′: A with the site's own `len < cap` test.
    VecDequeBounded,
    /// A plus one `nop`: the liveness plant on A.
    VecDequePlusOne,
    /// B, the `Reserve` crossing: a slot, then its publish.
    Reserve,
    /// B plus one `nop`: the liveness plant on B.
    ReservePlusOne,
    /// B plus four `nop`s: the verdict rule's own red.
    ReservePlusFour,
    /// B, the `Coalesce` crossing.
    Coalesce,
    /// The slab row's A: a `HashMap` of sequential `u64` ids.
    HashMap,
    /// A plus one `nop`: the liveness plant on the slab row's A.
    HashMapPlusOne,
    /// The slab row's B: a `CappedSlab<_, u32, Retire>`.
    Slab,
    /// B plus one `nop`: the liveness plant on the slab row's B.
    SlabPlusOne,
    /// The id queue alone, holding A's `u64` ids.
    IdsOnlyU64,
    /// The id queue alone, holding B's `SlotId<u32>` handles.
    IdsOnlySlot,
    /// The map row's A: a `HashMap` of sparse 48-bit keys.
    HashMapKeys,
    /// A plus one `nop`: the liveness plant on the map row's A.
    HashMapKeysPlusOne,
    /// The map row's B: a `CappedMap<u64, _>`.
    Map,
    /// B plus one `nop`: the liveness plant on the map row's B.
    MapPlusOne,
    /// The key queue and the keys' arithmetic alone, both legs' share.
    KeysOnly,
}

/// The one table of leg names: `--variant` is parsed against it.
const LEGS: [(&str, Leg); 18] = [
    ("vecdeque", Leg::VecDeque),
    ("vecdeque-bounded", Leg::VecDequeBounded),
    ("vecdeque-plus-one", Leg::VecDequePlusOne),
    ("reserve", Leg::Reserve),
    ("reserve-plus-one", Leg::ReservePlusOne),
    ("reserve-plus-four", Leg::ReservePlusFour),
    ("coalesce", Leg::Coalesce),
    ("hashmap", Leg::HashMap),
    ("hashmap-plus-one", Leg::HashMapPlusOne),
    ("slab", Leg::Slab),
    ("slab-plus-one", Leg::SlabPlusOne),
    ("ids-only-u64", Leg::IdsOnlyU64),
    ("ids-only-slot", Leg::IdsOnlySlot),
    ("hashmap-keys", Leg::HashMapKeys),
    ("hashmap-keys-plus-one", Leg::HashMapKeysPlusOne),
    ("map", Leg::Map),
    ("map-plus-one", Leg::MapPlusOne),
    ("keys-only", Leg::KeysOnly),
];

#[derive(Clone, Copy)]
enum Shape {
    /// One op per `#[inline(never)]` call through `&mut`: the judged shape.
    Call,
    /// The ops in one tight loop over a local deque: reported.
    Loop,
}

struct Args {
    variant: &'static str,
    leg: Leg,
    cap: Cap,
    ops: u64,
    shape: Shape,
}

fn parse() -> Result<Args, String> {
    let mut leg = None;
    let mut cap = None;
    let mut ops = None;
    let mut shape = Shape::Call;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bench" {
            continue; // cargo bench passes it to a harness-free binary
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => {
                let Some(&found) = LEGS.iter().find(|(name, _)| *name == value) else {
                    let names: Vec<&str> = LEGS.iter().map(|(name, _)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                leg = Some(found);
            }
            "--cap" => {
                cap = Some(match value.as_str() {
                    "16" => CAP_16,
                    "1024" => CAP_1024,
                    "16384" => CAP_16384,
                    "65536" => CAP_65536,
                    "15360" => CAP_15360,
                    other => {
                        return Err(format!("--cap {other}: 16, 1024, 16384, 65536 or 15360"));
                    }
                });
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            "--shape" => {
                shape = match value.as_str() {
                    "call" => Shape::Call,
                    "loop" => Shape::Loop,
                    other => return Err(format!("--shape {other}: call or loop")),
                };
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (variant, leg) = leg.ok_or("--variant is required")?;
    let cap = cap.ok_or("--cap is required")?;
    let deque = matches!(
        leg,
        Leg::VecDeque
            | Leg::VecDequeBounded
            | Leg::VecDequePlusOne
            | Leg::Reserve
            | Leg::ReservePlusOne
            | Leg::ReservePlusFour
            | Leg::Coalesce
    );
    let map = matches!(
        leg,
        Leg::HashMapKeys | Leg::HashMapKeysPlusOne | Leg::Map | Leg::MapPlusOne | Leg::KeysOnly
    );
    let call = matches!(shape, Shape::Call);
    let fits = if deque {
        cap.entries_max() <= 1024
    } else if map {
        cap.entries_max() == 15360 && call
    } else {
        cap.entries_max() >= 1024 && cap.entries_max() != 15360 && call
    };
    if !fits {
        return Err(format!(
            "{variant}: a deque leg takes --cap 16 or 1024; a slab leg 1024, 16384 or 65536, \
             a map leg 15360, at the call shape"
        ));
    }
    Ok(Args { variant, leg, cap, ops: ops.ok_or("--ops is required")?, shape })
}

/// The plants: `NOPS` instructions per op at the top of the loop body, each
/// a `nop` with no operand, so the count must read exactly `NOPS` more. An
/// `add` on the accumulator costs a register move beside it, and a
/// `black_box` a store and a load, so neither reads as one.
#[inline(always)]
fn plant<const NOPS: u8>() {
    // With no `nop` the leg emits no `asm!` at all: A and B carry no trace
    // of the plant.
    if NOPS > 0 {
        // SAFETY: a `nop` touches no register, memory or flag.
        unsafe {
            std::arch::asm!(
                ".rept {count}",
                "nop",
                ".endr",
                count = const NOPS,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

fn half(cap: Cap) -> u64 {
    // Exact: both caps are even.
    u64::from(cap.entries_max()) / 2
}

// One op through `&mut`, as a site performs it: the step is never inlined
// into the loop, so the deque's fields are loaded and stored per op.
#[inline(never)]
fn step_vecdeque(deque: &mut VecDeque<Entry>, i: u64) -> u64 {
    deque.push_back(Entry(i));
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_vecdeque_bounded(deque: &mut VecDeque<Entry>, cap: usize, i: u64) -> u64 {
    if deque.len() < cap {
        deque.push_back(Entry(i));
    }
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_reserve(deque: &mut CappedDeque<Entry, Reserve>, i: u64) -> u64 {
    if let Reserved::Slot(slot) = deque.reserve() {
        slot.publish(Entry(i));
    }
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_coalesce(deque: &mut CappedDeque<Entry, Coalesce<Later>>, i: u64) -> u64 {
    deque.push(Entry(i));
    deque.pop_front().map_or(0, |Entry(v)| v)
}

// `NOPS` and `BOUNDED` are consts, so a plant or the bound differs from its
// leg by those instructions alone and not by a select or a branch in both.
#[inline(never)]
fn calls_vecdeque<const NOPS: u8, const BOUNDED: bool>(cap: Cap, ops: u64) -> u64 {
    let entries_max = cap.entries_max() as usize;
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(entries_max);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        accumulator ^= if BOUNDED {
            step_vecdeque_bounded(&mut deque, entries_max, i)
        } else {
            step_vecdeque(&mut deque, i)
        };
    }
    accumulator
}

#[inline(never)]
fn calls_reserve<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        accumulator ^= step_reserve(&mut deque, i);
    }
    accumulator
}

#[inline(never)]
fn calls_coalesce(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque =
        CappedDeque::<Entry, Coalesce<Later>>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        deque.push(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        accumulator ^= step_coalesce(&mut deque, i);
    }
    accumulator
}

// Each leg is one function of the same shape: a leg inlined into `main`
// and one left standalone would differ in codegen, not in the type.
#[inline(never)]
fn leg_vecdeque<const NOPS: u8, const BOUNDED: bool>(cap: Cap, ops: u64) -> u64 {
    let entries_max = cap.entries_max() as usize;
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(entries_max);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        if !BOUNDED || deque.len() < entries_max {
            deque.push_back(Entry(i));
        }
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

#[inline(never)]
fn leg_reserve<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

#[inline(never)]
fn leg_coalesce(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque =
        CappedDeque::<Entry, Coalesce<Later>>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        deque.push(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        deque.push(Entry(i));
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

// The slab row. One op publishes one entry, reads it back by its id and
// takes the oldest, whose id the queue holds: `N / 2` entries throughout.

/// The driver's operation-id table: ids minted in sequence, never reused.
type IdTable = HashMap<u64, Entry, BuildIntHasher>;

#[inline(never)]
fn step_hashmap(map: &mut IdTable, ids: &mut VecDeque<u64>, id: u64) -> u64 {
    map.insert(id, Entry(id));
    ids.push_back(id);
    let read = map.get(&id).map_or(0, |Entry(v)| *v);
    let oldest = ids.pop_front().map_or(0, |old| map.remove(&old).map_or(0, |Entry(v)| v));
    read ^ oldest
}

#[inline(never)]
fn step_slab(
    slab: &mut CappedSlab<Entry, u32, Retire>,
    ids: &mut VecDeque<SlotId<u32>>,
    i: u64,
) -> u64 {
    if let SlabReserved::Slot(slot) = slab.reserve() {
        ids.push_back(slot.publish(Entry(i)));
    }
    let read = match ids.back().map(|id| slab.get(*id)) {
        Some(Lookup::Live(Entry(v))) => *v,
        Some(Lookup::Stale | Lookup::Vacant) | None => 0,
    };
    let oldest = match ids.pop_front().map(|id| slab.take(id)) {
        Some(Taken::Value(Entry(v))) => v,
        Some(Taken::Stale | Taken::Vacant) | None => 0,
    };
    read ^ oldest
}

#[inline(never)]
fn step_ids<I: Copy>(ids: &mut VecDeque<I>, id: I) -> I {
    ids.push_back(id);
    ids.pop_front().unwrap_or(id)
}

#[inline(never)]
fn calls_hashmap<const NOPS: u8>(cap: Cap, ops: u64) -> u64 {
    let entries_max = cap.entries_max() as usize;
    let mut map = IdTable::with_capacity_and_hasher(entries_max, BuildIntHasher);
    let mut ids = VecDeque::with_capacity(entries_max);
    let mut next = 0u64;
    for _ in 0..half(cap) {
        next += 1;
        map.insert(next, Entry(next));
        ids.push_back(next);
    }
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        next += 1;
        accumulator ^= step_hashmap(&mut map, &mut ids, next);
    }
    accumulator
}

fn slab(cap: Cap, census: &Rc<CapCensus>) -> CappedSlab<Entry, u32, Retire> {
    let mut builder = CappedSlab::builder(cap, census).expect("the bench cap fits");
    loop {
        builder = match builder.step() {
            Built::More(next) => next,
            Built::Live(slab) => return slab,
            Built::Refused(_, refused) => panic!("the bench slab: {refused}"),
        };
    }
}

#[inline(never)]
fn calls_slab<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut slab = slab(cap, census);
    let mut ids = VecDeque::with_capacity(cap.entries_max() as usize);
    for i in 0..half(cap) {
        if let SlabReserved::Slot(slot) = slab.reserve() {
            ids.push_back(slot.publish(Entry(i)));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        accumulator ^= step_slab(&mut slab, &mut ids, i);
    }
    // Dropped whole here: the bench's census never serves, so the drop is
    // an abandoned assembly, uncounted.
    accumulator
}

#[inline(never)]
fn calls_ids_u64(cap: Cap, ops: u64) -> u64 {
    let mut ids: VecDeque<u64> = (0..half(cap)).collect();
    let mut accumulator = 0u64;
    for i in 0..ops {
        accumulator ^= step_ids(&mut ids, i);
    }
    accumulator
}

#[inline(never)]
fn calls_ids_slot(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    // Real handles, minted once, cycled through the queue.
    let mut slab = slab(cap, census);
    let mut ids = VecDeque::with_capacity(cap.entries_max() as usize);
    for i in 0..half(cap) {
        if let SlabReserved::Slot(slot) = slab.reserve() {
            ids.push_back(slot.publish(Entry(i)));
        }
    }
    let mut accumulator = 0u64;
    for _ in 0..ops {
        let Some(&front) = ids.front() else { break };
        accumulator ^= u64::from(step_ids(&mut ids, front).index());
    }
    accumulator
}

// The map row. One op admits and publishes one fresh key, reads it back
// and removes the oldest, whose key the queue holds: `N / 2` entries
// throughout. The keys are sparse 48-bit words, an odd multiple of the op's
// count modulo 2^48, so no two live keys are equal.

/// A table of foreign 48-bit keys under the integer hasher.
type KeyTable = HashMap<u64, Entry, BuildIntHasher>;

const KEY_MASK_48: u64 = (1 << 48) - 1;

/// The `i`th key: a bijection of the 48-bit words, so every key is fresh.
#[inline(always)]
fn sparse_key(i: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & KEY_MASK_48
}

#[inline(never)]
fn step_hashmap_keys(map: &mut KeyTable, keys: &mut VecDeque<u64>, key: u64) -> u64 {
    map.insert(key, Entry(key));
    keys.push_back(key);
    let read = map.get(&key).map_or(0, |Entry(v)| *v);
    let oldest = keys.pop_front().map_or(0, |old| map.remove(&old).map_or(0, |Entry(v)| v));
    read ^ oldest
}

#[inline(never)]
fn step_map(map: &mut CappedMap<u64, Entry>, keys: &mut VecDeque<u64>, key: u64) -> u64 {
    if let Admission::Vacant(slot) = map.admit(key) {
        slot.publish(Entry(key));
        keys.push_back(key);
    }
    let read = map.get(key).map_or(0, |Entry(v)| *v);
    let oldest = keys.pop_front().map_or(0, |old| map.remove(old).map_or(0, |Entry(v)| v));
    read ^ oldest
}

#[inline(never)]
fn calls_hashmap_keys<const NOPS: u8>(cap: Cap, ops: u64) -> u64 {
    let entries_max = cap.entries_max() as usize;
    let mut map = KeyTable::with_capacity_and_hasher(entries_max, BuildIntHasher);
    let mut keys = VecDeque::with_capacity(entries_max);
    let mut next = 0u64;
    for _ in 0..half(cap) {
        next += 1;
        let key = sparse_key(next);
        map.insert(key, Entry(key));
        keys.push_back(key);
    }
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        next += 1;
        accumulator ^= step_hashmap_keys(&mut map, &mut keys, sparse_key(next));
    }
    accumulator
}

fn capped_map(cap: Cap, census: &Rc<CapCensus>) -> CappedMap<u64, Entry> {
    let mut builder = CappedMap::builder(cap, census).expect("the bench cap fits");
    loop {
        builder = match builder.step() {
            MapBuilt::More(next) => next,
            MapBuilt::Live(map) => return map,
            MapBuilt::Refused(_, refused) => panic!("the bench map: {refused}"),
        };
    }
}

#[inline(never)]
fn calls_map<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut map = capped_map(cap, census);
    let mut keys = VecDeque::with_capacity(cap.entries_max() as usize);
    let mut next = 0u64;
    for _ in 0..half(cap) {
        next += 1;
        let key = sparse_key(next);
        if let Admission::Vacant(slot) = map.admit(key) {
            slot.publish(Entry(key));
            keys.push_back(key);
        }
    }
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        next += 1;
        accumulator ^= step_map(&mut map, &mut keys, sparse_key(next));
    }
    // Dropped whole here: the bench's census never serves, so the drop is
    // an abandoned assembly, uncounted.
    accumulator
}

#[inline(never)]
fn calls_keys_only(cap: Cap, ops: u64) -> u64 {
    let mut keys: VecDeque<u64> = (1..=half(cap)).map(sparse_key).collect();
    let mut next = half(cap);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        next += 1;
        accumulator ^= step_ids(&mut keys, sparse_key(next));
    }
    accumulator
}

fn run(args: &Args, census: &Rc<CapCensus>) -> u64 {
    let (cap, ops) = (args.cap, args.ops);
    match (args.leg, args.shape) {
        (Leg::VecDeque, Shape::Call) => calls_vecdeque::<0, false>(cap, ops),
        (Leg::VecDequeBounded, Shape::Call) => calls_vecdeque::<0, true>(cap, ops),
        (Leg::VecDequePlusOne, Shape::Call) => calls_vecdeque::<1, false>(cap, ops),
        (Leg::Reserve, Shape::Call) => calls_reserve::<0>(cap, census, ops),
        (Leg::ReservePlusOne, Shape::Call) => calls_reserve::<1>(cap, census, ops),
        (Leg::ReservePlusFour, Shape::Call) => calls_reserve::<4>(cap, census, ops),
        (Leg::Coalesce, Shape::Call) => calls_coalesce(cap, census, ops),
        (Leg::VecDeque, Shape::Loop) => leg_vecdeque::<0, false>(cap, ops),
        (Leg::VecDequeBounded, Shape::Loop) => leg_vecdeque::<0, true>(cap, ops),
        (Leg::VecDequePlusOne, Shape::Loop) => leg_vecdeque::<1, false>(cap, ops),
        (Leg::Reserve, Shape::Loop) => leg_reserve::<0>(cap, census, ops),
        (Leg::ReservePlusOne, Shape::Loop) => leg_reserve::<1>(cap, census, ops),
        (Leg::ReservePlusFour, Shape::Loop) => leg_reserve::<4>(cap, census, ops),
        (Leg::Coalesce, Shape::Loop) => leg_coalesce(cap, census, ops),
        (Leg::HashMap, _) => calls_hashmap::<0>(cap, ops),
        (Leg::HashMapPlusOne, _) => calls_hashmap::<1>(cap, ops),
        (Leg::Slab, _) => calls_slab::<0>(cap, census, ops),
        (Leg::SlabPlusOne, _) => calls_slab::<1>(cap, census, ops),
        (Leg::IdsOnlyU64, _) => calls_ids_u64(cap, ops),
        (Leg::IdsOnlySlot, _) => calls_ids_slot(cap, census, ops),
        (Leg::HashMapKeys, _) => calls_hashmap_keys::<0>(cap, ops),
        (Leg::HashMapKeysPlusOne, _) => calls_hashmap_keys::<1>(cap, ops),
        (Leg::Map, _) => calls_map::<0>(cap, census, ops),
        (Leg::MapPlusOne, _) => calls_map::<1>(cap, census, ops),
        (Leg::KeysOnly, _) => calls_keys_only(cap, ops),
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bounded: {e}");
            return ExitCode::from(2);
        }
    };
    let (census, _serve_mark) = CapCensus::new(CellId(0));
    let accumulator = run(&args, &census);
    let shape = match args.shape {
        Shape::Call => "call",
        Shape::Loop => "loop",
    };
    println!(
        "{} {shape} cap {} ops {} accumulator {}",
        args.variant,
        args.cap.entries_max(),
        args.ops,
        black_box(accumulator)
    );
    ExitCode::SUCCESS
}
