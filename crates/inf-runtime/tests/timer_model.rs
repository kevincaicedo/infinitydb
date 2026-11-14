#![allow(
    clippy::disallowed_types,
    reason = "test target: the model's std containers, outside cell code (ADR-0163 D2)"
)]
//! The timer set against an independent model (ADR-0154 D2, D3): seeded
//! sequences of reserve, arm, cancel, close, time steps and delivery turns
//! with callbacks, against a `BTreeMap<(deadline_ms, position), owner>` of
//! the armed owners and a table of each owner's phase and sequence, which
//! share no code with the set beyond `limits`. Every outcome must be D2's,
//! every delivery the model's due minimum and never early, at most
//! `TIMER_DELIVERIES_PER_TURN` a turn, and after every operation the set's
//! armed count and every owner's phase must equal the model's: the heap's
//! entries are exactly the Armed owners (the bijection).
//!
//! Two sets run side by side on one census, driven by one operation
//! stream, so each holds every (position, generation) the other mints, and
//! some operations hand one set the other's owner or arm: each is `Stale`
//! and changes neither set. Its canaries, each a `--cfg` of the workspace
//! list with a lane row: a cancel that leaves its entry, a close that
//! leaves its entry past its position's take, a due a millisecond early, a
//! close in a callback that frees the position at once, and the slab's
//! identity compare skipped, under which another set's close releases this
//! set's owner.

use std::collections::BTreeMap;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_runtime::limits::TIMER_DELIVERIES_PER_TURN;
use inf_runtime::{
    ArmError, Cancelled, Closed, Consumed, OwnerPhase, Reserved, TimerArm, TimerNotice,
    TimerOutcome, TimerOwner, TimerSet,
};

/// The model's positions: a `Serving` test cap, so two sets register it
/// on one census.
const OWNERS: Cap = Cap::entries::<16>("timer-model", CapFill::Serving);
const POSITIONS: usize = 16;
/// Operations a seed.
const OPERATIONS: u64 = 1_000_000;
const NANOS_PER_MS: u128 = 1_000_000;

/// The model's view of one owner the test holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Armed { deadline_ms: u64 },
    Delivering,
}

#[derive(Clone, Copy, Debug)]
struct ModelOwner {
    /// The test's name for this owner, never reused.
    uid: u64,
    position: u32,
    sequence: u64,
    phase: Phase,
}

/// One set and its model.
struct Side {
    set: TimerSet,
    /// The owners the test holds, by slot of this table; `None` once closed.
    owners: Vec<Option<TimerOwner>>,
    model: Vec<Option<ModelOwner>>,
    /// Positions whose owner is Closing: held until its notice settles.
    closing: Vec<u32>,
    /// The model heap: (deadline, position) → owner slot.
    armed: BTreeMap<(u64, u32), usize>,
    /// Every arm this set made, current or not, with its owner's name.
    arms: Vec<(TimerArm, u64)>,
    next_uid: u64,
    /// The rows reached, for the run's engagement.
    reached: Reached,
}

/// How often each outcome the model checks was reached.
#[derive(Clone, Copy, Debug, Default)]
struct Reached {
    deliveries: u64,
    full: u64,
    busy: u64,
    clock_range: u64,
    drains: u64,
    successors: u64,
}

impl Side {
    fn new(census: &Rc<CapCensus>) -> Side {
        Side {
            set: TimerSet::build(OWNERS, census).expect("built"),
            owners: Vec::new(),
            model: Vec::new(),
            closing: Vec::new(),
            armed: BTreeMap::new(),
            arms: Vec::new(),
            next_uid: 0,
            reached: Reached::default(),
        }
    }

    /// Positions held: every live owner's, and every closing one's.
    fn held(&self) -> usize {
        self.model.iter().flatten().count() + self.closing.len()
    }

    /// The model's normalized deadline, or `None` past the clock's range:
    /// rounded up to whole ms, at least the turn's ms + 1, and back in `u64`
    /// nanoseconds.
    fn normalized(deadline: Nanos, now: Nanos) -> Option<u64> {
        let requested = u128::from(deadline.0).div_ceil(NANOS_PER_MS);
        let earliest = u128::from(now.0) / NANOS_PER_MS + 1;
        let ms = requested.max(earliest);
        (ms * NANOS_PER_MS <= u128::from(u64::MAX)).then(|| u64::try_from(ms).expect("fits"))
    }

