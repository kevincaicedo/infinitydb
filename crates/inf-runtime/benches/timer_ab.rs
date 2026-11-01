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
//!   each id kept in a vector reserved before the loop; once every owner is
//!   armed, every other one is cancelled and the rest are delivered at their
//!   deadline, so the wheel holds the whole cohort at once, as the set does.
//!   One op is the whole cohort, reported per op and per timer, never judged.
//!
//! The B legs, one op each, each the cell loop's own code for the path on
//! a set of the server's four positions, every owner reserved before the
//! loop (step 1 and the park decision as `reactor.rs` writes them, the hold
//! as the durable cell opens and ends an episode):
//!
//! - `set-idle`: nothing armed; the clock steps 1 µs, then step 1: the
//!   turn's allowance from the Maintenance budget and the empty heap's due
//!   check.
//! - `set-one-out`: the everysec owner 1 s out, its successor armed from
//!   the delivery's time; the clock steps 1 µs, then step 1: the allowance,
//!   the due check and, at each fire, the begin, the callback's route test,
//!   the successor and the consume.
//! - `set-park`: the park decision with the everysec owner armed: one
//!   clock read, the due check from it, and the timeout from that read.
//! - `set-hold`: one hold episode beside the everysec owner: the fill
//!   owner's arm one window out (with the armed high water it raises), the
//!   episode kept, then the issue's cancel of both policies' episodes.
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
//! at `n` = 10⁷, read outside this binary (`perf stat -e instructions:u`) on
//! one pinned performance core. A cohort leg is read per timer, `(I(n) −
//! I(0)) / (n · R)` for `R` owners, at `n` = 10² for 1,024 and 20 for
//! 65,536: two runs of a cohort leg differ by up to tens of instructions a
//! cohort, so a plant of one a cohort is not resolved, while one a timer is,
//! and the timer is the unit the row reports. A run counts only when its
//! controls, each a leg of this binary, hold:
//!
//! - the same binary against itself: each leg run twice agrees within 0.05
//!   per op, or per timer for a cohort leg, at a resolution of 0.01;
//! - liveness: each leg's `-plus-one` variant reads the leg + 1.00, ± 0.05:
//!   one `nop` per op at the top of the driving loop's body, outside the
//!   measured step, or for a cohort leg one per timer at the top of the
//!   step's arm loop, which visits every owner once a cohort;
//! - engagement: after its loop each leg asserts the timers it left armed
//!   and, for the hold and cohort legs, that it armed its episodes (a cohort
//!   leg, its whole cohort at once).
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
use inf_runtime::limits::TIMER_DELIVERIES_PER_TURN;
use inf_runtime::{
    Cancelled, Consumed, GroupClass, GroupScheduler, Reserved, TimerArm, TimerOutcome, TimerOwner,
    TimerSet,
};

use wheel::{TimerId, TimerWheel};

/// One microsecond and one millisecond of injected time.
const MICRO: Nanos = Nanos(1_000);
const MILLI: Nanos = Nanos(1_000_000);
/// The everysec timer's period.
const SECOND: Nanos = Nanos(1_000_000_000);
/// The park ceiling, `LoopConfig::park_default`'s.
const PARK_DEFAULT: Nanos = Nanos(100_000_000);
/// The fill hold's window at its design point.
const HOLD_WINDOW: Nanos = MILLI;
/// The callbacks' routing keys, the server's four.
const EVERYSEC_KEY: u64 = 1;
const FILL_KEY: u64 = 2;
const ACCEPT_KEY: u64 = 3;
const GROUP_KEY: u64 = 4;
/// A cohort's keys start above the server's, so no cohort timer is taken
/// for one of theirs.
const COHORT_KEY_BASE: u64 = 1 << 32;
/// The judged B legs' set: the server's four positions.
const OWNERS_4: Cap = Cap::entries::<4>("bench-owners-4", CapFill::Serving);
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
    SetIdle,
    SetOneOut,
    SetPark,
    SetHold,
    SetCohort,
}

