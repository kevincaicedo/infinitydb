//! One tiered placement by instructions (L4): the tail allocation through
//! the room question's one prospect and classification, with the hole
//! marks' full arm, and the seal-mark step, as a table's `append` makes
//! them.
//!
//! The judged difference is B − B₀ per placement: B is a `table-<shape>`
//! leg of this bench built on the product, B₀ the same leg of the same
//! source built on the product before the change (the base build), so the
//! base reads the product's own code and never a copy of it. Each
//! `parent-<shape>` leg runs a copy of the base's placement
//! (`placement/parent.rs`), which must read the same in both builds: a
//! control on the two builds, never a base.
//!
//! The rule: B − B₀ ≤ 2 instructions where a placement crosses nothing
//! (`plain`), ≤ 10 where it commits a page (`crossing`), ≤ 20 where it makes
//! a ring-top hole (`hole`), ≤ 22 where a full window refuses it (`refused`)
//! and ≤ 26 where that record needs a ring-top hole (`refused-hole`). Each
//! bound's own red is `table-<shape>-plus-<bound + 1>`, the base's leg with
//! one instruction a placement more than the bound, read as a B on the
//! base's build: it must read over the bound.
//!
//! The judged shapes: `plain`, a 64-byte record (the commit page and the
//! stride bucket unchanged, one crossing in 16,384 placements); `crossing`,
//! a 1 MiB record, so every placement commits a page and opens a bucket;
//! `hole`, a record of half the ring placed where it needs a ring-top hole,
//! so every placement pushes a hole mark, commits pages from the ring top
//! and opens a bucket; `refused`, a 1 MiB record at a full window, which
//! waits for flush progress below the tail; and `refused-hole`, the same
//! answer for a record that needs a ring-top hole. `plain`, `crossing` and
//! `refused` run on a 128 MiB ring of 1 MiB commit pages under a 64 MiB
//! window and release to the tail before the window would refuse
//! (`refused` fills the window first and never releases). `hole` runs on an
//! 8 MiB ring and window and, before each placement, places a 64-byte
//! filler and releases to the tail, so that the next record of half the
//! ring needs the hole; `refused-hole` places one filler and one record and
//! never releases.
//!
//! Five more legs read the other refusals a live placement meets, one
//! answer each: `refused-end` (the record would end past the 48-bit space),
//! `refused-long` (a record longer than the window), `refused-pad` (a
//! record past the ring top whose need lies above the tail, so the tail
//! pads to the ring top), `refused-pad-need` (the same record where that
//! pad does not fit the window) and `refused-page-pad` (a record within a
//! page of the window, so the tail pads to its commit page's end).
//!
//! The estimator: callgrind's inclusive count of the op function per call,
//! its plant and one placement, at 10⁵ ops (`crossing`, `hole`) or 10⁶ (the
//! others). The fillers, the releases, the fills, the warm-up and the
//! driving loop are outside it. The placement function calls the tail
//! allocation and the mark step out of line on both builds, so each step's
//! count is read apart. Its controls, each a leg of this binary:
//!
//! - the same binary against itself: each leg against itself within 0.05;
//! - liveness: each `-plus-one` leg, one `nop` per op, reads its leg +
//!   1.00 ± 0.05;
//! - engagement: after each leg's loop, outside the op, the leg's
//!   allocations equal its placements; at `crossing` a page committed an
//!   op and, on the product, the seal marks' census row rose to 2 or more
//!   (the base has no such row, and a base leg refuses one); at `hole` every
//!   op made a hole; at every refusal every op refused and the room question
//!   answers the leg's own refusal; or the leg panics.
//!
//! After its loop each `table-` leg asks `room` and `stall_target` for its
//! record's length, outside the op, so the classification keeps the three
//! callers it has in the product.
//!
//! A plant sits in the op function, outside the placement function: a
//! planted copy of the placement function would be compiled on its own, and
//! its inlining, not the plant, would move the count.
//!
//! ```text
//! cargo bench -p inf-store --bench placement --no-run
//! valgrind --tool=callgrind --toggle-collect=placement::op \
//!     --callgrind-out-file=<out> <binary> --variant <v> --ops <n> --build <b>
//! ```
//!
//! `v` is a leg of `LEGS`; `n` ≥ 0 (0 is the I(0) leg); `b` is `base` or
//! `tip`, the build the leg runs on.

#![allow(clippy::disallowed_types, reason = "bench target: the parent's unbounded lists")]

