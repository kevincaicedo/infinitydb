//! The fsync ledger's frame by instructions (L4): one frame per op through
//! the LOG step's ledger calls — staged, planned with its arguments at
//! runtime, sealed, queued with its barrier, written, and folded at the
//! barrier's completion — on the ledger as built (`ledger-<shape>`), from
//! one non-generic `#[inline(never)]` frame function called through
//! `&mut`, the shape every frame of a cell takes; the ledger's entry points
//! stay out of line. It stages one record and seals it on a real staging
//! ring at one frame in flight and releases the lease after the write.
//! `ledger-plane-<shape>` runs the same frame with the plane's consumer of
//! the flight its queue hands back: the stall sample, a linked barrier's
//! clock rebased and completed, a write-through completion.
//!
//! Three shapes, the three the LOG step runs: write-through (`always`
//! staged, the rotor answers yes), linked (`always` staged, the rotor
//! answers no: a linked fdatasync) and plain (`everysec` staged: no sync
//! due).
//!
//! A change to these calls is judged against its parent by the same frame
//! functions: the parent's ledger, copied into a bench-only module beside
//! these legs, is its A until the verdict is recorded, with the A/A
//! control, a `-plus-one` plant on every leg and the rule's own red.
//!
//! The instrument: exact user-space instructions per op,
//! `(I(n) − I(0)) / n` at `n` = 10⁷, read outside this binary
//! (`perf stat -e instructions:u`) on one pinned core. Its controls, each a
//! leg of this binary:
//!
//! - the same binary against itself: each leg against itself within 0.05
//!   per op, at a resolution of 0.01 per op at `n` = 10⁷;
//! - liveness: the six `-plus-one` legs, one `nop` per op, read their
//!   leg + 1.00 ± 0.05;
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

use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFile;
use inf_log::{
    FrameBarrier, FrameLayout, FramePlan, FrameQueued, FsyncClass, GroupCommit, Lsn,
    MutationEffect, NsId, SegmentId, StagingConfig, StagingRing,
};

/// The frame shapes the LOG step runs.
#[derive(Copy, Clone)]
enum Shape {
    WriteThrough,
    Linked,
    Plain,
}

/// The ledger alone, or with the plane's consumer of its flight.
#[derive(Copy, Clone)]
enum Version {
    Ledger,
    LedgerPlane,
}

/// Each leg: its name, its version, its shape and its plant in `nop`s.
const LEGS: [(&str, Version, Shape, u8); 12] = [
    ("ledger-write-through", Version::Ledger, Shape::WriteThrough, 0),
    ("ledger-write-through-plus-one", Version::Ledger, Shape::WriteThrough, 1),
    ("ledger-linked", Version::Ledger, Shape::Linked, 0),
    ("ledger-linked-plus-one", Version::Ledger, Shape::Linked, 1),
    ("ledger-plain", Version::Ledger, Shape::Plain, 0),
    ("ledger-plain-plus-one", Version::Ledger, Shape::Plain, 1),
    ("ledger-plane-write-through", Version::LedgerPlane, Shape::WriteThrough, 0),
    ("ledger-plane-write-through-plus-one", Version::LedgerPlane, Shape::WriteThrough, 1),
    ("ledger-plane-linked", Version::LedgerPlane, Shape::Linked, 0),
    ("ledger-plane-linked-plus-one", Version::LedgerPlane, Shape::Linked, 1),
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

struct LedgerLeg {
    staging: Staging,
    ledger: GroupCommit<MemFile>,
}

/// The ledger as built: the LOG step's calls, the lease carried by its
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
    let FramePlan::Seal(plan) = leg.ledger.frame_plan(write_through_ok, seal_ahead) else {
        panic!("no leg's input holds its frame");
    };
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let FrameQueued { id, barrier } = leg.ledger.note_frame_queued(end, lease, plan, now);
    let ticket = match barrier {
        FrameBarrier::WriteThrough(ticket) | FrameBarrier::Linked(ticket) => Some(ticket),
        FrameBarrier::None => None,
    };
    let written = leg.ledger.note_frame_written(id);
    leg.staging.ring.release(written.lease);
    let durable = ticket.and_then(|ticket| leg.ledger.on_fsync_complete(ticket, now));
    durable.map_or(0, Lsn::to_u64)
}

/// The same frame with the plane's consumer of the flight the queue hands
/// back.
#[inline(never)]
fn ledger_plane_frame(
    leg: &mut LedgerLeg,
    always: bool,
    write_through_ok: bool,
    seal_ahead: bool,
) -> u64 {
    let class = if always { FsyncClass::Always } else { FsyncClass::Everysec };
    leg.ledger.note_staged(class);
    let FramePlan::Seal(plan) = leg.ledger.frame_plan(write_through_ok, seal_ahead) else {
        panic!("no leg's input holds its frame");
    };
    let (lease, end) = leg.staging.seal();
    let now = leg.staging.now;
    let id = leg.ledger.note_frame_queued(end, lease, plan, now).id;
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
