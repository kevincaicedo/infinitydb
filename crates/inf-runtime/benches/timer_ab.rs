//! The timers' A/B by instructions (L4): the cell's timer paths on the
//! hierarchical wheel the cell ran before its fixed timer set (A, a
//! bench-only copy in `timer_ab/wheel.rs`, so every A leg measures the same
//! code after the engine's timer changes) and on the fixed timer set (B),
//! from one binary.
//!
//! The A legs, one op each:
//!
//! - `wheel-idle`: nothing armed; the clock steps 1 µs, then step 1's
//!   `advance` and the drain of the due keys.
//! - `wheel-one-out`: one timer 1 s out, armed again from the injected time
//!   at each fire (the everysec shape); the clock steps 1 µs, then
//!   `advance`, the drain, and each due key's callback.
//! - `wheel-park`: the park decision with the everysec timer armed: one
//!   clock read, `next_deadline` and the timeout from that read.
//! - `wheel-hold`: one hold episode beside the everysec timer: `insert` one
//!   window out (with the armed high water it raises), the clock steps to
//!   the deadline, and its delivery: `advance`, the due push and drain and
//!   the callback's key tests.
//! - `wheel-cohort`: `--owners` 1,024 or 65,536 timers armed at one instant,
//!   half cancelled, the rest delivered at their deadline; one op is the
//!   whole cohort, reported per op and per timer, never judged.
//!
//! The B legs, one op each:
//!
//! - `set-cohort`: the same cohort on a set of `--owners` positions, each
//!   reserved before the loop: every owner armed one millisecond out, half
//!   cancelled, the clock stepped to the deadline, and the rest delivered
//!   one notice at a time and consumed. Reported beside `wheel-cohort`,
//!   never judged.
//!
//! The shape is the cell's: the loop and its timer are a long-lived owner,
//! and each op is one `#[inline(never)]` step through `&mut`, so every op
//! loads and stores the owner's fields.
//!
//! The instrument: exact user-space instructions per op, `(I(n) − I(0)) / n`
//! at `n` = 10⁷ (10² for a cohort leg), read outside this binary
//! (`perf stat -e instructions:u`) on one pinned performance core. A run
//! counts only when its controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: each leg run twice agrees within 0.05
//!   per op, at a resolution of 0.01 per op at `n` = 10⁷;
//! - liveness: each leg's `-plus-one` variant, one `nop` per op at the top
//!   of the driving loop's body, outside the measured step, reads the leg
//!   + 1.00, ± 0.05;
//! - engagement: after its loop each leg asserts the timers it left armed
//!   and, for the hold and cohort legs, that it armed its episodes.
//!
//! One leg, then one leg counted: `perf` wraps the binary that `--no-run`
//! prints, never `cargo`, whose own instructions would join the count; on a
//! hybrid CPU `<core>` is a performance core and the `cpu_core` line counts.
//!
//!   cargo bench -p inf-runtime --bench timer_ab --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <v> --ops <n> [--owners <r>]

#[path = "timer_ab/wheel.rs"]
mod wheel;

use std::hint::black_box;
use std::process::ExitCode;

use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_runtime::{Cancelled, Consumed, Reserved, TimerArm, TimerOutcome, TimerOwner, TimerSet};

use wheel::TimerWheel;

/// One microsecond and one millisecond of injected time.
const MICRO: Nanos = Nanos(1_000);
const MILLI: Nanos = Nanos(1_000_000);
/// The everysec timer's period.
const SECOND: Nanos = Nanos(1_000_000_000);
/// The park ceiling, `LoopConfig::park_default`'s.
const PARK_DEFAULT: Nanos = Nanos(100_000_000);
/// The fill hold's window at its design point.
const HOLD_WINDOW: Nanos = MILLI;
/// The callbacks' routing keys, the server's three.
const EVERYSEC_KEY: u64 = 1;
const FILL_KEY: u64 = 2;
const ACCEPT_KEY: u64 = 3;
/// A cohort's keys start above the server's, so no cohort timer is taken
/// for one of theirs.
const COHORT_KEY_BASE: u64 = 1 << 32;
/// The B cohorts' sets.
const OWNERS_1024: Cap = Cap::entries::<1024>("bench-owners-1024", CapFill::Serving);
const OWNERS_65536: Cap = Cap::entries::<65536>("bench-owners-65536", CapFill::Serving);

/// A leg: the path it drives and the plant it carries.
#[derive(Clone, Copy)]
enum Leg {
    Idle,
    OneOut,
    Park,
    Hold,
    Cohort,
    SetCohort,
}