use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::CapCensus;
use inf_store::{AddressSpaceConfig, DemotionConfig, KeyHasher, LogicalAddr, Room, TieredTable};

#[path = "placement/parent.rs"]
mod parent;

const MIB: u64 = 1 << 20;
/// The filler `hole`, `refused-hole` and `refused-page-pad` place.
const FILLER_BYTES: usize = 64;
/// The tail `refused-pad` and `refused-pad-need` fill to, past the ring's
/// first 64 MiB, so that a 40 MiB record needs a ring-top hole.
const PAD_TAIL_BYTES: u64 = 100 * MIB;
/// The head of both fills: `refused-pad` releases past it to the tail;
/// `refused-pad-need` keeps it, so the window holds no room for the ring-top
/// pad.
const PAD_HEAD_BYTES: u64 = 60 * MIB;

/// The placement shapes.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Shape {
    Plain,
    Crossing,
    Hole,
    Refused,
    RefusedHole,
    RefusedEnd,
    RefusedLong,
    RefusedPad,
    RefusedPadNeed,
    RefusedPagePad,
}

/// The answer a refusal leg's state gives the room question, read on the
/// table after the leg's loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Exit {
    /// A wait for flush progress to this offset from the life's origin, at
    /// or below the tail.
    Demote(u64),
    /// A pad of the tail to this offset from the life's origin.
    Pad(u64),
    /// The end of the space, or a record no window holds.
    End,
}

impl Shape {
    /// The record's length.
    fn len(self) -> usize {
        match self {
            Shape::Plain => 64,
            Shape::Crossing | Shape::Refused | Shape::RefusedEnd => MIB as usize,
            Shape::Hole | Shape::RefusedHole => 4 * MIB as usize,
            Shape::RefusedLong => 8 * MIB as usize,
            Shape::RefusedPad | Shape::RefusedPadNeed => 40 * MIB as usize,
            Shape::RefusedPagePad => 64 * MIB as usize - 32,
        }
    }

    /// The ring and the window, in bytes, of 1 MiB commit pages.
    fn geometry(self) -> (u64, u64) {
        match self {
            Shape::Hole | Shape::RefusedHole => (8 * MIB, 8 * MIB),
            Shape::RefusedLong => (128 * MIB, 4 * MIB),
            Shape::Plain
            | Shape::Crossing
            | Shape::Refused
            | Shape::RefusedEnd
            | Shape::RefusedPad
            | Shape::RefusedPadNeed
            | Shape::RefusedPagePad => (128 * MIB, 64 * MIB),
        }
    }

    /// The life's origin: half a page below the end of the 48-bit space at
    /// `refused-end`, so a 1 MiB record at the tail would end past it.
    fn origin(self) -> LogicalAddr {
        match self {
            Shape::RefusedEnd => {
                LogicalAddr::from_raw(LogicalAddr::MAX_RAW + 1 - MIB / 2).expect("a 48-bit origin")
            }
            Shape::Plain
            | Shape::Crossing
            | Shape::Hole
            | Shape::Refused
            | Shape::RefusedHole
            | Shape::RefusedLong
            | Shape::RefusedPad
            | Shape::RefusedPadNeed
            | Shape::RefusedPagePad => LogicalAddr::ZERO,
        }
    }

    /// Placements between two releases at `plain` and `crossing`: fewer than
    /// the window holds after a release to the tail, so no placement refuses.
    fn placements_per_release(self) -> u64 {
        match self {
            Shape::Plain => 1 << 19,
            Shape::Crossing => 64,
            Shape::Hole
            | Shape::Refused
            | Shape::RefusedHole
            | Shape::RefusedEnd
            | Shape::RefusedLong
            | Shape::RefusedPad
            | Shape::RefusedPadNeed
            | Shape::RefusedPagePad => u64::MAX,
        }
    }

    /// A refusal leg's answer, from the geometry: `None` where the op places.
    fn exit(self) -> Option<Exit> {
        match self {
            Shape::Plain | Shape::Crossing | Shape::Hole => None,
            // A full window of 64 pages: the next page is the need.
            Shape::Refused => Some(Exit::Demote(MIB)),
            // The record starts at the ring top and ends 4 MiB past it, so
            // the window must release its first 4 MiB.
            Shape::RefusedHole => Some(Exit::Demote(4 * MIB)),
            Shape::RefusedEnd | Shape::RefusedLong => Some(Exit::End),
            // The ring-top pad from 100 MiB to the ring top fits the window.
            Shape::RefusedPad => Some(Exit::Pad(128 * MIB)),
            // With the head at 60 MiB the pad needs 64 MiB released.
            Shape::RefusedPadNeed => Some(Exit::Demote(64 * MIB)),
            // The tail pads to its commit page's end.
            Shape::RefusedPagePad => Some(Exit::Pad(MIB)),
        }
    }
}

