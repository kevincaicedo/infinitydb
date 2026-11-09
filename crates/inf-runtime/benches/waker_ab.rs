#![allow(
    clippy::disallowed_types,
    reason = "benchmark: leg A's executor copy keeps its std ready queue, outside cell code"
)]
//! The waker's A/B by instructions (L4, ADR-0003 A1.5, ADR-0205): four waker paths of the cell
//! executor, each on three legs, from one binary.
//!
//! - A: the executor with an `Rc` waker, a bench-only copy in `waker_ab/executor_a.rs`. Compiled
//!   in this crate beside the steps that call it, the copy is other code than the product's own
//!   build of the same source, so A is a control, never a base: B − A is reported, never judged.
//! - B: the product, `inf_runtime::CellExecutor`.
//! - P: B's operation with each waker operation in it also driven through a wake bridge of its
//!   own, [`ForeignLane`], by `inf_foundation::wake`'s foreign entries, and drained once per op:
//!   P carries the read-modify-writes of a waker whose every operation takes the foreign lane.
//!
//! The rows, one op each (`--variant <leg>-<row>`), with the budget of the product's change in
//! instructions per op:
//!
//! - `fast` (≤ 3): `poll_immediate` of a future Ready on its first poll, which runs in the
//!   reused scratch block and makes no task.
//! - `clone-drop` (≤ 10): one clone and one drop, on the executor's thread, of the waker a
//!   parked task's poll handed out.
//! - `suspend` (≤ 16): a `FabricGate` waiter: `poll_immediate` Pending, `complete`, then
//!   `run_ready(1)`, which polls the task to Ready.
//! - `wake-run` (≤ 8): `wake_by_ref` of a parked Idle task, then `run_ready(1)`, which polls it
//!   Pending again.
//!
//! Each op is one `#[inline(never)]` step through `&mut` of a long-lived owner, as the cell's
//! loop drives its executor.
//!
//! The instrument: exact user-space instructions, cycles and locked loads per op,
//! `(I(n) − I(0)) / n` at `n` = 10⁷, read outside this binary by `perf stat` on one pinned
//! performance core, the legs of a row in the order A B P B A, twice. A row's verdict is B less
//! the B of a second build of this same source on the product before the change: two builds,
//! each in its own clean checkout, whose bench files and every input leg A compiles from are
//! checked identical before either builds. A run counts only when its controls hold:
//!
//! - the same binary against itself: A's two legs, and B's, agree within 0.05 per op, at a
//!   resolution of 0.01; across the two builds A agrees within 0.05 too, a control that can
//!   show that the builds differ, never that they are the same;
//! - B runs no locked instruction on any row; one that does is attributed by precise samples
//!   before any number is read;
//! - liveness: each P leg reads its bridge's own count of foreign operations and its delivered
//!   bridges, 2, 2 and 1 foreign operations and one delivery per op on `clone-drop`, `suspend`
//!   and `wake-run`, and exits 1 on a mismatch; the PMU counts the locked instructions those
//!   calls run. The planted leg `p-skip-suspend` runs P's checks over a half that calls
//!   nothing, and must exit 1 with no locked load above B's;
//! - engagement: after its loop each leg checks what its ops left behind (the tasks still live,
//!   the polls, the value the gate delivered last), and exits 1 if one is not as the row's ops
//!   imply. The expected foreign counts are keyed on the leg, never on its half's type.
//!
//! One leg, then one leg counted: `perf` wraps the binary that `--no-run` prints, never `cargo`,
//! whose own instructions would join the count; on a hybrid CPU `<core>` is a performance core
//! and the `cpu_core` lines count.
//!
//!   cargo bench -p inf-runtime --bench waker_ab --no-run --locked
//!   taskset -c <core> perf stat -e instructions:u,cycles:u,mem_inst_retired.lock_loads:u \
//!       -x, -- <binary> --variant <leg>-<row> --ops <n>

#[allow(dead_code, reason = "leg A is the whole executor; the rows drive four of its paths")]
#[path = "waker_ab/executor_a.rs"]
mod executor_a;

use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_foundation::wake::{RemoteStack, ThreadMark, WakeBridge};
use inf_runtime::FabricGate;

/// Tasks each leg's executor reserves room for: the rows keep at most one.
const TASKS: usize = 64;

/// The two executors behind one static interface, so a row's step is one generic function.
trait Executor {
    fn with_capacity(capacity: usize) -> Self;
    /// Whether the first poll completed the future (true) or parked it (false).
    fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> bool;
    fn run_ready(&mut self, budget: usize) -> usize;
    fn live_tasks(&self) -> usize;
}

impl Executor for executor_a::CellExecutor {
    fn with_capacity(capacity: usize) -> Self {
        executor_a::CellExecutor::new(capacity)
    }

