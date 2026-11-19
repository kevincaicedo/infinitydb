//! The executor's A/B by instructions (L4, ADR-0205): the cell executor's task paths, each on
//! two legs, from one binary. A row's verdict is the product's count less the same row's count
//! on a second build of this same source at the slice's base, each in its own clean checkout.
//!
//! - B: the product, `inf_runtime::CellExecutor` through its task classes.
//! - P: B's operation with a planted body of `k` instructions inside its step, `k` the row's
//!   bound plus one: read on the base's build, P − B₀ must read `k` and be refused by the row's
//!   own rule, the rule's planted red.
//!
//! The rows, one op each (`--variant <leg>-<row>`):
//!
//! - `fast-<slots>`: `reserve` and `permit.poll_immediate` of a future Ready on its first poll.
//! - `deferred-<slots>`: work queued for a parked task, `wake(id)`, `run_ready(1)`, and the
//!   task's poll that takes the work and parks again (`park`).
//! - `suspend-<holders>`: the fabric gate's `reserve(&units)` and `register`, a task's first poll
//!   that suspends on the wait, `complete` and `run_ready(1)`, with all but one of the gate's
//!   1,024, 16,384 or 65,536 holders waiting.
//! - `keyed-<routes>`: the keyed gate's `reserve(key)` and `register` of the next counter key,
//!   `complete` of a live key drawn uniformly and that wait's poll, at 2,496 or 159,744 live
//!   routes from one counter, the draw and seed of `cold_path`'s counter legs.
//! - `keyed-full-<routes>`: the keyed row with a class queue's work for one issued read: each
//!   wait held (`register_held`), 257 probes by `waiting(&hold)` (an admit's 255 and a drain
//!   pass's two heads), and the drawn wait's `release` before its `complete`. Its P leg plants
//!   a `get` a probe on a map of this bench's own, built at the row's routes.
//! - `wait-wake`, `wait-wake-keys-15`: a wait list with one wait parked on its key, the unit key
//!   or one of 15 keys: a second wait and its poll, `wake_one`, the parked wait's poll and drop,
//!   the second parked in its place. The list takes a pool of a cell's wait nodes at 16 cells
//!   (962), and the keyed list holds a cell's 15 peers there, its waits on the one four branches
//!   deep.
//!
//! Each gate operation sits where the server places it. The fabric gate's `reserve` and
//! `register` have one caller, `send_once`, with the request's stage an opaque call between
//! them, as the plane's one attempt has. The keyed gate's `reserve`, `register` and
//! `register_held` have one caller, `admit`, and its `complete` one, `deliver`, as the cold
//! path's enqueue and completion have; `release` and the wait's poll are called from each step.
//! A keyed row's keys, waits and holds sit in arrays whose strides this file fixes, so no line
//! of the probe loop reads a size the gate sets. The loop takes a handle of the gate and the
//! holds' slice before it, so that no build reloads either after an inlined probe, and it
//! branches on each answer as the cold path's scans do: an op ends at a probe that reads its
//! wait gone. Summed, the answer would be widened by code of this file's own in one build and
//! folded into the gate's compare in the other.
//!
//! The fast and deferred rows' class holds `<slots>` tasks, 2,500 or 10,000, and its future is
//! padded so its slot takes the chunk of the `conn` class's (16 slots of 2,049 to 4,096 B), so the
//! class walks the product's depth: one directory level at 2,500 slots, two at 10,000. The pad is a
//! local held uninitialized across an await: the future is that large, and nothing writes the pad.
//!
//! Each op is one `#[inline(never)]` step through `&mut` of a long-lived owner, as the cell's loop
//! drives its executor. After its loop each leg checks what its ops left behind and exits 1 when
//! one is not as the ops imply.
//!
//! The instrument: exact user-space instructions per op, `(I(n) − I(0)) / n` at `n` = 10⁷, read
//! outside this binary by `perf stat` on one pinned performance core:
//!
//!   cargo bench -p inf-runtime --bench executor_ab --no-run --locked
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> --variant <leg>-<row> \
//!       --ops <n>

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{
    Admission, Built, Cap, CapCensus, CapFill, CappedMap, CappedSlab, MapBuilt, ServeMark,
    SlabReserved, SlotId, WrapWithProof,
};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_runtime::gate::ListWait;
use inf_runtime::{
    CellExecutor, Delivery, ExecutorLayout, FabricAdmission, FabricGate, GateHold, GateUnits,
    GateWait, KeyedAdmission, KeyedGate, KeyedWait, PollImmediate, TaskAdmission, TaskClass,
    TaskFactory, TaskId, WaitList, WaitNodes, Woken, park,
};

/// The pad that puts a row's slot in the `conn` class's chunk: a future of about 2,900 B.
const PAD_BYTES: usize = 2_900;

/// The rows' classes, at the two depths.
const SLOTS_2500: Cap = Cap::entries::<2_500>("executor-ab-2500", CapFill::Serving);
const SLOTS_10000: Cap = Cap::entries::<10_000>("executor-ab-10000", CapFill::Serving);

