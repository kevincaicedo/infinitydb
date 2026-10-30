#![allow(
    clippy::disallowed_types,
    reason = "bench target: the A legs are the parent's fsync ledger and its plane's in-flight \
              table over std containers"
)]
//! The fsync ledger's A/B by instructions (L4): one frame per op through the
//! LOG step's ledger calls — staged, planned with its arguments at
//! runtime, sealed, queued, its barrier registered by a match on the plan,
//! written, and folded at the barrier's completion — on the ledger before
//! its classes and capped holders (A, `parent`) and on the ledger as built
//! (B, `ledger`), from one binary. Each version is one non-generic
//! `#[inline(never)]` frame function called through `&mut`, the shape every
//! frame of a cell takes; the ledger's entry points stay out of line. Both
//! stage one record and seal it on a real staging ring at one frame in
//! flight and release the lease after the write, so the staging work is
//! common to both.
//!
//! Three shapes, the three the LOG step runs: write-through (`always`
//! staged, the rotor answers yes), linked (`always` staged, the rotor
//! answers no: a linked fdatasync) and plain (`everysec` staged: no sync
//! due).
//!
//! The verdict rule, B − A per frame at each shape the LOG step runs: ≤ 50
//! instructions for a write-through frame, ≤ 90 for a frame with a linked
//! fdatasync, ≤ 42 for a plain frame: the flight's custody the queue
//! carries, `frame_plan`'s hold reason and its new questions, the frame
//! queue's and the ledger's capped pairs, and the class counts, less the
//! parent's iterator for the write-through base. Over at any shape, the
//! change does not merge. The bounds are read on x86-64 at the pinned
//! toolchain; on another target or toolchain each term is re-read before a
//! verdict.
//!
//! The custody legs, the parent's ledger with its plane's in-flight table
//! against B with the same consumer, are reported beside them, never
//! judged: `parent-flight-<shape>` (A″) runs A with the table the parent's
//! plane kept for each frame in flight — pushed after the registration,
//! found by `position` and taken by `remove` at the write — and
//! `ledger-plane-<shape>` (B″) runs B with the flight its queue hands back.
//! Both consume the flight as the plane does: the stall sample, a linked
//! barrier's clock rebased and completed, a write-through completion.
//!
//! The instrument: exact user-space instructions per op,
//! `(I(n) − I(0)) / n` at `n` = 10⁷, read outside this binary
//! (`perf stat -e instructions:u`) on one pinned core, legs A B B A twice
//! and A″ B″ B″ A″ twice at each shape. A run judges only when its
//! controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: A against A, B against B, A″ against
//!   A″ and B″ against B″ within 0.05 per op at each shape, at a
//!   resolution of 0.01 per op at `n` = 10⁷;
//! - liveness: the twelve `-plus-one` legs (`parent-`, `ledger-`,
//!   `parent-flight-` and `ledger-plane-` at each shape), one `nop` per op,
//!   read their leg + 1.00 ± 0.05;
//! - the rule's own red at each shape: `ledger-write-through-plus-51`,
//!   `ledger-linked-plus-91` and `ledger-plain-plus-43`, the bound plus one
//!   in `nop`s, read over the bound whenever B is not cheaper than A;
//! - engagement: after each leg's loop, outside the counted function, the
//!   ledger's stats read one frame queued and one barrier of the shape per
//!   op (none at plain), or the leg panics.
//!
//! A plant sits at the top of the driving loop's body, outside the frame
//! function: a planted copy of a frame function is compiled on its own, so
//! its inlining, not the plant, would move the count.
//!
//! One leg, then one leg counted: `perf` wraps the binary that `--no-run`
//! prints, never `cargo`; on a hybrid CPU `<core>` is a performance core and
//! the `cpu_core` line counts.
//!
//! ```text
//! cargo bench -p inf-log --bench ledger_frame --no-run
//! taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!     --variant <v> --ops <n>
//! ```
//!
//! `v` is a leg of `LEGS`; `n` ≥ 0 (0 is the I(0) leg).