/// The one table of leg names: `--variant` is parsed against it.
const LEGS: [(&str, Leg, u8); 20] = [
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
    ("set-idle", Leg::SetIdle, 0),
    ("set-idle-plus-one", Leg::SetIdle, 1),
    ("set-one-out", Leg::SetOneOut, 0),
    ("set-one-out-plus-one", Leg::SetOneOut, 1),
    ("set-park", Leg::SetPark, 0),
    ("set-park-plus-one", Leg::SetPark, 1),
    ("set-hold", Leg::SetHold, 0),
    ("set-hold-plus-one", Leg::SetHold, 1),
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

/// The plants: `NOPS` operand-free `nop`s at the top of a loop body that
/// runs once per unit the leg reports (an op, or a cohort leg's timer), so
/// the count must read exactly `NOPS` more a unit.
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

/// The A side's cohort cell: the wheel's cell and the ids of one cohort's
/// arms, reserved before the loop.
struct CohortCell {
    cell: WheelCell,
    ids: Vec<TimerId>,
}

impl CohortCell {
    fn new(owners: u32) -> CohortCell {
        CohortCell { cell: WheelCell::new(), ids: Vec::with_capacity(owners as usize) }
    }
}

#[inline(never)]
fn step_cohort<const NOPS: u8>(cohort: &mut CohortCell, owners: u32) -> u64 {
    let cell = &mut cohort.cell;
    let now = cell.clock.now();
    cohort.ids.clear();
    for index in 0..u64::from(owners) {
        plant::<NOPS>();
        cohort.ids.push(cell.wheel.insert(now + MILLI, COHORT_KEY_BASE + index));
    }
    let mut cancelled = 0u64;
    for &id in cohort.ids.iter().step_by(2) {
        if cell.wheel.cancel(id) {
            cancelled += 1;
        }
    }
    cell.clock.advance(MILLI);
    let deadline = cell.clock.now();
    cancelled.wrapping_add(deliver(cell, deadline))
}

/// Drives `ops` wheel-cohort ops with `NOPS` planted per timer.
#[inline(never)]
fn drive_cohort<const NOPS: u8>(ops: u64, owners: u32) -> u64 {
    let mut cohort = CohortCell::new(owners);
    let cohort = black_box(&mut cohort);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        accumulator = accumulator.wrapping_add(step_cohort::<NOPS>(cohort, owners));
    }
    // Engagement, after the counted loop: every cohort held every owner
    // armed at once and left none armed.
    let wheel = &cohort.cell.wheel;
    assert_eq!(wheel.armed(), 0, "the leg's timers armed after its ops");
    if ops > 0 {
        assert_eq!(wheel.armed_max(), owners as usize, "the leg armed its whole cohort");
    }
    accumulator.wrapping_add(wheel.armed_max() as u64)
}

/// One open hold episode, as the durable cell keeps it.
#[derive(Clone, Copy)]
struct Episode {
    _since: Nanos,
    arm: TimerArm,
}

/// The judged B legs' cell: the server's set of four, its owners reserved
/// before the loop (routed as the A side's keys), the loop's scheduler,
/// the durable cell's two episodes, its injected clock, and the
/// deliveries the leg made.
struct LoopSetCell {
    set: TimerSet,
    everysec: TimerOwner,
    fill: TimerOwner,
    sched: GroupScheduler,
    fill_episode: Option<Episode>,
    group_episode: Option<Episode>,
    clock: VirtualClock,
    delivered: u64,
    _census: Rc<CapCensus>,
}

impl LoopSetCell {
    fn new(everysec_armed: bool) -> LoopSetCell {
        let (census, _mark) = CapCensus::new(CellId(0));
        let mut set = TimerSet::build(OWNERS_4, &census).expect("built");
        let mut owner = |route| match set.reserve(route) {
            Reserved::Owner(owner) => owner,
            other => panic!("a free position: {other:?}"),
        };
        let (_accept, everysec, fill, _group) =
            (owner(ACCEPT_KEY), owner(EVERYSEC_KEY), owner(FILL_KEY), owner(GROUP_KEY));
        let clock = VirtualClock::new(MILLI);
        if everysec_armed {
            let now = clock.now();
            set.arm(&everysec, now + SECOND, now).expect("armed");
        }
        LoopSetCell {
            set,
            everysec,
            fill,
            sched: GroupScheduler::m0_default(),
            fill_episode: None,
            group_episode: None,
            clock,
            delivered: 0,
            _census: census,
        }
    }
}