/// P's planted instructions per op: each row's bound plus one, the planted red its rule must
/// refuse. Each row's bound is fixed before any count of the row is read, outside this file;
/// each value copies that bound plus one and sets none of its own. The suspend rows' bounds
/// differ by size, so each size has its own. The two list rows' bounds are not fixed yet: each
/// holds 1 until its bound is, and no P leg of theirs is read before.
const K_FAST_2500: u16 = 78;
const K_DEFERRED_2500: u16 = 52;
const K_FAST_10000: u16 = 89;
const K_DEFERRED_10000: u16 = 62;
const K_SUSPEND_1024: u16 = 7;
const K_SUSPEND_16384: u16 = 2;
const K_SUSPEND_65536: u16 = 35;
const K_KEYED_2496: u16 = 1_067;
const K_KEYED_159744: u16 = 1_660;
const K_WAIT_WAKE: u16 = 1;
const K_WAIT_WAKE_KEYS_15: u16 = 1;

/// `NOPS` operand-free `nop`s: the count must read exactly `NOPS` more an op.
#[inline(always)]
fn plant<const NOPS: u16>() {
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

/// An executor of one class of `cap`, the future `factory` makes, on a census of its own.
fn executor_of<Args, Fac: TaskFactory<Args>>(
    factory: &Fac,
    cap: Cap,
) -> Result<(CellExecutor, TaskClass), String> {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let class = layout.class(factory, cap).map_err(|e| format!("the row's class: {e}"))?;
    let executor = layout.build().map_err(|e| format!("the row's executor: {e}"))?;
    Ok((executor, class))
}

// ---- The fast row ----------------------------------------------------------------------------

/// The fast row's command: a tiny read-modify-write of cell-local state.
#[inline(never)]
fn command(counter: &Cell<u64>) {
    counter.set(counter.get().wrapping_add(1));
}

/// The fast row's task: Ready on its first poll; its pad lives across a ready await.
async fn fast_task(counter: Rc<Cell<u64>>) {
    let pad = MaybeUninit::<[u8; PAD_BYTES]>::uninit();
    core::future::ready(()).await;
    black_box(&pad);
    command(black_box(&counter));
}

/// The fast row's owner: the executor, its class and the counter its command bumps.
struct FastCell {
    exec: CellExecutor,
    class: TaskClass,
    counter: Rc<Cell<u64>>,
}

/// One fast op: `reserve`, then `permit.poll_immediate`; true when the first poll completed.
#[inline(always)]
fn fast_op(cell: &mut FastCell) -> bool {
    let counter = Rc::clone(&cell.counter);
    match cell.exec.reserve(cell.class) {
        TaskAdmission::Granted(permit) => {
            matches!(permit.poll_immediate(|| fast_task(counter)), PollImmediate::Completed)
        }
        TaskAdmission::Full | TaskAdmission::Exhausted | TaskAdmission::Foreign => false,
    }
}

#[inline(never)]
fn step_fast(cell: &mut FastCell) -> bool {
    fast_op(cell)
}

#[inline(never)]
fn step_fast_planted<const K: u16>(cell: &mut FastCell) -> bool {
    plant::<K>();
    fast_op(cell)
}

fn drive_fast(cap: Cap, ops: u64, step: fn(&mut FastCell) -> bool) -> Result<(), String> {
    let (exec, class) = executor_of(&fast_task, cap)?;
    let mut cell = FastCell { exec, class, counter: Rc::new(Cell::new(0)) };
    let mut completed = 0u64;
    for _ in 0..ops {
        completed += u64::from(step(black_box(&mut cell)));
    }
    expect("fast ops completed on their first poll", completed, ops)?;
    expect("fast commands run", cell.counter.get(), ops)?;
    expect("fast live tasks", count(cell.exec.live_tasks()), 0)
}

// ---- The deferred row ------------------------------------------------------------------------

/// What the deferred row's task and its owner share: the work queued, the work taken, and the
/// park flag the task sets when it reads the queue empty.
#[derive(Default)]
struct Queue {
    queued: Cell<u64>,
    taken: Cell<u64>,
    parked: Cell<bool>,
}

/// The deferred row's task: it takes what is queued, parks when it reads the queue empty, and
/// never ends; its pad lives across every park.
async fn deferred_task(queue: Rc<Queue>) {
    let pad = MaybeUninit::<[u8; PAD_BYTES]>::uninit();
    loop {
        let work = queue.queued.replace(0);
        queue.taken.set(queue.taken.get() + work);
        queue.parked.set(true);
        park().await;
        black_box(&pad);
    }
}

/// The deferred row's owner: the executor, the parked task's id and its queue.
struct DeferredCell {
    exec: CellExecutor,
    task: TaskId,
    queue: Rc<Queue>,
}

/// One deferred op: queue one item, wake the parked task by id, and poll it once.
#[inline(always)]
fn deferred_op(cell: &mut DeferredCell) -> usize {
    cell.queue.queued.set(cell.queue.queued.get() + 1);
    if cell.queue.parked.replace(false) {
        black_box(cell.exec.wake(cell.task));
    }
    cell.exec.run_ready(1)
}

#[inline(never)]
fn step_deferred(cell: &mut DeferredCell) -> usize {
    deferred_op(cell)
}

#[inline(never)]
fn step_deferred_planted<const K: u16>(cell: &mut DeferredCell) -> usize {
    plant::<K>();
    deferred_op(cell)
}

fn drive_deferred(cap: Cap, ops: u64, step: fn(&mut DeferredCell) -> usize) -> Result<(), String> {
    let (mut exec, class) = executor_of(&deferred_task, cap)?;
    let queue = Rc::new(Queue::default());
    let task_queue = Rc::clone(&queue);
    let TaskAdmission::Granted(permit) = exec.reserve(class) else {
        return Err("the deferred row's class refused its one task".into());
    };
    let task = permit.spawn_local(|| deferred_task(task_queue));
    expect("the task's first poll", count(exec.run_ready(1)), 1)?;
    if !queue.parked.get() {
        return Err("engagement: the task did not park at its first poll".into());
    }
    let mut cell = DeferredCell { exec, task, queue };
    let mut polled = 0u64;
    for _ in 0..ops {
        polled += count(step(black_box(&mut cell)));
    }
    expect("deferred polls in run_ready", polled, ops)?;
    expect("deferred work taken", cell.queue.taken.get(), ops)?;
    expect("deferred live tasks", count(cell.exec.live_tasks()), 1)?;
    if cell.exec.wake(cell.task) == Woken::Stale {
        return Err("engagement: the parked task's id went stale".into());
    }
    Ok(())
}

// ---- The slice's interface ----------------------------------------------------------------------

/// The fabric gate of one origin over `holders` slots, every unit its holders bound issuable,
/// its parked senders' list on a pool of a cell's wait nodes.
fn fabric_gate(holders: Cap) -> Result<FabricGate<u64>, String> {
    let (census, _mark) = CapCensus::new(CellId(0));
    let nodes =
        WaitNodes::build(WAIT_NODES_962, &census).map_err(|e| format!("the row's pool: {e}"))?;
    FabricGate::build(0, holders.entries_max(), Some(holders), &census, &nodes)
        .map_err(|e| format!("the row's fabric gate: {e}"))
}

/// Delivers `value` to the fabric holder `key` names.
#[inline(always)]
fn fabric_complete(gate: &RefCell<FabricGate<u64>>, key: u64, value: u64) -> Delivery {
    FabricGate::complete(gate, key, value, Nanos(0))
}

/// A keyed gate of `cap` routes and holders.
fn keyed_gate(cap: Cap) -> Result<KeyedGate<u64, u64>, String> {
    let (census, _mark) = CapCensus::new(CellId(0));
    KeyedGate::build(cap, &census).map_err(|e| format!("the row's keyed gate: {e}"))
}

// ---- The suspend rows ---------------------------------------------------------------------------

/// The suspend rows' gates: 1,024 and 16,384 holders at one directory level, 65,536 at two.
const HOLDERS_1024: Cap = Cap::entries::<1_024>("executor-ab-holders-1024", CapFill::Serving);
const HOLDERS_16384: Cap = Cap::entries::<16_384>("executor-ab-holders-16384", CapFill::Serving);
const HOLDERS_65536: Cap = Cap::entries::<65_536>("executor-ab-holders-65536", CapFill::Serving);

/// The suspend rows' class: one task alive at a time.
const SUSPEND_SLOTS: Cap = Cap::entries::<64>("executor-ab-suspend", CapFill::Serving);

/// The suspend row's task: it awaits its gate's value and adds it to the sink.
async fn waiter_task(waiter: GateWait<'static, u64>, sink: &'static Cell<u64>) {
    let value = waiter.await;
    sink.set(sink.get().wrapping_add(value));
}

/// The suspend row's owner: the executor and its class, the gate and the task's units (both
/// borrowed by every wait for the leg's life), and the sink the tasks fill, leaked as those two
/// are so that a task's own lines take and drop no reference count.
struct SuspendCell {
    exec: CellExecutor,
    class: TaskClass,
    gate: &'static RefCell<FabricGate<u64>>,
    units: &'static GateUnits,
    sink: &'static Cell<u64>,
}

/// One send, placed as the plane's one attempt places it: the only caller of the fabric gate's
/// `reserve` and `register`, the permit held across the stage, one call it cannot see into and
/// that does not unwind past it. `None` when the gate refused or the stage did.
#[inline(never)]
fn send_once(
    cell: &SuspendCell,
    stage: &mut dyn FnMut(u64) -> bool,
) -> Option<GateWait<'static, u64>> {
    let mut gate = cell.gate.borrow_mut();
    let FabricAdmission::Granted(permit) = gate.reserve(cell.units) else { return None };
    let stops = StageStops;
    let staged = stage(permit.key());
    core::mem::forget(stops);
    if !staged {
        return None;
    }
    Some(permit.register(cell.gate, None))
}