    #[inline(always)]
    fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> bool {
        match executor_a::CellExecutor::poll_immediate(self, fut) {
            executor_a::PollImmediate::Completed => true,
            executor_a::PollImmediate::Suspended(_) => false,
        }
    }

    #[inline(always)]
    fn run_ready(&mut self, budget: usize) -> usize {
        executor_a::CellExecutor::run_ready(self, budget)
    }

    fn live_tasks(&self) -> usize {
        executor_a::CellExecutor::live_tasks(self)
    }
}

impl Executor for inf_runtime::CellExecutor {
    fn with_capacity(capacity: usize) -> Self {
        inf_runtime::CellExecutor::new(capacity)
    }

    #[inline(always)]
    fn poll_immediate<F: Future<Output = ()> + 'static>(&mut self, fut: F) -> bool {
        match inf_runtime::CellExecutor::poll_immediate(self, fut) {
            inf_runtime::PollImmediate::Completed => true,
            inf_runtime::PollImmediate::Suspended(_) => false,
        }
    }

    #[inline(always)]
    fn run_ready(&mut self, budget: usize) -> usize {
        inf_runtime::CellExecutor::run_ready(self, budget)
    }

    fn live_tasks(&self) -> usize {
        inf_runtime::CellExecutor::live_tasks(self)
    }
}

/// A leg's foreign half: one call per waker operation of B's op, and one per drain.
trait Foreign {
    fn new() -> Self;
    fn on_clone(&mut self);
    fn on_wake_by_ref(&mut self);
    fn on_wake(&mut self);
    fn on_drop(&mut self);
    fn on_drain(&mut self);
    /// The bridge's own count of foreign operations (`waker_foreign_ops`).
    fn foreign_ops(&self) -> u64;
    /// Bridges the drains delivered.
    fn delivered(&self) -> u64;
}

/// A's and B's foreign half: nothing, compiled away.
struct NoForeign;

impl Foreign for NoForeign {
    fn new() -> Self {
        NoForeign
    }
    #[inline(always)]
    fn on_clone(&mut self) {}
    #[inline(always)]
    fn on_wake_by_ref(&mut self) {}
    #[inline(always)]
    fn on_wake(&mut self) {}
    #[inline(always)]
    fn on_drop(&mut self) {}
    #[inline(always)]
    fn on_drain(&mut self) {}
    fn foreign_ops(&self) -> u64 {
        0
    }
    fn delivered(&self) -> u64 {
        0
    }
}

/// The liveness clause's planted red: run under P's expected counts, a half that calls
/// nothing must fail the leg's checks and run no locked instruction.
struct SkippedLane;

impl Foreign for SkippedLane {
    fn new() -> Self {
        SkippedLane
    }
    #[inline(always)]
    fn on_clone(&mut self) {}
    #[inline(always)]
    fn on_wake_by_ref(&mut self) {}
    #[inline(always)]
    fn on_wake(&mut self) {}
    #[inline(always)]
    fn on_drop(&mut self) {}
    #[inline(always)]
    fn on_drain(&mut self) {}
    fn foreign_ops(&self) -> u64 {
        0
    }
    fn delivered(&self) -> u64 {
        0
    }
}

/// P's foreign half: a wake bridge on a remote stack of its own, both allocated before the leg's
/// loop, driven through the foreign entries, one call per waker operation of B's op, and drained
/// once per op. Its liveness is the stack's own count of foreign operations and the bridges its
/// drains delivered, checked against the leg's counts after the loop, and the locked
/// instructions those calls run, which the PMU counts.
struct ForeignLane {
    stack: Box<RemoteStack>,
    bridge: Box<WakeBridge>,
    delivered: u64,
}

impl Foreign for ForeignLane {
    fn new() -> Self {
        let stack = Box::new(RemoteStack::new());
        let bridge = Box::new(WakeBridge::new(&stack, ThreadMark::claim()));
        ForeignLane { stack, bridge, delivered: 0 }
    }
    #[inline(always)]
    fn on_clone(&mut self) {
        self.bridge.foreign_clone(&self.stack);
    }
    #[inline(always)]
    fn on_wake_by_ref(&mut self) {
        black_box(self.bridge.foreign_wake_by_ref(&self.stack));
    }
    #[inline(always)]
    fn on_wake(&mut self) {
        let _ = black_box(self.bridge.foreign_wake(&self.stack));
    }
    #[inline(always)]
    fn on_drop(&mut self) {
        let _ = black_box(self.bridge.foreign_drop(&self.stack));
    }
    #[inline(always)]
    fn on_drain(&mut self) {
        if self.stack.pending() {
            let bridge = &*self.bridge;
            let mut drained = self.stack.take();
            while let Some((_, delivery)) = drained.deliver_next(|_| bridge) {
                black_box(delivery);
                self.delivered += 1;
            }
        }
    }
    fn foreign_ops(&self) -> u64 {
        self.stack.foreign_ops()
    }
    fn delivered(&self) -> u64 {
        self.delivered
    }
}

