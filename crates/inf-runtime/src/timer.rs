//! The cell's fixed timer set (ADR-0154): owner positions reserved before
//! serving, at most one arm an owner, the armed ones in an indexed minimum
//! heap by (deadline, position), delivered one notice at a time by the
//! cell's loop. Millisecond resolution on injected time; no arm falls due
//! before its rounded deadline or in the turn that made it, a cancel
//! removes its entry at once, and nothing allocates once the set is built.

use core::marker::PhantomData;
use core::mem::size_of;
use std::rc::Rc;

use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapError, CappedSlab, Lookup, LookupMut, Retire, Retiring, SlabReserved,
    SlabRetiring, SlotId, Taken,
};
use inf_foundation::time::Nanos;

use crate::heap::{ArrayBuilt, FixedArray, FixedArrayBuilder, HeapEntry, IndexedHeap};

/// Nanoseconds in a millisecond, the set's resolution.
const NANOS_PER_MS: u64 = 1_000_000;

/// One reserved owner position of a [`TimerSet`]: the slab's handle, its
/// position, generation and the set's identity (ADR-0194 D1, D5), so
/// another set's owner, or one whose position was taken, is `Stale`. Not
/// `Clone`: a position has one holder, and [`TimerSet::close`] takes it.
#[derive(Debug, PartialEq, Eq)]
pub struct TimerOwner {
    id: SlotId<u64>,
}

impl TimerOwner {
    /// The owner's position in its set.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.id.index()
    }
}

/// One arm of an owner: the owner and the arm's sequence. A copy: an arm
/// replaced, cancelled, delivered or closed is `Stale` to every operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerArm {
    owner: SlotId<u64>,
    sequence: u64,
}

impl TimerArm {
    /// The arm's owner position.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.owner.index()
    }

    /// The arm's sequence: 1 for an owner's first arm.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// The delivery of one arm: the exact arm and its owner's route. Made only
/// by [`TimerSet::begin`], never cloned, so a callback holds the notice of
/// the arm it answers and nothing else.
#[derive(Debug, PartialEq, Eq)]
pub struct TimerNotice {
    arm: TimerArm,
    route: u64,
}

impl TimerNotice {
    /// The arm delivered.
    #[must_use]
    pub fn arm(&self) -> TimerArm {
        self.arm
    }

    /// The route its owner was reserved with.
    #[must_use]
    pub fn route(&self) -> u64 {
        self.route
    }
}

/// A next deadline for the arm a notice delivered, checked when it was
/// made: its range and its sequence. It borrows the notice it answers and
/// is not `Clone`, so a callback can neither keep one past its delivery nor
/// return one from another delivery:
///
/// ```compile_fail,E0597
/// use inf_foundation::CellId;
/// use inf_foundation::bounded::{Cap, CapCensus, CapFill};
/// use inf_foundation::time::Nanos;
/// use inf_runtime::{Reserved, TimerSet};
///
/// const OWNERS: Cap = Cap::entries::<1>("doc-owners", CapFill::Serving);
/// let (census, _mark) = CapCensus::new(CellId(0));
/// let mut set = TimerSet::build(OWNERS, &census).unwrap();
/// let Reserved::Owner(owner) = set.reserve(7) else { panic!() };
/// set.arm(&owner, Nanos(1_000_000), Nanos(0)).unwrap();
/// let kept;
/// {
///     let notice = set.begin(Nanos(1_000_000)).unwrap();
///     kept = set.successor(&notice, Nanos(5_000_000), Nanos(1_000_000)).unwrap();
/// }
/// drop(kept);
/// ```
///
/// Its twin, the successor consumed with its notice, compiles:
///
/// ```
/// use inf_foundation::CellId;
/// use inf_foundation::bounded::{Cap, CapCensus, CapFill};
/// use inf_foundation::time::Nanos;
/// use inf_runtime::{Reserved, TimerOutcome, TimerSet};
///
/// const OWNERS: Cap = Cap::entries::<1>("doc-owners", CapFill::Serving);
/// let (census, _mark) = CapCensus::new(CellId(0));
/// let mut set = TimerSet::build(OWNERS, &census).unwrap();
/// let Reserved::Owner(owner) = set.reserve(7) else { panic!() };
/// set.arm(&owner, Nanos(1_000_000), Nanos(0)).unwrap();
/// let notice = set.begin(Nanos(1_000_000)).unwrap();
/// let next = set.successor(&notice, Nanos(5_000_000), Nanos(1_000_000)).unwrap();
/// set.consume(&notice, TimerOutcome::Successor(next));
/// ```
#[derive(Debug)]
pub struct Successor<'n> {
    arm: TimerArm,
    deadline_ms: u64,
    notice: PhantomData<&'n TimerNotice>,
}