/// The bench's own stop, as the plane's one attempt arms its own: a stage that unwinds stops the
/// process where it stands, so no unwind path of `send_once` names the permit.
struct StageStops;

impl Drop for StageStops {
    // Inlined always: as a call, the unwind path could return from it into the permit's drop.
    #[inline(always)]
    fn drop(&mut self) {
        std::process::abort();
    }
}

/// One suspend op: a holder reserved and its wait registered, a task whose first poll suspends
/// on it, the holder completed with its own key, and the task's poll that takes it; true when
/// each step answered as the op implies.
#[inline(always)]
fn suspend_op(cell: &mut SuspendCell) -> bool {
    let mut key = 0;
    let staged = send_once(cell, &mut |minted| {
        key = minted;
        true
    });
    let Some(waiter) = staged else { return false };
    let sink = cell.sink;
    let TaskAdmission::Granted(permit) = cell.exec.reserve(cell.class) else { return false };
    let first = permit.poll_immediate(|| waiter_task(waiter, sink));
    let suspended = matches!(first, PollImmediate::Suspended(_));
    let delivered = fabric_complete(cell.gate, key, key) == Delivery::Delivered;
    suspended & delivered & (cell.exec.run_ready(1) == 1)
}

#[inline(never)]
fn step_suspend(cell: &mut SuspendCell) -> bool {
    suspend_op(cell)
}

