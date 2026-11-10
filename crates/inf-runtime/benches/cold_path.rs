#![allow(
    clippy::disallowed_types,
    reason = "bench target: leg A is the cold-read host and its gate with their std tables, \
              the deep legs keep their waits in a deque reserved before the loop, and the rate \
              legs measure leg A's tables"
)]
//! The cold-read path's A/B by instructions (L4): the cell's cold-read
//! operations on the host as it stood before its fixed stores (A, a
//! bench-only copy in `cold_path/cold.rs` over its gate's in
//! `cold_path/gate.rs`, so every A leg measures the same code after the
//! engine's host and gate change) and, once they are built, on the
//! fixed stores (B), from one binary. Each leg's background is held by the
//! bench's device budget, which answers `NotThisSlice` to every maintain
//! offer and `Now` to the foreground, so no background read issues.
//!
//! The A legs, one op each:
//!
//! - `enqueue-cancel-64`, `enqueue-cancel-16384`: one foreground request
//!   enqueued, its wait dropped, and one drain pass, which purges it at its
//!   class head and issues nothing. The background: 64 maintain requests
//!   at a device depth of 64, or 16,384 (the class queue's cap) at 4,096,
//!   one per even-numbered file; the request is on an odd file between two
//!   live ones.
//! - `singleton-empty`, `singleton-512`, `singleton-16384`: one foreground
//!   request enqueued, one drain pass that issues it, its receipt, its
//!   value polled and dropped. The background: none at a depth of 128; 512
//!   maintain requests (the class queue's cap) at 128; 16,384 at 4,096.
//! - `cohort-empty`, `cohort-512`, `cohort-16384`: the same with 31
//!   adjacent foreground requests on one file, merged into one device read
//!   and delivered as 31 values. Read per logical read: `/ 31`.
//! - `deep-files`, `deep-stride`: one drain pass at a device depth of one
//!   over 256 queued foreground requests, which issues the head (its scan
//!   visits the other 255), the receipt, the value dropped, and the head
//!   enqueued again at the tail, so every op starts at 256 queued. The 256
//!   lie on 256 files, or on one file at a stride of one pool buffer plus
//!   one byte, so no two join.
//! - `reactor-iteration`: one iteration of the cell loop over a plane that
//!   holds no cold host and does nothing at its steps: the plane's default
//!   `before_execute`, A of the hook's row.
//! - `reactor-iteration-hook`: the same iteration over a plane whose
//!   `before_execute` is the server's: its cold host read under a short
//!   borrow of its tier slot, which holds no tiered namespace, B of the
//!   hook's row (`≤ 10` instructions over A an iteration).
//! - `reactor-iteration-server`, `reactor-iteration-server-hook`: the same
//!   pair over planes that both hold the hook's tier slot and whose
//!   `parse_execute` hands the loop context to a step out of line, as every
//!   server step does: the context escapes, so the loop's stores into it
//!   stay in both legs, as on a server cell, and the pair differs in the
//!   hook alone.
//!
//! Beside them, the rate legs (`cold_path/rates.rs`), reported and never
//! judged: one container operation a step, on the capped map and slab B
//! builds on and on leg A's hashed tables and queues, beside a `harness`
//! leg that runs their loop alone. A rate leg's reading less the harness's
//! is its operation's instructions, and the map legs' differences across
//! depths are a branch visit's: the rates each row's bound is evaluated at.
//!
//! The instrument: exact user-space instructions per op, `(I(n) − I(0)) /
//! n`, read outside this binary (`perf stat -e instructions:u`) on one
//! pinned performance core; cachegrind's `Ir` is the labelled substitute.
//! A run counts only when its controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: each leg run twice agrees within 0.05
//!   per op at a resolution of 0.01;
//! - liveness: each leg's `-plus-one` variant reads the leg + 1.00, ± 0.05:
//!   one `nop` per op at the top of the driving loop's body, outside the
//!   measured step;
//! - engagement: after its loop each leg asserts what its ops did (requests
//!   purged, reads issued and delivered, background left queued), so a leg
//!   that did nothing reads red.
//!
//! `perf` wraps the binary that `--no-run` prints, never `cargo`, whose own
//! instructions would join the count; on a hybrid CPU `<core>` is a
//! performance core and the `cpu_core` line counts.
//!
//!   cargo bench -p inf-runtime --bench cold_path --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <v> --ops <n>