use std::collections::VecDeque;
use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFile;
use inf_log::{
    FrameBarrier, FrameLayout, FramePlan, FsyncClass, GroupCommit, Lsn, MutationEffect, NsId,
    SegmentId, StagingConfig, StagingRing,
};

#[path = "ledger_frame/parent.rs"]
mod parent;

/// The frame shapes the LOG step runs.
#[derive(Copy, Clone)]
enum Shape {
    WriteThrough,
    Linked,
    Plain,
}

/// A, B, A″ and B″.
#[derive(Copy, Clone)]
enum Version {
    Parent,
    Ledger,
    ParentFlight,
    LedgerPlane,
}

/// Each leg: its name, its version, its shape and its plant in `nop`s.
const LEGS: [(&str, Version, Shape, u8); 27] = [
    ("parent-write-through", Version::Parent, Shape::WriteThrough, 0),
    ("parent-write-through-plus-one", Version::Parent, Shape::WriteThrough, 1),
    ("ledger-write-through", Version::Ledger, Shape::WriteThrough, 0),
    ("ledger-write-through-plus-one", Version::Ledger, Shape::WriteThrough, 1),
    ("ledger-write-through-plus-51", Version::Ledger, Shape::WriteThrough, 51),
    ("parent-linked", Version::Parent, Shape::Linked, 0),
    ("parent-linked-plus-one", Version::Parent, Shape::Linked, 1),
    ("ledger-linked", Version::Ledger, Shape::Linked, 0),
    ("ledger-linked-plus-one", Version::Ledger, Shape::Linked, 1),
    ("ledger-linked-plus-91", Version::Ledger, Shape::Linked, 91),
    ("parent-plain", Version::Parent, Shape::Plain, 0),
    ("parent-plain-plus-one", Version::Parent, Shape::Plain, 1),
    ("ledger-plain", Version::Ledger, Shape::Plain, 0),
    ("ledger-plain-plus-one", Version::Ledger, Shape::Plain, 1),
    ("ledger-plain-plus-43", Version::Ledger, Shape::Plain, 43),
    ("parent-flight-write-through", Version::ParentFlight, Shape::WriteThrough, 0),
    ("parent-flight-write-through-plus-one", Version::ParentFlight, Shape::WriteThrough, 1),
    ("ledger-plane-write-through", Version::LedgerPlane, Shape::WriteThrough, 0),
    ("ledger-plane-write-through-plus-one", Version::LedgerPlane, Shape::WriteThrough, 1),
    ("parent-flight-linked", Version::ParentFlight, Shape::Linked, 0),
    ("parent-flight-linked-plus-one", Version::ParentFlight, Shape::Linked, 1),
    ("ledger-plane-linked", Version::LedgerPlane, Shape::Linked, 0),
    ("ledger-plane-linked-plus-one", Version::LedgerPlane, Shape::Linked, 1),
    ("parent-flight-plain", Version::ParentFlight, Shape::Plain, 0),
    ("parent-flight-plain-plus-one", Version::ParentFlight, Shape::Plain, 1),
    ("ledger-plane-plain", Version::LedgerPlane, Shape::Plain, 0),
    ("ledger-plane-plain-plus-one", Version::LedgerPlane, Shape::Plain, 1),
];

/// The plants: `NOPS` instructions per op, each a `nop` with no operand,
/// so the count must read exactly `NOPS` more.
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

/// One cell's staging ring and the next frame's base, common to every leg.
struct Staging {
    ring: StagingRing,
    end: u32,
    now: Nanos,
}

impl Staging {
    fn new() -> Staging {
        let ring =
            StagingRing::new(StagingConfig { capacity_bytes: 64 << 10, frames_in_flight: 1 });
        Staging { ring, end: 0, now: Nanos(0) }
    }

