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
//!   pass's two heads), and the drawn wait's `release` before its `complete`. Its P leg probes
//!   through the route instead (`reserve` of a live key, answered `Duplicate`).
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
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_runtime::{
    CellExecutor, Delivery, ExecutorLayout, FabricAdmission, FabricGate, GateHold, GateUnits,
    GateWait, KeyedAdmission, KeyedGate, KeyedWait, PollImmediate, TaskAdmission, TaskClass,
    TaskFactory, TaskId, Woken, park,
};

/// The pad that puts a row's slot in the `conn` class's chunk: a future of about 2,900 B.
const PAD_BYTES: usize = 2_900;

/// The rows' classes, at the two depths.
const SLOTS_2500: Cap = Cap::entries::<2_500>("executor-ab-2500", CapFill::Serving);
const SLOTS_10000: Cap = Cap::entries::<10_000>("executor-ab-10000", CapFill::Serving);

/// P's planted instructions per op: each row's bound plus one, the planted red its rule must
/// refuse. The suspend rows' bound is 0; the keyed rows' is at most their ceiling, so their
/// plant is the ceiling plus one.
const K_FAST_2500: u16 = 78;
const K_DEFERRED_2500: u16 = 52;
const K_FAST_10000: u16 = 89;
const K_DEFERRED_10000: u16 = 62;
const K_SUSPEND: u16 = 1;
const K_KEYED_2496: u16 = 1_234;
const K_KEYED_159744: u16 = 2_107;

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

