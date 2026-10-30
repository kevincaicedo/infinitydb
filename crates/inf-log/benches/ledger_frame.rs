#![allow(
    clippy::disallowed_types,
    reason = "bench target: the A leg is the fsync ledger over std containers"
)]
//! The fsync ledger's A/B by instructions (L4): one frame through the
//! ledger per op — planned, queued, its write-through barrier registered,
//! written, and folded at the barrier's completion — on the ledger before
//! its classes and capped holders (A, `parent`) and on the ledger as built
//! (B, `ledger`), from one binary. Both legs stage one record and seal it on
//! a real staging ring and release the lease after the write, so the
//! staging work is common to both; B carries the lease through its frame
//! queue.
//!
//! The verdict rule: B − A ≤ 12 instructions per frame, the queue's and the
//! ledger's two capped pairs this crate holds, and the class counts beside
//! them. Over, the change does not merge.
//!
//! The instrument: exact user-space instructions per op,
//! `(I(n) − I(0)) / n` at `n` = 10⁷, read outside this binary
//! (`perf stat -e instructions:u`) on one pinned core, legs A B B A. A run
//! judges only when its controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: A against A and B against B within
//!   0.05 per op, at a resolution of 0.01 per op at `n` = 10⁷;
//! - liveness: `parent-plus-one` and `ledger-plus-one`, one `nop` per op at
//!   the top of the frame, read A + 1.00 and B + 1.00, ± 0.05;
//! - the rule's own red: `ledger-plus-thirteen`, thirteen `nop`s, reads
//!   B − A + 13: over the bound whenever B is not cheaper than A.
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
//! `v` is a leg of `LEGS`; `n` ≥ 0 (0 is the I(0) leg). Each op is one
//! `#[inline(never)]` call through `&mut`, the shape every frame of a cell
//! takes.

use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFile;
use inf_log::{
    FrameLayout, FramePlan, FsyncClass, GroupCommit, Lsn, MutationEffect, NsId, SegmentId,
    StagingConfig, StagingRing,
};

#[path = "ledger_frame/parent.rs"]
mod parent;

#[derive(Copy, Clone)]
enum Leg {
    Parent,
    ParentPlusOne,
    Ledger,
    LedgerPlusOne,
    LedgerPlusThirteen,
}

const LEGS: [(&str, Leg); 5] = [
    ("parent", Leg::Parent),
    ("parent-plus-one", Leg::ParentPlusOne),
    ("ledger", Leg::Ledger),
    ("ledger-plus-one", Leg::LedgerPlusOne),
    ("ledger-plus-thirteen", Leg::LedgerPlusThirteen),
];

/// The plants: `NOPS` instructions per op at the top of the frame, each a
/// `nop` with no operand, so the count must read exactly `NOPS` more.
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

/// One cell's staging ring and the next frame's base, common to both legs.
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

    /// One `always` record staged and sealed: the frame's lease and its end.
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

/// A's frame: the plan, the queue, the write-through ticket, the write and
/// the fold, on the ledger before its caps.
#[inline(never)]
fn parent_frame<const NOPS: u8>(leg: &mut ParentLeg) -> u64 {
    plant::<NOPS>();
    leg.ledger.note_staged(parent::FsyncClass::Always);
    let plan = leg.ledger.frame_plan(true, false);
    assert!(plan == parent::FramePlan::WriteThrough, "every frame is write-through");
    let (lease, end) = leg.staging.seal();
    let id = leg.ledger.note_frame_queued(end, lease.frame_len());
    let ticket = leg.ledger.register_write_through(leg.staging.now);
    leg.ledger.note_frame_written(id);
    leg.staging.ring.release(lease);
    let durable = leg.ledger.on_fsync_complete(ticket, leg.staging.now);
    durable.map_or(0, Lsn::to_u64)
}

/// B's frame: the same path on the ledger as built, the lease carried by
/// its frame queue.
#[inline(never)]
fn ledger_frame<const NOPS: u8>(leg: &mut LedgerLeg) -> u64 {
    plant::<NOPS>();
    leg.ledger.note_staged(FsyncClass::Always);
    let plan = leg.ledger.frame_plan(true, false);
    assert!(plan == FramePlan::WriteThrough, "every frame is write-through");
    let (lease, end) = leg.staging.seal();
    let id = leg.ledger.note_frame_queued(end, lease, leg.staging.now);
    let ticket = leg.ledger.register_write_through(leg.staging.now);
    let written = leg.ledger.note_frame_written(id);
    leg.staging.ring.release(written.lease);
    let durable = leg.ledger.on_fsync_complete(ticket, leg.staging.now);
    durable.map_or(0, Lsn::to_u64)
}

fn run(leg: Leg, ops: u64, census: &Rc<CapCensus>) -> u64 {
    let mut accumulator = 0u64;
    match leg {
        Leg::Parent | Leg::ParentPlusOne => {
            let mut state =
                ParentLeg { staging: Staging::new(), ledger: parent::GroupCommit::new(census) };
            for _ in 0..ops {
                let state = black_box(&mut state);
                accumulator ^= match leg {
                    Leg::ParentPlusOne => parent_frame::<1>(state),
                    _ => parent_frame::<0>(state),
                };
            }
        }
        Leg::Ledger | Leg::LedgerPlusOne | Leg::LedgerPlusThirteen => {
            let mut state = LedgerLeg { staging: Staging::new(), ledger: GroupCommit::new(census) };
            for _ in 0..ops {
                let state = black_box(&mut state);
                accumulator ^= match leg {
                    Leg::LedgerPlusOne => ledger_frame::<1>(state),
                    Leg::LedgerPlusThirteen => ledger_frame::<13>(state),
                    _ => ledger_frame::<0>(state),
                };
            }
        }
    }
    accumulator
}

fn parse() -> Result<(&'static str, Leg, u64), String> {
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
                let Some(&found) = LEGS.iter().find(|(name, _)| *name == value) else {
                    let names: Vec<&str> = LEGS.iter().map(|(name, _)| *name).collect();
                    return Err(format!("--variant {value}: one of {}", names.join(", ")));
                };
                leg = Some(found);
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (variant, leg) = leg.ok_or("--variant is required")?;
    Ok((variant, leg, ops.ok_or("--ops is required")?))
}

fn main() -> ExitCode {
    let (variant, leg, ops) = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("ledger_frame: {e}");
            return ExitCode::from(2);
        }
    };
    let (census, serve_mark) = CapCensus::new(CellId(0));
    serve_mark.take();
    let accumulator = run(leg, ops, &census);
    println!("{variant} ops {ops} accumulator {}", black_box(accumulator));
    ExitCode::SUCCESS
}