    /// One record staged and sealed: the frame's lease and its end.
    #[inline(always)]
    fn seal(&mut self) -> (inf_log::FrameLease, Lsn) {
        let effect = MutationEffect::StringSet { ns: NsId(1), key: b"k", value: b"v" };
        self.ring.stage(&effect).expect("the ring has room");
        let base = Lsn::new(SegmentId(0), self.end);
        let lease = self.ring.seal(base, 0, FrameLayout::Packed);
        self.end += lease.frame_len();
        self.now = Nanos(self.now.0 + 1);
        (lease, Lsn::new(SegmentId(0), self.end))
    }
}

struct ParentLeg {
    staging: Staging,
    ledger: parent::GroupCommit<MemFile>,
}

struct LedgerLeg {
    staging: Staging,
    ledger: GroupCommit<MemFile>,
}

/// A: the parent's ledger, the plan matched as the parent's LOG step did.
#[inline(never)]
fn parent_frame(
    leg: &mut ParentLeg,
    always: bool,
    write_through_ok: bool,
    seal_ahead: bool,
) -> u64 {
    let class = if always { parent::FsyncClass::Always } else { parent::FsyncClass::Everysec };
    leg.ledger.note_staged(class);
    let plan = leg.ledger.frame_plan(write_through_ok, seal_ahead);
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let id = leg.ledger.note_frame_queued(end, lease.frame_len());
    let ticket = match plan {
        parent::FramePlan::WriteThrough => Some(leg.ledger.register_write_through(now)),
        parent::FramePlan::LinkedFsync => Some(leg.ledger.register_linked_fsync(now)),
        parent::FramePlan::Plain => None,
        parent::FramePlan::Wait => unreachable!("a waiting frame is never sealed"),
    };
    leg.ledger.note_frame_written(id);
    leg.staging.ring.release(lease);
    let durable = ticket.and_then(|ticket| leg.ledger.on_fsync_complete(ticket, now));
    durable.map_or(0, Lsn::to_u64)
}

/// B: the ledger as built, the same LOG step, the lease carried by its
/// frame queue.
#[inline(never)]
fn ledger_frame(
    leg: &mut LedgerLeg,
    always: bool,
    write_through_ok: bool,
    seal_ahead: bool,
) -> u64 {
    let class = if always { FsyncClass::Always } else { FsyncClass::Everysec };
    leg.ledger.note_staged(class);
    let plan = leg.ledger.frame_plan(write_through_ok, seal_ahead);
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let id = leg.ledger.note_frame_queued(end, lease, now);
    let ticket = match plan {
        FramePlan::WriteThrough => Some(leg.ledger.register_write_through(now)),
        FramePlan::LinkedFsync => Some(leg.ledger.register_linked_fsync(now)),
        FramePlan::Plain => None,
        FramePlan::Wait(_) => unreachable!("a waiting frame is never sealed"),
    };
    let written = leg.ledger.note_frame_written(id);
    leg.staging.ring.release(written.lease);
    let durable = ticket.and_then(|ticket| leg.ledger.on_fsync_complete(ticket, now));
    durable.map_or(0, Lsn::to_u64)
}

/// The parent plane's entry for a frame in flight: the frame, its lease,
/// its submit time and its barrier, keyed by the frame.
struct ParentInFlight {
    id: parent::FrameId,
    lease: inf_log::FrameLease,
    submitted_at: Nanos,
    barrier: ParentBarrier,
}

/// The parent plane's barrier for a frame in flight.
#[derive(Copy, Clone)]
enum ParentBarrier {
    None,
    Linked(parent::FsyncTicket),
    WriteThrough(parent::FsyncTicket),
}

struct ParentFlightLeg {
    staging: Staging,
    ledger: parent::GroupCommit<MemFile>,
    in_flight: VecDeque<ParentInFlight>,
}