/// The fast row's command: a tiny read-modify-write of cell-local state.
#[inline(never)]
fn command(counter: &Cell<u64>) {
    counter.set(counter.get().wrapping_add(1));
}

/// A task that stays Pending, counts its polls and hands a clone of its waker out on its first.
struct Parked {
    waker: Rc<Cell<Option<Waker>>>,
    polls: Rc<Cell<u64>>,
}

impl Future for Parked {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        if self.polls.get() == 1 {
            self.waker.set(Some(cx.waker().clone()));
        }
        Poll::Pending
    }
}

/// An executor with one parked task, the waker its first poll handed out and its poll count.
struct ParkedCell<E> {
    exec: E,
    waker: Waker,
    polls: Rc<Cell<u64>>,
}

impl<E: Executor> ParkedCell<E> {
    fn new() -> Result<ParkedCell<E>, String> {
        let mut exec = E::with_capacity(TASKS);
        let slot = Rc::new(Cell::new(None));
        let polls = Rc::new(Cell::new(0));
        let task = Parked { waker: Rc::clone(&slot), polls: Rc::clone(&polls) };
        if exec.poll_immediate(task) {
            return Err("the parked task completed on its first poll".into());
        }
        let waker = slot.take().ok_or("the parked task's first poll handed out no waker")?;
        Ok(ParkedCell { exec, waker, polls })
    }
}

/// The fast row's owner: the executor and the counter its command bumps.
struct FastCell<E> {
    exec: E,
    counter: Rc<Cell<u64>>,
}

#[inline(never)]
fn step_fast<E: Executor>(cell: &mut FastCell<E>) -> bool {
    let counter = Rc::clone(&cell.counter);
    cell.exec.poll_immediate(async move {
        command(black_box(&counter));
    })
}

#[inline(never)]
fn step_clone_drop<E: Executor, L: Foreign>(cell: &mut ParkedCell<E>, lane: &mut L) {
    let clone = black_box(&cell.waker).clone();
    lane.on_clone();
    drop(black_box(clone));
    lane.on_drop();
    lane.on_drain();
}

/// The suspend row's owner: the executor, the gate, the next token and the value delivered last.
struct SuspendCell<E> {
    exec: E,
    gate: FabricGate<u64>,
    token: u64,
    sink: Rc<Cell<u64>>,
}

#[inline(never)]
fn step_suspend<E: Executor, L: Foreign>(cell: &mut SuspendCell<E>, lane: &mut L) -> usize {
    cell.token += 1;
    let token = cell.token;
    let waiter = cell.gate.waiter(token);
    let sink = Rc::clone(&cell.sink);
    let completed = cell.exec.poll_immediate(async move {
        sink.set(waiter.await);
    });
    lane.on_clone();
    black_box(completed);
    cell.gate.complete(token, token);
    lane.on_wake();
    let polled = cell.exec.run_ready(1);
    lane.on_drain();
    polled
}

#[inline(never)]
fn step_wake_run<E: Executor, L: Foreign>(cell: &mut ParkedCell<E>, lane: &mut L) -> usize {
    cell.waker.wake_by_ref();
    lane.on_wake_by_ref();
    let polled = cell.exec.run_ready(1);
    lane.on_drain();
    polled
}

/// What a leg left behind against what its ops imply; `Err` names the first mismatch.
fn expect(what: &str, got: u64, want: u64) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("engagement: {what} is {got}, expected {want}")) }
}

fn usize_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn drive_fast<E: Executor>(ops: u64) -> Result<(), String> {
    let mut cell = FastCell { exec: E::with_capacity(TASKS), counter: Rc::new(Cell::new(0)) };
    let mut completed = 0u64;
    for _ in 0..ops {
        completed += u64::from(step_fast(black_box(&mut cell)));
    }
    expect("fast ops completed on their first poll", completed, ops)?;
    expect("fast commands run", cell.counter.get(), ops)?;
    expect("fast live tasks", usize_count(cell.exec.live_tasks()), 0)
}

fn drive_clone_drop<E: Executor, L: Foreign>(ops: u64, per_op: u64) -> Result<(), String> {
    let mut cell = ParkedCell::<E>::new()?;
    let mut lane = L::new();
    for _ in 0..ops {
        step_clone_drop(black_box(&mut cell), &mut lane);
    }
    expect("clone-drop live tasks", usize_count(cell.exec.live_tasks()), 1)?;
    expect("clone-drop polls", cell.polls.get(), 1)?;
    expect("clone-drop foreign ops", lane.foreign_ops(), ops * 2 * per_op)?;
    expect("clone-drop delivered", lane.delivered(), ops * per_op)
}