/// What a callback hands back for the notice it answered.
#[derive(Debug)]
pub enum TimerOutcome<'n> {
    /// The owner goes Idle.
    Complete,
    /// The owner arms again, at the successor's deadline.
    Successor(Successor<'n>),
}

/// The answer to [`TimerSet::reserve`].
#[derive(Debug, PartialEq, Eq)]
pub enum Reserved {
    /// A position, Idle, at a generation no earlier owner held.
    Owner(TimerOwner),
    /// Every position that can serve holds an owner.
    Full,
    /// Every position has retired at its last generation.
    Exhausted,
}

/// Why an arm or a successor was refused; a refusal changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmError {
    /// The owner's callback is running: it may hand back a successor.
    Busy,
    /// Not an owner or a delivery of this set now.
    Stale,
    /// The deadline, rounded up to whole milliseconds, is past what
    /// `Nanos` holds.
    ClockRange,
    /// The owner has used its last arm sequence.
    SequenceExhausted,
}

/// The answer to [`TimerSet::cancel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancelled {
    /// The arm's entry is removed; the owner is Idle.
    Removed,
    /// The arm was delivered or cancelled already; the owner is Idle.
    NotArmed,
    /// The arm's callback is running and cannot be undone.
    InCallback,
    /// Another arm of the owner, another set's arm, or a taken position.
    Stale,
}

/// The answer to [`TimerSet::close`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The position is free; any arm was removed first.
    Released,
    /// The owner's callback is running: the position frees when its
    /// notice is consumed, and any successor it hands back is dropped.
    Draining,
    /// Not an owner of this set now.
    Stale,
}

/// An owner's phase, as [`TimerSet::phase`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPhase {
    Idle,
    Armed,
    Delivering,
    Closing,
    Stale,
}

/// The answer to [`TimerSet::consume`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consumed {
    /// The callback completed; the owner is Idle.
    Idle,
    /// The successor is armed.
    Armed(TimerArm),
    /// The owner was closed during its callback; its position is free.
    Released,
    /// The notice names no delivery in progress of this set.
    Stale,
}

/// One owner position's occupant: the route its host reserved it with,
/// its last arm sequence (0 before its first arm) and its phase. The
/// deadline lives in the heap entry alone.
struct OwnerSlot {
    route: u64,
    sequence: u64,
    phase: SlotPhase,
}

/// An owner's phase in its slot; an Armed owner keeps its entry's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    Idle,
    Armed { heap_index: u32 },
    Delivering,
    Closing,
}

/// A heap entry: the arm's whole-millisecond deadline and its owner's
/// position, ordered by both, so ties deliver by position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArmEntry {
    deadline_ms: u64,
    position: u32,
}

impl HeapEntry for ArmEntry {
    type Key = (u64, u32);

    #[inline]
    fn key(&self) -> (u64, u32) {
        (self.deadline_ms, self.position)
    }
}

// The per-position bill (ADR-0154 D1): an occupant of 24 B, the store's
// slot adding its generation; a 16 B heap entry; a 16 B handle, `None`
// taking the identity's niche.
const _: () = assert!(size_of::<OwnerSlot>() == 24, "an owner's occupant is 24 B");
const _: () = assert!(size_of::<ArmEntry>() == 16, "a heap entry is 16 B");
const _: () = assert!(size_of::<Option<SlotId<u64>>>() == 16, "a position's handle is 16 B");

/// A cell's fixed timer owners (ADR-0154 D1–D3): positions reserved before
/// serving in a capped slab under `Retire` (ADR-0194 D2), the armed ones in
/// an indexed minimum heap by (deadline, position), and beside the slab's
/// slots the handle each position's occupant holds, written at the reserve
/// and cleared at the take, through which a heap entry reaches its owner.
/// An entry stays fresh by order, not by a compare: a close removes an
/// Armed owner's entry before its take, and no position is taken while its
/// owner's callback runs.
///
/// Nothing allocates once built: the heap and the handle array are fixed
/// at the slab's cap and built with it, by steps of one allocation of at
/// most 64 KiB.
///
/// Delivery is one owner at a time: [`begin`](Self::begin) takes the due
/// minimum and makes its notice, the callback runs, and
/// [`consume`](Self::consume) settles it before the next. The set's one
/// holder owns that order, and reserves its owners before it serves.
pub struct TimerSet {
    slots: CappedSlab<OwnerSlot, u64, Retire>,
    heap: IndexedHeap<ArmEntry>,
    handles: FixedArray<Option<SlotId<u64>>>,
    armed_max: u32,
}