/// A″: A with the parent plane's in-flight table and the plane's consumer.
#[inline(never)]
fn parent_flight_frame(
    leg: &mut ParentFlightLeg,
    always: bool,
    write_through_ok: bool,
    seal_ahead: bool,
) -> u64 {
    let class = if always { parent::FsyncClass::Always } else { parent::FsyncClass::Everysec };
    leg.ledger.note_staged(class);
    let plan = leg.ledger.frame_plan(write_through_ok, seal_ahead);
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let id = leg.ledger.note_frame_queued(end, lease.frame_len());
    let barrier = match plan {
        parent::FramePlan::WriteThrough => {
            ParentBarrier::WriteThrough(leg.ledger.register_write_through(now))
        }
        parent::FramePlan::LinkedFsync => {
            ParentBarrier::Linked(leg.ledger.register_linked_fsync(now))
        }
        parent::FramePlan::Plain => ParentBarrier::None,
        parent::FramePlan::Wait => unreachable!("a waiting frame is never sealed"),
    };
    leg.in_flight.push_back(ParentInFlight { id, lease, submitted_at: now, barrier });
    let index = leg
        .in_flight
        .iter()
        .position(|frame| frame.id == id)
        .expect("LogWritten for a frame not in flight");
    let frame = leg.in_flight.remove(index).expect("index from position");
    let stall = now.saturating_sub(frame.submitted_at).as_micros();
    leg.ledger.note_frame_written(frame.id);
    leg.staging.ring.release(frame.lease);
    let durable = match frame.barrier {
        ParentBarrier::None => None,
        ParentBarrier::Linked(ticket) => {
            leg.ledger.rebase_clock(ticket, now);
            leg.ledger.on_fsync_complete(ticket, now)
        }
        ParentBarrier::WriteThrough(ticket) => leg.ledger.on_fsync_complete(ticket, now),
    };
    durable.map_or(0, Lsn::to_u64) ^ stall
}

/// B″: B with the same consumer, the flight handed back by the queue.
#[inline(never)]
fn ledger_plane_frame(
    leg: &mut LedgerLeg,
    always: bool,
    write_through_ok: bool,
    seal_ahead: bool,
) -> u64 {
    let class = if always { FsyncClass::Always } else { FsyncClass::Everysec };
    leg.ledger.note_staged(class);
    let plan = leg.ledger.frame_plan(write_through_ok, seal_ahead);
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let id = leg.ledger.note_frame_queued(end, lease, now);
    match plan {
        FramePlan::WriteThrough => {
            leg.ledger.register_write_through(now);
        }
        FramePlan::LinkedFsync => {
            leg.ledger.register_linked_fsync(now);
        }
        FramePlan::Plain => {}
        FramePlan::Wait(_) => unreachable!("a waiting frame is never sealed"),
    }
    let frame = leg.ledger.note_frame_written(id);
    let stall = now.saturating_sub(frame.submitted_at).as_micros();
    leg.staging.ring.release(frame.lease);
    let durable = match frame.barrier {
        FrameBarrier::None => None,
        FrameBarrier::Linked(ticket) => {
            leg.ledger.rebase_clock(ticket, now);
            leg.ledger.on_fsync_complete(ticket, now)
        }
        FrameBarrier::WriteThrough(ticket) => leg.ledger.on_fsync_complete(ticket, now),
    };
    durable.map_or(0, Lsn::to_u64) ^ stall
}

/// The driving loop: the leg's plant at the top of each op, then one frame
/// with the shape's arguments passed through `black_box`, as the LOG step
/// passes the rotor's answer and the rotation flag at runtime.
macro_rules! drive {
    ($state:expr, $frame:ident, $nops:expr, $ops:expr, $always:expr, $write_through_ok:expr) => {{
        let mut accumulator = 0u64;
        macro_rules! go {
            ($n:literal) => {
                for _ in 0..$ops {
                    plant::<$n>();
                    accumulator ^= $frame(
                        black_box(&mut $state),
                        black_box($always),
                        black_box($write_through_ok),
                        black_box(false),
                    );
                }
            };
        }
        match $nops {
            0 => go!(0),
            1 => go!(1),
            43 => go!(43),
            51 => go!(51),
            91 => go!(91),
            _ => unreachable!("a plant of LEGS"),
        }
        accumulator
    }};
}