fn drive_suspend<E: Executor, L: Foreign>(ops: u64, per_op: u64) -> Result<(), String> {
    let mut cell = SuspendCell {
        exec: E::with_capacity(TASKS),
        gate: FabricGate::new(),
        token: 0,
        sink: Rc::new(Cell::new(0)),
    };
    let mut lane = L::new();
    let mut polled = 0u64;
    for _ in 0..ops {
        polled += usize_count(step_suspend(black_box(&mut cell), &mut lane));
    }
    expect("suspend polls in run_ready", polled, ops)?;
    expect("suspend value delivered last", cell.sink.get(), ops)?;
    expect("suspend live tasks", usize_count(cell.exec.live_tasks()), 0)?;
    expect("suspend gate entries", usize_count(cell.gate.pending()), 0)?;
    expect("suspend foreign ops", lane.foreign_ops(), ops * 2 * per_op)?;
    expect("suspend delivered", lane.delivered(), ops * per_op)
}

fn drive_wake_run<E: Executor, L: Foreign>(ops: u64, per_op: u64) -> Result<(), String> {
    let mut cell = ParkedCell::<E>::new()?;
    let mut lane = L::new();
    let mut polled = 0u64;
    for _ in 0..ops {
        polled += usize_count(step_wake_run(black_box(&mut cell), &mut lane));
    }
    expect("wake-run polls in run_ready", polled, ops)?;
    expect("wake-run polls", cell.polls.get(), ops + 1)?;
    expect("wake-run live tasks", usize_count(cell.exec.live_tasks()), 1)?;
    expect("wake-run foreign ops", lane.foreign_ops(), ops * per_op)?;
    expect("wake-run delivered", lane.delivered(), ops * per_op)
}

#[derive(Clone, Copy)]
enum Leg {
    A,
    B,
    P,
    /// P's checks over [`SkippedLane`]: the liveness clause's planted red.
    PSkip,
}

#[derive(Clone, Copy)]
enum Row {
    Fast,
    CloneDrop,
    Suspend,
    WakeRun,
}

/// The one table of variant names: `--variant` is parsed against it.
const VARIANTS: [(&str, Leg, Row); 13] = [
    ("a-fast", Leg::A, Row::Fast),
    ("b-fast", Leg::B, Row::Fast),
    ("p-fast", Leg::P, Row::Fast),
    ("a-clone-drop", Leg::A, Row::CloneDrop),
    ("b-clone-drop", Leg::B, Row::CloneDrop),
    ("p-clone-drop", Leg::P, Row::CloneDrop),
    ("a-suspend", Leg::A, Row::Suspend),
    ("b-suspend", Leg::B, Row::Suspend),
    ("p-suspend", Leg::P, Row::Suspend),
    ("a-wake-run", Leg::A, Row::WakeRun),
    ("b-wake-run", Leg::B, Row::WakeRun),
    ("p-wake-run", Leg::P, Row::WakeRun),
    ("p-skip-suspend", Leg::PSkip, Row::Suspend),
];

/// `per_op` is the leg's: each foreign operation and delivery a P leg's row implies per op, 1
/// for P and its planted leg, 0 for A and B.
fn run_row<E: Executor, L: Foreign>(row: Row, ops: u64, per_op: u64) -> Result<(), String> {
    match row {
        Row::Fast => drive_fast::<E>(ops),
        Row::CloneDrop => drive_clone_drop::<E, L>(ops, per_op),
        Row::Suspend => drive_suspend::<E, L>(ops, per_op),
        Row::WakeRun => drive_wake_run::<E, L>(ops, per_op),
    }
}

fn parse() -> Result<(Leg, Row, u64), String> {
    let mut variant = None;
    let mut ops = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bench" {
            continue; // cargo bench passes it to a harness-free binary
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => {
                let Some(&(_, leg, row)) = VARIANTS.iter().find(|(name, ..)| *name == value) else {
                    let names: Vec<&str> = VARIANTS.iter().map(|(name, ..)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                variant = Some((leg, row));
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (leg, row) = variant.ok_or("--variant is required")?;
    Ok((leg, row, ops.ok_or("--ops is required")?))
}

fn main() -> ExitCode {
    let outcome = parse().and_then(|(leg, row, ops)| match leg {
        Leg::A => run_row::<executor_a::CellExecutor, NoForeign>(row, ops, 0),
        Leg::B => run_row::<inf_runtime::CellExecutor, NoForeign>(row, ops, 0),
        Leg::P => run_row::<inf_runtime::CellExecutor, ForeignLane>(row, ops, 1),
        Leg::PSkip => run_row::<inf_runtime::CellExecutor, SkippedLane>(row, ops, 1),
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("waker_ab: {why}");
            ExitCode::FAILURE
        }
    }
}