    fn reserve(&mut self, route: u64) {
        let answer = self.set.reserve(route);
        if self.held() == POSITIONS {
            assert_eq!(answer, Reserved::Full, "every position held: Full");
            self.reached.full += 1;
            return;
        }
        let Reserved::Owner(owner) = answer else { panic!("a free position: {answer:?}") };
        let position = owner.position();
        let held =
            self.model.iter().flatten().map(|m| m.position).chain(self.closing.iter().copied());
        for taken in held {
            assert_ne!(taken, position, "a held position handed out again (a delivering one too)");
        }
        self.owners.push(Some(owner));
        let uid = self.next_uid;
        self.next_uid += 1;
        self.model.push(Some(ModelOwner { uid, position, sequence: 0, phase: Phase::Idle }));
    }

    fn arm(&mut self, slot: usize, deadline: Nanos, now: Nanos) -> Option<TimerArm> {
        let (Some(owner), Some(model)) = (&self.owners[slot], self.model[slot]) else {
            return None;
        };
        let answer = self.set.arm(owner, deadline, now);
        let expected = match model.phase {
            Phase::Delivering => Err(ArmError::Busy),
            Phase::Idle | Phase::Armed { .. } => match Side::normalized(deadline, now) {
                None => Err(ArmError::ClockRange),
                Some(deadline_ms) => Ok(deadline_ms),
            },
        };
        match (answer, expected) {
            (Ok(arm), Ok(deadline_ms)) => {
                assert_eq!(arm.sequence(), model.sequence + 1, "an arm takes the next sequence");
                if let Phase::Armed { deadline_ms: old } = model.phase {
                    self.armed.remove(&(old, model.position));
                }
                self.armed.insert((deadline_ms, model.position), slot);
                self.model[slot] = Some(ModelOwner {
                    sequence: arm.sequence(),
                    phase: Phase::Armed { deadline_ms },
                    ..model
                });
                self.arms.push((arm, model.uid));
                Some(arm)
            }
            (Err(got), Err(want)) => {
                assert_eq!(got, want, "an arm's refusal (D2): {model:?}");
                match got {
                    ArmError::Busy => self.reached.busy += 1,
                    ArmError::ClockRange => self.reached.clock_range += 1,
                    ArmError::Stale | ArmError::SequenceExhausted => {}
                }
                None
            }
            (got, want) => {
                panic!("D2's outcome for an arm: got {got:?}, want {want:?} ({model:?})")
            }
        }
    }

    /// The model's slot for the owner named `uid`, if the test holds it.
    fn slot_of(&self, uid: u64) -> Option<usize> {
        self.model.iter().position(|m| m.is_some_and(|m| m.uid == uid))
    }

    fn cancel(&mut self, arm: &TimerArm, uid: u64) {
        let answer = self.set.cancel(arm);
        let slot = self.slot_of(uid).filter(|&s| self.model[s].unwrap().sequence == arm.sequence());
        let expected = match slot.map(|s| (s, self.model[s].unwrap())) {
            None => Cancelled::Stale,
            Some((_, model)) => match model.phase {
                Phase::Idle => Cancelled::NotArmed,
                Phase::Delivering => Cancelled::InCallback,
                Phase::Armed { .. } => Cancelled::Removed,
            },
        };
        assert_eq!(answer, expected, "D2's outcome for a cancel of {arm:?}");
        if let (Cancelled::Removed, Some(slot)) = (answer, slot) {
            let model = self.model[slot].as_mut().unwrap();
            if let Phase::Armed { deadline_ms } = model.phase {
                self.armed.remove(&(deadline_ms, model.position));
            }
            model.phase = Phase::Idle;
        }
    }

    fn close(&mut self, slot: usize) {
        let (Some(owner), Some(model)) = (self.owners[slot].take(), self.model[slot]) else {
            return;
        };
        let answer = self.set.close(owner);
        let expected = match model.phase {
            Phase::Delivering => Closed::Draining,
            Phase::Idle | Phase::Armed { .. } => Closed::Released,
        };
        assert_eq!(answer, expected, "D2's outcome for a close ({model:?})");
        if let Phase::Armed { deadline_ms } = model.phase {
            self.armed.remove(&(deadline_ms, model.position));
        }
        if expected == Closed::Draining {
            self.closing.push(model.position);
            self.reached.drains += 1;
        }
        self.model[slot] = None;
    }

