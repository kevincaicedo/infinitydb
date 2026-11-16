//! The fast path's executor share by instructions: each shape's fast op less its future's direct
//! poll, the two shapes' shares compared within 4 instructions an op before a padded shape's time
//! is read against the fast-path gate beside the bench class's (`executor.rs`). A control that
//! reads high lowers a share and leaves the band; a walk returning, a virtual call or a slot
//! copied raises one and leaves it.
//!
//! The legs, one a process (`--leg <name> --ops <n>`), each a fixed count of one op in one loop:
//!
//! - `fast`: the bench class's fast op, `reserve` and `permit.poll_immediate` Ready on its first
//!   poll; `direct`: its future polled once in a stack slot with no executor.
//! - `fast-padded-2500`, `fast-padded-10000`: the padded future's fast op, its class at one
//!   directory level and at two; `direct-padded`: the padded future polled directly.
//! - `relend-padded-2500`, the planted red: `class_tasks` of the padded class, then its fast op.
//!   The read borrows the class's slab, which forgets the kept lend, so the op re-lends by
//!   `get_mut`; its share must leave the band.
//! - `borrow-other-padded-2500`, the plant's control: the same executor and op with
//!   `class_tasks` of its other class, which keeps the padded class's lend; the plant less this
//!   leg is the re-lend alone.
//!
//! The futures, factories and fast step are `support/fast.rs`'s, which `executor.rs` times. After
//! its loop each leg checks what its ops left behind and exits 1 when one is not as they imply.
//!
//! The instrument: exact user-space instructions an op, `(I(n) − I(0)) / n` at `n` = 10⁸, each
//! count read outside this binary by `perf stat`, alone, on one pinned performance core:
//!
//!   cargo bench -p inf-runtime --bench executor_counts --no-run --locked
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> --leg <name> --ops <n>

use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_runtime::{CellExecutor, ExecutorLayout, TaskClass};

#[path = "support/fast.rs"]
mod fast;

use fast::{
    PADDED_2500, PADDED_10000, TASKS, command_task, executor_with, padded_task, poll_direct,
    ready_once,
};

/// One leg's ops in one loop: `op` is inlined into this leg's own copy, as criterion inlines an
/// iteration's closure, so a shape's fast leg and its direct leg pay the same loop.
#[inline(never)]
fn run_loop(ops: u64, mut op: impl FnMut()) {
    for _ in 0..ops {
        op();
    }
}

/// An executor of the padded class at 2,500 slots and the bench class beside it.
fn executor_padded_and_other() -> Result<(CellExecutor, TaskClass, TaskClass), String> {
    let (census, _mark) = CapCensus::new(CellId(0));
    let mut layout = ExecutorLayout::new(&census);
    let padded = layout.class(&padded_task, PADDED_2500).map_err(|e| format!("padded: {e}"))?;
    let other = layout.class(&command_task, TASKS).map_err(|e| format!("other: {e}"))?;
    let executor = layout.build().map_err(|e| format!("the executor: {e}"))?;
    Ok((executor, padded, other))
}

/// What a leg left behind against what its ops imply; `Err` names the first mismatch.
fn expect(what: &str, got: u64, want: u64) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("engagement: {what} is {got}, expected {want}")) }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A fast op's leg on `ex`'s `class`, the future `make` builds around the counter: every op
/// completes on its first poll and runs its command once. `make` is a factory's own type, so
/// the op calls it directly, as the bench's closure does.
fn drive_fast<F: Future<Output = ()> + 'static>(
    (mut ex, class): (CellExecutor, TaskClass),
    ops: u64,
    make: impl Fn(Rc<Cell<u64>>) -> F,
) -> Result<(), String> {
    let counter = Rc::new(Cell::new(0u64));
    run_loop(ops, || {
        let counter = Rc::clone(&counter);
        ready_once(&mut ex, class, || make(counter));
    });
    expect("commands run", counter.get(), ops)?;
    expect("live tasks", count(ex.live_tasks()), 0)
}

/// A direct poll's leg: the future `make` builds, polled once in a stack slot.
fn drive_direct<F: Future<Output = ()>>(
    ops: u64,
    make: impl Fn(Rc<Cell<u64>>) -> F,
) -> Result<(), String> {
    let counter = Rc::new(Cell::new(0u64));
    run_loop(ops, || poll_direct(make(Rc::clone(&counter))));
    expect("commands run", counter.get(), ops)
}

/// The plant and its control: `class_tasks` of `read` (`relend`: the padded class; else the
/// other class), then the padded fast op. Neither class counts a task held between ops.
fn drive_after_read(ops: u64, relend: bool) -> Result<(), String> {
    let (mut ex, padded, other) = executor_padded_and_other()?;
    let read = if relend { padded } else { other };
    let counter = Rc::new(Cell::new(0u64));
    let mut held = 0u64;
    run_loop(ops, || {
        held += u64::from(black_box(ex.class_tasks(read)));
        let counter = Rc::clone(&counter);
        ready_once(&mut ex, padded, || padded_task(counter));
    });
    expect("commands run", counter.get(), ops)?;
    expect("tasks counted between ops", held, 0)?;
    expect("live tasks", count(ex.live_tasks()), 0)
}

/// One leg: its ops, then its check.
type Drive = fn(u64) -> Result<(), String>;

/// The one table of leg names: `--leg` is parsed against it.
const LEGS: &[(&str, Drive)] = &[
    ("fast", |ops| drive_fast(executor_with(&command_task, TASKS), ops, command_task)),
    ("direct", |ops| drive_direct(ops, command_task)),
    ("fast-padded-2500", |ops| {
        drive_fast(executor_with(&padded_task, PADDED_2500), ops, padded_task)
    }),
    ("fast-padded-10000", |ops| {
        drive_fast(executor_with(&padded_task, PADDED_10000), ops, padded_task)
    }),
    ("direct-padded", |ops| drive_direct(ops, padded_task)),
    ("relend-padded-2500", |ops| drive_after_read(ops, true)),
    ("borrow-other-padded-2500", |ops| drive_after_read(ops, false)),
];

fn parse() -> Result<(Drive, u64), String> {
    let mut args = std::env::args().skip(1);
    let (mut drive, mut ops) = (None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--leg" => {
                let Some(&(_, found)) = LEGS.iter().find(|(name, _)| *name == value) else {
                    let names: Vec<&str> = LEGS.iter().map(|(name, _)| *name).collect();
                    return Err(format!("--leg {value}: one of {}", names.join(", ")));
                };
                drive = Some(found);
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok((drive.ok_or("--leg is required")?, ops.ok_or("--ops is required")?))
}

fn main() -> ExitCode {
    let run = parse().and_then(|(drive, ops)| drive(ops));
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("executor_counts: {why}");
            ExitCode::FAILURE
        }
    }
}