#[inline(never)]
fn step_suspend_planted<const K: u16>(cell: &mut SuspendCell) -> bool {
    plant::<K>();
    suspend_op(cell)
}

/// The gate's holders all registered but one, which each op takes and frees: the waits live for
/// the leg's life, as the gate and units they borrow do.
fn suspend_cell(holders: Cap) -> Result<SuspendCell, String> {
    let (exec, class) = executor_of(&waiter_task, SUSPEND_SLOTS)?;
    let gate: &'static RefCell<FabricGate<u64>> =
        Box::leak(Box::new(RefCell::new(fabric_gate(holders)?)));
    let units = gate.borrow().units(holders.entries_max()).ok_or("the row's units")?;
    let units: &'static GateUnits = Box::leak(Box::new(units));
    let sink: &'static Cell<u64> = Box::leak(Box::new(Cell::new(0)));
    let cell = SuspendCell { exec, class, gate, units, sink };
    let mut waiting = Vec::new();
    for _ in 1..holders.entries_max() {
        let Some(waiter) = send_once(&cell, &mut |_| true) else {
            return Err("the row's gate refused a waiting holder".into());
        };
        waiting.push(waiter);
    }
    std::mem::forget(waiting);
    Ok(cell)
}

fn drive_suspend(holders: Cap, ops: u64, step: fn(&mut SuspendCell) -> bool) -> Result<(), String> {
    let mut cell = suspend_cell(holders)?;
    let held = holders.entries_max() - 1;
    expect("holders waiting before the loop", count(cell.gate.borrow().holders()), held.into())?;
    let mut answered = 0u64;
    for _ in 0..ops {
        answered += u64::from(step(black_box(&mut cell)));
    }
    expect("suspend ops answered as implied", answered, ops)?;
    expect("suspend live tasks", count(cell.exec.live_tasks()), 0)?;
    expect("holders waiting after the loop", count(cell.gate.borrow().holders()), held.into())?;
    expect("units out after the loop", u64::from(cell.units.free()), 1)?;
    if ops > 0 && cell.sink.get() == 0 {
        return Err("engagement: no task took its value".into());
    }
    Ok(())
}

// ---- The keyed rows -----------------------------------------------------------------------------

/// The keyed rows' live routes, and their gates' caps: one more, the slot an op admits into
/// before it completes a drawn route. 2,496 is the cold gate's default, 159,744 its ceiling.
const ROUTES_2496: Cap = Cap::entries::<2_497>("executor-ab-routes-2496", CapFill::Serving);
const ROUTES_159744: Cap = Cap::entries::<159_745>("executor-ab-routes-159744", CapFill::Serving);

/// The draw's fixed seed, `cold_path`'s counter legs' own: a row and its legs complete their
/// routes in one order.
const ROUTE_SEED: u64 = 11;

/// The ops run before the counted loop, in live counts: a live key outlives them with
/// probability e⁻⁸, so the live set is the draw's steady one.
const ROUTE_WARM_UP: u64 = 8;

/// The class queue's probes for one issued read: an admit's 255 and a drain pass's two heads.
const QUEUE_PROBES: usize = 257;

/// The queued intents those probes read.
const QUEUED: usize = 256;