impl TimerSet {
    /// A set of `owners` positions on `census`, built whole before its
    /// loop serves: the heap and the handle array, then the slab, one
    /// allocation of at most 64 KiB per step. A refused step returns its
    /// `CapError` with nothing served: the arrays drop whole and the slab's
    /// prefix retires by steps. A shape no allocation holds is `Layout`,
    /// refused before the census registers the row.
    pub fn build(owners: Cap, census: &Rc<CapCensus>) -> Result<TimerSet, CapError> {
        let positions = owners.entries_max();
        let heap = build_array(FixedArray::builder(positions, VACANT_ENTRY)?)?;
        let handles = build_array(FixedArray::builder(positions, None)?)?;
        let mut builder = CappedSlab::<OwnerSlot, u64, Retire>::builder(owners, census)?;
        let slots = loop {
            builder = match builder.step() {
                Built::More(next) => next,
                Built::Live(slots) => break slots,
                Built::Refused(prefix, refused) => {
                    retire_prefix(prefix);
                    return Err(refused);
                }
            };
        };
        Ok(TimerSet { slots, heap: IndexedHeap::new(heap), handles, armed_max: 0 })
    }

    /// A free position for a host's owner, Idle, with `route` delivered in
    /// its notices; `Full` or `Exhausted` change nothing.
    pub fn reserve(&mut self, route: u64) -> Reserved {
        match self.slots.reserve() {
            SlabReserved::Slot(slot) => {
                let id = slot.publish(OwnerSlot { route, sequence: 0, phase: SlotPhase::Idle });
                self.handles.set(id.index(), Some(id));
                Reserved::Owner(TimerOwner { id })
            }
            SlabReserved::Full => Reserved::Full,
            SlabReserved::Exhausted => Reserved::Exhausted,
        }
    }

    /// Arms `owner` at `deadline`, replacing its arm if it holds one. The
    /// deadline is rounded up to whole milliseconds and raised to the turn
    /// `now`'s millisecond + 1, so no arm falls due in the turn that made
    /// it. Checked before any effect: the owner (`Stale`), its phase
    /// (`Busy` while its callback runs), the deadline's range and the next
    /// sequence. Then a removal of its entry if Armed, one sift, and a
    /// push, one sift.
    pub fn arm(
        &mut self,
        owner: &TimerOwner,
        deadline: Nanos,
        now: Nanos,
    ) -> Result<TimerArm, ArmError> {
        let LookupMut::Live(slot) = self.slots.get_mut(owner.id) else {
            return Err(ArmError::Stale);
        };
        let prior = match slot.phase {
            SlotPhase::Idle => None,
            SlotPhase::Armed { heap_index } => Some(heap_index),
            SlotPhase::Delivering | SlotPhase::Closing => return Err(ArmError::Busy),
        };
        let deadline_ms = normalized_ms(deadline, now)?;
        let sequence = slot.sequence.checked_add(1).ok_or(ArmError::SequenceExhausted)?;
        slot.sequence = sequence;
        slot.phase = SlotPhase::Armed { heap_index: UNPLACED };
        if let Some(heap_index) = prior {
            self.remove_entry(heap_index);
        }
        self.push_entry(ArmEntry { deadline_ms, position: owner.id.index() });
        Ok(TimerArm { owner: owner.id, sequence })
    }

    /// Cancels exactly `arm`: an Armed owner's entry is removed, one sift,
    /// and the owner goes Idle. Its callback running is `InCallback`; a
    /// delivered or cancelled arm is `NotArmed`; any other arm is `Stale`.
    pub fn cancel(&mut self, arm: &TimerArm) -> Cancelled {
        let LookupMut::Live(slot) = self.slots.get_mut(arm.owner) else {
            return Cancelled::Stale;
        };
        if slot.sequence != arm.sequence {
            return Cancelled::Stale;
        }
        match slot.phase {
            SlotPhase::Idle => Cancelled::NotArmed,
            SlotPhase::Delivering | SlotPhase::Closing => Cancelled::InCallback,
            SlotPhase::Armed { heap_index } => {
                slot.phase = SlotPhase::Idle;
                // Canary: the cancel leaves its entry in the heap.
                #[cfg(not(inf_canary_timer_lazy_cancel))]
                self.remove_entry(heap_index);
                #[cfg(inf_canary_timer_lazy_cancel)]
                let _ = heap_index;
                Cancelled::Removed
            }
        }
    }

