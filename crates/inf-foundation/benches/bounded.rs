#![allow(
    clippy::disallowed_types,
    reason = "bench target: the std deque is the A leg the capped type is measured against"
)]
//! The capped deque's A/B by instructions (L4): one push and one pop per op
//! on `VecDeque::with_capacity` (A) and on `CappedDeque` (B, both
//! crossings), at caps 16 and 1,024, from one binary.
//!
//! The verdict rule: B − A ≤ 10 instructions per push-and-pop pair at the
//! `call` shape, for either crossing at either cap; the `loop` shape is
//! reported, never judged. The bound is what the type adds to the pair at
//! that shape, read on its disassembly: the cap compare, the high-water
//! compare, and what their cold arms cost the pop that follows them. `call`
//! is judged because it is every site's shape: a site holds its deque in a
//! long-lived owner and performs one op per command, frame or parked op
//! through `&mut`, so every op loads and stores the deque's fields. `loop`,
//! the ops in one tight loop over a local deque, lets the compiler keep
//! those fields in registers, and an arm that calls out costs the whole loop
//! those registers; a verdict there would judge the register allocation of
//! a loop no site runs.
//!
//! The instrument: exact user-space instructions per op, `(I(n) − I(0)) / n`
//! at `n` = 10⁷, read outside this binary (`perf stat -e instructions:u`, or
//! an instruction-counting tool when the PMU is closed) on one pinned core,
//! legs A B B A twice at each cap and shape. A run judges only when its
//! controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: A against A and B against B within
//!   0.05 per op;
//! - liveness: `vecdeque-plus-one` and `reserve-plus-one`, one `nop` per op
//!   at the top of the loop body, read A + 1.00 and B + 1.00, ± 0.05;
//! - the rule's own red: `reserve-plus-four`, four `nop`s, reads over the
//!   bound (B − A = 11).
//!
//! `vecdeque-bounded` (A′) is A with the site's own `len < cap` test, the
//! cap a third argument of the step: the control that attributes the
//! bound's own cost, reported beside A.
//!
//!   cargo bench -p inf-foundation --bench bounded -- \
//!       --variant <v> --cap <c> --ops <n> [--shape <s>]
//!
//! `v` is a leg of `LEGS`; `c` ∈ 16 | 1024; `n` ≥ 0 (0 is the I(0) leg);
//! `s` ∈ call (the default: one op per `#[inline(never)]` call through
//! `&mut`) | loop. The deque is half full throughout, so every op pays the
//! cap compare and no op crosses it.

use std::collections::VecDeque;
use std::hint::black_box;
use std::process::ExitCode;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{
    Cap, CapCensus, CapFill, CappedDeque, Coalesce, Lossy, Merge, Reserve, Reserved,
};

const CAP_16: Cap = Cap::entries::<16>("bench-16", CapFill::Serving);
const CAP_1024: Cap = Cap::entries::<1024>("bench-1024", CapFill::Serving);

#[derive(Clone, Copy)]
struct Entry(u64);

impl Lossy for Entry {}

struct Later;

impl Merge<Entry> for Later {
    fn merge(back: &mut Entry, new: Entry) {
        *back = new;
    }
}

/// A leg: the deque it drives and the plant it carries.
#[derive(Clone, Copy)]
enum Leg {
    /// A.
    Vecdeque,
    /// A′: A with the site's own `len < cap` test.
    VecdequeBounded,
    /// A plus one `nop`: the liveness plant on A.
    VecdequePlusOne,
    /// B, the `Reserve` crossing: a slot, then its publish.
    Reserve,
    /// B plus one `nop`: the liveness plant on B.
    ReservePlusOne,
    /// B plus four `nop`s: the verdict rule's own red.
    ReservePlusFour,
    /// B, the `Coalesce` crossing.
    Coalesce,
}

/// The one table of leg names: `--variant` is parsed against it.
const LEGS: [(&str, Leg); 7] = [
    ("vecdeque", Leg::Vecdeque),
    ("vecdeque-bounded", Leg::VecdequeBounded),
    ("vecdeque-plus-one", Leg::VecdequePlusOne),
    ("reserve", Leg::Reserve),
    ("reserve-plus-one", Leg::ReservePlusOne),
    ("reserve-plus-four", Leg::ReservePlusFour),
    ("coalesce", Leg::Coalesce),
];

#[derive(Clone, Copy)]
enum Shape {
    /// One op per `#[inline(never)]` call through `&mut`: the judged shape.
    Call,
    /// The ops in one tight loop over a local deque: reported.
    Loop,
}

struct Args {
    variant: &'static str,
    leg: Leg,
    cap: Cap,
    ops: u64,
    shape: Shape,
}

