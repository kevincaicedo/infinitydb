//! One tiered placement by instructions (L4): the tail allocation through
//! the room question's one prospect and classification, with the hole
//! marks' full arm, and the seal-mark step, as a table's `append` makes
//! them. `table-<shape>` is the table as built; `parent-<shape>` is the
//! parent's placement, copied into the bench-only `placement/parent.rs`, its
//! A until the verdict is recorded.
//!
//! Four shapes, each an argument shape the placement takes at runtime or an
//! outcome its match takes: `plain` (a 64-byte record, the commit page and
//! the stride bucket unchanged, one crossing in 16,384 placements),
//! `crossing` (a 1 MiB record: every placement commits a page and opens a
//! bucket, so its mark step pushes), `hole` (a record of half the ring placed
//! where it needs a ring-top hole: every placement pushes a hole mark,
//! commits pages from the ring top and opens a bucket) and `refused` (a 1 MiB
//! record at a full window: every placement refuses and changes nothing).
//! `plain`, `crossing` and `refused` run on a 128 MiB ring of 1 MiB commit
//! pages under a 64 MiB window and release to the tail before the window
//! would refuse (`refused` fills the window first and never releases);
//! `hole` runs on an 8 MiB ring and window and, before each placement, places
//! a 64-byte filler and releases to the tail, so that the next record of half
//! the ring needs the hole. The fillers and the releases are outside the op
//! function, and every version runs them.
//!
//! The rule: B − A per placement ≤ 2 instructions at `plain`, ≤ 6 at
//! `crossing` and at `hole`, which cross a page. Its own red is
//! `parent-<shape>-plus-<bound + 1>`, the parent with one instruction more
//! than the bound per placement, read as a B: it must read over the bound.
//! `refused` is read beside the verdict, with its controls and a red at the
//! plain bound. Each version's tail allocation and mark step are out of line,
//! as the table's calls of the product's are, so the legs differ in the
//! placement's code alone.
//!
//! The instrument: exact user-space instructions, two estimators. Per op,
//! `(I(n) − I(0)) / n` read outside this binary (`perf stat -e
//! instructions:u`) on one pinned performance core, at `n` = 10⁷ (`plain`,
//! `refused`) or 10⁶ (`crossing`), a release amortized over a window's
//! placements. Per call of the op function (its plant and one placement),
//! callgrind's inclusive count, at `n` = 10⁶ (`plain`, `refused`) or 10⁵
//! (`crossing`, `hole`), every release and filler outside it. `hole` is read
//! per call alone: its op needs a filler and a release per ring lap, which a
//! whole-process count cannot leave out. Its controls, each a leg of this
//! binary:
//!
//! - the same binary against itself: each leg against itself within 0.05;
//! - liveness: each `-plus-one` leg, one `nop` per op, reads its leg +
//!   1.00 ± 0.05;
//! - engagement: after each leg's loop, outside the counted function, the
//!   leg's allocations equal its placements; at `crossing` its marks and its
//!   committed pages rose with every placement; at `hole` every op made a
//!   hole; at `refused` every op refused; or the leg panics.
//!
//! A plant sits in the op function, outside the placement function: a
//! planted copy of the placement function would be compiled on its own, and
//! its inlining, not the plant, would move the count.
//!
//! ```text
//! cargo bench -p inf-store --bench placement --no-run
//! taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!     --variant <v> --ops <n>
//! valgrind --tool=callgrind --callgrind-out-file=<out> <binary> \
//!     --variant <v> --ops <n>
//! ```
//!
//! `v` is a leg of `LEGS`; `n` ≥ 0 (0 is the I(0) leg).

#![allow(clippy::disallowed_types, reason = "bench target: the parent's unbounded lists")]

use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_store::{AddressSpaceConfig, DemotionConfig, KeyHasher, LogicalAddr, TieredTable};

#[path = "placement/parent.rs"]
mod parent;

const MIB: u64 = 1 << 20;
/// The filler `hole` places before each record.
const FILLER_BYTES: usize = 64;

/// The placement shapes.
#[derive(Copy, Clone)]
enum Shape {
    Plain,
    Crossing,
    Hole,
    Refused,
}

impl Shape {
    /// The record's length.
    fn len(self) -> usize {
        match self {
            Shape::Plain => 64,
            Shape::Crossing | Shape::Refused => MIB as usize,
            Shape::Hole => 4 * MIB as usize,
        }
    }

    /// The ring and the window, in bytes, of 1 MiB commit pages.
    fn geometry(self) -> (u64, u64) {
        match self {
            Shape::Plain | Shape::Crossing | Shape::Refused => (128 * MIB, 64 * MIB),
            Shape::Hole => (8 * MIB, 8 * MIB),
        }
    }