    /// Closes `owner`. Idle or Armed: its entry removed first, then its
    /// position taken (the generation advances; at the last it retires),
    /// `Released`. Its callback running: `Draining`, and the position
    /// frees when that notice is consumed.
    pub fn close(&mut self, owner: TimerOwner) -> Closed {
        let LookupMut::Live(slot) = self.slots.get_mut(owner.id) else {
            return Closed::Stale;
        };
        match slot.phase {
            SlotPhase::Delivering | SlotPhase::Closing => {
                slot.phase = SlotPhase::Closing;
                // Canary: a close while delivering frees the position at
                // once, so a reserve in the callback can take it.
                #[cfg(inf_canary_timer_reuse_in_callback)]
                {
                    self.release(owner.id);
                    Closed::Released
                }
                #[cfg(not(inf_canary_timer_reuse_in_callback))]
                {
                    Closed::Draining
                }
            }
            SlotPhase::Idle => {
                self.release(owner.id);
                Closed::Released
            }
            SlotPhase::Armed { heap_index } => {
                // Canary: the close takes the position and leaves its entry.
                #[cfg(not(inf_canary_timer_close_keeps_entry))]
                self.remove_entry(heap_index);
                #[cfg(inf_canary_timer_close_keeps_entry)]
                let _ = heap_index;
                self.release(owner.id);
                Closed::Released
            }
        }
    }

    /// `owner`'s phase; another set's owner, or a taken position, is
    /// `Stale`.
    #[must_use]
    pub fn phase(&self, owner: &TimerOwner) -> OwnerPhase {
        match self.slots.get(owner.id) {
            Lookup::Live(slot) => match slot.phase {
                SlotPhase::Idle => OwnerPhase::Idle,
                SlotPhase::Armed { .. } => OwnerPhase::Armed,
                SlotPhase::Delivering => OwnerPhase::Delivering,
                SlotPhase::Closing => OwnerPhase::Closing,
            },
            Lookup::Stale | Lookup::Vacant => OwnerPhase::Stale,
        }
    }

    /// Whether the minimum is due at `now`: its deadline, in whole
    /// milliseconds, at or before `now`'s millisecond.
    #[must_use]
    #[inline]
    pub fn due(&self, now: Nanos) -> bool {
        self.heap.peek().is_some_and(|entry| due_at(entry.deadline_ms, now))
    }

    /// The minimum's deadline, `None` with nothing armed.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Nanos> {
        // Exact: an arm checked that its deadline converts back to `Nanos`.
        self.heap.peek().map(|entry| Nanos(entry.deadline_ms * NANOS_PER_MS))
    }

    /// Begins the delivery of the due minimum at `now`: its entry popped,
    /// one sift, and its owner Delivering, its position kept until the
    /// notice is consumed. `None` when nothing is due.
    pub fn begin(&mut self, now: Nanos) -> Option<TimerNotice> {
        let entry = self.heap.peek().filter(|entry| due_at(entry.deadline_ms, now))?;
        let Self { slots, heap, handles, .. } = self;
        heap.pop(|moved, index| place(slots, handles, moved.position, index));
        let (id, slot) = occupant(slots, handles, entry.position);
        debug_assert!(
            matches!(slot.phase, SlotPhase::Armed { .. }),
            "a heap entry's owner is armed"
        );
        slot.phase = SlotPhase::Delivering;
        Some(TimerNotice {
            arm: TimerArm { owner: id, sequence: slot.sequence },
            route: slot.route,
        })
    }

    /// A successor for the arm `notice` delivered, at `deadline` rounded as
    /// [`arm`](Self::arm) rounds: its range and the owner's next sequence
    /// are checked now, so consuming it cannot refuse. `Stale` for a notice
    /// this set is not delivering.
    pub fn successor<'n>(
        &self,
        notice: &'n TimerNotice,
        deadline: Nanos,
        now: Nanos,
    ) -> Result<Successor<'n>, ArmError> {
        let Lookup::Live(slot) = self.slots.get(notice.arm.owner) else {
            return Err(ArmError::Stale);
        };
        let delivering = matches!(slot.phase, SlotPhase::Delivering | SlotPhase::Closing);
        if !delivering || slot.sequence != notice.arm.sequence {
            return Err(ArmError::Stale);
        }
        let deadline_ms = normalized_ms(deadline, now)?;
        let sequence = slot.sequence.checked_add(1).ok_or(ArmError::SequenceExhausted)?;
        let arm = TimerArm { owner: notice.arm.owner, sequence };
        Ok(Successor { arm, deadline_ms, notice: PhantomData })
    }

    /// Settles `notice` with its callback's `outcome`. Delivering:
    /// `Complete` makes the owner Idle, a successor arms it, one sift.
    /// Closing: the successor is dropped and the position taken. A notice
    /// with no delivery in progress, or a successor made for another
    /// notice, is `Stale` and changes nothing.
    pub fn consume<'n>(&mut self, notice: &'n TimerNotice, outcome: TimerOutcome<'n>) -> Consumed {
        let LookupMut::Live(slot) = self.slots.get_mut(notice.arm.owner) else {
            return Consumed::Stale;
        };
        if slot.sequence != notice.arm.sequence {
            return Consumed::Stale;
        }
        match (slot.phase, outcome) {
            (SlotPhase::Idle | SlotPhase::Armed { .. }, _) => Consumed::Stale,
            (SlotPhase::Closing, _) => {
                self.release(notice.arm.owner);
                Consumed::Released
            }
            (SlotPhase::Delivering, TimerOutcome::Complete) => {
                slot.phase = SlotPhase::Idle;
                Consumed::Idle
            }
            (SlotPhase::Delivering, TimerOutcome::Successor(next)) => {
                if next.arm.owner != notice.arm.owner || next.arm.sequence <= slot.sequence {
                    return Consumed::Stale;
                }
                slot.sequence = next.arm.sequence;
                slot.phase = SlotPhase::Armed { heap_index: UNPLACED };
                let position = notice.arm.owner.index();
                self.push_entry(ArmEntry { deadline_ms: next.deadline_ms, position });
                Consumed::Armed(next.arm)
            }
        }
    }

    /// Owners armed now: the heap's entries.
    #[must_use]
    pub fn armed(&self) -> u32 {
        self.heap.len()
    }

    /// The most owners armed at once since the set was built, raised at
    /// each push.
    #[must_use]
    pub fn armed_max(&self) -> u32 {
        self.armed_max
    }

    /// Positions that can still serve: the cap, less those retired at their
    /// last generation.
    #[must_use]
    pub fn capacity_now(&self) -> u32 {
        self.slots.capacity_now()
    }

    /// Pushes an Armed owner's entry, one sift, writing each moved entry's
    /// index into its owner's slot.
    fn push_entry(&mut self, entry: ArmEntry) {
        let Self { slots, heap, handles, .. } = self;
        heap.push(entry, |moved, index| place(slots, handles, moved.position, index))
            .expect("an owner holds at most one heap entry, so the heap has room");
        self.armed_max = self.armed_max.max(self.heap.len());
    }

    /// Removes the entry at `heap_index`, one sift.
    fn remove_entry(&mut self, heap_index: u32) {
        let Self { slots, heap, handles, .. } = self;
        let removed = heap.remove(heap_index, |moved, index| {
            place(slots, handles, moved.position, index);
        });
        debug_assert!(removed.is_some(), "an Armed owner's index names its entry");
    }

    /// Takes the occupant of `id` and clears its handle, in one step.
    fn release(&mut self, id: SlotId<u64>) {
        let taken = self.slots.take(id);
        debug_assert!(matches!(taken, Taken::Value(_)), "a released owner is live");
        self.handles.set(id.index(), None);
    }
}