/// The index of the live route an op completes: `(x × N) >> 64` of the stream's next output
/// over the `N` live routes, `cold_path`'s `draw_and_swap` draw.
#[inline(always)]
fn draw(live: usize, rng: &mut SplitMix64) -> usize {
    ((u128::from(rng.next_u64()) * live as u128) >> 64) as usize
}

/// A keyed wait's value on its one poll, `None` when it is not ready.
#[inline(always)]
fn poll_keyed(wait: &mut KeyedWait<u64, u64>) -> Option<u64> {
    let mut cx = Context::from_waker(Waker::noop());
    match Pin::new(wait).poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// A keyed wait in a slot of a stride this file fixes: an index into a row's waits scales by 64
/// whatever the gate makes of its wait.
#[repr(align(64))]
struct WaitSlot(KeyedWait<u64, u64>);

/// A queue's hold in a slot of a stride this file fixes.
#[repr(align(16))]
struct HoldSlot(GateHold);

/// A keyed row's owner: the gate, its live routes, the draw and the next counter key. A route
/// is one index of `keys`, `waits` and, on the full rows, `holds`; the route an op admits sits
/// past the live ones until it replaces the drawn one.
struct KeyedCell {
    gate: KeyedGate<u64, u64>,
    keys: Vec<u64>,
    waits: Vec<WaitSlot>,
    holds: Vec<HoldSlot>,
    rng: SplitMix64,
    next: u64,
    /// Whether each route is held (the full rows): read by every op, so the hold's release
    /// has a site in each step, as the cold path calls it from several.
    held: bool,
    /// Probes that read their wait live (the full rows).
    live_probes: u64,
    /// The full rows' planted map, a route map's shape with keys `0..live` from one counter,
    /// and the planted probes that found their key (P).
    planted: Option<CappedMap<u64, SlotId<u32>>>,
    planted_probes: u64,
}

/// The next counter key pushed past the live routes, routed and registered, held when `held`:
/// the only caller of the keyed gate's `reserve`, `register` and `register_held`. The key is
/// pushed before the reservation, so that no call sits between the reservation and its
/// registration, as at the cold path's enqueue, and popped when the gate refused: false then.
#[inline(never)]
fn admit(cell: &mut KeyedCell, held: bool) -> bool {
    let key = cell.next;
    cell.next += 1;
    cell.keys.push(key);
    let KeyedAdmission::Granted(permit) = cell.gate.reserve(key) else {
        cell.keys.pop();
        return false;
    };
    if held {
        let (wait, hold) = permit.register_held();
        cell.waits.push(WaitSlot(wait));
        cell.holds.push(HoldSlot(hold));
    } else {
        let wait = permit.register();
        cell.waits.push(WaitSlot(wait));
    }
    true
}

/// Delivers `key` its own word: the only caller of the keyed gate's `complete`.
#[inline(never)]
fn deliver(gate: &KeyedGate<u64, u64>, key: u64) -> bool {
    gate.complete(key, key) == Delivery::Delivered
}

/// The drawn route replaced by the one `admit` pushed, then released when held, delivered and
/// polled: true when the poll took its key.
#[inline(always)]
fn complete_drawn(cell: &mut KeyedCell) -> bool {
    let index = draw(cell.keys.len() - 1, &mut cell.rng);
    let key = cell.keys.swap_remove(index);
    let mut wait = cell.waits.swap_remove(index);
    if cell.held {
        cell.gate.release(cell.holds.swap_remove(index).0);
    }
    deliver(&cell.gate, key) & (poll_keyed(&mut wait.0) == Some(key))
}

/// One keyed op: the next counter key reserved and registered, a drawn route completed and its
/// wait polled.
#[inline(always)]
fn keyed_op(cell: &mut KeyedCell) -> bool {
    admit(cell, false) && complete_drawn(cell)
}

/// One full-queue op: the keyed op on held routes, with the queue's probes of the first 256
/// live routes' holds (the head probed twice) before the drawn route's release; false at the
/// first probe that reads its wait gone.
#[inline(always)]
fn keyed_full_op(cell: &mut KeyedCell) -> bool {
    if !admit(cell, true) {
        return false;
    }
    let gate = cell.gate.clone();
    let holds = cell.holds.as_slice();
    for at in 0..QUEUE_PROBES {
        let hold = &holds[at % QUEUED].0;
        if !gate.waiting(hold) {
            return false;
        }
    }
    cell.live_probes += QUEUE_PROBES as u64;
    complete_drawn(cell)
}

/// P's planted body on the full rows: a `get` a probe of the queued route's index on the map
/// this file built at the row's routes, one body in both builds.
#[inline(always)]
fn planted_probes(cell: &mut KeyedCell) -> bool {
    let Some(planted) = cell.planted.as_ref() else { return false };
    let mut found = 0u64;
    for probe in 0..QUEUE_PROBES {
        found += u64::from(planted.get((probe % QUEUED) as u64).is_some());
    }
    cell.planted_probes += found;
    true
}

#[inline(never)]
fn step_keyed(cell: &mut KeyedCell) -> bool {
    keyed_op(cell)
}

#[inline(never)]
fn step_keyed_planted<const K: u16>(cell: &mut KeyedCell) -> bool {
    plant::<K>();
    keyed_op(cell)
}

#[inline(never)]
fn step_keyed_full(cell: &mut KeyedCell) -> bool {
    keyed_full_op(cell)
}

/// P of the full rows: the planted probes, then B's own step, called, so that the gate's
/// `waiting` keeps the one site the queue's probe loop gives it.
#[inline(never)]
fn step_keyed_full_planted(cell: &mut KeyedCell) -> bool {
    planted_probes(cell) && step_keyed_full(cell)
}

/// One slot handle, minted by a slab of one slot: the value every planted key maps to.
fn planted_handle() -> Result<SlotId<u32>, String> {
    const ONE: Cap = Cap::entries::<1>("executor-ab-planted-handle", CapFill::Serving);
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut builder = CappedSlab::<u64, u32, WrapWithProof>::builder(ONE, &census)
        .map_err(|e| format!("the planted handle's slab: {e}"))?;
    let mut slab = loop {
        builder = match builder.step() {
            Built::More(next) => next,
            Built::Live(slab) => break slab,
            Built::Refused(_, refused) => return Err(format!("the planted handle: {refused}")),
        };
    };
    match slab.reserve() {
        SlabReserved::Slot(slot) => Ok(slot.publish(0)),
        SlabReserved::Full | SlabReserved::Exhausted => Err("the planted handle's slot".into()),
    }
}

/// A route map of `cap` with keys `0..live` from one counter, each mapped to one handle: the
/// full rows' planted body walks it.
fn planted_map(cap: Cap) -> Result<CappedMap<u64, SlotId<u32>>, String> {
    let handle = planted_handle()?;
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut builder =
        CappedMap::builder(cap, &census).map_err(|e| format!("the planted map: {e}"))?;
    let mut map = loop {
        builder = match builder.step() {
            MapBuilt::More(next) => next,
            MapBuilt::Live(map) => break map,
            MapBuilt::Refused(_, refused) => return Err(format!("the planted map: {refused}")),
        };
    };
    for key in 0..u64::from(cap.entries_max() - 1) {
        let Admission::Vacant(slot) = map.admit(key) else {
            return Err("the planted map refused a counter key".into());
        };
        slot.publish(handle);
    }
    Ok(map)
}

/// A keyed row's gate with its cap's routes but one live, keys `0..live` from the counter, each
/// held on the full rows, whose planted map is built beside it.
fn keyed_cell(cap: Cap, held: bool) -> Result<KeyedCell, String> {
    let gate = keyed_gate(cap)?;
    let live = cap.entries_max() - 1;
    let slots = live as usize + 1;
    let mut cell = KeyedCell {
        gate,
        keys: Vec::with_capacity(slots),
        waits: Vec::with_capacity(slots),
        holds: Vec::with_capacity(if held { slots } else { 0 }),
        rng: SplitMix64::new(ROUTE_SEED),
        next: 0,
        held,
        live_probes: 0,
        planted: if held { Some(planted_map(cap)?) } else { None },
        planted_probes: 0,
    };
    for _ in 0..live {
        if !admit(&mut cell, held) {
            return Err("the row's gate refused a live route".into());
        }
    }
    Ok(cell)
}

/// What a keyed leg's probes read: nothing, the holds alone (B), or the planted map too (P).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Probes {
    None,
    Holds,
    Planted,
}