/// The shape's inputs: whether `always` traffic is staged, and the rotor's
/// answer.
fn inputs(shape: Shape) -> (bool, bool) {
    match shape {
        Shape::WriteThrough => (true, true),
        Shape::Linked => (true, false),
        Shape::Plain => (false, true),
    }
}

/// Engagement: every op queued one frame and ran the shape's barrier, read
/// after the loop, outside the counted frame.
fn engaged(shape: Shape, ops: u64, write_through: u64, linked: u64, queued: u64) {
    assert_eq!(queued, ops, "every op queued one frame");
    let (write_through_wanted, linked_wanted) = match shape {
        Shape::WriteThrough => (ops, 0),
        Shape::Linked => (0, ops),
        Shape::Plain => (0, 0),
    };
    assert_eq!(write_through, write_through_wanted, "write-through barriers");
    assert_eq!(linked, linked_wanted, "linked barriers");
}

fn run(version: Version, shape: Shape, nops: u8, ops: u64, census: &Rc<CapCensus>) -> u64 {
    let (always, write_through_ok) = inputs(shape);
    match version {
        Version::Parent => {
            let mut state =
                ParentLeg { staging: Staging::new(), ledger: parent::GroupCommit::new(census) };
            let accumulator = drive!(state, parent_frame, nops, ops, always, write_through_ok);
            let stats = state.ledger.stats();
            engaged(
                shape,
                ops,
                stats.fsyncs_write_through,
                stats.fsyncs_linked,
                stats.frames_queued,
            );
            accumulator
        }
        Version::Ledger => {
            let mut state = LedgerLeg { staging: Staging::new(), ledger: GroupCommit::new(census) };
            let accumulator = drive!(state, ledger_frame, nops, ops, always, write_through_ok);
            let stats = state.ledger.stats();
            engaged(
                shape,
                ops,
                stats.fsyncs_write_through,
                stats.fsyncs_linked,
                stats.frames_queued,
            );
            accumulator
        }
        Version::ParentFlight => {
            let mut state = ParentFlightLeg {
                staging: Staging::new(),
                ledger: parent::GroupCommit::new(census),
                in_flight: VecDeque::with_capacity(1),
            };
            let accumulator =
                drive!(state, parent_flight_frame, nops, ops, always, write_through_ok);
            let stats = state.ledger.stats();
            engaged(
                shape,
                ops,
                stats.fsyncs_write_through,
                stats.fsyncs_linked,
                stats.frames_queued,
            );
            accumulator
        }
        Version::LedgerPlane => {
            let mut state = LedgerLeg { staging: Staging::new(), ledger: GroupCommit::new(census) };
            let accumulator =
                drive!(state, ledger_plane_frame, nops, ops, always, write_through_ok);
            let stats = state.ledger.stats();
            engaged(
                shape,
                ops,
                stats.fsyncs_write_through,
                stats.fsyncs_linked,
                stats.frames_queued,
            );
            accumulator
        }
    }
}

type Leg = (&'static str, Version, Shape, u8);

fn parse() -> Result<(Leg, u64), String> {
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
                let Some(&found) = LEGS.iter().find(|(name, ..)| *name == value) else {
                    let names: Vec<&str> = LEGS.iter().map(|(name, ..)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                leg = Some(found);
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let leg = leg.ok_or("--variant is required")?;
    Ok((leg, ops.ok_or("--ops is required")?))
}

fn main() -> ExitCode {
    let ((variant, version, shape, nops), ops) = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("ledger_frame: {e}");
            return ExitCode::from(2);
        }
    };
    let (census, serve_mark) = CapCensus::new(CellId(0));
    serve_mark.take();
    let accumulator = run(version, shape, nops, ops, &census);
    println!("{variant} ops {ops} accumulator {}", black_box(accumulator));
    ExitCode::SUCCESS
}