    /// The bijection and every phase, after each operation.
    fn check(&self, label: &str) {
        assert_eq!(
            self.set.armed() as usize,
            self.armed.len(),
            "{label}: the heap's entries are not the model's Armed owners (the bijection)"
        );
        for (owner, model) in self.owners.iter().zip(&self.model) {
            let (Some(owner), Some(model)) = (owner, model) else { continue };
            let want = match model.phase {
                Phase::Idle => OwnerPhase::Idle,
                Phase::Armed { .. } => OwnerPhase::Armed,
                Phase::Delivering => OwnerPhase::Delivering,
            };
            assert_eq!(self.set.phase(owner), want, "{label}: an owner's phase");
        }
    }
}

/// A deadline from the hostile set: 0, the turn's time, 1 ns past it,
/// 1 ms before it, whole-ms boundaries, near deadlines, the last ms that
/// converts back to `Nanos`, and one past it.
fn deadline(rng: &mut SplitMix64, now: Nanos) -> Nanos {
    const LAST_MS: u64 = u64::MAX / 1_000_000;
    match rng.next_u64() % 12 {
        0 => Nanos(0),
        1 => now,
        2 => Nanos(now.0 + 1),
        3 => Nanos(now.0.saturating_sub(1_000_000)),
        4 => Nanos((now.0 / 1_000_000 + 1 + rng.next_u64() % 4) * 1_000_000),
        5 => Nanos(LAST_MS * 1_000_000),
        6 => Nanos(LAST_MS * 1_000_000 + 1),
        _ => Nanos(now.0 + rng.next_u64() % 8_000_000),
    }
}

/// What a callback does, drawn once and applied to both sides.
#[derive(Clone, Copy, Debug)]
enum Action {
    None,
    /// Arm its own owner: `Busy`.
    ArmSelf,
    /// Cancel its own arm: `InCallback`.
    CancelSelf,
    /// Close its own owner: `Draining`.
    CloseSelf,
    /// Arm another owner.
    ArmOther(usize),
    /// Close another owner.
    CloseOther(usize),
    /// Reserve a position: never the delivering one.
    Reserve,
}

/// Both sides, one stream.
struct Run {
    sides: [Side; 2],
    now: Nanos,
    rng: SplitMix64,
}

impl Run {
    fn new(seed: u64) -> Run {
        let (census, _mark) = CapCensus::new(CellId(0));
        Run {
            sides: [Side::new(&census), Side::new(&census)],
            now: Nanos(0),
            rng: SplitMix64::new(seed),
        }
    }

    fn check(&self, label: &str) {
        for side in &self.sides {
            side.check(label);
        }
    }

    fn step(&mut self) {
        let roll = self.rng.next_u64() % 100;
        let now = self.now;
        let pick = (self.rng.next_u64() % 64) as usize;
        match roll {
            0..15 => {
                let route = self.rng.next_u64();
                self.sides.iter_mut().for_each(|side| side.reserve(route));
            }
            15..45 => {
                let deadline = deadline(&mut self.rng, now);
                for side in &mut self.sides {
                    if !side.owners.is_empty() {
                        let slot = pick % side.owners.len();
                        side.arm(slot, deadline, now);
                    }
                }
            }
            45..55 => {
                for side in &mut self.sides {
                    if let Some(&(arm, uid)) = side.arms.get(pick % side.arms.len().max(1)) {
                        side.cancel(&arm, uid);
                    }
                }
            }
            55..62 => {
                for side in &mut self.sides {
                    if !side.owners.is_empty() {
                        let slot = pick % side.owners.len();
                        side.close(slot);
                    }
                }
            }
            62..70 => self.foreign(pick),
            70..85 => self.advance(),
            _ => self.turn(),
        }
        self.compact();
    }

    /// One side's owner or arm handed to the other: `Stale` everywhere,
    /// nothing changes on either side (the slab's identity, ADR-0194 D6).
    /// The sides are driven alike, so the other set holds a live owner at
    /// the same position and generation.
    fn foreign(&mut self, pick: usize) {
        let now = self.now;
        let [left, right] = &mut self.sides;
        foreign_on(left, right, pick, now);
        foreign_on(right, left, pick, now);
    }

    fn advance(&mut self) {
        let delta = match self.rng.next_u64() % 10 {
            0 => 0,
            1 => 1,
            2 => 3_600_000_000_000,
            _ => self.rng.next_u64() % 3_000_000,
        };
        self.now = Nanos(self.now.0 + delta);
    }