#[path = "cold_path/cold.rs"]
#[allow(dead_code, reason = "the host is copied whole; each leg drives part of it")]
mod cold;
#[path = "cold_path/gate.rs"]
#[allow(dead_code, reason = "the gate is copied whole; leg A drives part of it")]
mod gate;
#[path = "cold_path/rates.rs"]
mod rates;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use inf_alloc::{AlignedPool, BufferPool};
use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::time::{Nanos, VirtualClock};
use inf_runtime::{
    BackendDriver, Capabilities, CellLoop, CellPlane, Completion, CompletionResult, IoOp, Issue,
    LoopConfig, LoopCx, SubmitStats, TimerSet, Wait,
};

use cold::{ColdReadConfig, ColdReads, ColdWait, ReadClass, TierFileId};
use rates::RateLeg;

/// One pool buffer, the server's cold window: four 4 KiB frames.
const BUFFER_BYTES: usize = 4 * inf_alloc::TIER_READ_ALIGN;
/// The fixture's descriptor: the bench issues no I/O.
const FD: i32 = -1;
/// A singleton request's window.
const READ_BYTES: usize = 64;
/// A cohort: 31 adjacent requests of 512 B, one pool buffer's union.
const COHORT: usize = 31;
const COHORT_READ_BYTES: usize = 512;
/// The deep legs' queue.
const DEEP: usize = 256;
/// The deep stride: one pool buffer plus one byte, so no two requests join.
const DEEP_STRIDE: u64 = BUFFER_BYTES as u64 + 1;
/// The reactor leg's timer set: one position, never reserved.
const LOOP_OWNERS: Cap = Cap::entries::<1>("bench-loop-owners", CapFill::Serving);

/// A leg: the operation it drives, at its device depth over its background.
#[derive(Clone, Copy)]
enum Leg {
    EnqueueCancel { depth: usize, background: usize },
    Singleton { depth: usize, background: usize },
    Cohort { depth: usize, background: usize },
    Deep { stride: bool },
    Reactor(ReactorPlane),
    Rate(RateLeg),
}

/// A reactor leg's plane: the hook's row is read on two pairs of them.
#[derive(Clone, Copy)]
enum ReactorPlane {
    /// No state and nothing at any step: the default `before_execute`.
    Idle,
    /// The server's `before_execute`, nothing at any other step.
    Hook,
    /// The hook's tier slot and the server's out-of-line steps, the
    /// default `before_execute`.
    Server,
    /// [`ReactorPlane::Server`] with the server's `before_execute`.
    ServerHook,
}