/// A turn's hold on its loop's [`TimerSet`] (ADR-0154 D3): arm, cancel,
/// close and read an owner, and make the successor of the notice the plane
/// is answering, each against the turn's injected time. Delivery and its
/// per-turn allowance are the loop's alone: nothing here begins, consumes
/// or reserves, so no plane step can deliver a timer or hand out a position.
pub struct Timers<'a> {
    set: &'a mut TimerSet,
    now: Nanos,
}

impl<'a> Timers<'a> {
    /// The loop's set at the turn's time `now`.
    pub(crate) fn new(set: &'a mut TimerSet, now: Nanos) -> Timers<'a> {
        Timers { set, now }
    }

    /// Arms `owner` at `deadline`, rounded against the turn's time
    /// ([`TimerSet::arm`]): replacing its arm if it holds one, `Busy` while
    /// its callback runs, and a refusal changes nothing.
    pub fn arm(&mut self, owner: &TimerOwner, deadline: Nanos) -> Result<TimerArm, ArmError> {
        self.set.arm(owner, deadline, self.now)
    }

    /// Cancels exactly `arm` ([`TimerSet::cancel`]).
    pub fn cancel(&mut self, arm: &TimerArm) -> Cancelled {
        self.set.cancel(arm)
    }

    /// Closes `owner` ([`TimerSet::close`]); while its callback runs the
    /// position frees when the loop consumes that notice.
    pub fn close(&mut self, owner: TimerOwner) -> Closed {
        self.set.close(owner)
    }

    /// `owner`'s phase ([`TimerSet::phase`]).
    #[must_use]
    pub fn phase(&self, owner: &TimerOwner) -> OwnerPhase {
        self.set.phase(owner)
    }

    /// The successor of the arm `notice` delivers, at `deadline` rounded
    /// against the turn's time and checked now ([`TimerSet::successor`]):
    /// the plane returns it from the callback that answers `notice`.
    pub fn successor<'n>(
        &self,
        notice: &'n TimerNotice,
        deadline: Nanos,
    ) -> Result<Successor<'n>, ArmError> {
        self.set.successor(notice, deadline, self.now)
    }

    /// The most owners armed at once since the set was built.
    #[must_use]
    pub fn armed_max(&self) -> u32 {
        self.set.armed_max()
    }
}