/// The room question's answer after a leg's loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Answer {
    /// The parent's copy, which is not asked.
    NotAsked,
    Fits,
    Refused(Exit),
}

/// The table as built, or the parent's placement.
#[derive(Copy, Clone)]
enum Version {
    Table,
    Parent,
}

/// The build a leg runs on: the base's has no seal-marks census row.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Build {
    Base,
    Tip,
}

/// Each leg: its name, its version, its shape and its plant in `nop`s.
const LEGS: [(&str, Version, Shape, u8); 25] = [
    ("table-plain", Version::Table, Shape::Plain, 0),
    ("table-plain-plus-one", Version::Table, Shape::Plain, 1),
    ("table-plain-plus-three", Version::Table, Shape::Plain, 3),
    ("parent-plain", Version::Parent, Shape::Plain, 0),
    ("table-crossing", Version::Table, Shape::Crossing, 0),
    ("table-crossing-plus-one", Version::Table, Shape::Crossing, 1),
    ("table-crossing-plus-eleven", Version::Table, Shape::Crossing, 11),
    ("parent-crossing", Version::Parent, Shape::Crossing, 0),
    ("table-hole", Version::Table, Shape::Hole, 0),
    ("table-hole-plus-one", Version::Table, Shape::Hole, 1),
    ("table-hole-plus-twenty-one", Version::Table, Shape::Hole, 21),
    ("parent-hole", Version::Parent, Shape::Hole, 0),
    ("table-refused", Version::Table, Shape::Refused, 0),
    ("table-refused-plus-one", Version::Table, Shape::Refused, 1),
    ("table-refused-plus-twenty-three", Version::Table, Shape::Refused, 23),
    ("parent-refused", Version::Parent, Shape::Refused, 0),
    ("table-refused-hole", Version::Table, Shape::RefusedHole, 0),
    ("table-refused-hole-plus-one", Version::Table, Shape::RefusedHole, 1),
    ("table-refused-hole-plus-twenty-seven", Version::Table, Shape::RefusedHole, 27),
    ("parent-refused-hole", Version::Parent, Shape::RefusedHole, 0),
    ("table-refused-end", Version::Table, Shape::RefusedEnd, 0),
    ("table-refused-long", Version::Table, Shape::RefusedLong, 0),
    ("table-refused-pad", Version::Table, Shape::RefusedPad, 0),
    ("table-refused-pad-need", Version::Table, Shape::RefusedPadNeed, 0),
    ("table-refused-page-pad", Version::Table, Shape::RefusedPagePad, 0),
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

/// One placement on the parent's table, called as the table's is: one
/// table, its tail allocation and its mark step.
#[inline(never)]
fn parent_place(table: &mut parent::Table, len: usize) -> Option<LogicalAddr> {
    table.place(len)
}

/// What a leg's engagement reads after its loop.
struct Engagement {
    allocs: u64,
    commits: u64,
    holes: u64,
    /// The seal marks' census row's high water, `None` with no row (the
    /// table), or the marks' count (the parent).
    seal_marks: Option<u64>,
}

/// A version's placement, its releases, its engagement and the room
/// question's answer. Each version's table is the placer itself, so the op
/// passes its pointer to the placement function unchanged on every version.
trait Placer {
    fn place(&mut self, len: usize) -> Option<LogicalAddr>;
    fn release(&mut self);
    /// The tail's offset from the life's origin.
    fn tail_offset(&self) -> u64;
    fn engagement(&self, census: &CapCensus) -> Engagement;
    /// Asks `room` and `stall_target` for `len` bytes, outside the op: the
    /// room's answer, its offsets from the origin.
    fn exit(&mut self, len: usize) -> Answer;
}

/// The table as built for `shape`.
fn table(shape: Shape, census: &Rc<CapCensus>) -> TieredTable {
    let (ring, window) = shape.geometry();
    let config = AddressSpaceConfig {
        reserve_bytes: ring as usize,
        page_bytes: MIB as usize,
        life_origin: shape.origin(),
    };
    // The window is the budget and one slice of one commit page.
    let demotion = DemotionConfig::for_budget(window - MIB, MIB);
    TieredTable::new(config, demotion, 64, KeyHasher::default(), census).expect("the ring reserves")
}

impl Placer for TieredTable {
    #[inline]
    fn place(&mut self, len: usize) -> Option<LogicalAddr> {
        table_place(self, len)
    }

    /// Seal, flush and release to the tail, as demotion's slices would.
    fn release(&mut self) {
        let space = self.space_mut();
        let tail = space.tail();
        space.advance_ro_boundary(tail);
        space.advance_flushed(tail);
        space.advance_head(tail);
    }

    fn tail_offset(&self) -> u64 {
        self.space().tail().to_raw() - self.space().life_origin().to_raw()
    }

    fn engagement(&self, census: &CapCensus) -> Engagement {
        let counters = self.space().counters();
        let seal_marks = census.row("seal-marks").map(|row| u64::from(row.high_water));
        Engagement {
            allocs: counters.tail_allocs,
            commits: counters.region_commit_pages,
            holes: counters.seal_holes,
            seal_marks,
        }
    }

    fn exit(&mut self, len: usize) -> Answer {
        let space = self.space_mut();
        let origin = space.life_origin().to_raw();
        black_box(space.stall_target(len));
        match space.room(len) {
            Room::Fits => Answer::Fits,
            Room::Demote(to) => Answer::Refused(Exit::Demote(to.to_raw() - origin)),
            Room::Pad(pad) => Answer::Refused(Exit::Pad(pad.to().to_raw() - origin)),
            Room::End => Answer::Refused(Exit::End),
        }
    }
}

impl Placer for parent::Table {
    #[inline]
    fn place(&mut self, len: usize) -> Option<LogicalAddr> {
        parent_place(self, len)
    }

    fn release(&mut self) {
        self.release_to_tail();
    }

    fn tail_offset(&self) -> u64 {
        panic!("the parent's copy runs the judged shapes alone, which read no tail");
    }

    fn engagement(&self, _census: &CapCensus) -> Engagement {
        let (allocs, commits, holes, marks) = self.counts();
        Engagement { allocs, commits, holes, seal_marks: Some(marks as u64) }
    }

    fn exit(&mut self, _len: usize) -> Answer {
        Answer::NotAsked
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

/// Places 1 MiB records from the tail to `tail`, each inside the window;
/// returns the placements.
fn fill_to<P: Placer>(placer: &mut P, tail: u64) -> u64 {
    let from = placer.tail_offset();
    assert!(tail >= from && (tail - from).is_multiple_of(MIB), "a fill of whole records");
    let records = (tail - from) / MIB;
    for _ in 0..records {
        fill(placer, MIB as usize).expect("a fill inside the window places");
    }
    records
}

/// Brings a refusal leg to its refusing state, outside the op; returns the
/// placements it made.
fn prepare<P: Placer>(placer: &mut P, shape: Shape, len: usize) -> u64 {
    match shape {
        Shape::Refused => {
            let mut filled = 0;
            while fill(placer, len).is_some() {
                filled += 1;
            }
            filled
        }
        Shape::RefusedHole => {
            // A filler and a record below the ring top: the next record
            // needs a ring-top hole, and the window is full.
            fill(placer, FILLER_BYTES).expect("the filler places");
            fill(placer, len).expect("the record below the ring top places");
            2
        }
        Shape::RefusedPad | Shape::RefusedPadNeed => {
            let placed = fill_to(placer, PAD_HEAD_BYTES);
            placer.release();
            let placed = placed + fill_to(placer, PAD_TAIL_BYTES);
            if shape == Shape::RefusedPad {
                placer.release();
            }
            placed
        }
        Shape::RefusedPagePad => {
            fill(placer, FILLER_BYTES).expect("the filler places");
            placer.release();
            1
        }
        Shape::RefusedEnd | Shape::RefusedLong => 0,
        Shape::Plain | Shape::Crossing | Shape::Hole => {
            unreachable!("a placing shape prepares in its own loop")
        }
    }
}

/// Drives `ops` ops of `shape` and checks the leg's engagement.
fn drive<P: Placer, const NOPS: u8>(
    placer: &mut P,
    shape: Shape,
    ops: u64,
    build: Build,
    census: &CapCensus,
) -> u64 {
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
        Shape::Refused
        | Shape::RefusedHole
        | Shape::RefusedEnd
        | Shape::RefusedLong
        | Shape::RefusedPad
        | Shape::RefusedPadNeed
        | Shape::RefusedPagePad => {
            let filled = prepare(placer, shape, len);
            for _ in 0..ops {
                accumulator += u64::from(op::<P, NOPS>(placer, len).is_none());
            }
            assert_eq!(accumulator, ops, "engagement: every op refused");
            filled
        }
    };
    engage(placer, shape, ops, placed, build, census);
    accumulator
}

/// The leg's engagement, read after its loop and outside the op.
fn engage<P: Placer>(
    placer: &mut P,
    shape: Shape,
    ops: u64,
    placed: u64,
    build: Build,
    census: &CapCensus,
) {
    let answer = placer.exit(shape.len());
    let engagement = placer.engagement(census);
    assert_eq!(engagement.allocs, placed, "engagement: one allocation a placement");
    if let Some(want) = shape.exit()
        && answer != Answer::NotAsked
    {
        assert_eq!(answer, Answer::Refused(want), "engagement: the leg's refusal");
    }
    if ops == 0 {
        return;
    }
    match shape {
        Shape::Plain => {}
        Shape::Crossing => {
            assert!(engagement.commits >= ops, "engagement: a page committed an op");
            if build == Build::Tip {
                let marks = engagement.seal_marks.expect("the product's seal-marks census row");
                assert!(marks >= 2, "engagement: the crossing pushes marks, {marks}");
            }
        }
        Shape::Hole => {
            assert_eq!(engagement.holes, ops, "engagement: a ring-top hole an op");
            assert!(engagement.commits >= 4 * ops, "engagement: the record's pages");
        }
        Shape::Refused
        | Shape::RefusedHole
        | Shape::RefusedEnd
        | Shape::RefusedLong
        | Shape::RefusedPad
        | Shape::RefusedPadNeed
        | Shape::RefusedPagePad => {
            assert_eq!(engagement.holes, 0, "engagement: a refusal makes no hole");
        }
    }
}

fn run_leg<P: Placer>(
    placer: &mut P,
    shape: Shape,
    nops: u8,
    ops: u64,
    build: Build,
    census: &CapCensus,
) -> u64 {
    match nops {
        0 => drive::<P, 0>(placer, shape, ops, build, census),
        1 => drive::<P, 1>(placer, shape, ops, build, census),
        3 => drive::<P, 3>(placer, shape, ops, build, census),
        11 => drive::<P, 11>(placer, shape, ops, build, census),
        21 => drive::<P, 21>(placer, shape, ops, build, census),
        23 => drive::<P, 23>(placer, shape, ops, build, census),
        27 => drive::<P, 27>(placer, shape, ops, build, census),
        other => panic!("no leg plants {other}"),
    }
}

fn run(version: Version, shape: Shape, nops: u8, ops: u64, build: Build) -> u64 {
    let (census, serve_mark) = CapCensus::new(CellId(0));
    serve_mark.take();
    match version {
        Version::Table => {
            let mut table = table(shape, &census);
            // The product's tables register their seal marks on the census;
            // the base's hold them on a plain list.
            let row = census.row("seal-marks").is_some();
            assert_eq!(row, build == Build::Tip, "engagement: the seal-marks row and the build");
            run_leg(&mut table, shape, nops, ops, build, &census)
        }
        Version::Parent => {
            let (ring, window) = shape.geometry();
            let mut parent = parent::Table::new(ring as usize, MIB as usize, window);
            run_leg(&mut parent, shape, nops, ops, build, &census)
        }
    }
}

type Leg = (&'static str, Version, Shape, u8);

fn parse() -> Result<(Leg, u64, Build), String> {
    let mut leg = None;
    let mut ops = None;
    let mut build = None;
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
            "--build" => {
                build = Some(match value.as_str() {
                    "base" => Build::Base,
                    "tip" => Build::Tip,
                    other => return Err(format!("--build {other}: base or tip")),
                });
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let leg = leg.ok_or("--variant is required")?;
    let ops = ops.ok_or("--ops is required")?;
    Ok((leg, ops, build.ok_or("--build is required")?))
}

fn main() -> ExitCode {
    let ((variant, version, shape, nops), ops, build) = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("placement: {e}");
            return ExitCode::from(2);
        }
    };
    let accumulator = run(version, shape, nops, ops, build);
    println!("{variant} ops {ops} accumulator {}", black_box(accumulator));
    ExitCode::SUCCESS
}