    /// One delivery turn: up to `TIMER_DELIVERIES_PER_TURN` callbacks, each
    /// the model's due minimum, with the same callback on both sides.
    fn turn(&mut self) {
        let now = self.now;
        let now_ms = now.0 / 1_000_000;
        for _ in 0..TIMER_DELIVERIES_PER_TURN {
            let actions = [self.action(), self.action()];
            let successor =
                self.rng.next_u64().is_multiple_of(3).then(|| deadline(&mut self.rng, now));
            let mut delivered = false;
            for side in &mut self.sides {
                let due = side.armed.first_key_value().filter(|((ms, _), _)| *ms <= now_ms);
                let due = due.map(|(key, slot)| (*key, *slot));
                assert_eq!(side.set.due(now), due.is_some(), "the set's due is the model's");
                let notice = side.set.begin(now);
                let Some(((deadline_ms, position), slot)) = due else {
                    assert!(notice.is_none(), "a delivery the model has none due for: early");
                    continue;
                };
                let notice = notice.expect("the model's due minimum is delivered");
                assert_eq!(
                    notice.arm().position(),
                    position,
                    "deliveries in (deadline, position) order"
                );
                let model = side.model[slot].unwrap();
                assert_eq!(notice.arm().sequence(), model.sequence, "the exact arm is delivered");
                side.armed.remove(&(deadline_ms, position));
                side.model[slot] = Some(ModelOwner { phase: Phase::Delivering, ..model });
                side.reached.deliveries += 1;
                side.callback(slot, &notice, actions, successor, now);
                delivered = true;
            }
            self.compact();
            self.check("a delivery");
            if !delivered {
                return;
            }
        }
    }

    fn action(&mut self) -> Action {
        let pick = (self.rng.next_u64() % 64) as usize;
        match self.rng.next_u64() % 9 {
            0 => Action::ArmSelf,
            1 => Action::CancelSelf,
            2 => Action::CloseSelf,
            3 => Action::ArmOther(pick),
            4 => Action::CloseOther(pick),
            5 => Action::Reserve,
            _ => Action::None,
        }
    }

    /// Drops the table rows of closed owners on both sides alike, so the
    /// tables stay bounded and aligned.
    fn compact(&mut self) {
        let keep: Vec<bool> = self.sides[0].owners.iter().map(Option::is_some).collect();
        for side in &mut self.sides {
            assert_eq!(side.owners.len(), keep.len(), "the two sides hold one table");
            let mut index = 0;
            side.owners.retain(|_| {
                index += 1;
                keep[index - 1]
            });
            let mut index = 0;
            side.model.retain(|_| {
                index += 1;
                keep[index - 1]
            });
            let remap: Vec<usize> = {
                let mut next = 0;
                keep.iter()
                    .map(|&kept| {
                        let at = next;
                        if kept {
                            next += 1;
                        }
                        at
                    })
                    .collect()
            };
            for slot in side.armed.values_mut() {
                *slot = remap[*slot];
            }
            if side.arms.len() > 256 {
                side.arms.drain(..128);
            }
        }
    }
}

/// `from`'s owner and arm handed to `to`.
fn foreign_on(from: &Side, to: &mut Side, pick: usize, now: Nanos) {
    let held = from.owners.iter().flatten().count();
    if let Some(owner) = from.owners.iter().flatten().nth(pick % held.max(1)) {
        assert_eq!(to.set.phase(owner), OwnerPhase::Stale, "another set's owner reads Stale");
        let arm = to.set.arm(owner, Nanos(now.0 + 5_000_000), now);
        assert_eq!(arm, Err(ArmError::Stale), "another set's owner arms nothing");
    }
    if let Some((arm, _)) = from.arms.last() {
        assert_eq!(to.set.cancel(arm), Cancelled::Stale, "another set's arm cancels nothing");
    }
}