fn drive_keyed(
    cap: Cap,
    probes: Probes,
    ops: u64,
    step: fn(&mut KeyedCell) -> bool,
) -> Result<(), String> {
    let mut cell = keyed_cell(cap, probes != Probes::None)?;
    let live = u64::from(cap.entries_max() - 1);
    for _ in 0..ROUTE_WARM_UP * live {
        if !step(black_box(&mut cell)) {
            return Err("engagement: a warm-up op did not answer as implied".into());
        }
    }
    cell.live_probes = 0;
    cell.planted_probes = 0;
    let mut answered = 0u64;
    for _ in 0..ops {
        answered += u64::from(step(black_box(&mut cell)));
    }
    expect("keyed ops answered as implied", answered, ops)?;
    expect("live routes after the loop", count(cell.keys.len()), live)?;
    expect("routes after the loop", count(cell.gate.routes()), live)?;
    expect("holders after the loop", count(cell.gate.pending()), live)?;
    let probed = ops * QUEUE_PROBES as u64;
    let held = if probes == Probes::None { 0 } else { probed };
    expect("probes that read a live wait", cell.live_probes, held)?;
    let planted = if probes == Probes::Planted { probed } else { 0 };
    expect("planted probes that found their key", cell.planted_probes, planted)
}

// ---- The list rows -----------------------------------------------------------------------------