    /// Placements between two releases at `plain` and `crossing`: fewer than
    /// the window holds after a release to the tail, so no placement refuses.
    fn placements_per_release(self) -> u64 {
        match self {
            Shape::Plain => 1 << 19,
            Shape::Crossing => 64,
            Shape::Hole | Shape::Refused => u64::MAX,
        }
    }
}

/// The table as built, or the parent's placement.
#[derive(Copy, Clone)]
enum Version {
    Table,
    Parent,
}

/// Each leg: its name, its version, its shape and its plant in `nop`s.
const LEGS: [(&str, Version, Shape, u8); 20] = [
    ("table-plain", Version::Table, Shape::Plain, 0),
    ("table-plain-plus-one", Version::Table, Shape::Plain, 1),
    ("parent-plain", Version::Parent, Shape::Plain, 0),
    ("parent-plain-plus-one", Version::Parent, Shape::Plain, 1),
    ("parent-plain-plus-three", Version::Parent, Shape::Plain, 3),
    ("table-crossing", Version::Table, Shape::Crossing, 0),
    ("table-crossing-plus-one", Version::Table, Shape::Crossing, 1),
    ("parent-crossing", Version::Parent, Shape::Crossing, 0),
    ("parent-crossing-plus-one", Version::Parent, Shape::Crossing, 1),
    ("parent-crossing-plus-seven", Version::Parent, Shape::Crossing, 7),
    ("table-hole", Version::Table, Shape::Hole, 0),
    ("table-hole-plus-one", Version::Table, Shape::Hole, 1),
    ("parent-hole", Version::Parent, Shape::Hole, 0),
    ("parent-hole-plus-one", Version::Parent, Shape::Hole, 1),
    ("parent-hole-plus-seven", Version::Parent, Shape::Hole, 7),
    ("table-refused", Version::Table, Shape::Refused, 0),
    ("table-refused-plus-one", Version::Table, Shape::Refused, 1),
    ("parent-refused", Version::Parent, Shape::Refused, 0),
    ("parent-refused-plus-one", Version::Parent, Shape::Refused, 1),
    ("parent-refused-plus-three", Version::Parent, Shape::Refused, 3),
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

/// One placement on the table as built.
#[inline(never)]
fn table_place(table: &mut TieredTable, len: usize) -> Option<LogicalAddr> {
    table.place_for_bench(len)
}

/// One placement on the parent's.
#[inline(never)]
fn parent_place(
    space: &mut parent::Space,
    marks: &mut parent::Marks,
    len: usize,
) -> Option<LogicalAddr> {
    let addr = space.alloc(len)?;
    marks.note(space, addr);
    Some(addr)
}

/// What a leg's engagement reads after its loop.
struct Engagement {
    allocs: u64,
    commits: u64,
    holes: u64,
    /// The seal marks' high water (the table) or count (the parent).
    seal_marks: u64,
}

/// A version's placement, its release to the tail and its engagement.
trait Placer {
    fn place(&mut self, len: usize) -> Option<LogicalAddr>;
    fn release(&mut self);
    fn engagement(&self) -> Engagement;
}

struct Table {
    table: TieredTable,
    census: Rc<CapCensus>,
}

impl Table {
    fn new(shape: Shape, census: &Rc<CapCensus>) -> Table {
        let (ring, window) = shape.geometry();
        let config = AddressSpaceConfig {
            reserve_bytes: ring as usize,
            page_bytes: MIB as usize,
            life_origin: LogicalAddr::ZERO,
        };
        // The window is the budget and one slice of one commit page.
        let demotion = DemotionConfig::for_budget(window - MIB, MIB);
        let table = TieredTable::new(config, demotion, 64, KeyHasher::default(), census)
            .expect("the ring reserves");
        Table { table, census: Rc::clone(census) }
    }
}

impl Placer for Table {
    #[inline]
    fn place(&mut self, len: usize) -> Option<LogicalAddr> {
        table_place(&mut self.table, len)
    }

    /// Seal, flush and release to the tail, as demotion's slices would.
    fn release(&mut self) {
        let space = self.table.space_mut();
        let tail = space.tail();
        space.advance_ro_boundary(tail);
        space.advance_flushed(tail);
        space.advance_head(tail);
    }

    fn engagement(&self) -> Engagement {
        let counters = self.table.space().counters();
        let seal_marks = self.census.row("seal-marks").map_or(0, |row| row.high_water);
        Engagement {
            allocs: counters.tail_allocs,
            commits: counters.region_commit_pages,
            holes: counters.seal_holes,
            seal_marks: u64::from(seal_marks),
        }
    }
}

struct Parent {
    space: parent::Space,
    marks: parent::Marks,
}

impl Parent {
    fn new(shape: Shape) -> Parent {
        let (ring, window) = shape.geometry();
        let space = parent::Space::new(ring as usize, MIB as usize, window);
        Parent { space, marks: parent::Marks::new() }
    }
}

impl Placer for Parent {
    #[inline]
    fn place(&mut self, len: usize) -> Option<LogicalAddr> {
        parent_place(&mut self.space, &mut self.marks, len)
    }

    fn release(&mut self) {
        self.space.release_to_tail();
    }

    fn engagement(&self) -> Engagement {
        let (allocs, commits, holes) = self.space.counts();
        Engagement { allocs, commits, holes, seal_marks: self.marks.len() as u64 }
    }
}

/// The counted op: the plant, then one placement.
#[inline(never)]
fn op<P: Placer, const NOPS: u8>(placer: &mut P, len: usize) -> Option<LogicalAddr> {
    plant::<NOPS>();
    black_box(placer.place(len))
}

/// A placement outside the counted op: a filler, a warm-up or the fill. It is
/// inlined into the driving loop, so it is never the op function's code (an
/// out-of-line copy would be folded with the unplanted op).
#[inline(always)]
fn fill<P: Placer>(placer: &mut P, len: usize) -> Option<LogicalAddr> {
    black_box(placer.place(len))
}

/// Drives `ops` ops of `shape` and checks the leg's engagement.
fn drive<P: Placer, const NOPS: u8>(placer: &mut P, shape: Shape, ops: u64) -> u64 {
    let len = black_box(shape.len());
    let mut accumulator = 0u64;
    let placed = match shape {
        Shape::Plain | Shape::Crossing => {
            let per_release = shape.placements_per_release();
            let mut until_release = per_release;
            for _ in 0..ops {
                if until_release == 0 {
                    placer.release();
                    until_release = per_release;
                }
                until_release -= 1;
                let addr = op::<P, NOPS>(placer, len).expect("a window with room places");
                accumulator ^= addr.to_raw();
            }
            ops
        }
        Shape::Hole => {
            let filler = black_box(FILLER_BYTES);
            // Warm-up: a filler and a record that fit below the ring top,
            // so that from here every record needs a ring-top hole.
            fill(placer, filler).expect("the warm-up's filler places");
            fill(placer, len).expect("the warm-up's record places");
            for _ in 0..ops {
                placer.release();
                let addr = fill(placer, filler).expect("a filler places");
                accumulator ^= addr.to_raw();
                placer.release();
                let addr = op::<P, NOPS>(placer, len).expect("a released window places");
                accumulator ^= addr.to_raw();
            }
            2 + 2 * ops
        }
        Shape::Refused => {
            let mut filled = 0;
            while fill(placer, len).is_some() {
                filled += 1;
            }
            for _ in 0..ops {
                accumulator += u64::from(op::<P, NOPS>(placer, len).is_none());
            }
            assert_eq!(accumulator, ops, "engagement: every op refused");
            filled
        }
    };
    let engagement = placer.engagement();
    assert_eq!(engagement.allocs, placed, "engagement: one allocation a placement");
    if ops > 0 {
        match shape {
            Shape::Plain => {}
            Shape::Crossing => {
                assert!(engagement.commits >= ops, "engagement: a page committed an op");
                let marks = engagement.seal_marks;
                assert!(marks >= 2, "engagement: the crossing pushes marks, {marks}");
            }
            Shape::Hole => {
                assert_eq!(engagement.holes, ops, "engagement: a ring-top hole an op");
                assert!(engagement.commits >= 4 * ops, "engagement: the record's pages");
            }
            Shape::Refused => {
                assert_eq!(engagement.holes, 0, "engagement: a refusal makes no hole");
            }
        }
    }
    accumulator
}

fn run_leg<P: Placer>(placer: &mut P, shape: Shape, nops: u8, ops: u64) -> u64 {
    match nops {
        0 => drive::<P, 0>(placer, shape, ops),
        1 => drive::<P, 1>(placer, shape, ops),
        3 => drive::<P, 3>(placer, shape, ops),
        7 => drive::<P, 7>(placer, shape, ops),
        other => panic!("no leg plants {other}"),
    }
}

fn run(version: Version, shape: Shape, nops: u8, ops: u64, census: &Rc<CapCensus>) -> u64 {
    match version {
        Version::Table => run_leg(&mut Table::new(shape, census), shape, nops, ops),
        Version::Parent => run_leg(&mut Parent::new(shape), shape, nops, ops),
    }
}

type Leg = (&'static str, Version, Shape, u8);

fn parse() -> Result<(Leg, u64), String> {
    let mut leg = None;
    let mut ops = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bench" {
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => {
                let found = LEGS.iter().find(|(name, ..)| *name == value);
                let Some(&found) = found else {
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
            eprintln!("placement: {e}");
            return ExitCode::from(2);
        }
    };
    let (census, serve_mark) = CapCensus::new(CellId(0));
    serve_mark.take();
    let accumulator = run(version, shape, nops, ops, &census);
    println!("{variant} ops {ops} accumulator {}", black_box(accumulator));
    ExitCode::SUCCESS
}
