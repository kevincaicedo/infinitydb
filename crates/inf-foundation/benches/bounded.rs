#![allow(
    clippy::disallowed_types,
    reason = "bench target: the std deque is the A leg the capped type is measured against"
)]
//! The capped deque's A/B by instructions (L4): one push and one pop per op
//! on `VecDeque::with_capacity` (A) and on `CappedDeque` (B, both
//! crossings), at caps 16 and 1,024, from one binary. The instrument is
//! exact user-space instructions per op, `(I(n) − I(0)) / n`, read outside
//! this binary (`perf stat -e instructions:u`, or an instruction-counting
//! tool when the PMU is closed) on one pinned core, legs A B B A twice. The
//! liveness leg `reserve-plus-one` adds one instruction per op (a `nop`) at
//! the top of the loop body, where it reads B + 1.00 ± 0.05 at both shapes;
//! placed after the pop it read B + 2 at the `loop` shape, the `nop` having
//! changed the loop's own codegen around the high-water branch by one
//! instruction. The verdict rule: B − A ≤ 6 per push-and-pop pair.
//!
//!   cargo bench -p inf-foundation --bench bounded -- \
//!       --variant <v> --cap <c> --ops <n> [--shape <s>]
//!
//! `v` ∈ vecdeque | reserve | coalesce | reserve-plus-one; `c` ∈ 16 | 1024;
//! `n` ≥ 0 (0 is the I(0) leg). The deque is half full throughout, so every
//! op pays the cap compare and no op crosses it. `s` ∈ loop (the default:
//! the ops in one tight loop over a local deque, whose fields a compiler may
//! keep in registers) | call (one op per `#[inline(never)]` call through
//! `&mut`, the shape of a site that holds its deque in a long-lived owner
//! and performs one op per reactor iteration, so every op loads and stores
//! the fields). Both are reported; the two differ in what the loop around
//! the op lets the compiler do, not in the type.

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

struct Args {
    variant: String,
    cap: Cap,
    ops: u64,
    per_call: bool,
}

fn parse() -> Result<Args, String> {
    let mut variant = None;
    let mut cap = None;
    let mut ops = None;
    let mut per_call = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bench" {
            continue; // cargo bench passes it to a harness-free binary
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--variant" => variant = Some(value),
            "--cap" => {
                cap = Some(match value.as_str() {
                    "16" => CAP_16,
                    "1024" => CAP_1024,
                    other => return Err(format!("--cap {other}: 16 or 1024")),
                });
            }
            "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
            "--shape" => {
                per_call = match value.as_str() {
                    "loop" => false,
                    "call" => true,
                    other => return Err(format!("--shape {other}: loop or call")),
                };
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(Args {
        variant: variant.ok_or("--variant is required")?,
        cap: cap.ok_or("--cap is required")?,
        ops: ops.ok_or("--ops is required")?,
        per_call,
    })
}

/// The instrument's planted red: exactly one instruction per op, which the
/// count must read as B + 1.00. One `nop`, with no operand: an `add` on the
/// accumulator costs a register move beside it, and a `black_box` a store
/// and a load, so neither reads as one.
#[inline(always)]
fn plus_one() {
    // SAFETY: a `nop` touches no register, memory or flag.
    unsafe {
        std::arch::asm!("nop", options(nomem, nostack, preserves_flags));
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

#[inline(never)]
fn calls_vecdeque(cap: Cap, ops: u64) -> u64 {
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(cap.entries_max() as usize);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        accumulator ^= step_vecdeque(&mut deque, i);
    }
    accumulator
}

#[inline(never)]
fn calls_reserve<const PLUS_ONE: bool>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        if PLUS_ONE {
            plus_one();
        }
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
fn leg_vecdeque(cap: Cap, ops: u64) -> u64 {
    let mut deque: VecDeque<Entry> = VecDeque::with_capacity(cap.entries_max() as usize);
    for i in 0..half(cap) {
        deque.push_back(Entry(i));
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        deque.push_back(Entry(i));
        if let Some(Entry(v)) = deque.pop_front() {
            accumulator ^= v;
        }
    }
    accumulator
}

// `PLUS_ONE` is a const so the liveness leg differs from B by one `nop`
// and not by a select or a branch in both.
#[inline(never)]
fn leg_reserve<const PLUS_ONE: bool>(cap: Cap, census: &Rc<CapCensus>, ops: u64) -> u64 {
    let mut deque = CappedDeque::<Entry, Reserve>::new(cap, census).expect("the bench cap fits");
    for i in 0..half(cap) {
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(Entry(i));
        }
    }
    let mut accumulator = 0u64;
    for i in 0..ops {
        if PLUS_ONE {
            plus_one();
        }
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

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bounded: {e}");
            return ExitCode::from(2);
        }
    };
    let census = CapCensus::new(CellId(0));
    let accumulator = match (args.variant.as_str(), args.per_call) {
        ("vecdeque", false) => leg_vecdeque(args.cap, args.ops),
        ("reserve", false) => leg_reserve::<false>(args.cap, &census, args.ops),
        ("reserve-plus-one", false) => leg_reserve::<true>(args.cap, &census, args.ops),
        ("coalesce", false) => leg_coalesce(args.cap, &census, args.ops),
        ("vecdeque", true) => calls_vecdeque(args.cap, args.ops),
        ("reserve", true) => calls_reserve::<false>(args.cap, &census, args.ops),
        ("reserve-plus-one", true) => calls_reserve::<true>(args.cap, &census, args.ops),
        ("coalesce", true) => calls_coalesce(args.cap, &census, args.ops),
        (other, _) => {
            eprintln!(
                "bounded: --variant {other}: vecdeque, reserve, reserve-plus-one or coalesce"
            );
            return ExitCode::from(2);
        }
    };
    let shape = if args.per_call { "call" } else { "loop" };
    println!(
        "{} {shape} cap {} ops {} accumulator {}",
        args.variant,
        args.cap.entries_max(),
        args.ops,
        black_box(accumulator)
    );
    ExitCode::SUCCESS
}