fn parse() -> Result<Args, String> {
    let mut leg = None;
    let mut cap = None;
    let mut ops = None;
    let mut shape = Shape::Call;
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
            "--cap" => {
                cap = Some(match value.as_str() {
                    "16" => CAP_16,
                    "1024" => CAP_1024,
                    other => return Err(format!("--cap {other}: 16 or 1024")),
                });
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            "--shape" => {
                shape = match value.as_str() {
                    "call" => Shape::Call,
                    "loop" => Shape::Loop,
                    other => return Err(format!("--shape {other}: call or loop")),
                };
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (variant, leg) = leg.ok_or("--variant is required")?;
    Ok(Args {
        variant,
        leg,
        cap: cap.ok_or("--cap is required")?,
        ops: ops.ok_or("--ops is required")?,
        shape,
    })
}

/// The plants: `NOPS` instructions per op at the top of the loop body, each
/// a `nop` with no operand, so the count must read exactly `NOPS` more. An
/// `add` on the accumulator costs a register move beside it, and a
/// `black_box` a store and a load, so neither reads as one.
#[inline(always)]
fn plant<const NOPS: u8>() {
    // With no `nop` the leg emits no `asm!` at all: A and B carry no trace
    // of the plant.
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

fn half(cap: Cap) -> u64 {
    // Exact: both caps are even.
    u64::from(cap.entries_max()) / 2
}

// One op through `&mut`, as a site performs it: the step is never inlined
// into the loop, so the deque's fields are loaded and stored per op.
#[inline(never)]
fn step_vecdeque(deque: &mut VecDeque<Entry>, i: u64) -> u64 {
    deque.push_back(Entry(i));
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_vecdeque_bounded(deque: &mut VecDeque<Entry>, cap: usize, i: u64) -> u64 {
    if deque.len() < cap {
        deque.push_back(Entry(i));
    }
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_reserve(deque: &mut CappedDeque<Entry, Reserve>, i: u64) -> u64 {
    if let Reserved::Slot(slot) = deque.reserve() {
        slot.publish(Entry(i));
    }
    deque.pop_front().map_or(0, |Entry(v)| v)
}

#[inline(never)]
fn step_coalesce(deque: &mut CappedDeque<Entry, Coalesce<Later>>, i: u64) -> u64 {
    deque.push(Entry(i));
    deque.pop_front().map_or(0, |Entry(v)| v)
}

// `NOPS` and `BOUNDED` are consts, so a plant or the bound differs from its
// leg by those instructions alone and not by a select or a branch in both.
#[inline(never)]
fn calls_vecdeque<const NOPS: u8, const BOUNDED: bool>(cap: Cap, ops: u64) -> u64 {
    let entries = cap.entries_max() as usize;
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(entries);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        accumulator ^= if BOUNDED {
            step_vecdeque_bounded(&mut deque, entries, i)
        } else {
            step_vecdeque(&mut deque, i)
        };
    }
    accumulator
}

#[inline(never)]
fn calls_reserve<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        accumulator ^= step_reserve(&mut deque, i);
    }
    accumulator
}

#[inline(never)]
fn calls_coalesce(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque =
        CappedDeque::<Entry, Coalesce<Later>>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        deque.push(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        accumulator ^= step_coalesce(&mut deque, i);
    }
    accumulator
}

// Each leg is one function of the same shape: a leg inlined into `main`
// and one left standalone would differ in codegen, not in the type.
#[inline(never)]
fn leg_vecdeque<const NOPS: u8, const BOUNDED: bool>(cap: Cap, ops: u64) -> u64 {
    let entries = cap.entries_max() as usize;
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(entries);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        if !BOUNDED || deque.len() < entries {
            deque.push_back(Entry(i));
        }
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

#[inline(never)]
fn leg_reserve<const NOPS: u8>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        plant::<NOPS>();
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

#[inline(never)]
fn leg_coalesce(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque =
        CappedDeque::<Entry, Coalesce<Later>>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        deque.push(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        deque.push(Entry(i));
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

fn run(args: &Args, census: &Rc<CapCensus>) -> u64 {
    let (cap, ops) = (args.cap, args.ops);
    match (args.leg, args.shape) {
        (Leg::Vecdeque, Shape::Call) => calls_vecdeque::<0, false>(cap, ops),
        (Leg::VecdequeBounded, Shape::Call) => calls_vecdeque::<0, true>(cap, ops),
        (Leg::VecdequePlusOne, Shape::Call) => calls_vecdeque::<1, false>(cap, ops),
        (Leg::Reserve, Shape::Call) => calls_reserve::<0>(cap, census, ops),
        (Leg::ReservePlusOne, Shape::Call) => calls_reserve::<1>(cap, census, ops),
        (Leg::ReservePlusFour, Shape::Call) => calls_reserve::<4>(cap, census, ops),
        (Leg::Coalesce, Shape::Call) => calls_coalesce(cap, census, ops),
        (Leg::Vecdeque, Shape::Loop) => leg_vecdeque::<0, false>(cap, ops),
        (Leg::VecdequeBounded, Shape::Loop) => leg_vecdeque::<0, true>(cap, ops),
        (Leg::VecdequePlusOne, Shape::Loop) => leg_vecdeque::<1, false>(cap, ops),
        (Leg::Reserve, Shape::Loop) => leg_reserve::<0>(cap, census, ops),
        (Leg::ReservePlusOne, Shape::Loop) => leg_reserve::<1>(cap, census, ops),
        (Leg::ReservePlusFour, Shape::Loop) => leg_reserve::<4>(cap, census, ops),
        (Leg::Coalesce, Shape::Loop) => leg_coalesce(cap, census, ops),
    }
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bounded: {e}");
            return ExitCode::from(2);
        }
    };
    let census = CapCensus::new(CellId(0));
    let accumulator = run(&args, &census);
    let shape = match args.shape {
        Shape::Call => "call",
        Shape::Loop => "loop",
    };
    println!(
        "{} {shape} cap {} ops {} accumulator {}",
        args.variant,
        args.cap.entries_max(),
        args.ops,
        black_box(accumulator)
    );
    ExitCode::SUCCESS
}