/// The one table of leg names: `--variant` is parsed against it.
const LEGS: [(&str, Leg, u8); 66] = [
    ("enqueue-cancel-64", Leg::EnqueueCancel { depth: 64, background: 64 }, 0),
    ("enqueue-cancel-64-plus-one", Leg::EnqueueCancel { depth: 64, background: 64 }, 1),
    ("enqueue-cancel-16384", Leg::EnqueueCancel { depth: 4096, background: 16_384 }, 0),
    ("enqueue-cancel-16384-plus-one", Leg::EnqueueCancel { depth: 4096, background: 16_384 }, 1),
    ("singleton-empty", Leg::Singleton { depth: 128, background: 0 }, 0),
    ("singleton-empty-plus-one", Leg::Singleton { depth: 128, background: 0 }, 1),
    ("singleton-512", Leg::Singleton { depth: 128, background: 512 }, 0),
    ("singleton-512-plus-one", Leg::Singleton { depth: 128, background: 512 }, 1),
    ("singleton-16384", Leg::Singleton { depth: 4096, background: 16_384 }, 0),
    ("singleton-16384-plus-one", Leg::Singleton { depth: 4096, background: 16_384 }, 1),
    ("cohort-empty", Leg::Cohort { depth: 128, background: 0 }, 0),
    ("cohort-empty-plus-one", Leg::Cohort { depth: 128, background: 0 }, 1),
    ("cohort-512", Leg::Cohort { depth: 128, background: 512 }, 0),
    ("cohort-512-plus-one", Leg::Cohort { depth: 128, background: 512 }, 1),
    ("cohort-16384", Leg::Cohort { depth: 4096, background: 16_384 }, 0),
    ("cohort-16384-plus-one", Leg::Cohort { depth: 4096, background: 16_384 }, 1),
    ("deep-files", Leg::Deep { stride: false }, 0),
    ("deep-files-plus-one", Leg::Deep { stride: false }, 1),
    ("deep-stride", Leg::Deep { stride: true }, 0),
    ("deep-stride-plus-one", Leg::Deep { stride: true }, 1),
    ("reactor-iteration", Leg::Reactor(ReactorPlane::Idle), 0),
    ("reactor-iteration-plus-one", Leg::Reactor(ReactorPlane::Idle), 1),
    ("reactor-iteration-hook", Leg::Reactor(ReactorPlane::Hook), 0),
    ("reactor-iteration-hook-plus-one", Leg::Reactor(ReactorPlane::Hook), 1),
    ("reactor-iteration-server", Leg::Reactor(ReactorPlane::Server), 0),
    ("reactor-iteration-server-plus-one", Leg::Reactor(ReactorPlane::Server), 1),
    ("reactor-iteration-server-hook", Leg::Reactor(ReactorPlane::ServerHook), 0),
    ("reactor-iteration-server-hook-plus-one", Leg::Reactor(ReactorPlane::ServerHook), 1),
    ("harness", Leg::Rate(RateLeg::Harness), 0),
    ("harness-plus-one", Leg::Rate(RateLeg::Harness), 1),
    ("map-get-0", Leg::Rate(RateLeg::MapGet { depth: 0 }), 0),
    ("map-get-0-plus-one", Leg::Rate(RateLeg::MapGet { depth: 0 }), 1),
    ("map-get-6", Leg::Rate(RateLeg::MapGet { depth: 6 }), 0),
    ("map-get-6-plus-one", Leg::Rate(RateLeg::MapGet { depth: 6 }), 1),
    ("map-get-9", Leg::Rate(RateLeg::MapGet { depth: 9 }), 0),
    ("map-get-9-plus-one", Leg::Rate(RateLeg::MapGet { depth: 9 }), 1),
    ("map-get-14", Leg::Rate(RateLeg::MapGet { depth: 14 }), 0),
    ("map-get-14-plus-one", Leg::Rate(RateLeg::MapGet { depth: 14 }), 1),
    ("map-admit-remove-empty", Leg::Rate(RateLeg::MapAdmitRemoveEmpty), 0),
    ("map-admit-remove-empty-plus-one", Leg::Rate(RateLeg::MapAdmitRemoveEmpty), 1),
    ("map-admit-remove-6", Leg::Rate(RateLeg::MapAdmitRemove { depth: 6 }), 0),
    ("map-admit-remove-6-plus-one", Leg::Rate(RateLeg::MapAdmitRemove { depth: 6 }), 1),
    ("map-admit-remove-9", Leg::Rate(RateLeg::MapAdmitRemove { depth: 9 }), 0),
    ("map-admit-remove-9-plus-one", Leg::Rate(RateLeg::MapAdmitRemove { depth: 9 }), 1),
    ("map-admit-remove-14", Leg::Rate(RateLeg::MapAdmitRemove { depth: 14 }), 0),
    ("map-admit-remove-14-plus-one", Leg::Rate(RateLeg::MapAdmitRemove { depth: 14 }), 1),
    ("slab-cycle", Leg::Rate(RateLeg::SlabCycle), 0),
    ("slab-cycle-plus-one", Leg::Rate(RateLeg::SlabCycle), 1),
    ("slab-get", Leg::Rate(RateLeg::SlabGet), 0),
    ("slab-get-plus-one", Leg::Rate(RateLeg::SlabGet), 1),
    ("hash-get-6", Leg::Rate(RateLeg::HashGet { depth: 6 }), 0),
    ("hash-get-6-plus-one", Leg::Rate(RateLeg::HashGet { depth: 6 }), 1),
    ("hash-get-9", Leg::Rate(RateLeg::HashGet { depth: 9 }), 0),
    ("hash-get-9-plus-one", Leg::Rate(RateLeg::HashGet { depth: 9 }), 1),
    ("hash-get-14", Leg::Rate(RateLeg::HashGet { depth: 14 }), 0),
    ("hash-get-14-plus-one", Leg::Rate(RateLeg::HashGet { depth: 14 }), 1),
    ("hash-insert-remove-6", Leg::Rate(RateLeg::HashInsertRemove { depth: 6 }), 0),
    ("hash-insert-remove-6-plus-one", Leg::Rate(RateLeg::HashInsertRemove { depth: 6 }), 1),
    ("hash-insert-remove-9", Leg::Rate(RateLeg::HashInsertRemove { depth: 9 }), 0),
    ("hash-insert-remove-9-plus-one", Leg::Rate(RateLeg::HashInsertRemove { depth: 9 }), 1),
    ("hash-insert-remove-14", Leg::Rate(RateLeg::HashInsertRemove { depth: 14 }), 0),
    ("hash-insert-remove-14-plus-one", Leg::Rate(RateLeg::HashInsertRemove { depth: 14 }), 1),
    ("deque-push-or-pop", Leg::Rate(RateLeg::DequePushOrPop), 0),
    ("deque-push-or-pop-plus-one", Leg::Rate(RateLeg::DequePushOrPop), 1),
    ("vec-push-or-pop", Leg::Rate(RateLeg::VecPushOrPop), 0),
    ("vec-push-or-pop-plus-one", Leg::Rate(RateLeg::VecPushOrPop), 1),
];