impl core::fmt::Debug for Timers<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Timers {{ now: {}, armed: {} }}", self.now, self.set.armed())
    }
}

/// A heap entry's index before its sift places it.
const UNPLACED: u32 = u32::MAX;

/// The heap's unused storage.
const VACANT_ENTRY: ArmEntry = ArmEntry { deadline_ms: 0, position: 0 };

/// The heap's hook: entry of `position` now at `index`, written into its
/// Armed owner's slot.
fn place(
    slots: &mut CappedSlab<OwnerSlot, u64, Retire>,
    handles: &FixedArray<Option<SlotId<u64>>>,
    position: u32,
    index: u32,
) {
    let (_, slot) = occupant(slots, handles, position);
    debug_assert!(matches!(slot.phase, SlotPhase::Armed { .. }), "a moved entry's owner is armed");
    slot.phase = SlotPhase::Armed { heap_index: index };
}

/// The live owner a heap entry names, through its position's handle.
fn occupant<'s>(
    slots: &'s mut CappedSlab<OwnerSlot, u64, Retire>,
    handles: &FixedArray<Option<SlotId<u64>>>,
    position: u32,
) -> (SlotId<u64>, &'s mut OwnerSlot) {
    let handle = handles.get(position);
    match handle.map(|id| (id, slots.get_mut(id))) {
        Some((id, LookupMut::Live(slot))) => (id, slot),
        Some((_, LookupMut::Stale | LookupMut::Vacant)) | None => {
            unreachable!("a heap entry names a live owner through its position's handle")
        }
    }
}

/// `deadline` in whole milliseconds, rounded up and raised to `now`'s
/// millisecond + 1; `ClockRange` when that millisecond is past what `Nanos`
/// holds.
fn normalized_ms(deadline: Nanos, now: Nanos) -> Result<u64, ArmError> {
    let requested = deadline.0.div_ceil(NANOS_PER_MS);
    let earliest = (now.0 / NANOS_PER_MS).checked_add(1).ok_or(ArmError::ClockRange)?;
    let deadline_ms = requested.max(earliest);
    deadline_ms.checked_mul(NANOS_PER_MS).ok_or(ArmError::ClockRange)?;
    Ok(deadline_ms)
}

/// Whether a deadline of `deadline_ms` is due at `now`.
#[inline]
fn due_at(deadline_ms: u64, now: Nanos) -> bool {
    let now_ms = now.0 / NANOS_PER_MS;
    // Canary: due a millisecond early.
    #[cfg(inf_canary_timer_fires_early)]
    let now_ms = now_ms + 1;
    deadline_ms <= now_ms
}

/// Steps an array's build to its end.
fn build_array<E: Copy>(mut builder: FixedArrayBuilder<E>) -> Result<FixedArray<E>, CapError> {
    loop {
        builder = match builder.step() {
            ArrayBuilt::More(next) => next,
            ArrayBuilt::Done(array) => return Ok(array),
            ArrayBuilt::Refused(refused) => return Err(refused),
        };
    }
}

/// Retires a refused slab's prefix by steps.
fn retire_prefix(mut prefix: SlabRetiring<OwnerSlot, u64, Retire>) {
    loop {
        prefix = match prefix.step() {
            Retiring::More(next) => next,
            Retiring::Done => return,
        };
    }
}

#[cfg(test)]
mod set_tests {
    use super::*;
    use inf_foundation::CellId;
    use inf_foundation::bounded::CapFill;

    const ONE: Cap = Cap::entries::<1>("set-one", CapFill::Serving);
    const FOUR: Cap = Cap::entries::<4>("set-four", CapFill::Serving);

    fn ms(value: u64) -> Nanos {
        Nanos::from_millis(value)
    }

    fn set(cap: Cap) -> TimerSet {
        let (census, _mark) = CapCensus::new(CellId(0));
        TimerSet::build(cap, &census).expect("built")
    }

    /// `owner`'s generation set to its last value, its handle re-stamped,
    /// so its close retires the position.
    fn place_generation_last(set: &mut TimerSet, owner: &mut TimerOwner) {
        let index = owner.id.index();
        assert!(matches!(set.slots.place_generation_last(&mut owner.id), LookupMut::Live(_)));
        set.handles.set(index, Some(owner.id));
    }

    /// `owner`'s last arm sequence set to `sequence`.
    fn place_sequence(set: &mut TimerSet, owner: &TimerOwner, sequence: u64) {
        let LookupMut::Live(slot) = set.slots.get_mut(owner.id) else { panic!("a live owner") };
        slot.sequence = sequence;
    }

    fn owner(set: &mut TimerSet, route: u64) -> TimerOwner {
        match set.reserve(route) {
            Reserved::Owner(owner) => owner,
            other => panic!("a free position: {other:?}"),
        }
    }