/// The list rows' pool: a cell's wait nodes at 16 cells, one for each of its 962 tasks.
const WAIT_NODES_962: Cap = Cap::entries::<962>("executor-ab-wait-nodes", CapFill::Serving);

/// The keyed row's keys: a cell's 15 peers at 16 cells, every one held before the cell serves.
const PEER_KEYS_15: Cap = Cap::entries::<15>("executor-ab-peer-keys", CapFill::Assembly);

/// The key the keyed row waits on: four branches deep among the 15.
const PEER_KEY: CellId = CellId(7);

/// A list row's owner: the pool, the list, its one key and the wait parked on it throughout.
/// Generic over the list and wait types themselves, so this file names no bound of the gate's.
struct ListCell<L, W, K> {
    nodes: WaitNodes,
    list: L,
    key: K,
    parked: Option<W>,
}

type UnitListCell = ListCell<WaitList<()>, ListWait<()>, ()>;
type KeyedListCell = ListCell<WaitList<CellId>, ListWait<CellId>, CellId>;

/// The list rows' op, one body for each key type: a wait and its poll, `wake_one`, the parked
/// wait's poll and drop, the new wait parked in its place. Answers whether each did as implied.
macro_rules! wait_wake_op {
    ($cell:expr) => {{
        let cell = $cell;
        let mut cx = Context::from_waker(Waker::noop());
        let mut wait = cell.list.wait(cell.key);
        let pending = Pin::new(&mut wait).poll(&mut cx).is_pending();
        let woken = cell.list.wake_one(cell.key);
        match cell.parked.replace(wait) {
            Some(mut parked) => {
                let ready = Pin::new(&mut parked).poll(&mut cx).is_ready();
                drop(parked);
                pending && woken && ready
            }
            None => false,
        }
    }};
}

#[inline(never)]
fn step_wait_wake(cell: &mut UnitListCell) -> bool {
    wait_wake_op!(cell)
}

#[inline(never)]
fn step_wait_wake_planted<const K: u16>(cell: &mut UnitListCell) -> bool {
    plant::<K>();
    wait_wake_op!(cell)
}

#[inline(never)]
fn step_wait_wake_keys_15(cell: &mut KeyedListCell) -> bool {
    wait_wake_op!(cell)
}

#[inline(never)]
fn step_wait_wake_keys_15_planted<const K: u16>(cell: &mut KeyedListCell) -> bool {
    plant::<K>();
    wait_wake_op!(cell)
}

/// The list rows' pool on a census of its own, and the census's serve mark.
fn list_pool() -> Result<(WaitNodes, Rc<CapCensus>, ServeMark), String> {
    let (census, serve_mark) = CapCensus::new(CellId(0));
    let nodes =
        WaitNodes::build(WAIT_NODES_962, &census).map_err(|e| format!("the row's pool: {e}"))?;
    Ok((nodes, census, serve_mark))
}

/// A list row's ops on `$cell`, then what they left: every op as implied, one wait parked, the
/// pool's nodes back at their count before the loop, and no first poll that found no node.
macro_rules! drive_list {
    ($cell:expr, $ops:expr, $step:expr) => {{
        let (mut cell, ops) = ($cell, $ops);
        let start = cell.nodes.len();
        let mut answered = 0u64;
        for _ in 0..ops {
            answered += u64::from($step(black_box(&mut cell)));
        }
        expect("list ops answered as implied", answered, ops)?;
        if cell.list.waiting() != 1 {
            return Err("engagement: one wait is not left parked after the loop".into());
        }
        expect("the pool's nodes after the loop", u64::from(cell.nodes.len()), u64::from(start))?;
        expect("first polls that found no node", cell.nodes.full(), 0)
    }};
}

/// The unit list's row: its list on the pool, its first wait parked, the cell serving.
fn drive_unit_list(ops: u64, step: fn(&mut UnitListCell) -> bool) -> Result<(), String> {
    let (nodes, _census, serve_mark) = list_pool()?;
    let list = WaitList::unit(&nodes);
    serve_mark.take();
    let mut parked = list.wait(());
    if !Pin::new(&mut parked).poll(&mut Context::from_waker(Waker::noop())).is_pending() {
        return Err("engagement: the first wait did not park".into());
    }
    drive_list!(ListCell { nodes, list, key: (), parked: Some(parked) }, ops, step)
}

/// The keyed list's row: every peer key held before the cell serves, its first wait parked,
/// and no key admitted while it serves.
fn drive_keyed_list(ops: u64, step: fn(&mut KeyedListCell) -> bool) -> Result<(), String> {
    let (nodes, census, serve_mark) = list_pool()?;
    let peers = (1..=15).map(CellId);
    let list = WaitList::keyed(&nodes, Some(PEER_KEYS_15), &census, peers)
        .map_err(|e| format!("the row's keyed list: {e}"))?;
    serve_mark.take();
    let mut parked = list.wait(PEER_KEY);
    if !Pin::new(&mut parked).poll(&mut Context::from_waker(Waker::noop())).is_pending() {
        return Err("engagement: the first wait did not park".into());
    }
    drive_list!(ListCell { nodes, list, key: PEER_KEY, parked: Some(parked) }, ops, step)?;
    expect("keys admitted while serving", u64::from(census.cap_assembly_violations()), 0)
}