/// The fabric gate of one origin over `holders` slots, every unit its holders bound issuable.
fn fabric_gate(holders: Cap) -> Result<FabricGate<u64>, String> {
    let (census, _mark) = CapCensus::new(CellId(0));
    FabricGate::build(0, holders.entries_max(), Some(holders), &census)
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
async fn waiter_task(waiter: GateWait<'static, u64>, sink: Rc<Cell<u64>>) {
    let value = waiter.await;
    sink.set(sink.get().wrapping_add(value));
}

/// The suspend row's owner: the executor and its class, the gate and the task's units (both
/// borrowed by every wait for the leg's life), and the sink the tasks fill.
struct SuspendCell {
    exec: CellExecutor,
    class: TaskClass,
    gate: &'static RefCell<FabricGate<u64>>,
    units: &'static GateUnits,
    sink: Rc<Cell<u64>>,
}

/// One suspend op: a holder reserved and its wait registered, a task whose first poll suspends
/// on it, the holder completed with its own key, and the task's poll that takes it; true when
/// each step answered as the op implies.
#[inline(always)]
fn suspend_op(cell: &mut SuspendCell) -> bool {
    let (key, waiter) = {
        let mut gate = cell.gate.borrow_mut();
        let FabricAdmission::Granted(permit) = gate.reserve(cell.units) else { return false };
        let key = permit.key();
        (key, permit.register(cell.gate, None))
    };
    let sink = Rc::clone(&cell.sink);
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
    let mut waiting = Vec::new();
    for _ in 1..holders.entries_max() {
        let mut open = gate.borrow_mut();
        let FabricAdmission::Granted(permit) = open.reserve(units) else {
            return Err("the row's gate refused a waiting holder".into());
        };
        waiting.push(permit.register(gate, None));
    }
    std::mem::forget(waiting);
    Ok(SuspendCell { exec, class, gate, units, sink: Rc::new(Cell::new(0)) })
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

/// One live route of a keyed row: its key, its wait and, on the full rows, its hold.
struct Route {
    key: u64,
    wait: KeyedWait<u64, u64>,
    hold: Option<GateHold>,
}

/// A keyed row's owner: the gate, its live routes, the draw and the next counter key.
struct KeyedCell {
    gate: KeyedGate<u64, u64>,
    routes: Vec<Route>,
    rng: SplitMix64,
    next: u64,
    /// Probes that read their wait live (the full rows).
    live_probes: u64,
}

/// The next counter key routed and registered, held when `held`.
#[inline(always)]
fn admit(cell: &mut KeyedCell, held: bool) -> Option<Route> {
    let key = cell.next;
    cell.next += 1;
    let KeyedAdmission::Granted(permit) = cell.gate.reserve(key) else { return None };
    Some(if held {
        let (wait, hold) = permit.register_held();
        Route { key, wait, hold: Some(hold) }
    } else {
        Route { key, wait: permit.register(), hold: None }
    })
}

/// The drawn route replaced by `route`, then released when held, completed with its key and
/// polled: true when the poll took that key.
#[inline(always)]
fn complete_drawn(cell: &mut KeyedCell, route: Route) -> bool {
    let index = draw(cell.routes.len(), &mut cell.rng);
    let mut drawn = std::mem::replace(&mut cell.routes[index], route);
    if let Some(hold) = drawn.hold.take() {
        cell.gate.release(hold);
    }
    let delivered = cell.gate.complete(drawn.key, drawn.key) == Delivery::Delivered;
    delivered & (poll_keyed(&mut drawn.wait) == Some(drawn.key))
}

/// One keyed op: the next counter key reserved and registered, a drawn route completed and its
/// wait polled.
#[inline(always)]
fn keyed_op(cell: &mut KeyedCell) -> bool {
    let Some(route) = admit(cell, false) else { return false };
    complete_drawn(cell, route)
}

/// One full-queue op: the keyed op on held routes, with the queue's probes of the first 256
/// live routes' holds (the head probed twice) before the drawn route's release.
#[inline(always)]
fn keyed_full_op(cell: &mut KeyedCell) -> bool {
    let Some(route) = admit(cell, true) else { return false };
    let mut live = 0u64;
    for probe in 0..QUEUE_PROBES {
        let queued = &cell.routes[probe % QUEUED];
        live += u64::from(queued.hold.as_ref().is_some_and(|hold| cell.gate.waiting(hold)));
    }
    cell.live_probes += live;
    complete_drawn(cell, route)
}

/// P of the full rows: the same op with each probe resolved through the route, a `reserve` of
/// the queued route's live key, answered `Duplicate`.
#[inline(always)]
fn keyed_full_route_op(cell: &mut KeyedCell) -> bool {
    let Some(route) = admit(cell, true) else { return false };
    let mut live = 0u64;
    for probe in 0..QUEUE_PROBES {
        let key = cell.routes[probe % QUEUED].key;
        live += u64::from(matches!(cell.gate.reserve(key), KeyedAdmission::Duplicate));
    }
    cell.live_probes += live;
    complete_drawn(cell, route)
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

#[inline(never)]
fn step_keyed_full_route(cell: &mut KeyedCell) -> bool {
    keyed_full_route_op(cell)
}

/// A keyed row's gate with its cap's routes but one live, keys `0..live` from the counter, each
/// held on the full rows.
fn keyed_cell(cap: Cap, held: bool) -> Result<KeyedCell, String> {
    let gate = keyed_gate(cap)?;
    let live = cap.entries_max() - 1;
    let mut cell = KeyedCell {
        gate,
        routes: Vec::with_capacity(live as usize),
        rng: SplitMix64::new(ROUTE_SEED),
        next: 0,
        live_probes: 0,
    };
    for _ in 0..live {
        let route = admit(&mut cell, held).ok_or("the row's gate refused a live route")?;
        cell.routes.push(route);
    }
    Ok(cell)
}

fn drive_keyed(
    cap: Cap,
    held: bool,
    ops: u64,
    step: fn(&mut KeyedCell) -> bool,
) -> Result<(), String> {
    let mut cell = keyed_cell(cap, held)?;
    let live = u64::from(cap.entries_max() - 1);
    for _ in 0..ROUTE_WARM_UP * live {
        if !step(black_box(&mut cell)) {
            return Err("engagement: a warm-up op did not answer as implied".into());
        }
    }
    cell.live_probes = 0;
    let mut answered = 0u64;
    for _ in 0..ops {
        answered += u64::from(step(black_box(&mut cell)));
    }
    expect("keyed ops answered as implied", answered, ops)?;
    expect("routes after the loop", count(cell.gate.routes()), live)?;
    expect("holders after the loop", count(cell.gate.pending()), live)?;
    let probes = if held { ops * QUEUE_PROBES as u64 } else { 0 };
    expect("probes that read a live wait", cell.live_probes, probes)
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
    ("p-suspend-1024", |ops| drive_suspend(HOLDERS_1024, ops, step_suspend_planted::<K_SUSPEND>)),
    ("b-suspend-16384", |ops| drive_suspend(HOLDERS_16384, ops, step_suspend)),
    ("p-suspend-16384", |ops| drive_suspend(HOLDERS_16384, ops, step_suspend_planted::<K_SUSPEND>)),
    ("b-suspend-65536", |ops| drive_suspend(HOLDERS_65536, ops, step_suspend)),
    ("p-suspend-65536", |ops| drive_suspend(HOLDERS_65536, ops, step_suspend_planted::<K_SUSPEND>)),
    ("b-keyed-2496", |ops| drive_keyed(ROUTES_2496, false, ops, step_keyed)),
    ("p-keyed-2496", |ops| {
        drive_keyed(ROUTES_2496, false, ops, step_keyed_planted::<K_KEYED_2496>)
    }),
    ("b-keyed-159744", |ops| drive_keyed(ROUTES_159744, false, ops, step_keyed)),
    ("p-keyed-159744", |ops| {
        drive_keyed(ROUTES_159744, false, ops, step_keyed_planted::<K_KEYED_159744>)
    }),
    ("b-keyed-full-2496", |ops| drive_keyed(ROUTES_2496, true, ops, step_keyed_full)),
    ("p-keyed-full-2496", |ops| drive_keyed(ROUTES_2496, true, ops, step_keyed_full_route)),
    ("b-keyed-full-159744", |ops| drive_keyed(ROUTES_159744, true, ops, step_keyed_full)),
    ("p-keyed-full-159744", |ops| drive_keyed(ROUTES_159744, true, ops, step_keyed_full_route)),
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