    /// Every position once, then `Full`; a closed position serves again at a
    /// later generation, so the old owner's arm is `Stale`.
    #[test]
    fn positions_reserve_once_then_full_and_a_closed_one_serves_again() {
        let mut set = set(FOUR);
        let owners: Vec<TimerOwner> = (0..4).map(|route| owner(&mut set, route)).collect();
        let mut positions: Vec<u32> = owners.iter().map(TimerOwner::position).collect();
        positions.sort_unstable();
        assert_eq!(positions, [0, 1, 2, 3]);
        assert_eq!(set.reserve(9), Reserved::Full);
        let mut owners = owners.into_iter();
        let first = owners.next().expect("an owner");
        let old_arm = set.arm(&first, ms(5), ms(0)).expect("armed");
        let position = first.position();
        assert_eq!(set.close(first), Closed::Released);
        let again = owner(&mut set, 10);
        assert_eq!(again.position(), position, "the freed position");
        assert_eq!(set.cancel(&old_arm), Cancelled::Stale, "the earlier owner's arm");
        assert_eq!(set.armed(), 0, "the close removed the entry");
    }

    /// The deadline is rounded up to whole ms and raised to the turn's ms
    /// + 1; it falls due at that ms and not one nanosecond before.
    #[test]
    fn an_arm_rounds_up_and_never_falls_due_in_its_own_turn() {
        let mut set = set(FOUR);
        let now = Nanos(10_500_000);
        let late = owner(&mut set, 1);
        let soon = owner(&mut set, 2);
        set.arm(&late, Nanos(12_000_001), now).expect("armed");
        set.arm(&soon, Nanos(0), now).expect("armed");
        assert_eq!(set.next_deadline(), Some(ms(11)), "raised to the turn's ms + 1");
        assert!(!set.due(now));
        assert!(!set.due(Nanos(10_999_999)));
        assert!(set.due(ms(11)));
        let notice = set.begin(ms(12)).expect("the minimum");
        assert_eq!(notice.route(), 2);
        assert_eq!(set.consume(&notice, TimerOutcome::Complete), Consumed::Idle);
        assert!(!set.due(Nanos(12_999_999)), "12.000001 ms rounds up to 13 ms");
        assert_eq!(set.next_deadline(), Some(ms(13)));
    }

    /// The last ms that converts back to `Nanos` arms; one past it is
    /// `ClockRange` and keeps the prior arm.
    #[test]
    fn the_clock_range_is_refused_before_any_effect() {
        let mut set = set(ONE);
        let owner = owner(&mut set, 1);
        let last_ms = u64::MAX / NANOS_PER_MS;
        let arm = set.arm(&owner, ms(3), ms(0)).expect("armed");
        assert_eq!(
            set.arm(&owner, Nanos(last_ms * NANOS_PER_MS + 1), ms(0)),
            Err(ArmError::ClockRange)
        );
        assert_eq!(set.arm(&owner, Nanos(u64::MAX), ms(0)), Err(ArmError::ClockRange));
        assert_eq!(set.next_deadline(), Some(ms(3)), "the prior arm is kept");
        assert_eq!(set.cancel(&arm), Cancelled::Removed, "and still current");
        let far = set.arm(&owner, Nanos(last_ms * NANOS_PER_MS), ms(0)).expect("the last ms");
        assert_eq!(set.next_deadline(), Some(Nanos(last_ms * NANOS_PER_MS)));
        assert_eq!(far.sequence(), 2, "each accepted arm takes a sequence; a refused one none");
    }

    /// D2's rows for an exact arm: a replacement makes the old arm `Stale`;
    /// a cancel removes, then `NotArmed`.
    #[test]
    fn a_replacement_and_a_cancel_act_on_the_exact_arm() {
        let mut set = set(ONE);
        let owner = owner(&mut set, 1);
        let first = set.arm(&owner, ms(4), ms(0)).expect("armed");
        let second = set.arm(&owner, ms(9), ms(0)).expect("replaced");
        assert_eq!(set.armed(), 1, "a replacement keeps one entry");
        assert_eq!(set.cancel(&first), Cancelled::Stale);
        assert_eq!(set.cancel(&second), Cancelled::Removed);
        assert_eq!(set.cancel(&second), Cancelled::NotArmed);
        assert_eq!(set.phase(&owner), OwnerPhase::Idle);
        assert!(set.begin(ms(100)).is_none(), "a cancelled arm is never delivered");
    }

