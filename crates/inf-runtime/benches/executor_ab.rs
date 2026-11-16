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
//!
//! Each row's class holds `<slots>` tasks, 2,500 or 10,000, and its future is padded so its slot
//! takes the chunk of the `conn` class's (16 slots of 2,049 to 4,096 B), so the class walks the
//! product's depth: one directory level at 2,500 slots, two at 10,000. The pad is a local held
//! uninitialized across an await: the future is that large, and nothing writes the pad.
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

use std::cell::Cell;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_runtime::{
    CellExecutor, ExecutorLayout, PollImmediate, TaskAdmission, TaskClass, TaskFactory, TaskId,
    Woken, park,
};

/// The pad that puts a row's slot in the `conn` class's chunk: a future of about 2,900 B.
const PAD_BYTES: usize = 2_900;

/// The rows' classes, at the two depths.
const SLOTS_2500: Cap = Cap::entries::<2_500>("executor-ab-2500", CapFill::Serving);
const SLOTS_10000: Cap = Cap::entries::<10_000>("executor-ab-10000", CapFill::Serving);

/// P's planted instructions per op: each row's bound plus one, the planted red its rule must
/// refuse.
const K_FAST_2500: u8 = 78;
const K_DEFERRED_2500: u8 = 52;
const K_FAST_10000: u8 = 89;
const K_DEFERRED_10000: u8 = 62;

/// `NOPS` operand-free `nop`s: the count must read exactly `NOPS` more an op.
#[inline(always)]
fn plant<const NOPS: u8>() {
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
fn step_fast_planted<const K: u8>(cell: &mut FastCell) -> bool {
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
fn step_deferred_planted<const K: u8>(cell: &mut DeferredCell) -> usize {
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