/// Step 1's timers on the set, as `reactor.rs` writes them: the allowance
/// from the Maintenance budget, then each due arm begun, answered by the
/// callback (the everysec route hands back its successor a second on, as
/// the plane does) and consumed, while the allowance lasts.
#[inline(always)]
fn deliver_set(cell: &mut LoopSetCell, now: Nanos) -> u64 {
    let mut left = TIMER_DELIVERIES_PER_TURN.min(cell.sched.budget(GroupClass::Maintenance));
    let mut take = || match left.checked_sub(1) {
        Some(rest) => {
            left = rest;
            true
        }
        None => false,
    };
    let mut effect = 0u64;
    while cell.set.due(now) && take() {
        let Some(notice) = cell.set.begin(now) else { break };
        cell.sched.charge(GroupClass::Maintenance, 1);
        effect = effect.wrapping_add(on_timer(notice.route()));
        let outcome = if notice.route() == EVERYSEC_KEY {
            TimerOutcome::Successor(cell.set.successor(&notice, now + SECOND, now).expect("next"))
        } else {
            TimerOutcome::Complete
        };
        if cell.set.consume(&notice, outcome) == Consumed::Stale {
            effect += 1;
        }
        cell.delivered += 1;
    }
    effect
}

#[inline(never)]
fn step_set_idle(cell: &mut LoopSetCell) -> u64 {
    cell.clock.advance(MICRO);
    let now = cell.clock.now();
    deliver_set(cell, now)
}

/// The park decision's timer half, as `reactor.rs` writes it: one clock
/// read with a timer armed, the due check from it, and the timeout from the
/// same read.
#[inline(never)]
fn step_set_park(cell: &mut LoopSetCell) -> u64 {
    let now = (cell.set.armed() > 0).then(|| cell.clock.now());
    if now.is_some_and(|now| cell.set.due(now)) {
        return 0;
    }
    let timeout = now.zip(cell.set.next_deadline()).map(|(now, d)| d.saturating_sub(now));
    timeout.map_or(PARK_DEFAULT, |t| t.min(PARK_DEFAULT)).0
}

/// One hold episode, as the durable cell opens and ends it: the fill
/// owner armed one window out, the episode kept; then the issue's cancel of
/// both policies' episodes.
#[inline(never)]
fn step_set_hold(cell: &mut LoopSetCell) -> u64 {
    let since = cell.clock.now();
    let deadline = since.saturating_add(HOLD_WINDOW);
    let arm = cell.set.arm(&cell.fill, deadline, since).ok();
    cell.fill_episode = arm.map(|arm| Episode { _since: since, arm });
    let ended = [cell.fill_episode.take(), cell.group_episode.take()];
    let mut effect = 0u64;
    for episode in ended.into_iter().flatten() {
        effect += match cell.set.cancel(&episode.arm) {
            Cancelled::Removed => 1,
            Cancelled::NotArmed => 2,
            Cancelled::InCallback | Cancelled::Stale => 3,
        };
    }
    effect
}