impl Side {
    /// The callback for `notice`: the drawn actions, then its outcome, then
    /// its consumption, each against the model.
    fn callback(
        &mut self,
        slot: usize,
        notice: &TimerNotice,
        actions: [Action; 2],
        successor: Option<Nanos>,
        now: Nanos,
    ) {
        for action in actions {
            match action {
                Action::None => {}
                Action::ArmSelf => {
                    let owner = self.owners[slot].as_ref();
                    if let Some(owner) = owner {
                        let answer = self.set.arm(owner, Nanos(now.0 + 2_000_000), now);
                        assert_eq!(answer, Err(ArmError::Busy), "an arm during its callback");
                        self.reached.busy += 1;
                    }
                }
                Action::CancelSelf => {
                    assert_eq!(self.set.cancel(&notice.arm()), Cancelled::InCallback);
                }
                Action::CloseSelf => self.close(slot),
                Action::ArmOther(pick) => {
                    let other = pick % self.owners.len();
                    if other != slot {
                        self.arm(other, Nanos(now.0 + 1_500_000), now);
                    }
                }
                Action::CloseOther(pick) => {
                    let other = pick % self.owners.len();
                    if other != slot {
                        self.close(other);
                    }
                }
                Action::Reserve => self.reserve(notice.route()),
            }
        }
        let position = notice.arm().position();
        let outcome = match successor.map(|at| self.set.successor(notice, at, now)) {
            Some(Ok(next)) => TimerOutcome::Successor(next),
            Some(Err(refused)) => {
                assert_eq!(refused, ArmError::ClockRange, "a successor's one refusal here");
                TimerOutcome::Complete
            }
            None => TimerOutcome::Complete,
        };
        let armed_next = match &outcome {
            TimerOutcome::Successor(_) => successor.and_then(|at| Side::normalized(at, now)),
            TimerOutcome::Complete => None,
        };
        let answer = self.set.consume(notice, outcome);
        if let Some(at) = self.closing.iter().position(|p| *p == position) {
            assert_eq!(answer, Consumed::Released, "a closed owner's notice frees its position");
            self.closing.swap_remove(at);
            return;
        }
        let model = self.model[slot].expect("an owner closed in its callback is closing");
        match (answer, armed_next) {
            (Consumed::Idle, None) => {
                self.model[slot] = Some(ModelOwner { phase: Phase::Idle, ..model });
            }
            (Consumed::Armed(arm), Some(deadline_ms)) => {
                assert_eq!(
                    arm.sequence(),
                    model.sequence + 1,
                    "a successor takes the next sequence"
                );
                self.armed.insert((deadline_ms, position), slot);
                self.model[slot] = Some(ModelOwner {
                    sequence: arm.sequence(),
                    phase: Phase::Armed { deadline_ms },
                    ..model
                });
                self.arms.push((arm, model.uid));
                self.reached.successors += 1;
            }
            (got, want) => {
                panic!("D2's outcome for a consumption: got {got:?}, successor {want:?}")
            }
        }
        assert_eq!(
            self.set.consume(notice, TimerOutcome::Complete),
            Consumed::Stale,
            "consumed once"
        );
    }
}

/// The model over two seeds, 10⁶ operations each, with the engagement each
/// row needs: deliveries, `Full` answers, `Busy` and `ClockRange` refusals,
/// closes that drain and successors, each reached.
#[test]
fn the_set_matches_the_model_on_two_sets_driven_alike() {
    for seed in [0x5EED_0001_u64, 0x5EED_0002] {
        let mut run = Run::new(seed);
        for _ in 0..OPERATIONS {
            run.step();
            run.check("an operation");
        }
        run.check("the end");
        let reached = run.sides[0].reached;
        println!("seed {seed:#x}: {reached:?}");
        let rows = [
            reached.deliveries,
            reached.full,
            reached.busy,
            reached.clock_range,
            reached.drains,
            reached.successors,
        ];
        assert!(rows.iter().all(|&count| count > 0), "VACUOUS: a row never reached: {reached:?}");
    }
}

/// Another set's close (ADR-0194 D6): two sets on one census, built and
/// reserved alike, so the second holds a live owner at the position and
/// generation of the first's owner; that owner's close on the second set
/// is `Stale` and leaves the second set's own owner where it was, Idle or
/// Armed.
#[test]
fn another_sets_close_releases_nothing() {
    let (census, _mark) = CapCensus::new(CellId(0));
    for armed in [false, true] {
        let mut left = TimerSet::build(OWNERS, &census).expect("built");
        let mut right = TimerSet::build(OWNERS, &census).expect("built");
        let (Reserved::Owner(stray), Reserved::Owner(own)) = (left.reserve(1), right.reserve(1))
        else {
            panic!("a free position on each set");
        };
        assert_eq!(stray.position(), own.position(), "both sets minted one position");
        if armed {
            right.arm(&own, Nanos(5_000_000), Nanos(0)).expect("armed");
        }
        let before = right.phase(&own);
        assert_eq!(right.close(stray), Closed::Stale, "another set's close is Stale");
        assert_eq!(right.phase(&own), before, "the set's own owner is where it was");
        assert_eq!(right.armed(), u32::from(armed), "and its entry with it");
        assert_eq!(right.close(own), Closed::Released);
    }
}
