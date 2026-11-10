//! One tiered placement by instructions (L4): the tail allocation through
//! the room question's one prospect and classification, with the hole
//! marks' full arm, and the seal-mark step, as a table's `append` makes
//! them, on the table as built (`table-<shape>`). A change to these calls is
//! judged against its parent by the same placement function: the parent's
//! placement, copied into a bench-only module beside these legs, is its A
//! until the verdict is recorded, with the A/A control, a `-plus-one` plant
//! on every leg and the rule's own red.
//!
//! Two shapes, the two a placement takes: `plain` (a 64-byte record, the
//! commit page and the stride bucket unchanged, one crossing in 16,384
//! placements) and `crossing` (a 1 MiB record: every placement commits a
//! page and opens a bucket, so its mark step pushes). Every leg places on a
//! 128 MiB ring of 1 MiB commit pages under a 64 MiB window and releases to
//! the tail when the window refuses, as MAINTAIN's demotion would; the
//! release is amortized over the placements of a window, on every version.
//!
//! The rule: B − A per placement ≤ 2 instructions at `plain`, ≤ 6 at
//! `crossing`. Its own red is the parent's leg with one instruction more than
//! the bound per placement, read as a B: it must read over the bound. Each
//! version's tail allocation and mark step are out of line, as the table's
//! calls of the product's are, so the two legs differ in the placement's code
//! alone.
//!
//! The instrument: exact user-space instructions per op,
//! `(I(n) − I(0)) / n` at `n` = 10⁷ (`plain`) or 10⁶ (`crossing`), read
//! outside this binary (`perf stat -e instructions:u`) on one pinned
//! performance core. Its controls, each a leg of this binary:
//!
//! - the same binary against itself: each leg against itself within 0.05
//!   per op;
//! - liveness: each `-plus-one` leg, one `nop` per op, reads its leg +
//!   1.00 ± 0.05;
//! - engagement: after each leg's loop, outside the counted function, the
//!   leg's allocations equal its ops, and at `crossing` its marks and its
//!   committed pages rose with every placement, or the leg panics.
//!
//! A plant sits at the top of the driving loop's body, outside the
//! placement function: a planted copy of the function would be compiled on
//! its own, and its inlining, not the plant, would move the count.
//!
//! ```text
//! cargo bench -p inf-store --bench placement --no-run
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
use inf_store::{AddressSpaceConfig, DemotionConfig, KeyHasher, LogicalAddr, TieredTable};

const MIB: u64 = 1 << 20;
/// The ring: 128 commit pages of 1 MiB.
const RING_BYTES: u64 = 128 * MIB;
/// The window: 64 pages.
const WINDOW_BYTES: u64 = 64 * MIB;

/// The placement shapes.
#[derive(Copy, Clone)]
enum Shape {
    Plain,
    Crossing,
}

impl Shape {
    fn len(self) -> usize {
        match self {
            Shape::Plain => 64,
            Shape::Crossing => MIB as usize,
        }
    }
}

/// Each leg: its name, its shape and its plant in `nop`s.
const LEGS: [(&str, Shape, u8); 4] = [
    ("table-plain", Shape::Plain, 0),
    ("table-plain-plus-one", Shape::Plain, 1),
    ("table-crossing", Shape::Crossing, 0),
    ("table-crossing-plus-one", Shape::Crossing, 1),
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

/// The table's demotion: a 64 MiB window of 1 MiB commit pages.
fn demotion() -> DemotionConfig {
    DemotionConfig::for_budget(WINDOW_BYTES - MIB, MIB)
}

/// The table's release when the window refuses: seal, flush and release to
/// the tail, as demotion's slices would.
fn release_table(table: &mut TieredTable) {
    let space = table.space_mut();
    let tail = space.tail();
    space.advance_ro_boundary(tail);
    space.advance_flushed(tail);
    space.advance_head(tail);
}

fn run_table<const NOPS: u8>(shape: Shape, ops: u64, census: &Rc<CapCensus>) -> u64 {
    let config = AddressSpaceConfig {
        reserve_bytes: RING_BYTES as usize,
        page_bytes: MIB as usize,
        life_origin: LogicalAddr::ZERO,
    };
    let mut table = TieredTable::new(config, demotion(), 64, KeyHasher::default(), census)
        .expect("the ring reserves");
    let len = black_box(shape.len());
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        let addr = match table_place(&mut table, len) {
            Some(addr) => addr,
            None => {
                release_table(&mut table);
                table_place(&mut table, len).expect("a released window places")
            }
        };
        accumulator ^= addr.to_raw();
    }
    let counters = table.space().counters();
    assert_eq!(counters.tail_allocs, ops, "engagement: one allocation an op");
    if matches!(shape, Shape::Crossing) && ops > 0 {
        assert!(counters.region_commit_pages >= ops, "engagement: a page committed an op");
        let marks = census.row("seal-marks").map_or(0, |row| row.high_water);
        assert!(marks >= 2, "engagement: the crossing pushes marks, high water {marks}");
    }
    accumulator
}

fn run(shape: Shape, nops: u8, ops: u64, census: &Rc<CapCensus>) -> u64 {
    match nops {
        0 => run_table::<0>(shape, ops, census),
        1 => run_table::<1>(shape, ops, census),
        other => panic!("no leg plants {other}"),
    }
}

type Leg = (&'static str, Shape, u8);

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
    let ((variant, shape, nops), ops) = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("placement: {e}");
            return ExitCode::from(2);
        }
    };
    let (census, serve_mark) = CapCensus::new(CellId(0));
    serve_mark.take();
    let accumulator = run(shape, nops, ops, &census);
    println!("{variant} ops {ops} accumulator {}", black_box(accumulator));
    ExitCode::SUCCESS
}