// ---- The table and the driver ------------------------------------------------------------------

/// What a leg left behind against what its ops imply; `Err` names the first mismatch.
fn expect(what: &str, got: u64, want: u64) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("engagement: {what} is {got}, expected {want}")) }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// One leg of one row.
type Drive = fn(u64) -> Result<(), String>;

/// The one table of variant names: `--variant` is parsed against it.
const VARIANTS: &[(&str, Drive)] = &[
    ("b-fast-2500", |ops| drive_fast(SLOTS_2500, ops, step_fast)),
    ("p-fast-2500", |ops| drive_fast(SLOTS_2500, ops, step_fast_planted::<K_FAST_2500>)),
    ("b-deferred-2500", |ops| drive_deferred(SLOTS_2500, ops, step_deferred)),
    ("p-deferred-2500", |ops| {
        drive_deferred(SLOTS_2500, ops, step_deferred_planted::<K_DEFERRED_2500>)
    }),
    ("b-fast-10000", |ops| drive_fast(SLOTS_10000, ops, step_fast)),
    ("p-fast-10000", |ops| drive_fast(SLOTS_10000, ops, step_fast_planted::<K_FAST_10000>)),
    ("b-deferred-10000", |ops| drive_deferred(SLOTS_10000, ops, step_deferred)),
    ("p-deferred-10000", |ops| {
        drive_deferred(SLOTS_10000, ops, step_deferred_planted::<K_DEFERRED_10000>)
    }),
    ("b-suspend-1024", |ops| drive_suspend(HOLDERS_1024, ops, step_suspend)),
    ("p-suspend-1024", |ops| {
        drive_suspend(HOLDERS_1024, ops, step_suspend_planted::<K_SUSPEND_1024>)
    }),
    ("b-suspend-16384", |ops| drive_suspend(HOLDERS_16384, ops, step_suspend)),
    ("p-suspend-16384", |ops| {
        drive_suspend(HOLDERS_16384, ops, step_suspend_planted::<K_SUSPEND_16384>)
    }),
    ("b-suspend-65536", |ops| drive_suspend(HOLDERS_65536, ops, step_suspend)),
    ("p-suspend-65536", |ops| {
        drive_suspend(HOLDERS_65536, ops, step_suspend_planted::<K_SUSPEND_65536>)
    }),
    ("b-keyed-2496", |ops| drive_keyed(ROUTES_2496, Probes::None, ops, step_keyed)),
    ("p-keyed-2496", |ops| {
        drive_keyed(ROUTES_2496, Probes::None, ops, step_keyed_planted::<K_KEYED_2496>)
    }),
    ("b-keyed-159744", |ops| drive_keyed(ROUTES_159744, Probes::None, ops, step_keyed)),
    ("p-keyed-159744", |ops| {
        drive_keyed(ROUTES_159744, Probes::None, ops, step_keyed_planted::<K_KEYED_159744>)
    }),
    ("b-keyed-full-2496", |ops| drive_keyed(ROUTES_2496, Probes::Holds, ops, step_keyed_full)),
    ("p-keyed-full-2496", |ops| {
        drive_keyed(ROUTES_2496, Probes::Planted, ops, step_keyed_full_planted)
    }),
    ("b-keyed-full-159744", |ops| drive_keyed(ROUTES_159744, Probes::Holds, ops, step_keyed_full)),
    ("p-keyed-full-159744", |ops| {
        drive_keyed(ROUTES_159744, Probes::Planted, ops, step_keyed_full_planted)
    }),
    ("b-wait-wake", |ops| drive_unit_list(ops, step_wait_wake)),
    ("p-wait-wake", |ops| drive_unit_list(ops, step_wait_wake_planted::<K_WAIT_WAKE>)),
    ("b-wait-wake-keys-15", |ops| drive_keyed_list(ops, step_wait_wake_keys_15)),
    ("p-wait-wake-keys-15", |ops| {
        drive_keyed_list(ops, step_wait_wake_keys_15_planted::<K_WAIT_WAKE_KEYS_15>)
    }),
];

fn parse() -> Result<(Drive, u64), String> {
    let mut args = std::env::args().skip(1);
    let (mut drive, mut ops) = (None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => {
                let Some(&(_, found)) = VARIANTS.iter().find(|(name, _)| *name == value) else {
                    let names: Vec<&str> = VARIANTS.iter().map(|(name, _)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                drive = Some(found);
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok((drive.ok_or("--variant is required")?, ops.ok_or("--ops is required")?))
}

fn main() -> ExitCode {
    let run = parse().and_then(|(drive, ops)| drive(ops));
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("executor_ab: {why}");
            ExitCode::FAILURE
        }
    }
}