struct Args {
    leg: Leg,
    nops: u8,
    ops: u64,
}

fn parse() -> Result<Args, String> {
    let mut leg = None;
    let mut ops = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bench" {
            continue; // cargo bench passes it to a harness-free binary
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => {
                let Some(&(_, found, nops)) = LEGS.iter().find(|(name, ..)| *name == value) else {
                    let names: Vec<&str> = LEGS.iter().map(|(name, ..)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                leg = Some((found, nops));
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (leg, nops) = leg.ok_or("--variant is required")?;
    Ok(Args { leg, nops, ops: ops.ok_or("--ops is required")? })
}

/// The plants: `NOPS` operand-free `nop`s at the top of a loop body that
/// runs once per op, so the count must read exactly `NOPS` more an op.
#[inline(always)]
fn plant<const NOPS: u8>() {
    // With no `nop` the leg emits no `asm!` at all.
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

/// The bench's device budget: the foreground issues now, every maintain
/// offer waits for a later slice.
fn admit(class: ReadClass, _bytes: u64) -> Issue {
    match class {
        ReadClass::Foreground => Issue::Now,
        ReadClass::Maintain => Issue::NotThisSlice,
    }
}

/// A host leg's state: the host, its background's waits (held for the
/// whole run, so the background stays queued), the waits and device reads
/// of one op, all reserved before the loop.
struct HostCell {
    cold: ColdReads,
    background: Vec<ColdWait>,
    waits: VecDeque<ColdWait>,
    ops: Vec<IoOp>,
    request_file: TierFileId,
}

impl HostCell {
    /// A host at `depth` (its pool and its device cap) with class queues of
    /// `class_queue` and `background` maintain requests on even files.
    fn new(depth: usize, class_queue: usize, background: usize) -> HostCell {
        let config =
            ColdReadConfig { qd_cap: depth, overflow_cap: class_queue, ..Default::default() };
        let cold = ColdReads::with_config(AlignedPool::new(depth, BUFFER_BYTES), config);
        let file = |index: usize| TierFileId::new(u32::try_from(index).expect("fixture file"));
        let background: Vec<ColdWait> = (0..background)
            .map(|index| {
                let class = ReadClass::Maintain;
                cold.enqueue(FD, file(2 * index), 0, READ_BYTES, class, 0).expect("queued")
            })
            .collect();
        // Odd: between two live files' keys whenever the background is not
        // empty.
        let request_file = file(2 * (background.len() / 2) + 1);
        HostCell {
            cold,
            background,
            waits: VecDeque::with_capacity(DEEP),
            ops: Vec::with_capacity(DEEP),
            request_file,
        }
    }

    /// One drain pass under the bench's device budget, its reads kept.
    #[inline(always)]
    fn drain(&mut self) -> u32 {
        let ops = &mut self.ops;
        self.cold.drain_budgeted(admit, |_, _, _| {}, |op| ops.push(op))
    }

    /// Every kept read's receipt, success.
    #[inline(always)]
    fn complete(&mut self) -> u32 {
        let mut delivered = 0;
        for op in self.ops.drain(..) {
            let IoOp::TierRead { token, .. } = op else { unreachable!("the host issues reads") };
            delivered += self.cold.on_completion(token, CompletionResult::TierRead, 1);
        }
        delivered
    }
}

/// Polls `wait` once with the no-op waker: its value, delivered already.
#[inline(always)]
fn ready(mut wait: ColdWait) -> cold::ColdDone {
    match Pin::new(&mut wait).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(done) => done,
        Poll::Pending => panic!("a delivered value is ready at its first poll"),
    }
}

#[inline(never)]
fn step_enqueue_cancel(cell: &mut HostCell) -> u64 {
    let class = ReadClass::Foreground;
    let wait = cell.cold.enqueue(FD, cell.request_file, 0, READ_BYTES, class, 0);
    drop(wait);
    u64::from(cell.drain())
}

#[inline(never)]
fn step_singleton(cell: &mut HostCell) -> u64 {
    let class = ReadClass::Foreground;
    let wait = cell.cold.enqueue(FD, cell.request_file, 0, READ_BYTES, class, 0).expect("room");
    let issued = cell.drain();
    let delivered = cell.complete();
    drop(ready(wait));
    u64::from(issued) + u64::from(delivered)
}

#[inline(never)]
fn step_cohort(cell: &mut HostCell) -> u64 {
    let class = ReadClass::Foreground;
    for member in 0..COHORT {
        let offset = (member * COHORT_READ_BYTES) as u64;
        let enqueued =
            cell.cold.enqueue(FD, cell.request_file, offset, COHORT_READ_BYTES, class, 0);
        cell.waits.push_back(enqueued.expect("room"));
    }
    let issued = cell.drain();
    let delivered = cell.complete();
    for wait in cell.waits.drain(..) {
        drop(ready(wait));
    }
    u64::from(issued) + u64::from(delivered)
}

/// The deep legs' request at queue position `index`: its file and offset.
fn deep_request(stride: bool, index: usize) -> (TierFileId, u64) {
    let index32 = u32::try_from(index).expect("fixture position");
    if stride {
        (TierFileId::new(0), u64::from(index32) * DEEP_STRIDE)
    } else {
        (TierFileId::new(index32), 0)
    }
}

#[inline(never)]
fn step_deep(cell: &mut HostCell, stride: bool, op: u64) -> u64 {
    let issued = cell.drain();
    let delivered = cell.complete();
    let head = cell.waits.pop_front().expect("the queue's head");
    drop(ready(head));
    let index = usize::try_from(op % DEEP as u64).expect("fixture position");
    let (file, offset) = deep_request(stride, index);
    let class = ReadClass::Foreground;
    cell.waits.push_back(cell.cold.enqueue(FD, file, offset, READ_BYTES, class, 0).expect("room"));
    u64::from(issued) + u64::from(delivered)
}

/// Drives `ops` ops of a host leg with `NOPS` planted per op, then checks
/// what they did.
#[inline(never)]
fn drive_host<const NOPS: u8>(leg: Leg, ops: u64) -> u64 {
    let (depth, background) = match leg {
        Leg::EnqueueCancel { depth, background }
        | Leg::Singleton { depth, background }
        | Leg::Cohort { depth, background } => (depth, background),
        Leg::Deep { .. } => (1, 0),
        Leg::Reactor(_) | Leg::Rate(_) => {
            unreachable!("a reactor or rate leg drives its own state")
        }
    };
    // The class queue: four times the depth, as the server sizes it, or the
    // deep legs' 256 at their depth of one.
    let class_queue = if matches!(leg, Leg::Deep { .. }) { DEEP } else { 4 * depth };
    let mut cell = HostCell::new(depth, class_queue, background);
    if let Leg::Deep { stride } = leg {
        for index in 0..DEEP {
            let (file, offset) = deep_request(stride, index);
            let class = ReadClass::Foreground;
            let wait = cell.cold.enqueue(FD, file, offset, READ_BYTES, class, 0).expect("room");
            cell.waits.push_back(wait);
        }
    }
    let cell = black_box(&mut cell);
    let mut accumulator = 0u64;
    for op in 0..ops {
        plant::<NOPS>();
        let effect = match leg {
            Leg::EnqueueCancel { .. } => step_enqueue_cancel(cell),
            Leg::Singleton { .. } => step_singleton(cell),
            Leg::Cohort { .. } => step_cohort(cell),
            Leg::Deep { stride } => step_deep(cell, stride, op),
            Leg::Reactor(_) | Leg::Rate(_) => {
                unreachable!("a reactor or rate leg drives its own state")
            }
        };
        accumulator = accumulator.wrapping_add(effect);
    }
    engaged(leg, cell, ops);
    accumulator.wrapping_add(cell.cold.counters().issued)
}

/// Engagement, after the counted loop: each leg's ops did what it names,
/// and its background stayed queued.
fn engaged(leg: Leg, cell: &HostCell, ops: u64) {
    let counts = cell.cold.counters();
    let background = cell.background.len();
    let (issued, delivered, merged, purged, deferred) = match leg {
        Leg::EnqueueCancel { background, .. } => (0, 0, 0, ops, ops * u64::from(background > 0)),
        Leg::Singleton { background, .. } => (ops, ops, 0, 0, ops * u64::from(background > 0)),
        Leg::Cohort { background, .. } => {
            let members = COHORT as u64;
            (ops, ops * members, ops * (members - 1), 0, ops * u64::from(background > 0))
        }
        Leg::Deep { .. } => (ops, ops, 0, 0, 0),
        Leg::Reactor(_) | Leg::Rate(_) => {
            unreachable!("a reactor or rate leg drives its own state")
        }
    };
    assert_eq!(counts.issued, issued, "the leg's reads issued");
    assert_eq!(counts.delivered, delivered, "the leg's values delivered");
    assert_eq!(counts.merged_waiters, merged, "the leg's merged members");
    assert_eq!(counts.cancelled_queued, purged, "the leg's requests purged");
    assert_eq!(counts.maintain_deferred, deferred, "the background deferred once a pass");
    let queued = background + if matches!(leg, Leg::Deep { .. }) { DEEP } else { 0 };
    assert_eq!(cell.cold.queue_depth(), queued, "the background stayed queued");
    assert_eq!(cell.cold.inflight_total(), 0, "no read left in flight");
}

/// The reactor leg's driver: every entry reaps nothing.
struct IdleDriver;

impl BackendDriver for IdleDriver {
    fn push(&mut self, op: IoOp) {
        panic!("the reactor leg pushes no op: {op:?}");
    }
    fn submit_and_reap(
        &mut self,
        _pool: &mut BufferPool,
        _wait: Wait,
        _out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        Ok(0)
    }
    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "cold-path-bench",
            multishot_accept: false,
            multishot_recv: false,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: false,
            defer_taskrun: false,
            performance_tier: false,
        }
    }
    fn submit_stats(&self) -> SubmitStats {
        SubmitStats::default()
    }
}

/// The reactor leg's plane: no cold host, nothing at any step.
struct IdlePlane;

impl CellPlane for IdlePlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, c: Completion) {
        panic!("the reactor leg reaps nothing: {c:?}");
    }
    fn parse_execute(&mut self, _cx: &mut LoopCx<'_>) {}
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// The server's tier slot as its hook reads it: the cell's tiered state,
/// absent on a cell that never created a tiered namespace, and in it the
/// cold host.
struct TierSlot {
    cold: Option<inf_runtime::ColdReads>,
}

/// The state the server's planes share, as the hook reaches its tier slot.
struct SharedTier {
    tier: RefCell<Option<TierSlot>>,
}

/// The hook legs' plane: the server's `before_execute` over a cell with no
/// tiered namespace. `SERVER_STEPS` false, nothing at any other step;
/// true, its `parse_execute` is [`server_steps`].
struct HookPlane<const SERVER_STEPS: bool> {
    shared: Rc<SharedTier>,
}

impl<const SERVER_STEPS: bool> CellPlane for HookPlane<SERVER_STEPS> {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, c: Completion) {
        panic!("the reactor leg reaps nothing: {c:?}");
    }
    fn before_execute(&mut self, cx: &mut LoopCx<'_>) {
        let engine = self.shared.tier.borrow().as_ref().is_some_and(|t| t.cold.is_some());
        if engine {
            deliver_cold(&self.shared, cx);
        }
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        if SERVER_STEPS {
            server_steps(&self.shared, cx);
        }
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// The server's delivery, out of the iteration's line: the cold handle
/// cloned under a short tier borrow, released before any gate wake.
#[inline(never)]
fn deliver_cold(shared: &SharedTier, cx: &mut LoopCx<'_>) {
    let cold = shared.tier.borrow().as_ref().and_then(|t| t.cold.clone());
    if let Some(cold) = cold {
        let now_us = cx.now.as_micros();
        let _ = cold.deliver_ready(cx.cold_delivery_budget(), now_us);
    }
}

/// The server-shaped control: [`HookPlane`]'s state and its server steps
/// with the plane's default `before_execute`, so the pair differs in the
/// hook alone.
struct ServerShapedPlane {
    shared: Rc<SharedTier>,
}

impl CellPlane for ServerShapedPlane {
    fn on_completion(&mut self, _cx: &mut LoopCx<'_>, c: Completion) {
        panic!("the reactor leg reaps nothing: {c:?}");
    }
    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        server_steps(&self.shared, cx);
    }
    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}
}