/// The one table of leg names: `--variant` is parsed against it.
const LEGS: [(&str, Leg, u8); 12] = [
    ("wheel-idle", Leg::Idle, 0),
    ("wheel-idle-plus-one", Leg::Idle, 1),
    ("wheel-one-out", Leg::OneOut, 0),
    ("wheel-one-out-plus-one", Leg::OneOut, 1),
    ("wheel-park", Leg::Park, 0),
    ("wheel-park-plus-one", Leg::Park, 1),
    ("wheel-hold", Leg::Hold, 0),
    ("wheel-hold-plus-one", Leg::Hold, 1),
    ("wheel-cohort", Leg::Cohort, 0),
    ("wheel-cohort-plus-one", Leg::Cohort, 1),
    ("set-cohort", Leg::SetCohort, 0),
    ("set-cohort-plus-one", Leg::SetCohort, 1),
];

struct Args {
    leg: Leg,
    nops: u8,
    ops: u64,
    owners: u32,
}

fn parse() -> Result<Args, String> {
    let mut leg = None;
    let mut ops = None;
    let mut owners = None;
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
            "--owners" => {
                owners = Some(match value.as_str() {
                    "1024" => 1024,
                    "65536" => 65_536,
                    other => return Err(format!("--owners {other}: 1024 or 65536")),
                });
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let (leg, nops) = leg.ok_or("--variant is required")?;
    let owners = match (leg, owners) {
        (Leg::Cohort | Leg::SetCohort, Some(owners)) => owners,
        (Leg::Cohort | Leg::SetCohort, None) => {
            return Err("a cohort leg takes --owners 1024 or 65536".into());
        }
        (_, None) => 0,
        (_, Some(_)) => return Err("--owners is a cohort leg's alone".into()),
    };
    Ok(Args { leg, nops, ops: ops.ok_or("--ops is required")?, owners })
}

/// The plants: `NOPS` instructions per op at the top of the driving loop's
/// body, each a `nop` with no operand, so the count must read exactly
/// `NOPS` more.
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

/// The A side's cell: its wheel, the loop's due-key vector reused across
/// iterations, and its injected clock.
struct WheelCell {
    wheel: TimerWheel,
    due: Vec<u64>,
    clock: VirtualClock,
}

impl WheelCell {
    fn new() -> WheelCell {
        WheelCell { wheel: TimerWheel::new(), due: Vec::new(), clock: VirtualClock::new(MILLI) }
    }
}

/// The plane's callback: the server's key tests, then the effect.
#[inline(never)]
fn on_timer(key: u64) -> u64 {
    match key {
        EVERYSEC_KEY => 11,
        FILL_KEY => 13,
        ACCEPT_KEY => 17,
        _ => 19,
    }
}

/// Step 1 on the wheel: `advance` to the turn's time, then each due key's
/// callback; the everysec key arms its successor, as the plane does.
#[inline(always)]
fn deliver(cell: &mut WheelCell, now: Nanos) -> u64 {
    let due = &mut cell.due;
    due.clear();
    cell.wheel.advance(now, |key| due.push(key));
    let mut effect = 0u64;
    for key in cell.due.drain(..) {
        effect = effect.wrapping_add(on_timer(key));
        if key == EVERYSEC_KEY {
            cell.wheel.insert(now + SECOND, EVERYSEC_KEY);
        }
    }
    effect
}

#[inline(never)]
fn step_idle(cell: &mut WheelCell) -> u64 {
    cell.clock.advance(MICRO);
    let now = cell.clock.now();
    deliver(cell, now)
}

#[inline(never)]
fn step_park(cell: &mut WheelCell) -> u64 {
    let now = cell.clock.now();
    let timeout = cell.wheel.next_deadline().map(|deadline| deadline.saturating_sub(now));
    timeout.map_or(PARK_DEFAULT, |t| t.min(PARK_DEFAULT)).0
}

#[inline(never)]
fn step_hold(cell: &mut WheelCell) -> u64 {
    let now = cell.clock.now();
    cell.wheel.insert(now + HOLD_WINDOW, FILL_KEY);
    cell.clock.advance(HOLD_WINDOW);
    let deadline = cell.clock.now();
    deliver(cell, deadline)
}

#[inline(never)]
fn step_cohort(cell: &mut WheelCell, owners: u32) -> u64 {
    let now = cell.clock.now();
    let mut cancelled = 0u64;
    for index in 0..u64::from(owners) {
        let id = cell.wheel.insert(now + MILLI, COHORT_KEY_BASE + index);
        if index % 2 == 0 && cell.wheel.cancel(id) {
            cancelled += 1;
        }
    }
    cell.clock.advance(MILLI);
    let deadline = cell.clock.now();
    cancelled.wrapping_add(deliver(cell, deadline))
}

/// The B side's cell: its set, the owners reserved before the loop, the
/// arms of one cohort, and its injected clock.
struct SetCell {
    set: TimerSet,
    owners: Vec<TimerOwner>,
    arms: Vec<TimerArm>,
    clock: VirtualClock,
    _census: Rc<CapCensus>,
}

impl SetCell {
    fn new(owners: u32) -> SetCell {
        let (census, _mark) = CapCensus::new(CellId(0));
        let cap = if owners == 1024 { OWNERS_1024 } else { OWNERS_65536 };
        let mut set = TimerSet::build(cap, &census).expect("built");
        let owners = (0..owners)
            .map(|index| match set.reserve(COHORT_KEY_BASE + u64::from(index)) {
                Reserved::Owner(owner) => owner,
                other => panic!("a free position: {other:?}"),
            })
            .collect();
        let arms = Vec::with_capacity(cap.entries_max() as usize);
        SetCell { set, owners, arms, clock: VirtualClock::new(MILLI), _census: census }
    }
}

#[inline(never)]
fn step_set_cohort(cell: &mut SetCell) -> u64 {
    let now = cell.clock.now();
    cell.arms.clear();
    for owner in &cell.owners {
        cell.arms.push(cell.set.arm(owner, now + MILLI, now).expect("armed"));
    }
    let mut effect = 0u64;
    for arm in cell.arms.iter().step_by(2) {
        if cell.set.cancel(arm) == Cancelled::Removed {
            effect += 1;
        }
    }
    cell.clock.advance(MILLI);
    let deadline = cell.clock.now();
    while let Some(notice) = cell.set.begin(deadline) {
        effect = effect.wrapping_add(on_timer(notice.route()));
        if cell.set.consume(&notice, TimerOutcome::Complete) == Consumed::Idle {
            effect += 1;
        }
    }
    effect
}

/// Drives `ops` set-cohort ops with `NOPS` planted per op.
#[inline(never)]
fn drive_set<const NOPS: u8>(ops: u64, owners: u32) -> u64 {
    let mut cell = SetCell::new(owners);
    let cell = black_box(&mut cell);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        accumulator = accumulator.wrapping_add(step_set_cohort(cell));
    }
    // Engagement, after the counted loop: every cohort armed every owner
    // and left none armed.
    assert_eq!(cell.set.armed(), 0, "the leg's timers armed after its ops");
    if ops > 0 {
        assert_eq!(cell.set.armed_max(), owners, "the leg armed its whole cohort");
    }
    accumulator.wrapping_add(u64::from(cell.set.armed_max()))
}