    /// D2's delivery rows: while its callback runs the owner is Busy to an
    /// arm and `InCallback` to a cancel, and its close drains; a reserve in
    /// the callback never gets the delivering position.
    #[test]
    fn a_delivering_owner_keeps_its_position_until_its_notice_is_consumed() {
        let mut set = set(FOUR);
        let owner = owner(&mut set, 5);
        let arm = set.arm(&owner, ms(2), ms(0)).expect("armed");
        let notice = set.begin(ms(2)).expect("due");
        assert_eq!(notice.arm(), arm);
        assert_eq!(set.phase(&owner), OwnerPhase::Delivering);
        assert_eq!(set.arm(&owner, ms(9), ms(2)), Err(ArmError::Busy));
        assert_eq!(set.cancel(&arm), Cancelled::InCallback);
        let position = owner.position();
        assert_eq!(set.close(owner), Closed::Draining);
        for route in 0..3 {
            let other = self::owner(&mut set, route);
            assert_ne!(other.position(), position, "the delivering position is not handed out");
        }
        assert_eq!(set.reserve(9), Reserved::Full, "the draining position is still held");
        let next = set.successor(&notice, ms(20), ms(2)).expect("a successor");
        assert_eq!(set.consume(&notice, TimerOutcome::Successor(next)), Consumed::Released);
        assert_eq!(set.armed(), 0, "a closing owner's successor is dropped");
        assert_eq!(set.consume(&notice, TimerOutcome::Complete), Consumed::Stale);
        assert!(matches!(set.reserve(9), Reserved::Owner(_)), "the position frees at consumption");
    }

    /// A successor arms the next sequence at its checked deadline.
    #[test]
    fn a_successor_rearms_its_owner_at_the_next_sequence() {
        let mut set = set(ONE);
        let owner = owner(&mut set, 1);
        set.arm(&owner, ms(1), ms(0)).expect("armed");
        let notice = set.begin(ms(1)).expect("due");
        let next = set.successor(&notice, ms(1_001), ms(1)).expect("a successor");
        let Consumed::Armed(arm) = set.consume(&notice, TimerOutcome::Successor(next)) else {
            panic!("armed");
        };
        assert_eq!(arm.sequence(), 2);
        assert_eq!(set.next_deadline(), Some(ms(1_001)));
        assert_eq!(set.phase(&owner), OwnerPhase::Armed);
        assert!(set.begin(ms(1_000)).is_none());
        let notice = set.begin(ms(1_001)).expect("the successor is delivered");
        assert_eq!(notice.arm(), arm);
    }

    /// Ties deliver by position, one notice at a time.
    #[test]
    fn equal_deadlines_deliver_in_position_order() {
        let mut set = set(FOUR);
        let owners: Vec<TimerOwner> = (0..4).map(|route| owner(&mut set, route)).collect();
        for owner in owners.iter().rev() {
            set.arm(owner, ms(3), ms(0)).expect("armed");
        }
        let mut order = Vec::new();
        while let Some(notice) = set.begin(ms(3)) {
            order.push(notice.arm().position());
            assert_eq!(set.consume(&notice, TimerOutcome::Complete), Consumed::Idle);
        }
        assert_eq!(order, [0, 1, 2, 3]);
        assert_eq!(set.armed_max(), 4);
    }

    /// The owner's generation at its last value: its close retires the
    /// position (`Retire`, ADR-0194 D2), and a set with none left answers
    /// `Exhausted`.
    #[test]
    fn a_position_at_its_last_generation_retires_and_the_set_is_exhausted() {
        let mut set = set(ONE);
        let mut owner = owner(&mut set, 1);
        place_generation_last(&mut set, &mut owner);
        set.arm(&owner, ms(5), ms(0)).expect("armed at the last generation");
        assert_eq!(set.close(owner), Closed::Released);
        assert_eq!(set.capacity_now(), 0);
        assert_eq!(set.reserve(2), Reserved::Exhausted);
        assert_eq!(set.armed(), 0);
    }

    /// The arm sequence at its last value: an arm and a successor past it
    /// are `SequenceExhausted` before any effect.
    #[test]
    fn an_owner_at_its_last_sequence_refuses_its_next_arm() {
        let mut set = set(ONE);
        let owner = owner(&mut set, 1);
        place_sequence(&mut set, &owner, u64::MAX - 1);
        let arm = set.arm(&owner, ms(1), ms(0)).expect("the last sequence arms");
        assert_eq!(arm.sequence(), u64::MAX);
        let notice = set.begin(ms(1)).expect("due");
        let refused = set.successor(&notice, ms(9), ms(1)).map(|_| ());
        assert_eq!(refused, Err(ArmError::SequenceExhausted));
        assert_eq!(set.consume(&notice, TimerOutcome::Complete), Consumed::Idle);
        assert_eq!(set.arm(&owner, ms(9), ms(1)), Err(ArmError::SequenceExhausted));
        assert_eq!(set.phase(&owner), OwnerPhase::Idle);
        assert_eq!(set.armed(), 0);
    }
}