/// A server step as the loop sees it: out of line and handed the loop
/// context, which therefore escapes the iteration, as it does on a server
/// cell, whose `fabric_in`, `parse_execute` and `respond` each take it.
#[inline(never)]
fn server_steps(shared: &SharedTier, cx: &mut LoopCx<'_>) {
    black_box(shared);
    black_box(cx);
}

/// Drives `ops` iterations of the cell loop over `plane` with `NOPS`
/// planted per op.
#[inline(never)]
fn drive_loop<const NOPS: u8, P: CellPlane>(ops: u64, mut plane: P) -> u64 {
    let (census, serve_mark) = CapCensus::new(CellId(0));
    let timers = TimerSet::build(LOOP_OWNERS, &census).expect("the leg's timer set");
    // Spinning forever: the loop polls every entry and never parks.
    let config = LoopConfig { spin_iters: u32::MAX, ..LoopConfig::default() };
    let pool = BufferPool::new(1, 1024);
    let clock = VirtualClock::new(Nanos(1));
    let mut lp = CellLoop::new(IdleDriver, clock, pool, config, serve_mark, timers);
    let lp = black_box(&mut lp);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        let stats = lp.run_iteration(&mut plane).expect("an idle iteration");
        accumulator = accumulator.wrapping_add(u64::from(stats.parked));
    }
    // Engagement, after the counted loop: every iteration ran and none
    // parked.
    assert_eq!(accumulator, 0, "an iteration parked");
    accumulator.wrapping_add(lp.executor().live_tasks() as u64)
}