/// Drives `ops` ops of `leg` with `NOPS` planted per op.
#[inline(never)]
fn drive<const NOPS: u8>(leg: Leg, ops: u64, owners: u32) -> u64 {
    let mut cell = WheelCell::new();
    if matches!(leg, Leg::OneOut | Leg::Park | Leg::Hold) {
        let now = cell.clock.now();
        cell.wheel.insert(now + SECOND, EVERYSEC_KEY);
    }
    let cell = black_box(&mut cell);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        let effect = match leg {
            Leg::Idle | Leg::OneOut => step_idle(cell),
            Leg::Park => step_park(cell),
            Leg::Hold => step_hold(cell),
            Leg::Cohort => step_cohort(cell, owners),
            Leg::SetCohort => unreachable!("the set cohort drives its own cell"),
        };
        accumulator = accumulator.wrapping_add(effect);
    }
    // Engagement, after the counted loop: the leg left the population it
    // drove, so a leg that armed or delivered nothing reads red here.
    let armed = cell.wheel.armed();
    let expected = match leg {
        Leg::Idle | Leg::Cohort | Leg::SetCohort => 0,
        Leg::OneOut | Leg::Park | Leg::Hold => 1,
    };
    assert_eq!(armed, expected, "the leg's timers armed after its ops");
    if ops > 0 && matches!(leg, Leg::Hold | Leg::Cohort) {
        assert!(cell.wheel.armed_max() > expected, "the leg armed its episodes");
    }
    accumulator.wrapping_add(cell.wheel.armed_max() as u64)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("timer_ab: {error}");
            return ExitCode::from(2);
        }
    };
    let result = match (args.leg, args.nops) {
        (Leg::SetCohort, 0) => drive_set::<0>(args.ops, args.owners),
        (Leg::SetCohort, _) => drive_set::<1>(args.ops, args.owners),
        (_, 0) => drive::<0>(args.leg, args.ops, args.owners),
        (_, _) => drive::<1>(args.leg, args.ops, args.owners),
    };
    println!("{}", black_box(result));
    ExitCode::SUCCESS
}