/// Drives `ops` ops of a judged B leg with `NOPS` planted per op.
#[inline(never)]
fn drive_loop_set<const NOPS: u8>(leg: Leg, ops: u64) -> u64 {
    let mut cell = LoopSetCell::new(!matches!(leg, Leg::SetIdle));
    let cell = black_box(&mut cell);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        plant::<NOPS>();
        let effect = match leg {
            Leg::SetIdle | Leg::SetOneOut => step_set_idle(cell),
            Leg::SetPark => step_set_park(cell),
            Leg::SetHold => step_set_hold(cell),
            Leg::Idle | Leg::OneOut | Leg::Park | Leg::Hold | Leg::Cohort | Leg::SetCohort => {
                unreachable!("a judged B leg drives its own cell")
            }
        };
        accumulator = accumulator.wrapping_add(effect);
    }
    // Engagement, after the counted loop: the everysec owner armed on every
    // leg but the idle one, the one-out leg delivered its tick once a
    // simulated second, and the hold leg armed its episodes.
    let expected = u32::from(!matches!(leg, Leg::SetIdle));
    assert_eq!(cell.set.armed(), expected, "the leg's timers armed after its ops");
    assert_eq!(cell.set.phase(&cell.everysec) == inf_runtime::OwnerPhase::Armed, expected == 1);
    if matches!(leg, Leg::SetOneOut) {
        assert_eq!(cell.delivered, ops / 1_000_000, "the tick delivered once a second");
    }
    if ops > 0 && matches!(leg, Leg::SetHold) {
        assert!(cell.set.armed_max() > expected, "the leg armed its episodes");
    }
    accumulator.wrapping_add(u64::from(cell.set.armed_max())).wrapping_add(cell.delivered)
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
fn step_set_cohort<const NOPS: u8>(cell: &mut SetCell) -> u64 {
    let now = cell.clock.now();
    cell.arms.clear();
    for owner in &cell.owners {
        plant::<NOPS>();
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

/// Drives `ops` set-cohort ops with `NOPS` planted per timer.
#[inline(never)]
fn drive_set<const NOPS: u8>(ops: u64, owners: u32) -> u64 {
    let mut cell = SetCell::new(owners);
    let cell = black_box(&mut cell);
    let mut accumulator = 0u64;
    for _ in 0..ops {
        accumulator = accumulator.wrapping_add(step_set_cohort::<NOPS>(cell));
    }
    // Engagement, after the counted loop: every cohort armed every owner
    // and left none armed.
    assert_eq!(cell.set.armed(), 0, "the leg's timers armed after its ops");
    if ops > 0 {
        assert_eq!(cell.set.armed_max(), owners, "the leg armed its whole cohort");
    }
    accumulator.wrapping_add(u64::from(cell.set.armed_max()))
}

/// Drives `ops` ops of a one-op leg with `NOPS` planted per op.
#[inline(never)]
fn drive<const NOPS: u8>(leg: Leg, ops: u64) -> u64 {
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
            Leg::Cohort
            | Leg::SetIdle
            | Leg::SetOneOut
            | Leg::SetPark
            | Leg::SetHold
            | Leg::SetCohort => unreachable!("a cohort or B leg drives its own cell"),
        };
        accumulator = accumulator.wrapping_add(effect);
    }
    // Engagement, after the counted loop: the leg left the population it
    // drove, so a leg that armed or delivered nothing reads red here.
    let armed = cell.wheel.armed();
    let expected = match leg {
        Leg::Idle
        | Leg::Cohort
        | Leg::SetIdle
        | Leg::SetOneOut
        | Leg::SetPark
        | Leg::SetHold
        | Leg::SetCohort => 0,
        Leg::OneOut | Leg::Park | Leg::Hold => 1,
    };
    assert_eq!(armed, expected, "the leg's timers armed after its ops");
    if ops > 0 && matches!(leg, Leg::Hold) {
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
        (Leg::Cohort, 0) => drive_cohort::<0>(args.ops, args.owners),
        (Leg::Cohort, _) => drive_cohort::<1>(args.ops, args.owners),
        (Leg::SetCohort, 0) => drive_set::<0>(args.ops, args.owners),
        (Leg::SetCohort, _) => drive_set::<1>(args.ops, args.owners),
        (Leg::SetIdle | Leg::SetOneOut | Leg::SetPark | Leg::SetHold, 0) => {
            drive_loop_set::<0>(args.leg, args.ops)
        }
        (Leg::SetIdle | Leg::SetOneOut | Leg::SetPark | Leg::SetHold, _) => {
            drive_loop_set::<1>(args.leg, args.ops)
        }
        (_, 0) => drive::<0>(args.leg, args.ops),
        (_, _) => drive::<1>(args.leg, args.ops),
    };
    println!("{}", black_box(result));
    ExitCode::SUCCESS
}