/// A cell's shared state with no tiered namespace.
fn shared_tier() -> Rc<SharedTier> {
    Rc::new(SharedTier { tier: RefCell::new(None) })
}

/// Drives `ops` iterations of a reactor leg over its plane.
fn drive_reactor<const NOPS: u8>(plane: ReactorPlane, ops: u64) -> u64 {
    match plane {
        ReactorPlane::Idle => drive_loop::<NOPS, _>(ops, IdlePlane),
        ReactorPlane::Hook => {
            drive_loop::<NOPS, _>(ops, HookPlane::<false> { shared: shared_tier() })
        }
        ReactorPlane::Server => {
            drive_loop::<NOPS, _>(ops, ServerShapedPlane { shared: shared_tier() })
        }
        ReactorPlane::ServerHook => {
            drive_loop::<NOPS, _>(ops, HookPlane::<true> { shared: shared_tier() })
        }
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("cold_path: {error}");
            return ExitCode::from(2);
        }
    };
    let result = match (args.leg, args.nops) {
        (Leg::Reactor(plane), 0) => drive_reactor::<0>(plane, args.ops),
        (Leg::Reactor(plane), _) => drive_reactor::<1>(plane, args.ops),
        (Leg::Rate(leg), 0) => rates::drive::<0>(leg, args.ops),
        (Leg::Rate(leg), _) => rates::drive::<1>(leg, args.ops),
        (leg, 0) => drive_host::<0>(leg, args.ops),
        (leg, _) => drive_host::<1>(leg, args.ops),
    };
    println!("{}", black_box(result));
    ExitCode::SUCCESS
}
