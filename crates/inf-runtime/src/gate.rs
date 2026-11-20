//! Typed suspension primitives — the only ways a cell future may suspend
//! (ADR-0003). Each is a thin shape over one cell-local pattern: a future
//! registers its waker under a key; the event source completes the key and
//! wakes exactly the right task(s). No atomics, no locks (L1).
//!
//! - [`FabricGate`]: fabric replies. The gate mints each key, and a task's
//!   [`GateUnits`] bound the holders it has out (ADR-0177 D4).
//! - [`KeyedGate`] and [`IoGate`]: completions keyed by the caller's key (the
//!   cold gate's logical id, a driver token), behind one holder each.
//! - [`WaitList`]: key-keyed FIFO (M1 blocking ops: BLPOP, WAIT…), taking
//!   the cell's [`WaitNodes`] at its constructor.
//! - [`WatermarkGate`]: LSN-keyed (M2 fsync-gated acks).
//!
//! A holder wakes exactly one task. Completion may arrive before the waiter
//! first polls (same-iteration delivery): the value parks in the holder and
//! the first poll returns `Ready` without suspending. Every waiter future is
//! cancellation-safe: dropping one leaves no waker behind, and a value that
//! arrives for no waiter goes to its gate's [`Cleanup`].

use core::cell::{Cell, RefCell, RefMut};
use core::future::Future;
use core::marker::PhantomData;
use core::num::NonZeroU64;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::BTreeMap;
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{
    Admission, AssemblyFull, Built, Cap, CapCensus, CapError, CappedMap, CappedSlab, ExactKey,
    Lookup, LookupMut, MapBuilder, MapBuilt, MapRetired, NamedReservation, Released, Retiring,
    SlabBuilder, SlabReserved, SlotId, SlotWord, Taken, WordLookup, WrapWithProof,
};
use inf_foundation::limits::{GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS};

use inf_foundation::LogHistogram;
use inf_foundation::time::Nanos;

use crate::driver::CompletionResult;
use crate::heap::{ArrayBuilt, FixedArray};
use crate::token::CompletionToken;

// ---- the holder both gates keep (ADR-0177 D4) ------------------------------

/// Where a value goes when no waiter takes it: fixed per gate, so the
/// completing side never decides it (ADR-0177 D4).
pub trait Cleanup<V> {
    /// Takes a value its waiter will never take.
    fn orphan(value: V);
}

/// The RAII cleanup: a value no waiter takes is dropped.
#[derive(Debug)]
pub struct DropValue;

impl<V> Cleanup<V> for DropValue {
    fn orphan(value: V) {
        drop(value);
    }
}

/// What a completion did with its value.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// The value is its waiter's: parked, or handed to a suspended waiter.
    Delivered,
    /// No waiter takes it: the gate's [`Cleanup`] took it. A completion for a
    /// holder that is not waiting, or a second completion of one, is this.
    Orphan,
}

/// A recorded hop's send time, or none, in one word: the nanoseconds plus
/// one, so the word's zero is no send time. The two states that await a
/// value carry it, so a recorded hop has no state of its own: with a twin
/// of each awaiting state, every poll and completion would tell six states
/// apart for these four.
///
/// The last instant has no stamp: a hop sent at `u64::MAX` ns is not
/// sampled (derived never: the clock counts nanoseconds from its owner's
/// origin).
#[derive(Copy, Clone)]
struct Sent(Option<NonZeroU64>);

impl Sent {
    /// No send time: an unrecorded hop's, and every keyed holder's.
    const NONE: Sent = Sent(None);

    /// The stamp of a hop sent at `sent`.
    fn at(sent: Option<Nanos>) -> Sent {
        Sent(sent.and_then(|sent| sent.0.checked_add(1)).and_then(NonZeroU64::new))
    }

    /// The send time the stamp holds.
    fn time(self) -> Option<Nanos> {
        self.0.map(|stamp| Nanos(stamp.get() - 1))
    }
}

/// A holder's state (ADR-0177 D4). One holder implementation serves both
/// gates: the fabric gate reaches it by the key it minted, a keyed gate by
/// its caller's key.
///
/// `Delivered` is declared last by measurement. A slab drops the slot it
/// writes over at each publish and take: an inline test of the slot's tag,
/// and this enum's drop a call only for a live slot. Declared third, where
/// the states' order would put it, the compiler folds this enum's drop
/// into the slot's and calls that at each publish and take of a slab whose
/// values have a drop of their own (the cold gate's). Declared last, in
/// any order of the other three, the test stays inline.
enum HolderState<V> {
    /// Registered; the waiter has not suspended yet. A recorded hop's
    /// request was sent at the stamp's time: its round trip is recorded at
    /// the completion.
    Registered(Sent),
    /// The waiter suspended: wake this when the value arrives. The stamp is
    /// the one its registration held.
    Waiting(Waker, Sent),
    /// The waiter is gone and the slot still has a claim on it: a fabric
    /// request not yet answered, or a keyed queue hold not yet released.
    Orphaned,
    /// The value arrived before the waiter took it.
    Delivered(V),
}

/// One holder: its state, and whether a queue's [`GateHold`] pins it.
struct Holder<V> {
    state: HolderState<V>,
    held: bool,
}

impl<V> Holder<V> {
    const fn registered(held: bool) -> Holder<V> {
        Holder { state: HolderState::Registered(Sent::NONE), held }
    }
}

/// The `Future` contract: a waiter is not polled after it was `Ready`.
#[cold]
fn polled_after_completion() -> ! {
    panic!("GateWait polled after completion")
}

/// The waiter's poll before its value: stores `waker` in a holder whose
/// wait awaits it, keeping the send time's stamp; false for a holder that
/// does not await.
fn store_waker<V>(state: &mut HolderState<V>, waker: &Waker) -> bool {
    let sent = match state {
        HolderState::Registered(sent) | HolderState::Waiting(_, sent) => *sent,
        HolderState::Delivered(_) | HolderState::Orphaned => return false,
    };
    *state = HolderState::Waiting(waker.clone(), sent);
    true
}

// ---- GateUnits ---------------------------------------------------------------

/// The units a gate issued against its holders: shared by the gate and every
/// task's [`GateUnits`].
struct UnitBook {
    issued: Cell<u32>,
    max: u32,
}

/// A task's units on a [`FabricGate`] (ADR-0177 D4): issued by the gate when
/// its task is admitted and returned to it when dropped. A [`GatePermit`] and
/// its [`GateWait`] hold one from `reserve` until the wait's value is taken
/// or the wait is dropped, so a task has at most its count of holders out.
/// Not `Clone`: units are a task's, never shared.
pub struct GateUnits {
    book: Rc<UnitBook>,
    count: u32,
    free: Cell<u32>,
}

impl GateUnits {
    /// The units issued to this task.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The units no permit or wait of this task holds.
    pub fn free(&self) -> u32 {
        self.free.get()
    }

    fn take(&self) -> bool {
        // Canary: `reserve` grants without a unit, so a fan is never windowed.
        if cfg!(inf_canary_gate_units_unbounded) {
            return true;
        }
        let free = self.free.get();
        if free == 0 {
            return false;
        }
        self.free.set(free - 1);
        true
    }

    fn give(&self) {
        if cfg!(inf_canary_gate_units_unbounded) {
            return;
        }
        let free = self.free.get();
        debug_assert!(free < self.count, "a unit returns only after it was taken");
        self.free.set(free + 1);
    }
}

impl Drop for GateUnits {
    fn drop(&mut self) {
        // A permit and a wait borrow their task's units, so both are gone.
        debug_assert_eq!(self.free.get(), self.count, "every unit came back");
        let issued = self.book.issued.get();
        debug_assert!(issued >= self.count, "the gate issued these units");
        self.book.issued.set(issued - self.count);
    }
}

impl core::fmt::Debug for GateUnits {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "GateUnits {{ count: {}, free: {} }}", self.count, self.free.get())
    }
}

// ---- FabricGate ----------------------------------------------------------------

/// The key's tag bit, above the generation and index fields: it separates a
/// gate key from the tokens the mesh mints for the accept hand-off, which
/// wrap below it (ADR-0177 D4).
pub const GATE_KEY_TAG_BIT: u32 = GATE_KEY_INDEX_BITS + GATE_KEY_GENERATION_BITS;

/// Where the origin cell sits in a fabric token: above the 48-bit sequence
/// whose top bit is the tag.
const GATE_KEY_ORIGIN_SHIFT: u32 = GATE_KEY_TAG_BIT + 1;

const _: () = assert!(GATE_KEY_ORIGIN_SHIFT + u16::BITS == u64::BITS, "origin:16 | seq:48");

/// The bits below the tag: the generation and index fields.
const GATE_KEY_FIELDS: u64 = (1 << GATE_KEY_TAG_BIT) - 1;

/// A gate's holders: one slot a request, its generation wrapping under
/// ADR-0177 D4's proof (D7).
type HolderSlab<V> = CappedSlab<Holder<V>, u32, WrapWithProof>;

/// A holder slab being built.
type HolderBuilder<V> = SlabBuilder<Holder<V>, u32, WrapWithProof>;

/// The fabric key's adapter: a holder's index and its generation's low
/// bits, built from the gate's own slab (ADR-0194 D6 with A2).
type GateWord = SlotWord<u32, WrapWithProof, GATE_KEY_INDEX_BITS, GATE_KEY_GENERATION_BITS>;

/// A fabric gate's holder slab and the one adapter its keys pass through.
struct FabricHolders<V> {
    slab: HolderSlab<V>,
    word: GateWord,
    /// Takes of occupants at a generation past the key's field (the
    /// simulator's near-wrap tap; `fault-points` only).
    #[cfg(feature = "fault-points")]
    wrapped_takes: u64,
}

impl<V> FabricHolders<V> {
    /// Frees the holder `slot` names, its occupant returned. Under
    /// `fault-points` a take of an occupant whose generation passed the
    /// key's field is counted: it reads the generation publish stamped on
    /// the handle, which no encode writes, so an encode fault cannot fake
    /// a wrap.
    fn take(&mut self, slot: SlotId<u32>) -> Taken<Holder<V>> {
        let taken = self.slab.take(slot);
        #[cfg(feature = "fault-points")]
        self.count_wrapped(&taken, slot);
        taken
    }

    /// [`take`](Self::take) for the delivered poll, in its frame: the
    /// slab's take with its body there (`CappedSlab::take_inlined`), and the
    /// same count.
    #[inline(always)]
    fn take_inlined(&mut self, slot: SlotId<u32>) -> Taken<Holder<V>> {
        let taken = self.slab.take_inlined(slot);
        #[cfg(feature = "fault-points")]
        self.count_wrapped(&taken, slot);
        taken
    }

    /// Counts a take of an occupant whose generation passed the key's field.
    #[cfg(feature = "fault-points")]
    fn count_wrapped(&mut self, taken: &Taken<Holder<V>>, slot: SlotId<u32>) {
        if matches!(taken, Taken::Value(_)) && slot.generation() >= 1 << GATE_KEY_GENERATION_BITS {
            self.wrapped_takes += 1;
        }
    }
}

/// The fabric round-trip gate (ADR-0177 D4): it mints the key a request
/// carries, `origin:16 | tag:1 | generation:23 | index:24`, from the slab
/// handle of the holder it reserves, and holds the waiter's state there.
/// Its owner keeps it in a `RefCell`, which every [`GateWait`] borrows for
/// its life; [`reserve`](Self::reserve) takes the gate mutably, so a
/// [`GatePermit`] exists only inside its owner's borrow, one synchronous
/// step.
pub struct FabricGate<V, C: Cleanup<V> = DropValue> {
    /// `None` on a node of one cell, which sends no request.
    holders: Option<FabricHolders<V>>,
    /// The origin cell's bits and the tag: every key's high bits.
    high: u64,
    units: Rc<UnitBook>,
    /// Completions for no holder of this gate (`gate_stale`).
    stale: u64,
    /// Reserves that found every holder taken (`gate_full`).
    full: u64,
    /// Senders parked on a full gate, one woken per holder freed.
    room: WaitList<()>,
    /// Recorded hops' round trips, nanoseconds, each taken at its
    /// completion from the send time its holder kept (the hop RTT gate).
    round_trips: LogHistogram,
    _cleanup: PhantomData<fn(V) -> C>,
}

/// [`FabricGate::reserve`]'s answer.
pub enum FabricAdmission<'g, 'u, V, C: Cleanup<V> = DropValue> {
    /// A holder and a unit are the permit's; its key is minted.
    Granted(GatePermit<'g, 'u, V, C>),
    /// The task has every unit out: it settles its command's oldest leg
    /// and reserves again.
    NoUnit,
    /// Every holder is taken (derived never): the sender parks on
    /// [`FabricGate::room`].
    Full,
}

impl<V, C: Cleanup<V>> FabricGate<V, C> {
    /// The gate of cell `origin`, issuing at most `units_max` units, the
    /// holders its tasks may have waiting (`Σ T_j × W_j`), over `holders`
    /// slots, `H`, which add the orphans a peer's un-replied ops may hold
    /// (ADR-0177 D4); `None` builds no holder at a node of one cell. Its
    /// parked senders' list takes the cell's `nodes` (ADR-0177 D5). Built
    /// whole before the cell serves.
    ///
    /// # Errors
    /// `CapError` from the slab's build or its census row; `Layout` past
    /// the key's index field. Nothing is served.
    pub fn build(
        origin: u16,
        units_max: u32,
        holders: Option<Cap>,
        census: &Rc<CapCensus>,
        nodes: &WaitNodes,
    ) -> Result<FabricGate<V, C>, CapError> {
        FabricGate::build_with(origin, units_max, holders, census, nodes, |builder| builder)
    }

    /// [`build`](Self::build) with every holder slot free at
    /// `first_generation`, so a test or the simulator reaches the key's
    /// generation field without that many uses of a slot (`fault-points`
    /// only).
    ///
    /// # Errors
    /// As [`build`](Self::build).
    #[cfg(feature = "fault-points")]
    pub fn build_at_generation(
        origin: u16,
        units_max: u32,
        holders: Option<Cap>,
        census: &Rc<CapCensus>,
        nodes: &WaitNodes,
        first_generation: u32,
    ) -> Result<FabricGate<V, C>, CapError> {
        FabricGate::build_with(origin, units_max, holders, census, nodes, |builder| {
            builder.first_generation(first_generation)
        })
    }

    fn build_with(
        origin: u16,
        units_max: u32,
        holders: Option<Cap>,
        census: &Rc<CapCensus>,
        nodes: &WaitNodes,
        configure: impl FnOnce(HolderBuilder<V>) -> HolderBuilder<V>,
    ) -> Result<FabricGate<V, C>, CapError> {
        let holders = match holders {
            None => None,
            Some(cap) => {
                debug_assert!(cap.entries_max() >= units_max, "H counts every unit");
                let slab = build_slab(configure(CappedSlab::builder(cap, census)?))?;
                let word = GateWord::for_slab(&slab)?;
                Some(FabricHolders {
                    slab,
                    word,
                    #[cfg(feature = "fault-points")]
                    wrapped_takes: 0,
                })
            }
        };
        Ok(FabricGate {
            holders,
            high: (u64::from(origin) << GATE_KEY_ORIGIN_SHIFT) | (1 << GATE_KEY_TAG_BIT),
            units: Rc::new(UnitBook { issued: Cell::new(0), max: units_max }),
            stale: 0,
            full: 0,
            room: WaitList::unit(nodes),
            round_trips: LogHistogram::new(),
            _cleanup: PhantomData,
        })
    }

    /// A holder slot's bytes: the stride the idle bill counts a holder at,
    /// in chunks of 64 slots (ADR-0177 D8 with A1).
    pub const HOLDER_SLOT_BYTES: usize = HolderSlab::<V>::SLOT_BYTES;

    /// A task's `count` units, or `None` when the gate has issued every unit
    /// its holders bound: the task is not admitted.
    pub fn units(&self, count: u32) -> Option<GateUnits> {
        let issued = self.units.issued.get().checked_add(count)?;
        if issued > self.units.max {
            return None;
        }
        self.units.issued.set(issued);
        Some(GateUnits { book: Rc::clone(&self.units), count, free: Cell::new(count) })
    }

    /// Reserves a holder for one request under one of `units`, and mints its
    /// key from the holder's handle before it is published (ADR-0194 A2's
    /// named reservation). The permit is registered after a successful send
    /// or dropped, which returns the unit and frees the holder at its next
    /// generation, so the key it minted names no later occupant.
    pub fn reserve<'g, 'u>(&'g mut self, units: &'u GateUnits) -> FabricAdmission<'g, 'u, V, C> {
        debug_assert!(Rc::ptr_eq(&units.book, &self.units), "a task's units are its gate's");
        if !units.take() {
            return FabricAdmission::NoUnit;
        }
        let FabricGate { holders, high, full, room, .. } = self;
        let Some(fabric) = holders.as_mut() else {
            units.give();
            *full += 1;
            return FabricAdmission::Full;
        };
        let reservation = match fabric.slab.reserve() {
            SlabReserved::Slot(slot) => slot.named(),
            SlabReserved::Full | SlabReserved::Exhausted => {
                units.give();
                *full += 1;
                return FabricAdmission::Full;
            }
        };
        let Some(fields) = fabric.word.encode(reservation.id()) else {
            // No key was minted, so nothing names the slot: forgotten, the
            // reservation leaves it free at its generation, as an unnamed
            // one's drop does. Dropped here instead, its drop would be a
            // call from this arm that takes the reservation's address, and
            // the reservation would then live in memory on the granting
            // path too.
            core::mem::forget(reservation);
            unreachable!("the gate's adapter is built from its own slab")
        };
        FabricAdmission::Granted(GatePermit {
            reservation,
            unit: HeldUnit { units, room },
            key: *high | fields,
            _cleanup: PhantomData,
        })
    }

    /// Delivers `value` to the holder `key` names, waking its waiter if it is
    /// suspended; a recorded hop's round trip, from its send to `now`, is
    /// recorded in the same step. A key of another origin, without the tag,
    /// of no live holder, or of a holder already delivered gives `value` to
    /// the cleanup and counts `stale`; an orphaned holder's one completion
    /// frees it.
    pub fn complete(gate: &RefCell<Self>, key: u64, value: V, now: Nanos) -> Delivery {
        let (delivery, waker, orphan) = gate.borrow_mut().settle(key, value, now);
        if let Some(waker) = waker {
            waker.wake();
        }
        if let Some(value) = orphan {
            C::orphan(value);
        }
        delivery
    }

    /// One completion's transition inside the gate's borrow: what it did,
    /// the waker to wake and the value to clean up, both outside it.
    fn settle(&mut self, key: u64, value: V, now: Nanos) -> (Delivery, Option<Waker>, Option<V>) {
        let FabricGate { holders, high, stale, room, round_trips, .. } = self;
        let own = key & !GATE_KEY_FIELDS == *high;
        let Some(fabric) = holders.as_mut().filter(|_| own) else {
            *stale += 1;
            return (Delivery::Orphan, None, Some(value));
        };
        let (id, holder) = match fabric.slab.get_mut_by_word(&fabric.word, key & GATE_KEY_FIELDS) {
            WordLookup::Live(id, holder) => (id, holder),
            WordLookup::Stale | WordLookup::Vacant => {
                *stale += 1;
                return (Delivery::Orphan, None, Some(value));
            }
        };
        match &holder.state {
            HolderState::Registered(_) | HolderState::Waiting(..) => {}
            // A second completion: the parked value stays the waiter's.
            HolderState::Delivered(_) => {
                *stale += 1;
                return (Delivery::Orphan, None, Some(value));
            }
            // The waiter was dropped: this is its request's one completion.
            HolderState::Orphaned => {
                let _freed = fabric.take(id);
                wake_room(room);
                return (Delivery::Orphan, None, Some(value));
            }
        }
        let (waker, sent) =
            match core::mem::replace(&mut holder.state, HolderState::Delivered(value)) {
                HolderState::Waiting(waker, sent) => (Some(waker), sent),
                HolderState::Registered(sent) => (None, sent),
                HolderState::Delivered(_) | HolderState::Orphaned => (None, Sent::NONE),
            };
        if let Some(sent) = sent.time() {
            round_trips.record(now.saturating_sub(sent).0);
        }
        (Delivery::Delivered, waker, None)
    }

    /// The wait a sender parks on when [`reserve`](Self::reserve) answers
    /// `Full`: one is woken each time a holder frees.
    pub fn room(&self) -> ListWait<()> {
        self.room.wait(())
    }

    /// Holders taken: registered, waiting, delivered or orphaned.
    pub fn holders(&self) -> usize {
        self.holders.as_ref().map_or(0, |holders| holders.slab.len() as usize)
    }

    /// Completions that found no holder waiting (`gate_stale`): derived never.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// Reserves that found every holder taken (`gate_full`): derived never.
    pub fn full(&self) -> u64 {
        self.full
    }

    /// Recorded hops' round trips, nanoseconds, each taken at its
    /// completion (`fabric_rtt_p50_ns`).
    pub fn round_trips(&self) -> &LogHistogram {
        &self.round_trips
    }

    /// Takes of holders whose generation passed the key's 23-bit field:
    /// the near-wrap leg's engagement (`fault-points` only).
    #[cfg(feature = "fault-points")]
    pub fn wrapped_takes(&self) -> u64 {
        self.holders.as_ref().map_or(0, |holders| holders.wrapped_takes)
    }
}

/// Steps a slab's build to its end; a refused build retires its prefix by
/// steps before it answers.
fn build_slab<T>(
    mut builder: SlabBuilder<T, u32, WrapWithProof>,
) -> Result<CappedSlab<T, u32, WrapWithProof>, CapError> {
    loop {
        builder = match builder.step() {
            Built::More(next) => next,
            Built::Live(slab) => return Ok(slab),
            Built::Refused(mut retiring, refused) => loop {
                retiring = match retiring.step() {
                    Retiring::More(next) => next,
                    Retiring::Done => return Err(refused),
                };
            },
        };
    }
}

/// A holder was freed: one sender parked on the room is woken, if any is
/// queued (ADR-0177 D4). Every end that frees a holder takes this one step.
/// A room has a sender only after its gate was full (derived never), so the
/// step reads the room first: the wake is a call that takes the list's
/// queues, to find no one on an idle room.
// Inlined at each end, so the read is all an idle room costs. As a body of
// its own the step is the one caller of the list's wake: the compiler folds
// the whole wake into it, and a second copy into a room wait's drop.
#[inline]
fn wake_room(room: &WaitList<()>) {
    if !room.is_idle() {
        room.wake_one(());
    }
}

/// Runs `step` as a call of its own, for a caller whose body must stay
/// small.
#[inline(never)]
fn out_of_line(step: impl FnOnce()) {
    step();
}

/// A dropped wait's holder: orphaned until its request's one completion
/// frees it, or freed now with the value that completion parked. Returns
/// the waker it stored and that value, both dropped outside the gate's
/// borrow.
fn orphan<V>(
    fabric: &mut FabricHolders<V>,
    room: &WaitList<()>,
    slot: SlotId<u32>,
) -> (Option<Waker>, Option<V>) {
    let LookupMut::Live(holder) = fabric.slab.get_mut(slot) else { return (None, None) };
    let prior = core::mem::replace(&mut holder.state, HolderState::Orphaned);
    // Canary: the holder frees at its waiter's drop, with no Orphaned state,
    // so its request's late completion finds the slot at its next generation.
    if cfg!(inf_canary_gate_frees_on_drop) {
        let _freed = fabric.take(slot);
        wake_room(room);
        return match prior {
            HolderState::Delivered(value) => (None, Some(value)),
            HolderState::Waiting(waker, _) => (Some(waker), None),
            HolderState::Registered(_) | HolderState::Orphaned => (None, None),
        };
    }
    match prior {
        HolderState::Delivered(value) => {
            let _freed = fabric.take(slot);
            wake_room(room);
            (None, Some(value))
        }
        HolderState::Waiting(waker, _) => (Some(waker), None),
        HolderState::Registered(_) | HolderState::Orphaned => (None, None),
    }
}

impl<V, C: Cleanup<V>> core::fmt::Debug for FabricGate<V, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FabricGate {{ holders: {} }}", self.holders())
    }
}

/// One reserved holder, named before its publish, and one unit, under the
/// gate's key for it. Held only inside its owner's borrow of the gate: it
/// cannot cross an `await` (ADR-0144 A2's list).
///
/// Two owned parts and no `Drop` of its own. A permit that never registers
/// (the send was refused) drops them in their order: the named reservation
/// advances its slot's generation, so the key names no later occupant
/// (ADR-0194 A2), and only then does the unit return and the room wake, on
/// a gate whose holder is already free.
pub struct GatePermit<'g, 'u, V, C: Cleanup<V> = DropValue> {
    /// The holder's slot until the permit registers or drops. First: the
    /// holder is free before the unit's drop wakes the room.
    reservation: NamedReservation<'g, Holder<V>, u32>,
    unit: HeldUnit<'g, 'u>,
    key: u64,
    _cleanup: PhantomData<fn(V) -> C>,
}

/// A permit's unit, and the room its holder's return wakes. Dropped with a
/// permit that never registered, it returns the unit and wakes one sender;
/// a permit that registers hands the unit to its wait instead.
struct HeldUnit<'g, 'u> {
    units: &'u GateUnits,
    room: &'g WaitList<()>,
}

impl<'u> HeldUnit<'_, 'u> {
    /// The unit passes to the wait: nothing returns and no sender wakes.
    fn kept(self) -> &'u GateUnits {
        let units = self.units;
        // The wait holds the unit from here and returns it at its own end:
        // this part's drop, which would return it now and wake a sender on
        // a gate with no holder freed, must not run.
        core::mem::forget(self);
        units
    }
}

impl Drop for HeldUnit<'_, '_> {
    // Inlined always: as a call, this drop takes the address of a part of
    // the permit wherever the permit may be dropped, and the whole permit
    // then lives in memory from `reserve` to the holder's publish.
    #[inline(always)]
    fn drop(&mut self) {
        self.units.give();
        // One call, as the wake it replaces was: with the step's read in
        // this body the compiler stops inlining the permit's drop at its
        // cold ends, calls it there with the permit's address, and the
        // permit lives in memory again.
        let room = self.room;
        out_of_line(move || wake_room(room));
    }
}

impl<'u, V, C: Cleanup<V>> GatePermit<'_, 'u, V, C> {
    /// The word the request carries: the fabric token it is sent under.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Publishes the holder once the request was sent: the key is routable.
    /// `gate` is the cell this permit's gate lives in. A recorded hop gives
    /// the time it was sent at: its holder keeps its stamp, and the gate
    /// records the round trip at the completion, whatever order the replies
    /// arrive in. One entry for both, so the holder's publish has one site.
    pub fn register(
        self,
        gate: &'u RefCell<FabricGate<V, C>>,
        sent: Option<Nanos>,
    ) -> GateWait<'u, V, C> {
        self.publish(gate, HolderState::Registered(Sent::at(sent)))
    }

    fn publish(
        self,
        gate: &'u RefCell<FabricGate<V, C>>,
        state: HolderState<V>,
    ) -> GateWait<'u, V, C> {
        let GatePermit { reservation, unit, .. } = self;
        let slot = reservation.publish(Holder { state, held: false });
        GateWait { gate, units: unit.kept(), slot, done: false }
    }
}

/// Future of one fabric request's reply. Its first poll stores the waker;
/// a value delivered first is taken at once. Dropped before its value, the
/// holder is orphaned until the request's one completion frees it.
pub struct GateWait<'u, V, C: Cleanup<V> = DropValue> {
    gate: &'u RefCell<FabricGate<V, C>>,
    units: &'u GateUnits,
    slot: SlotId<u32>,
    done: bool,
}

// No self-references — state lives in the gate's holders.
impl<V, C: Cleanup<V>> Unpin for GateWait<'_, V, C> {}

impl<V, C: Cleanup<V>> Future for GateWait<'_, V, C> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<V> {
        let this = Pin::into_inner(self);
        let mut gate = this.gate.borrow_mut();
        let FabricGate { holders, room, .. } = &mut *gate;
        let Some(fabric) = holders.as_mut() else { polled_after_completion() };
        let LookupMut::Live(holder) = fabric.slab.get_mut(this.slot) else {
            polled_after_completion()
        };
        if store_waker(&mut holder.state, cx.waker()) {
            return Poll::Pending;
        }
        match &holder.state {
            HolderState::Delivered(_) => {}
            HolderState::Registered(_) | HolderState::Waiting(..) | HolderState::Orphaned => {
                polled_after_completion()
            }
        }
        let Taken::Value(Holder { state: HolderState::Delivered(value), .. }) =
            fabric.take_inlined(this.slot)
        else {
            polled_after_completion()
        };
        wake_room(room);
        drop(gate);
        this.done = true;
        this.units.give();
        Poll::Ready(value)
    }
}

impl<V, C: Cleanup<V>> Drop for GateWait<'_, V, C> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut gate = self.gate.borrow_mut();
        let FabricGate { holders, room, .. } = &mut *gate;
        let (waker, value) = match holders.as_mut() {
            Some(fabric) => orphan(fabric, room, self.slot),
            None => (None, None),
        };
        drop(gate);
        self.units.give();
        drop(waker);
        if let Some(value) = value {
            C::orphan(value);
        }
    }
}

// ---- KeyedGate (the cold gate, IoGate) ---------------------------------------

/// A key a [`KeyedGate`] routes by: its exact word (ADR-0151 D1).
pub trait GateKey: Copy {
    /// The key's word; two keys with one word are one key.
    fn word(self) -> u64;
}

impl GateKey for u64 {
    fn word(self) -> u64 {
        self
    }
}

impl GateKey for CompletionToken {
    fn word(self) -> u64 {
        self.as_u64()
    }
}

/// A keyed gate's routes and holders (ADR-0177 D4): the fabric gate's
/// holder slab and state enum, in front of which a capped map routes a
/// key's exact word to its holder's handle. A route exists exactly while
/// its holder awaits a value: it leaves in the step that delivers or drops.
struct KeyedCore<V> {
    routes: CappedMap<u64, SlotId<u32>>,
    holders: HolderSlab<V>,
    /// Reserves that found every route or holder taken (`cold_gate_full`).
    full: u64,
}

/// A keyed gate's key type and fixed cleanup, which no field holds.
type KeyedMarker<K, V, C> = PhantomData<(K, fn(V) -> C)>;

/// A gate keyed by its caller's key (ADR-0149 D4, ADR-0177 D4): the cold
/// gate's logical id, a driver's completion token. A key is not supplied
/// again while an old effect can complete under it: that rule is the
/// caller's, and a duplicate is answered, never parked. Cheap-clone shared
/// handle: the completing side and every waiter share one table.
pub struct KeyedGate<K: GateKey, V, C: Cleanup<V> = DropValue> {
    core: Rc<RefCell<KeyedCore<V>>>,
    _key: KeyedMarker<K, V, C>,
}

/// [`KeyedGate::reserve`]'s answer.
pub enum KeyedAdmission<'g, K: GateKey, V, C: Cleanup<V> = DropValue> {
    /// The key is free and a holder is the permit's.
    Granted(KeyedPermit<'g, K, V, C>),
    /// A waiter is registered under the key.
    Duplicate,
    /// Every route or holder is taken (`cold_gate_full`).
    Full,
}

impl<K: GateKey, V, C: Cleanup<V>> KeyedGate<K, V, C> {
    /// A gate of `cap` routes and holders on `census`: the holder slab and
    /// the route map share the cap's one row (ADR-0177 D4). Built whole by
    /// steps; nothing is served until it returns.
    ///
    /// # Errors
    /// `CapError` from either build or the census; a refused route build
    /// retires the built holders by steps first.
    pub fn build(cap: Cap, census: &Rc<CapCensus>) -> Result<KeyedGate<K, V, C>, CapError> {
        let holders = build_slab(CappedSlab::builder(cap, census)?)?;
        let routes = match CappedMap::builder(cap, census).and_then(build_routes) {
            Ok(routes) => routes,
            Err(refused) => {
                retire_slab(holders);
                return Err(refused);
            }
        };
        let core = KeyedCore { routes, holders, full: 0 };
        Ok(KeyedGate { core: Rc::new(RefCell::new(core)), _key: PhantomData })
    }

    /// A holder slot's bytes, the stride the keyed gate's holders take.
    pub const HOLDER_SLOT_BYTES: usize = HolderSlab::<V>::SLOT_BYTES;

    /// Routes `key` to a free holder, published registered: the permit
    /// holds the gate until it registers, one synchronous step, and a
    /// permit dropped unregistered takes both back. A present route is
    /// `Duplicate`; no free route or holder is `Full`, counted, and nothing
    /// is taken.
    pub fn reserve(&self, key: K) -> KeyedAdmission<'_, K, V, C> {
        let mut core = self.core.borrow_mut();
        let word = key.word();
        let KeyedCore { routes, holders, full } = &mut *core;
        let route = match routes.admit(word) {
            Admission::Vacant(route) => route,
            Admission::Occupied(_) => return KeyedAdmission::Duplicate,
            Admission::Full => {
                *full += 1;
                return KeyedAdmission::Full;
            }
        };
        let slot = match holders.reserve() {
            SlabReserved::Slot(slot) => slot.publish(Holder::registered(false)),
            // The vacant route drops unpublished: the map is as it was.
            SlabReserved::Full | SlabReserved::Exhausted => {
                *full += 1;
                return KeyedAdmission::Full;
            }
        };
        route.publish(slot);
        KeyedAdmission::Granted(KeyedPermit { gate: self, core, slot: Some(slot), word })
    }

    /// Delivers `value` to `key`'s waiter, waking it if it is suspended,
    /// and takes the route in the same step. No route for `key`, a second
    /// completion among them, gives `value` to the cleanup.
    pub fn complete(&self, key: K, value: V) -> Delivery {
        let mut core = self.core.borrow_mut();
        let KeyedCore { routes, holders, .. } = &mut *core;
        let Some(slot) = routes.remove(key.word()) else {
            drop(core);
            C::orphan(value);
            return Delivery::Orphan;
        };
        let holder = match holders.get_mut(slot) {
            LookupMut::Live(holder) if awaits(&holder.state) => holder,
            LookupMut::Live(_) | LookupMut::Stale | LookupMut::Vacant => {
                debug_assert!(false, "a route names a holder awaiting its value");
                drop(core);
                C::orphan(value);
                return Delivery::Orphan;
            }
        };
        let prior = core::mem::replace(&mut holder.state, HolderState::Delivered(value));
        drop(core);
        if let HolderState::Waiting(waker, _) = prior {
            waker.wake();
        }
        Delivery::Delivered
    }

    /// Whether the wait `hold` pins is still registered or waiting: one read
    /// of the slot the hold pins, never the route and never another
    /// occupant.
    pub fn waiting(&self, hold: &GateHold) -> bool {
        match self.core.borrow().holders.get(hold.slot) {
            Lookup::Live(holder) => awaits(&holder.state),
            Lookup::Stale | Lookup::Vacant => false,
        }
    }

    /// Releases `hold` when its request leaves the queue: an orphaned holder
    /// frees now, a live one when its wait is done.
    pub fn release(&self, hold: GateHold) {
        let mut core = self.core.borrow_mut();
        let LookupMut::Live(holder) = core.holders.get_mut(hold.slot) else {
            debug_assert!(false, "a hold pins its holder");
            return;
        };
        debug_assert!(holder.held, "a hold pins its holder");
        holder.held = false;
        if matches!(holder.state, HolderState::Orphaned) {
            let _freed = core.holders.take(hold.slot);
        }
    }

    /// Holders taken: registered, waiting, delivered or held (tests, gauges).
    pub fn pending(&self) -> usize {
        self.core.borrow().holders.len() as usize
    }

    /// Routes present: one for each holder that awaits its value.
    pub fn routes(&self) -> usize {
        self.core.borrow().routes.len() as usize
    }

    /// Reserves that found every route or holder taken (`cold_gate_full`):
    /// derived never where the cap is its product.
    pub fn full(&self) -> u64 {
        self.core.borrow().full
    }
}

/// Whether a holder's wait still awaits its value: registered or waiting.
fn awaits<V>(state: &HolderState<V>) -> bool {
    match state {
        HolderState::Registered(_) | HolderState::Waiting(..) => true,
        HolderState::Delivered(_) | HolderState::Orphaned => false,
    }
}

/// Steps a route map's build to its end; a refused build retires its
/// prefix by steps before it answers.
fn build_routes(
    mut builder: MapBuilder<u64, SlotId<u32>>,
) -> Result<CappedMap<u64, SlotId<u32>>, CapError> {
    loop {
        builder = match builder.step() {
            MapBuilt::More(next) => next,
            MapBuilt::Live(routes) => return Ok(routes),
            MapBuilt::Refused(mut retiring, refused) => loop {
                retiring = match retiring.step() {
                    MapRetired::More(next) => next,
                    MapRetired::Done => return Err(refused),
                };
            },
        };
    }
}

/// Retires a built slab by steps: a build refused after its slab, a gate's
/// or the wait pool's, frees what it built without a whole drop.
fn retire_slab<T>(slab: CappedSlab<T, u32, WrapWithProof>) {
    let mut retiring = slab.retire();
    loop {
        retiring = match retiring.step() {
            Retiring::More(next) => next,
            Retiring::Done => return,
        };
    }
}

impl<K: GateKey, V, C: Cleanup<V>> Clone for KeyedGate<K, V, C> {
    /// Gates are shared handles (event source + futures share one table).
    fn clone(&self) -> Self {
        KeyedGate { core: Rc::clone(&self.core), _key: PhantomData }
    }
}

impl<K: GateKey, V, C: Cleanup<V>> core::fmt::Debug for KeyedGate<K, V, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "KeyedGate {{ pending: {} }}", self.pending())
    }
}

/// A route and holder for one key, published registered, holding the gate
/// until it registers: it cannot cross an `await` (ADR-0144 A2's list).
pub struct KeyedPermit<'g, K: GateKey, V, C: Cleanup<V> = DropValue> {
    gate: &'g KeyedGate<K, V, C>,
    core: RefMut<'g, KeyedCore<V>>,
    /// The holder's handle until the permit registers or drops.
    slot: Option<SlotId<u32>>,
    word: u64,
}

impl<K: GateKey, V, C: Cleanup<V>> KeyedPermit<'_, K, V, C> {
    /// The wait on the route and holder `reserve` published: the key is
    /// routable from here.
    pub fn register(mut self) -> KeyedWait<K, V, C> {
        let Some(slot) = self.slot.take() else {
            unreachable!("a keyed permit registers once, consuming itself")
        };
        KeyedWait {
            core: Rc::clone(&self.gate.core),
            slot,
            word: self.word,
            done: false,
            _key: PhantomData,
        }
    }

    /// [`register`](Self::register) for a request its producer queues before
    /// issuing: the [`GateHold`] is a second claim on the holder, kept with
    /// the queued request and given back by [`KeyedGate::release`].
    pub fn register_held(mut self) -> (KeyedWait<K, V, C>, GateHold) {
        let Some(slot) = self.slot.take() else {
            unreachable!("a keyed permit registers once, consuming itself")
        };
        if let LookupMut::Live(holder) = self.core.holders.get_mut(slot) {
            holder.held = true;
        }
        let wait = KeyedWait {
            core: Rc::clone(&self.gate.core),
            slot,
            word: self.word,
            done: false,
            _key: PhantomData,
        };
        (wait, GateHold { slot })
    }
}

impl<K: GateKey, V, C: Cleanup<V>> Drop for KeyedPermit<'_, K, V, C> {
    fn drop(&mut self) {
        // Never registered: the route and the holder `reserve` published
        // return, and nothing awaited them.
        if let Some(slot) = self.slot.take() {
            let KeyedCore { routes, holders, .. } = &mut *self.core;
            let _route = routes.remove(self.word);
            let _freed = holders.take(slot);
        }
    }
}

/// A queue's claim on a keyed holder (ADR-0177 D4): move-only, consumed by
/// [`KeyedGate::release`]. While it is out the holder's slot stays, so
/// [`KeyedGate::waiting`] reads that slot and no later occupant.
#[must_use = "a hold is released when its request leaves the queue"]
#[derive(Debug)]
pub struct GateHold {
    slot: SlotId<u32>,
}

/// Future returned by a keyed registration. Dropped before its value, the
/// route returns at once and the holder with it, or at its hold's release.
pub struct KeyedWait<K: GateKey, V, C: Cleanup<V> = DropValue> {
    core: Rc<RefCell<KeyedCore<V>>>,
    slot: SlotId<u32>,
    /// The route's key, which the wait's drop takes while it awaits.
    word: u64,
    done: bool,
    _key: KeyedMarker<K, V, C>,
}

// No self-references — state lives behind the shared `Rc` table.
impl<K: GateKey, V, C: Cleanup<V>> Unpin for KeyedWait<K, V, C> {}

impl<K: GateKey, V, C: Cleanup<V>> Future for KeyedWait<K, V, C> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<V> {
        let this = Pin::into_inner(self);
        let mut core = this.core.borrow_mut();
        let LookupMut::Live(holder) = core.holders.get_mut(this.slot) else {
            polled_after_completion()
        };
        if store_waker(&mut holder.state, cx.waker()) {
            return Poll::Pending;
        }
        match &holder.state {
            HolderState::Delivered(_) => {}
            HolderState::Registered(_) | HolderState::Waiting(..) | HolderState::Orphaned => {
                polled_after_completion()
            }
        }
        let state = if holder.held {
            // The hold keeps the slot until its release; the route left at
            // the delivery.
            core::mem::replace(&mut holder.state, HolderState::Orphaned)
        } else {
            let Taken::Value(Holder { state, .. }) = core.holders.take_inlined(this.slot) else {
                polled_after_completion()
            };
            state
        };
        let HolderState::Delivered(value) = state else { polled_after_completion() };
        this.done = true;
        Poll::Ready(value)
    }
}

impl<K: GateKey, V, C: Cleanup<V>> Drop for KeyedWait<K, V, C> {
    fn drop(&mut self) {
        // Canary: a dropped wait leaves its route, and its holder with the
        // waker, so `waiting` answers yes for it and the routes fill.
        if self.done || cfg!(inf_canary_keyed_route_kept_on_drop) {
            return;
        }
        let mut core = self.core.borrow_mut();
        let KeyedCore { routes, holders, .. } = &mut *core;
        let LookupMut::Live(holder) = holders.get_mut(self.slot) else { return };
        let prior = core::mem::replace(&mut holder.state, HolderState::Orphaned);
        // Canary: a held wait frees its slot at the drop, before its hold's
        // release, so the hold can read the slot's next occupant.
        let held = holder.held && !cfg!(inf_canary_keyed_hold_frees_on_drop);
        if awaits(&prior) {
            let _route = routes.remove(self.word);
        }
        if !held {
            let _freed = holders.take(self.slot);
        }
        drop(core);
        // The waker and a delivered value drop outside the gate's borrow.
        match prior {
            HolderState::Delivered(value) => C::orphan(value),
            HolderState::Registered(_) | HolderState::Waiting(..) | HolderState::Orphaned => {}
        }
    }
}

/// Completion-keyed gate for suspending on backend I/O (M7 disk reads; the
/// seam exists from M0 so the executor contract is complete).
pub type IoGate = KeyedGate<CompletionToken, CompletionResult>;

// ---- WaitNodes ----------------------------------------------------------------

/// A wait node's handle: its slot in the pool's slab.
type NodeId = SlotId<u32>;

/// The pool's slab (ADR-0177 D5, D7): a `u32` generation that wraps.
type NodeSlab = CappedSlab<WaitNode, u32, WrapWithProof>;

/// A queued node's state: on its list, or taken off it by a wake its wait
/// has not read yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NodeState {
    Queued,
    Woken,
}

/// A parked wait (ADR-0177 D5): its task's waker, its place on its one list
/// or the watermark gate's heap, and its state. 48 B, a 56 B slot: the
/// position sits in the padding the other fields leave.
struct WaitNode {
    waker: Option<Waker>,
    prev: Option<NodeId>,
    next: Option<NodeId>,
    /// Its entry's index on the watermark gate's heap, which the heap's hook
    /// writes at every move, so a dropped wait removes its entry in one sift
    /// (ADR-0177 D5). A list's node has no entry, and nothing reads its
    /// position.
    #[expect(
        dead_code,
        reason = "storage nothing reads yet: the gate's waits still queue in its B-tree"
    )]
    position: u32,
    state: NodeState,
}

const _: () = assert!(size_of::<WaitNode>() == 48, "a wait node is 48 B");

/// A queued watermark wait's entry on the gate's heap (ADR-0177 D5): the LSN
/// it waits for and its node, 24 B.
#[derive(Clone, Copy)]
struct WaitEntry {
    #[expect(
        dead_code,
        reason = "storage nothing reads yet: the gate's waits still queue in its B-tree"
    )]
    lsn: u64,
    node: NodeId,
}

/// What a pool's handles share: the slab and its counter.
struct Pool {
    nodes: RefCell<NodeSlab>,
    /// First polls that found no node or no key (`wait_nodes_full`, cell
    /// scope): derived never, a task holding at most one parked wait.
    full: Cell<u64>,
}

/// The cell's wait nodes (ADR-0177 D5): one pool a cell, a capped slab of
/// `T` nodes, one for each live task's one parked wait, on the cell's census
/// under its cap's row and resident whole from its build. Every
/// [`WaitList`] of the cell takes it at its constructor and links its waits
/// through it; a clone is a handle on the same pool.
#[derive(Clone)]
pub struct WaitNodes {
    pool: Rc<Pool>,
}

impl WaitNodes {
    /// A node's slot bytes: the stride the idle bill counts the pool at, in
    /// its chunks (ADR-0177 D8 with A1).
    pub const NODE_SLOT_BYTES: usize = NodeSlab::SLOT_BYTES;

    /// The pool of `cap` nodes on the cell's `census`, its row registered and
    /// its slab built whole by steps before the cell serves, then its one
    /// [`WatermarkGate`]'s entry array, one entry a node, by steps of its own;
    /// answered with the seat of that gate, which carries the array
    /// (ADR-0177 D5).
    ///
    /// # Errors
    /// `CapError` from either build or the slab's census row; a refused slab
    /// retires its prefix first, and a refused array the built slab, each by
    /// steps. Nothing is served.
    pub fn build(cap: Cap, census: &Rc<CapCensus>) -> Result<WaitPool, CapError> {
        let mut nodes = build_slab(NodeSlab::builder(cap, census)?)?;
        let entries = match build_entries(&mut nodes, cap.entries_max()) {
            Ok(entries) => entries,
            Err(refused) => {
                retire_slab(nodes);
                return Err(refused);
            }
        };
        let pool = Pool { nodes: RefCell::new(nodes), full: Cell::new(0) };
        let nodes = WaitNodes { pool: Rc::new(pool) };
        let seat = WatermarkSeat { nodes: nodes.clone(), entries };
        Ok(WaitPool { nodes, seat })
    }

    /// Nodes held by parked waits now (cell scope).
    #[must_use]
    pub fn len(&self) -> u32 {
        self.pool.nodes.borrow().len()
    }

    /// Whether no parked wait holds a node.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// First polls that found no node or no key (`wait_nodes_full`, cell
    /// scope).
    #[must_use]
    pub fn full(&self) -> u64 {
        self.pool.full.get()
    }
}

impl core::fmt::Debug for WaitNodes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WaitNodes {{ len: {}, full: {} }}", self.len(), self.full())
    }
}

/// A cell's pool as [`WaitNodes::build`] answers it: the handle every wait
/// list of the cell takes, and the seat of the pool's one watermark gate.
#[derive(Debug)]
pub struct WaitPool {
    /// The pool's handle; each wait list takes a clone.
    pub nodes: WaitNodes,
    /// The seat [`WatermarkGate::build`] consumes, with the gate's entry
    /// array.
    pub seat: WatermarkSeat,
}

/// The seat of a pool's one [`WatermarkGate`] (ADR-0177 D5): answered once,
/// by the pool's build, and carrying its pool and the gate's entry array,
/// which the pool built in its steps. It is move-only, with no
/// `Clone`, and [`WatermarkGate::build`] consumes it, so a second gate on one
/// pool does not type-check. One gate on one seat builds:
///
/// ```
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill};
/// # use inf_foundation::CellId;
/// # use inf_runtime::{WaitNodes, WatermarkGate};
/// let (census, _serve_mark) = CapCensus::new(CellId(0));
/// let Ok(pool) = WaitNodes::build(Cap::entries::<4>("x", CapFill::Serving), &census) else {
///     return;
/// };
/// let _gate = WatermarkGate::build(pool.seat);
/// ```
///
/// A second gate on the same seat does not, the seat having moved into the
/// first:
///
/// ```compile_fail
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill};
/// # use inf_foundation::CellId;
/// # use inf_runtime::{WaitNodes, WatermarkGate};
/// let (census, _serve_mark) = CapCensus::new(CellId(0));
/// let Ok(pool) = WaitNodes::build(Cap::entries::<4>("x", CapFill::Serving), &census) else {
///     return;
/// };
/// let _gate = WatermarkGate::build(pool.seat);
/// let _second = WatermarkGate::build(pool.seat); // use of a moved seat
/// ```
///
/// Nor does a copy of the seat to build a second gate on:
///
/// ```compile_fail
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill};
/// # use inf_foundation::CellId;
/// # use inf_runtime::WaitNodes;
/// let (census, _serve_mark) = CapCensus::new(CellId(0));
/// let Ok(pool) = WaitNodes::build(Cap::entries::<4>("x", CapFill::Serving), &census) else {
///     return;
/// };
/// let _copy = pool.seat.clone(); // the seat is not `Clone`
/// ```
pub struct WatermarkSeat {
    nodes: WaitNodes,
    /// The gate's heap entries, one a node of the pool, built in the pool's
    /// steps and resident whole from them, whether or not a gate takes them.
    entries: FixedArray<WaitEntry>,
}

impl core::fmt::Debug for WatermarkSeat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let entries = self.entries.capacity();
        write!(f, "WatermarkSeat {{ nodes: {:?}, entries: {entries} }}", self.nodes)
    }
}

/// The most steps an entry array of `capacity` entries takes to build: a
/// step allocates one chunk, which holds at least one entry, or one
/// directory, each under the top one holding at least one chunk, and the top
/// is one step, so `2 × capacity + 1`.
const fn entry_steps_max(capacity: u32) -> u64 {
    2 * capacity as u64 + 1
}

/// The gate's entry array for a pool of `capacity` nodes, built by steps
/// after its slab, before the cell serves (ADR-0177 D5): the array is fixed
/// at the pool's cap, in chunks of at most 64 KiB, and allocates nothing
/// once built. Every element is filled with a handle the pool minted and
/// never published, which names no node: the heap reads only the entries it
/// holds, each placed by a push, and never the fill.
///
/// # Errors
/// The array's `CapError`; a builder that asks for more steps than
/// [`entry_steps_max`] has broken its own contract, and is refused as
/// `Layout`. Nothing is served either way.
fn build_entries(nodes: &mut NodeSlab, capacity: u32) -> Result<FixedArray<WaitEntry>, CapError> {
    let fill = WaitEntry { lsn: 0, node: unpublished_node(nodes)? };
    debug_assert!(matches!(nodes.get(fill.node), Lookup::Stale), "the fill names no node");
    let mut builder = FixedArray::builder(capacity, fill)?;
    for _ in 0..entry_steps_max(capacity) {
        builder = match builder.step() {
            ArrayBuilt::More(next) => next,
            ArrayBuilt::Done(entries) => return Ok(entries),
            ArrayBuilt::Refused(refused) => return Err(refused),
        };
    }
    Err(CapError::Layout)
}

/// A handle of the pool that names no node: a named reservation dropped
/// unpublished, so its slot is left free at its next generation and every
/// lookup by the handle reads `Stale` (ADR-0194 D1 with A2). Nothing of the
/// pool's counts or census row moves. A pool just built has a free slot, as
/// its cap is at least one; were none free, the build is refused as
/// `Layout` before the cell serves.
fn unpublished_node(nodes: &mut NodeSlab) -> Result<NodeId, CapError> {
    match nodes.reserve() {
        SlabReserved::Slot(slot) => Ok(slot.named().id()),
        SlabReserved::Full | SlabReserved::Exhausted => Err(CapError::Layout),
    }
}

// ---- WaitList ---------------------------------------------------------------

/// A keyed [`WaitList`]'s refusal at its build, before the cell serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeysRefused {
    /// The key map's build was refused.
    Cap(CapError),
    /// A key past the map's cap: the cap is short of its host's demand.
    Full(AssemblyFull),
}

impl core::fmt::Display for KeysRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KeysRefused::Cap(refused) => write!(f, "a wait list's keys: {refused}"),
            KeysRefused::Full(refused) => write!(f, "{refused}"),
        }
    }
}

impl std::error::Error for KeysRefused {}

/// The node `id` names, which its wait keeps live from its first poll to its
/// release.
// Inlined always, as `live_mut` and `release` are: each is called by more
// than one of a row's terms, and as a body of its own it would hold several
// terms' instructions under one name, where a listing reads a term by its
// function.
#[inline(always)]
fn live(nodes: &NodeSlab, id: NodeId) -> &WaitNode {
    match nodes.get(id) {
        Lookup::Live(node) => node,
        Lookup::Stale | Lookup::Vacant => unreachable!("a held wait node is live"),
    }
}

/// [`live`], mutably.
#[inline(always)]
fn live_mut(nodes: &mut NodeSlab, id: NodeId) -> &mut WaitNode {
    match nodes.get_mut(id) {
        LookupMut::Live(node) => node,
        LookupMut::Stale | LookupMut::Vacant => unreachable!("a held wait node is live"),
    }
}

/// The one place a node returns to the pool. Its waker left it at its wake
/// or in the access that unlinked it, so the release, which drops the node
/// where it lies, runs no waker's drop under the slab's borrow.
#[inline(always)]
fn release(nodes: &mut NodeSlab, id: NodeId) {
    debug_assert!(
        matches!(nodes.get(id), Lookup::Live(node) if node.waker.is_none()),
        "a node is released after its waker left it"
    );
    let released = nodes.release(id);
    debug_assert_eq!(released, Released::Dropped, "a held wait node is live");
}

/// A first poll found no node or no key: counted, and the task wakes itself
/// with the waker it cloned, the pool's borrow already ended.
#[cold]
#[inline(never)]
fn starve(pool: &Pool, waker: Waker) {
    pool.full.set(pool.full.get().saturating_add(1));
    waker.wake();
}

/// A queued wait polled again by another waker: the new one is cloned before
/// the pool is borrowed and stored only while the node is still queued; the
/// one it replaces drops after the borrow ends.
#[cold]
#[inline(never)]
fn rewake(pool: &Pool, id: NodeId, cx: &Context<'_>) {
    let fresh = cx.waker().clone();
    let stale = {
        let mut nodes = pool.nodes.borrow_mut();
        let node = live_mut(&mut nodes, id);
        match node.state {
            NodeState::Queued => node.waker.replace(fresh),
            NodeState::Woken => Some(fresh),
        }
    };
    drop(stale);
}

/// A list's storage, sealed: its items are `pub`, so `ListKey` names them, in a module no
/// caller can name, so the list's ends stay out of the crate's API.
mod lists {
    use core::cell::Cell;
    use std::rc::Rc;

    use inf_foundation::bounded::{CappedMap, ExactKey};

    use super::{ListKey, NodeId, Pool};

    /// One key's FIFO: its two ends and its queued waits, each a cell, so a list
    /// reads and moves them with no borrow of the pool.
    #[derive(Default, Debug)]
    pub struct Ends {
        pub(super) head: Cell<Option<NodeId>>,
        pub(super) tail: Cell<Option<NodeId>>,
        pub(super) queued: Cell<u32>,
    }

    /// A list's ends by key, and its count of queued waits.
    pub trait Queues<K> {
        /// The key's ends; `None` for a key the list does not hold.
        fn ends(&self, key: K) -> Option<&Ends>;
        /// Waits queued on the list, on every key: one load, with no borrow.
        fn queued(&self) -> u32;
        /// A wait joined `ends`' tail.
        fn joined(&self, ends: &Ends);
        /// A wait left `ends`: woken, or unlinked at its drop.
        fn left(&self, ends: &Ends);
    }

    /// The unit key's list keeps its one pair of ends in its own state, with no
    /// map: its ends' count is the list's count, one fact.
    impl Queues<()> for Ends {
        // Folded always, as the keyed list's `ends` (ADR-0210 D1).
        #[inline(always)]
        fn ends(&self, (): ()) -> Option<&Ends> {
            Some(self)
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn queued(&self) -> u32 {
            self.queued.get()
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn joined(&self, ends: &Ends) {
            ends.queued.set(ends.queued.get() + 1);
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn left(&self, ends: &Ends) {
            ends.queued.set(ends.queued.get() - 1);
        }
    }

    /// A keyed list's ends: every key admitted before the cell serves, so a
    /// lookup is a shared read and the map takes no borrow of its own; `None`
    /// holds no key.
    pub struct KeyedEnds<K: ExactKey> {
        pub(super) queued: Cell<u32>,
        pub(super) by_key: Option<CappedMap<K, Ends>>,
    }

    impl<K: ExactKey> Queues<K> for KeyedEnds<K> {
        // Folded always: under the keys' bodies a hint alone left it a body
        // (ADR-0210 D1).
        #[inline(always)]
        fn ends(&self, key: K) -> Option<&Ends> {
            self.by_key.as_ref()?.get(key)
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn queued(&self) -> u32 {
            self.queued.get()
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn joined(&self, ends: &Ends) {
            ends.queued.set(ends.queued.get() + 1);
            self.queued.set(self.queued.get() + 1);
        }

        // Folded always, as `ends` (ADR-0210 D1).
        #[inline(always)]
        fn left(&self, ends: &Ends) {
            ends.queued.set(ends.queued.get() - 1);
            self.queued.set(self.queued.get() - 1);
        }
    }

    /// What a list's handles share: the cell's pool and the list's ends.
    pub struct List<K: ListKey> {
        pub(super) pool: Rc<Pool>,
        pub(super) queues: K::Queues,
    }
}

use lists::{Ends, KeyedEnds, List, Queues};

/// A wait list's key: the unit key, whose list keeps its ends in its own
/// state, or a key a capped map holds.
pub trait ListKey: ExactKey {
    /// Where the list keeps its ends.
    type Queues: Queues<Self>;

    // The list's two bodies, each a method of a non-generic impl: one compiled
    // copy a key, in this crate, which every binary links (ADR-0210 D1).
    /// [`ListWait`]'s poll at this key.
    #[doc(hidden)]
    fn poll_wait(wait: Pin<&mut ListWait<Self>>, cx: &mut Context<'_>) -> Poll<()>;

    /// The wake of this key's head on `list`.
    #[doc(hidden)]
    fn wake_head_at(list: &List<Self>, key: Self) -> bool;
}

impl ListKey for () {
    type Queues = Ends;

    // A body, never folded: the one copy at this key that the bench and the
    // server both call (ADR-0210 D1).
    #[inline(never)]
    fn poll_wait(wait: Pin<&mut ListWait<Self>>, cx: &mut Context<'_>) -> Poll<()> {
        wait.poll_in(cx)
    }

    // A body, never folded, as `poll_wait` (ADR-0210 D1).
    #[inline(never)]
    fn wake_head_at(list: &List<Self>, key: Self) -> bool {
        list.wake_head_in(key)
    }
}

macro_rules! keyed_list_key {
    ($($key:ty),*) => {$(
        impl ListKey for $key {
            type Queues = KeyedEnds<$key>;

            // A body, never folded: the one copy at this key that the bench and
            // the server both call (ADR-0210 D1).
            #[inline(never)]
            fn poll_wait(wait: Pin<&mut ListWait<Self>>, cx: &mut Context<'_>) -> Poll<()> {
                wait.poll_in(cx)
            }

            // A body, never folded, as `poll_wait` (ADR-0210 D1).
            #[inline(never)]
            fn wake_head_at(list: &List<Self>, key: Self) -> bool {
                list.wake_head_in(key)
            }
        }
    )*};
}

keyed_list_key!(u8, u16, u32, u64, CellId);

/// Key-keyed FIFO wait list over the cell's pool (ADR-0177 D5): a wait takes
/// a node at its first poll and gives it back at the poll that reads its
/// wake or at its drop. `wake_one` hands the key to its longest-waiting
/// task, `wake_all` to every task it held as the call began.
pub struct WaitList<K: ListKey> {
    list: Rc<List<K>>,
}

impl WaitList<()> {
    /// The unit key's list on the cell's `nodes`: no map, no build step, no
    /// refusal.
    #[must_use]
    pub fn unit(nodes: &WaitNodes) -> WaitList<()> {
        let list = List { pool: Rc::clone(&nodes.pool), queues: Ends::default() };
        WaitList { list: Rc::new(list) }
    }
}

impl<K: ListKey<Queues = KeyedEnds<K>>> WaitList<K> {
    /// A keyed list on the cell's `nodes` that holds every key of `keys`,
    /// in a map of `cap` on `census`, built and filled whole before the
    /// cell serves (`CapFill::Assembly`). `None` builds no map: a list that
    /// holds no key, at a node of one cell, which has no peer; a key given
    /// with `None` is past a cap of none, and refused.
    ///
    /// # Errors
    /// [`KeysRefused`]: the map's build refused, or a key past its cap
    /// (`Full`, naming the cap's row, or `crate::limits::WAIT_LIST_KEYLESS`'s
    /// with `None`). Nothing is served.
    pub fn keyed(
        nodes: &WaitNodes,
        cap: Option<Cap>,
        census: &Rc<CapCensus>,
        keys: impl IntoIterator<Item = K>,
    ) -> Result<WaitList<K>, KeysRefused> {
        let by_key = match cap {
            None => {
                if keys.into_iter().next().is_some() {
                    let keyless = crate::limits::WAIT_LIST_KEYLESS;
                    return Err(KeysRefused::Full(AssemblyFull::new(keyless)));
                }
                None
            }
            Some(cap) => Some(keys_held(cap, census, keys)?),
        };
        let queues = KeyedEnds { queued: Cell::new(0), by_key };
        let list = List { pool: Rc::clone(&nodes.pool), queues };
        Ok(WaitList { list: Rc::new(list) })
    }
}

/// A keyed list's map of `cap` on `census`, built by steps and filled with
/// every key of `keys` before the cell serves.
fn keys_held<K: ExactKey>(
    cap: Cap,
    census: &Rc<CapCensus>,
    keys: impl IntoIterator<Item = K>,
) -> Result<CappedMap<K, Ends>, KeysRefused> {
    let mut builder = CappedMap::<K, Ends>::builder(cap, census).map_err(KeysRefused::Cap)?;
    let mut by_key = loop {
        builder = match builder.step() {
            MapBuilt::More(next) => next,
            MapBuilt::Live(map) => break map,
            MapBuilt::Refused(mut retiring, refused) => loop {
                retiring = match retiring.step() {
                    MapRetired::More(next) => next,
                    MapRetired::Done => return Err(KeysRefused::Cap(refused)),
                };
            },
        };
    };
    for key in keys {
        match by_key.admit(key) {
            Admission::Vacant(slot) => slot.publish(Ends::default()),
            Admission::Occupied(_) => {}
            Admission::Full => return Err(KeysRefused::Full(AssemblyFull::new(cap))),
        }
    }
    Ok(by_key)
}

impl<K: ListKey> WaitList<K> {
    /// A wait on `key`. It joins the key's FIFO at its first poll and
    /// resolves when a wake reaches it; dropped while queued it unlinks in
    /// place, and dropped after an unread wake it passes the wake on, so a
    /// wake is never silently lost.
    // Folded always: neither binary holds a body of it, so the attribute holds
    // that place against a neighbour's attribute (ADR-0210 D1).
    #[inline(always)]
    pub fn wait(&self, key: K) -> ListWait<K> {
        ListWait { list: Rc::clone(&self.list), key, state: ListState::Fresh }
    }

    /// Wakes the longest-waiting task on `key`. Returns whether one was woken.
    // Folded always: with its head's placement moved, a bench outlined it where
    // the server folded it; one place in both (ADR-0210 D1).
    #[inline(always)]
    pub fn wake_one(&self, key: K) -> bool {
        // A wake that meets an idle list reads the count and leaves: no
        // borrow and no lookup.
        if self.list.queues.queued() == 0 {
            return false;
        }
        self.list.wake_head(key)
    }

    /// Wakes every task that waits on `key` as the call begins. Returns how
    /// many. A wait that joins during the call, from a waker, stays queued.
    pub fn wake_all(&self, key: K) -> usize {
        let Some(ends) = self.list.queues.ends(key) else { return 0 };
        let at_entry = ends.queued.get();
        let mut woken = 0;
        for _ in 0..at_entry {
            if !self.list.wake_head(key) {
                break;
            }
            woken += 1;
        }
        woken
    }

    /// Whether no wait is queued on any key: one load of the list's count,
    /// with no borrow of the pool and no walk, so a caller whose wake almost
    /// always meets an empty list reads this and skips the call.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.list.queues.queued() == 0
    }

    /// Waits queued across all keys (gauges, tests, leak asserts): the same
    /// count.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.list.queues.queued() as usize
    }
}

impl<K: ListKey> Clone for WaitList<K> {
    /// A shared handle: wake sites and waits share the list.
    fn clone(&self) -> Self {
        WaitList { list: Rc::clone(&self.list) }
    }
}

impl<K: ListKey> core::fmt::Debug for WaitList<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WaitList {{ waiting: {} }}", self.waiting())
    }
}

impl<K: ListKey> List<K> {
    /// Unlinks and wakes `key`'s head; returns whether the key held one.
    // Folded always: the body is its key's `wake_head_at` (ADR-0210 D1).
    #[inline(always)]
    fn wake_head(&self, key: K) -> bool {
        K::wake_head_at(self, key)
    }

    /// [`List::wake_head`]'s body: the waker is taken in the access that
    /// marks the node woken, and the pool's borrow ends before `wake`, so a
    /// waker that reaches the pool finds it free.
    // Folded always into its key's `wake_head_at`, its one body (ADR-0210 D1).
    #[inline(always)]
    fn wake_head_in(&self, key: K) -> bool {
        let waker = {
            let mut nodes = self.pool.nodes.borrow_mut();
            let Some(ends) = self.queues.ends(key) else { return false };
            let Some(head) = ends.head.get() else { return false };
            let node = live_mut(&mut nodes, head);
            let waker = node.waker.take();
            node.state = NodeState::Woken;
            let next = node.next;
            ends.head.set(next);
            match next {
                Some(next) => live_mut(&mut nodes, next).prev = None,
                None => ends.tail.set(None),
            }
            self.queues.left(ends);
            waker
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }
}

/// A list wait's phase.
#[derive(Clone, Copy)]
enum ListState {
    /// Made, on no list: its first poll joins.
    Fresh,
    /// On its key's FIFO or woken off it, holding its node.
    Queued(NodeId),
    /// Its last poll found no node or no key and woke its task: the next
    /// poll is Ready with no wake, which its caller reads through `starved`.
    Starved,
    Done,
}

/// Future returned by [`WaitList::wait`].
pub struct ListWait<K: ListKey> {
    list: Rc<List<K>>,
    key: K,
    state: ListState,
}

// No self-references: the wait's state lives in the pool and the list.
impl<K: ListKey> Unpin for ListWait<K> {}

impl<K: ListKey> ListWait<K> {
    /// Whether this wait's last poll found no node or no key, so that its
    /// next poll is Ready with no wake landed: a caller that counts on a wake
    /// reads this before it trusts a Ready.
    #[must_use]
    pub fn starved(&self) -> bool {
        matches!(self.state, ListState::Starved)
    }

    /// The wait's poll, the body of its `Future::poll`.
    // Folded always into its key's `poll_wait`, its one body (ADR-0210 D1).
    #[inline(always)]
    fn poll_in(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.state {
            ListState::Queued(id) => this.poll_queued(id, cx),
            ListState::Fresh => this.poll_first(cx),
            // A starved wait resolves with no wake; a done one answers Ready
            // again: no node and no count either way.
            ListState::Starved | ListState::Done => {
                this.state = ListState::Done;
                Poll::Ready(())
            }
        }
    }

    /// The first poll: the wait takes a node and joins its key's tail.
    #[inline]
    fn poll_first(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // The clone runs before the pool is borrowed or a slot reserved, so
        // nothing of the pool is live across a waker's call.
        let waker = cx.waker().clone();
        let list = &*self.list;
        let mut nodes = list.pool.nodes.borrow_mut();
        let Some(ends) = list.queues.ends(self.key) else {
            drop(nodes);
            return self.starved_by(waker);
        };
        let slot = match nodes.reserve() {
            SlabReserved::Slot(slot) => slot,
            SlabReserved::Full | SlabReserved::Exhausted => {
                drop(nodes);
                return self.starved_by(waker);
            }
        };
        let tail = ends.tail.get();
        // A list's node has no heap entry, so its position is never read.
        let node = WaitNode {
            waker: Some(waker),
            prev: tail,
            next: None,
            position: 0,
            state: NodeState::Queued,
        };
        let id = slot.publish(node);
        match tail {
            Some(tail) => live_mut(&mut nodes, tail).next = Some(id),
            None => ends.head.set(Some(id)),
        }
        ends.tail.set(Some(id));
        list.queues.joined(ends);
        drop(nodes);
        self.state = ListState::Queued(id);
        Poll::Pending
    }

    fn starved_by(&mut self, waker: Waker) -> Poll<()> {
        starve(&self.list.pool, waker);
        self.state = ListState::Starved;
        Poll::Pending
    }

    /// A later poll: Ready once a wake took the node off its list.
    #[inline]
    fn poll_queued(&mut self, id: NodeId, cx: &mut Context<'_>) -> Poll<()> {
        let pool = &*self.list.pool;
        let mut nodes = pool.nodes.borrow_mut();
        let node = live(&nodes, id);
        match node.state {
            NodeState::Woken => {
                release(&mut nodes, id);
                drop(nodes);
                self.state = ListState::Done;
                Poll::Ready(())
            }
            NodeState::Queued => {
                let same = node.waker.as_ref().is_some_and(|held| held.will_wake(cx.waker()));
                drop(nodes);
                if !same {
                    rewake(pool, id, cx);
                }
                Poll::Pending
            }
        }
    }

    /// A wait dropped while it holds its node. Queued, it unlinks in O(1)
    /// and its waker leaves in that access, dropped once the borrow has
    /// ended; woken and unread, it passes its wake to the key's next wait.
    #[cold]
    fn leave(&mut self, id: NodeId) {
        let list = &*self.list;
        // Canary: a wait dropped while queued leaves its node linked.
        #[cfg(inf_canary_wait_drop_keeps_node)]
        {
            if live(&list.pool.nodes.borrow(), id).state == NodeState::Queued {
                return;
            }
        }
        let (waker, state) = {
            let mut nodes = list.pool.nodes.borrow_mut();
            let node = live_mut(&mut nodes, id);
            let waker = node.waker.take();
            let (state, prev, next) = (node.state, node.prev, node.next);
            if state == NodeState::Queued {
                let Some(ends) = list.queues.ends(self.key) else {
                    unreachable!("a queued wait's key is its list's")
                };
                match prev {
                    Some(prev) => live_mut(&mut nodes, prev).next = next,
                    None => ends.head.set(next),
                }
                match next {
                    Some(next) => live_mut(&mut nodes, next).prev = prev,
                    None => ends.tail.set(prev),
                }
                list.queues.left(ends);
            }
            release(&mut nodes, id);
            (waker, state)
        };
        drop(waker);
        if state == NodeState::Woken && list.queues.queued() != 0 {
            list.wake_head(self.key);
        }
    }
}

impl<K: ListKey> Future for ListWait<K> {
    type Output = ();

    // Folded always: the body is its key's `poll_wait` (ADR-0210 D1).
    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        K::poll_wait(self, cx)
    }
}

impl<K: ListKey> Drop for ListWait<K> {
    fn drop(&mut self) {
        match self.state {
            ListState::Queued(id) => self.leave(id),
            ListState::Fresh | ListState::Starved | ListState::Done => {}
        }
    }
}

// ---- WatermarkGate ----------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WaiterState {
    Queued,
    Woken,
    Cancelled,
}

struct Waiter {
    state: Cell<WaiterState>,
    waker: RefCell<Option<Waker>>,
}

struct WatermarkInner {
    watermark: Cell<u64>,
    waiters: RefCell<BTreeMap<u64, Vec<Rc<Waiter>>>>,
    /// The seat's entry array, kept resident from the pool's build for the
    /// heap the gate's waits will queue on (ADR-0177 D5).
    entries: FixedArray<WaitEntry>,
}

/// LSN-keyed gate: tasks wait for the durability watermark to reach their
/// LSN; `advance` wakes every task at or below the new watermark (M2 group
/// commit acks). A waiter at or below the current watermark is ready on its
/// first poll. One gate a pool, built on the pool's seat (ADR-0177 D5).
#[derive(Clone)]
pub struct WatermarkGate {
    inner: Rc<WatermarkInner>,
}

impl WatermarkGate {
    /// A heap entry's bytes: the stride the idle bill counts the gate's entry
    /// array at, one entry a node of the pool, resident whole beside the
    /// pool's slab (ADR-0177 D5, D8 with A1).
    pub const ENTRY_BYTES: usize = size_of::<WaitEntry>();

    /// The pool's one gate, on its `seat`: never refused, built before the
    /// cell serves (ADR-0177 D5).
    #[must_use]
    pub fn build(seat: WatermarkSeat) -> WatermarkGate {
        // The waits queue in the B-tree, which takes no node, so the pool's
        // handle is dropped; the entry array stays, resident as the pool's
        // build left it.
        let WatermarkSeat { nodes, entries } = seat;
        drop(nodes);
        WatermarkGate {
            inner: Rc::new(WatermarkInner {
                watermark: Cell::new(0),
                waiters: RefCell::new(BTreeMap::new()),
                entries,
            }),
        }
    }

    pub fn watermark(&self) -> u64 {
        self.inner.watermark.get()
    }

    /// Wait until the watermark reaches `lsn`.
    pub fn wait_for(&self, lsn: u64) -> WatermarkWait {
        let waiter = Rc::new(Waiter {
            state: Cell::new(if self.inner.watermark.get() >= lsn {
                WaiterState::Woken
            } else {
                WaiterState::Queued
            }),
            waker: RefCell::new(None),
        });
        if waiter.state.get() == WaiterState::Queued {
            self.inner.waiters.borrow_mut().entry(lsn).or_default().push(Rc::clone(&waiter));
        }
        WatermarkWait { waiter, done: false }
    }

    /// Advance the watermark (monotonic; lower values are no-ops) and wake
    /// every waiter at or below it.
    pub fn advance(&self, to: u64) {
        if to <= self.inner.watermark.get() {
            return;
        }
        self.inner.watermark.set(to);
        let due = {
            let mut waiters = self.inner.waiters.borrow_mut();
            let still_waiting = waiters.split_off(&(to + 1));
            core::mem::replace(&mut *waiters, still_waiting)
        };
        for waiter in due.into_values().flatten() {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
            }
        }
    }

    /// Wakes the queued waits the watermark covers, in LSN order, at most
    /// `budget` of them, and answers how many it woke (ADR-0177 A2.2). The
    /// B-tree's `advance` wakes every wait it covers, so none is left: 0.
    #[must_use]
    pub fn wake_covered(&self, budget: usize) -> usize {
        let _ = budget;
        0
    }

    /// Whether a covered wait may be left queued for
    /// [`wake_covered`](Self::wake_covered) (ADR-0177 A2.2): never, while the
    /// B-tree's `advance` wakes every wait it covers.
    #[must_use]
    pub fn owes(&self) -> bool {
        false
    }

    /// Live waiters (tests + leak asserts).
    pub fn waiting(&self) -> usize {
        self.inner
            .waiters
            .borrow()
            .values()
            .flat_map(|v| v.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

impl core::fmt::Debug for WatermarkGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "WatermarkGate {{ watermark: {}, waiting: {}, entries: {} }}",
            self.watermark(),
            self.waiting(),
            self.inner.entries.capacity()
        )
    }
}

/// Future returned by [`WatermarkGate::wait_for`].
pub struct WatermarkWait {
    waiter: Rc<Waiter>,
    done: bool,
}

impl Future for WatermarkWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = Pin::into_inner(self);
        match this.waiter.state.get() {
            WaiterState::Woken => {
                this.done = true;
                Poll::Ready(())
            }
            WaiterState::Queued => {
                *this.waiter.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            WaiterState::Cancelled => unreachable!("watermark waiters are never cancelled"),
        }
    }
}

impl Drop for WatermarkWait {
    fn drop(&mut self) {
        // Lazy cleanup: mark cancelled; `advance` skips dead entries. (The
        // BTreeMap entry itself is removed when its LSN is crossed.)
        if !self.done && self.waiter.state.get() == WaiterState::Queued {
            self.waiter.state.set(WaiterState::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::bounded::CapFill;
    use inf_foundation::rng::Entropy;

    /// The tests' pools of wait nodes.
    const TEST_WAIT_NODES: Cap = Cap::entries::<64>("test-wait-nodes", CapFill::Serving);
    /// The tests' keyed lists' keys.
    const TEST_LIST_KEYS: Cap = Cap::entries::<8>("test-list-keys", CapFill::Assembly);

    /// A pool of wait nodes on `census`.
    fn wait_nodes(census: &Rc<CapCensus>) -> WaitNodes {
        WaitNodes::build(TEST_WAIT_NODES, census).expect("the test pool").nodes
    }

    /// A list of `keys` on a census and a pool of its own, and the pool.
    fn pooled_list<K: ListKey<Queues = KeyedEnds<K>>>(
        keys: impl IntoIterator<Item = K>,
    ) -> (WaitList<K>, WaitNodes) {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let nodes = wait_nodes(&census);
        let list =
            WaitList::keyed(&nodes, Some(TEST_LIST_KEYS), &census, keys).expect("the test list");
        (list, nodes)
    }

    /// A list of `keys` on a census and a pool of its own.
    fn keyed_list<K: ListKey<Queues = KeyedEnds<K>>>(
        keys: impl IntoIterator<Item = K>,
    ) -> WaitList<K> {
        pooled_list(keys).0
    }

    /// One poll of `wait` on a waker that does nothing: whether it is Ready.
    fn polled<K: ListKey>(wait: &mut ListWait<K>) -> bool {
        Pin::new(wait).poll(&mut Context::from_waker(Waker::noop())).is_ready()
    }

    /// A fabric gate of 8 holders on a census of its own, at cell 1.
    fn fabric_gate(units: u32) -> RefCell<FabricGate<u64>> {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let holders = Cap::entries::<8>("test-gate-holders", CapFill::Serving);
        let nodes = wait_nodes(&census);
        RefCell::new(
            FabricGate::build(1, units, Some(holders), &census, &nodes).expect("the test gate"),
        )
    }

    /// One registered wait on `gate` under `units`, and the key its request
    /// carries.
    fn fabric_wait<'u>(
        gate: &'u RefCell<FabricGate<u64>>,
        units: &'u GateUnits,
    ) -> (u64, GateWait<'u, u64>) {
        let mut borrowed = gate.borrow_mut();
        let FabricAdmission::Granted(permit) = borrowed.reserve(units) else {
            panic!("a unit and a holder are free");
        };
        let key = permit.key();
        (key, permit.register(gate, None))
    }

    /// The holder table: Registered or Waiting × the waiter dropped → Orphaned, then
    /// Orphaned × `complete` → Free: the slot waits for its request's one
    /// completion, which frees it as an orphan and counts nothing stale.
    #[test]
    fn an_orphaned_holder_frees_at_its_late_completion() {
        let gate = fabric_gate(4);
        let units = gate.borrow().units(1).expect("a unit");
        let mut cx = Context::from_waker(Waker::noop());
        let (key, mut wait) = fabric_wait(&gate, &units);
        assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "engagement: waiting");
        drop(wait);
        assert_eq!(units.free(), 1, "the unit returns at the drop");
        assert_eq!(gate.borrow().holders(), 1, "orphan-freed-at-drop: the slot waits, Orphaned");
        assert_eq!(FabricGate::complete(&gate, key, 7, Nanos(1)), Delivery::Orphan);
        assert_eq!(gate.borrow().holders(), 0, "the late completion frees it");
        assert_eq!(gate.borrow().stale(), 0, "an orphan's one completion is not stale");
    }

    /// The holder table: Delivered × `complete` → unchanged, a second completion of one
    /// key is an orphan and stale, and the parked value stays the waiter's.
    #[test]
    fn a_second_completion_leaves_the_parked_value() {
        let gate = fabric_gate(4);
        let units = gate.borrow().units(1).expect("a unit");
        let mut cx = Context::from_waker(Waker::noop());
        let (key, mut wait) = fabric_wait(&gate, &units);
        assert_eq!(FabricGate::complete(&gate, key, 1, Nanos(1)), Delivery::Delivered);
        assert_eq!(FabricGate::complete(&gate, key, 2, Nanos(2)), Delivery::Orphan);
        assert_eq!(gate.borrow().stale(), 1);
        assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(1), "the first value");
        assert_eq!(gate.borrow().holders(), 0);
    }

    /// The holder table: a key of another origin or without the tag → unchanged, an
    /// orphan, counted stale, and the holder it would alias still waits.
    #[test]
    fn a_foreign_or_untagged_key_completes_nothing() {
        let gate = fabric_gate(4);
        let units = gate.borrow().units(1).expect("a unit");
        let mut cx = Context::from_waker(Waker::noop());
        let (key, mut wait) = fabric_wait(&gate, &units);
        let foreign = key ^ (1 << GATE_KEY_ORIGIN_SHIFT);
        let untagged = key & !(1 << GATE_KEY_TAG_BIT);
        assert_eq!(FabricGate::complete(&gate, foreign, 1, Nanos(1)), Delivery::Orphan);
        assert_eq!(FabricGate::complete(&gate, untagged, 2, Nanos(1)), Delivery::Orphan);
        assert_eq!(gate.borrow().stale(), 2);
        assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "the holder still waits");
        assert_eq!(FabricGate::complete(&gate, key, 3, Nanos(1)), Delivery::Delivered);
        assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(3));
    }

    /// The holder table: Reserved × the permit dropped → Free at the next generation, the
    /// dropped permit's key names no later occupant of its slot, `Stale`.
    #[test]
    fn a_dropped_permits_key_is_stale_at_the_next_occupant() {
        let gate = fabric_gate(4);
        let units = gate.borrow().units(1).expect("a unit");
        let mut cx = Context::from_waker(Waker::noop());
        let dropped = {
            let mut borrowed = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
                panic!("a unit and a holder are free");
            };
            permit.key()
        };
        assert_eq!(units.free(), 1, "the dropped permit's unit returns");
        let (key, mut wait) = fabric_wait(&gate, &units);
        assert_eq!(key & 0xFF_FFFF, dropped & 0xFF_FFFF, "engagement: the same slot");
        assert_ne!(key, dropped, "at its next generation");
        assert_eq!(FabricGate::complete(&gate, dropped, 1, Nanos(1)), Delivery::Orphan);
        assert_eq!(gate.borrow().stale(), 1, "the dropped key is stale");
        assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "the occupant still waits");
        assert_eq!(FabricGate::complete(&gate, key, 2, Nanos(1)), Delivery::Delivered);
        assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(2));
    }

    /// The holder table's refusals: a task with every unit out is `NoUnit`; a gate whose
    /// every holder is taken (here by orphans, whose units came back) is
    /// `Full`, counted; and `units` refuses past the units the gate issues.
    #[test]
    fn no_unit_full_and_units_past_the_max_are_refused() {
        let gate = fabric_gate(8);
        let one = gate.borrow().units(1).expect("a unit");
        let (_key, kept) = fabric_wait(&gate, &one);
        assert!(matches!(gate.borrow_mut().reserve(&one), FabricAdmission::NoUnit));
        drop(kept);
        let seven = gate.borrow().units(7).expect("the other seven");
        assert!(gate.borrow().units(1).is_none(), "every unit is issued");
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..7 {
            let (_key, mut wait) = fabric_wait(&gate, &seven);
            assert!(Pin::new(&mut wait).poll(&mut cx).is_pending());
        }
        assert_eq!(gate.borrow().holders(), 8, "eight orphans hold every slot");
        assert!(matches!(gate.borrow_mut().reserve(&seven), FabricAdmission::Full));
        assert_eq!(gate.borrow().full(), 1);
        assert_eq!(seven.free(), 7, "a refused reserve keeps no unit");
    }

    /// A model fabric holder: what its key's request awaits.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum FabricModel {
        Awaiting,
        Delivered(u64),
        Orphaned,
    }

    /// Seeded operations on a fabric gate of 8 holders and 8 units, against
    /// a `BTreeMap<key, state>` that shares no code with it (ADR-0177 D4's table):
    /// reserves registered or dropped, completions of live, finished and
    /// never-minted keys, polls and drops. Every answer, and after every step
    /// the holders taken and the `stale` and `full` counts, equal the
    /// model's.
    #[test]
    fn the_fabric_gate_holds_what_its_model_holds() {
        const CAP: usize = 8;
        let gate = fabric_gate(CAP as u32);
        let units = gate.borrow().units(CAP as u32).expect("every unit");
        let mut model: BTreeMap<u64, FabricModel> = BTreeMap::new();
        let mut finished: Vec<u64> = Vec::new();
        let (mut stale, mut full) = (0u64, 0u64);
        let mut waits: Vec<(u64, GateWait<'_, u64>)> = Vec::new();
        let mut rng = inf_foundation::rng::SplitMix64::new(0x6761_7465_0000_00E6);
        let mut cx = Context::from_waker(Waker::noop());
        let (mut orphan_frees, mut seconds) = (0u32, 0u32);
        for step in 0..20_000u32 {
            let roll = rng.next_u64();
            match roll % 5 {
                0 => {
                    let units_out = waits.len();
                    let mut borrowed = gate.borrow_mut();
                    match borrowed.reserve(&units) {
                        FabricAdmission::Granted(permit) => {
                            assert!(units_out < CAP && model.len() < CAP, "step {step}: granted");
                            let key = permit.key();
                            assert!(!model.contains_key(&key), "step {step}: a key minted twice");
                            if roll & 8 == 0 {
                                drop(permit);
                                finished.push(key);
                            } else {
                                waits.push((key, permit.register(&gate, None)));
                                model.insert(key, FabricModel::Awaiting);
                            }
                        }
                        FabricAdmission::NoUnit => {
                            assert_eq!(units_out, CAP, "step {step}: NoUnit");
                        }
                        FabricAdmission::Full => {
                            assert_eq!(model.len(), CAP, "step {step}: Full");
                            full += 1;
                        }
                    }
                }
                1 => {
                    let pool = model.len() + finished.len();
                    if pool == 0 {
                        continue;
                    }
                    let at = usize::try_from(rng.next_u64() % pool as u64).expect("small");
                    let key = match model.keys().nth(at) {
                        Some(key) => *key,
                        None => finished[at - model.len()],
                    };
                    let value = rng.next_u64();
                    let expected = match model.get(&key).copied() {
                        Some(FabricModel::Awaiting) => {
                            model.insert(key, FabricModel::Delivered(value));
                            Delivery::Delivered
                        }
                        Some(FabricModel::Orphaned) => {
                            model.remove(&key);
                            finished.push(key);
                            orphan_frees += 1;
                            Delivery::Orphan
                        }
                        Some(FabricModel::Delivered(_)) | None => {
                            seconds += 1;
                            stale += 1;
                            Delivery::Orphan
                        }
                    };
                    let got = FabricGate::complete(&gate, key, value, Nanos(1));
                    assert_eq!(got, expected, "step {step}: key {key:#x}");
                }
                2 if !waits.is_empty() => {
                    let at = usize::try_from(rng.next_u64() % waits.len() as u64).expect("small");
                    let key = waits[at].0;
                    let polled = Pin::new(&mut waits[at].1).poll(&mut cx);
                    match model[&key] {
                        FabricModel::Delivered(value) => {
                            assert_eq!(polled, Poll::Ready(value), "step {step}");
                            model.remove(&key);
                            finished.push(key);
                            drop(waits.swap_remove(at));
                        }
                        state => assert_eq!(polled, Poll::Pending, "step {step}: {state:?}"),
                    }
                }
                3 if !waits.is_empty() => {
                    let at = usize::try_from(rng.next_u64() % waits.len() as u64).expect("small");
                    let (key, wait) = waits.swap_remove(at);
                    drop(wait);
                    match model[&key] {
                        FabricModel::Awaiting => {
                            model.insert(key, FabricModel::Orphaned);
                        }
                        FabricModel::Delivered(_) | FabricModel::Orphaned => {
                            model.remove(&key);
                            finished.push(key);
                        }
                    }
                }
                _ => {}
            }
            assert_eq!(gate.borrow().holders(), model.len(), "fabric-slots: step {step}");
            assert_eq!(gate.borrow().stale(), stale, "fabric-stale: step {step}");
            assert_eq!(gate.borrow().full(), full, "fabric-full: step {step}");
            assert_eq!(units.free() as usize, CAP - waits.len(), "fabric-units: step {step}");
        }
        assert!(orphan_frees > 0, "VACUOUS: no orphan met its late completion");
        assert!(seconds > 0, "VACUOUS: no completion was stale");
        assert!(full > 0, "VACUOUS: the gate never filled");
    }

    /// A recorded hop's holder keeps its send time, and the gate records its
    /// round trip at the completion, whatever order the replies arrive in
    /// and whether or not its waiter suspended first; an unrecorded hop
    /// records nothing.
    #[test]
    fn a_recorded_hop_is_timed_at_its_completion() {
        let gate = fabric_gate(4);
        let units = gate.borrow().units(3).expect("three units");
        let mut cx = Context::from_waker(Waker::noop());
        let send = |sent: Option<u64>| {
            let mut borrowed = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
                panic!("a unit and a holder are free");
            };
            let key = permit.key();
            (key, permit.register(&gate, sent.map(Nanos)))
        };
        let (suspended_key, mut suspended) = send(Some(100));
        let (parked_key, mut parked) = send(Some(1_000));
        let (plain_key, mut plain) = send(None);
        assert!(Pin::new(&mut suspended).poll(&mut cx).is_pending(), "engagement: suspended");
        // The replies arrive out of their send order.
        assert_eq!(FabricGate::complete(&gate, plain_key, 3, Nanos(9_000)), Delivery::Delivered);
        assert_eq!(FabricGate::complete(&gate, parked_key, 2, Nanos(1_700)), Delivery::Delivered);
        assert_eq!(FabricGate::complete(&gate, suspended_key, 1, Nanos(350)), Delivery::Delivered);
        let (count, sum) = {
            let gate = gate.borrow();
            (gate.round_trips().count(), gate.round_trips().sum())
        };
        assert_eq!(count, 2, "the two recorded hops, and not the plain one");
        assert_eq!(sum, 250 + 700, "each from its own send");
        assert_eq!(Pin::new(&mut suspended).poll(&mut cx), Poll::Ready(1));
        assert_eq!(Pin::new(&mut parked).poll(&mut cx), Poll::Ready(2));
        assert_eq!(Pin::new(&mut plain).poll(&mut cx), Poll::Ready(3));
    }

    /// The send time's stamp at its two ends, in each state that carries
    /// it. A hop sent at time 0, where the simulator's clock starts, is
    /// sampled, whether its waiter suspended first or not: a stamp that
    /// read time 0 as no send time would lose those samples without a
    /// trace. So is a hop sent at the last instant a stamp holds. A hop
    /// sent at `u64::MAX` ns has no stamp and is not sampled, as a send
    /// with no time is not.
    #[test]
    fn a_send_at_time_zero_is_sampled_and_one_past_the_last_stamp_is_not() {
        let gate = fabric_gate(5);
        let units = gate.borrow().units(5).expect("five units");
        let mut cx = Context::from_waker(Waker::noop());
        let send = |sent: Option<Nanos>| {
            let mut borrowed = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
                panic!("a unit and a holder are free");
            };
            let key = permit.key();
            (key, permit.register(&gate, sent))
        };
        let trips = || {
            let gate = gate.borrow();
            (gate.round_trips().count(), gate.round_trips().sum())
        };
        let (suspended_key, mut suspended) = send(Some(Nanos(0)));
        let (parked_key, _parked) = send(Some(Nanos(0)));
        let (last_key, _last) = send(Some(Nanos(u64::MAX - 1)));
        let (past_key, _past) = send(Some(Nanos(u64::MAX)));
        let (plain_key, _plain) = send(None);
        assert!(Pin::new(&mut suspended).poll(&mut cx).is_pending(), "engagement: suspended");
        assert_eq!(FabricGate::complete(&gate, suspended_key, 1, Nanos(350)), Delivery::Delivered);
        assert_eq!(trips(), (1, 350), "time 0 is a send time in a waiting holder");
        assert_eq!(FabricGate::complete(&gate, parked_key, 1, Nanos(350)), Delivery::Delivered);
        assert_eq!(trips(), (2, 700), "time 0 is a send time in a registered holder");
        let end = Nanos(u64::MAX);
        assert_eq!(FabricGate::complete(&gate, last_key, 1, end), Delivery::Delivered);
        assert_eq!(trips(), (3, 701), "the last stamp is a send time: its hop took 1 ns");
        for key in [past_key, plain_key] {
            assert_eq!(FabricGate::complete(&gate, key, 1, end), Delivery::Delivered);
        }
        assert_eq!(trips(), (3, 701), "no stamp holds u64::MAX ns, and no time is none");
    }

    /// Counts the wakes a parked sender receives: the room's oracle reads
    /// this waker, never the list's own state.
    #[derive(Default)]
    struct WakeCount(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: std::sync::Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A sender parked on a gate's room, polled once so its waker is stored.
    struct ParkedSender {
        room: ListWait<()>,
        wakes: std::sync::Arc<WakeCount>,
    }

    impl ParkedSender {
        fn park(gate: &RefCell<FabricGate<u64>>) -> ParkedSender {
            let wakes = std::sync::Arc::new(WakeCount::default());
            let mut sender = ParkedSender { room: gate.borrow().room(), wakes };
            assert_eq!(sender.woken(), (0, false), "engagement: the sender parks");
            sender
        }

        /// The wakes received so far, and whether the room's wait is ready.
        fn woken(&mut self) -> (usize, bool) {
            let wakes = self.wakes.0.load(std::sync::atomic::Ordering::Relaxed);
            let waker = Waker::from(std::sync::Arc::clone(&self.wakes));
            let mut cx = Context::from_waker(&waker);
            (wakes, Pin::new(&mut self.room).poll(&mut cx).is_ready())
        }
    }

    /// A list is idle exactly when it holds no waiter: the read a gated wake
    /// rests on. A wait made and not yet polled is on no list; its first poll
    /// queues it, and its drop unlinks it at once, so no entry outlives its
    /// wait. A wake that takes the last waiter makes the list idle again.
    #[test]
    fn a_list_is_idle_exactly_when_it_holds_no_waiter() {
        let list = keyed_list([7u64]);
        assert!(list.is_idle(), "a new list");
        let mut left = list.wait(7);
        assert!(list.is_idle(), "a wait not yet polled is on no list");
        assert!(!polled(&mut left), "engagement: the first poll queues it");
        assert!(!list.is_idle(), "a queued waiter");
        drop(left);
        assert_eq!(list.waiting(), 0, "engagement: the dropped waiter unlinked");
        assert!(list.is_idle(), "a dropped waiter leaves nothing behind");
        assert!(!list.wake_one(7), "no waiter to wake");
        let mut woken = list.wait(7);
        assert!(!polled(&mut woken), "engagement: queued");
        assert!(list.wake_one(7), "engagement: the waiter is woken");
        assert!(list.is_idle(), "the woken waiter left its queue");
        assert!(polled(&mut woken), "and reads its wake");
    }

    /// The gate's one wake step (ADR-0177 D4). On an idle room it wakes no
    /// one and the room stays idle. With senders parked it wakes the
    /// longest-parked one, once, and no other. A sender that left unlinked
    /// at its drop, so the next step finds the room idle and wakes no one.
    #[test]
    fn the_room_step_wakes_one_parked_sender_and_no_one_on_an_idle_room() {
        let gate = fabric_gate(1);
        let room = gate.borrow().room.clone();
        wake_room(&room);
        assert!(room.is_idle(), "an idle room stays idle");
        let mut first = ParkedSender::park(&gate);
        let mut second = ParkedSender::park(&gate);
        wake_room(&room);
        assert_eq!(first.woken(), (1, true), "room-wake-lost: the step, a sender parked");
        assert_eq!(second.woken(), (0, false), "one sender a step");
        drop(second);
        assert!(room.is_idle(), "the sender that left unlinked at its drop");
        wake_room(&room);
        assert!(room.is_idle(), "a step on the idle room changes nothing");
        assert_eq!(first.wakes.0.load(std::sync::atomic::Ordering::Relaxed), 1, "and woke no one");
    }

    /// The room's test gate: 8 holders, `taken` of them registered waits
    /// under `units`, in reserve order with their keys.
    fn taken_holders<'u>(
        gate: &'u RefCell<FabricGate<u64>>,
        units: &'u GateUnits,
        taken: usize,
    ) -> Vec<(u64, GateWait<'u, u64>)> {
        let waits: Vec<_> = (0..taken).map(|_| fabric_wait(gate, units)).collect();
        assert_eq!(gate.borrow().holders(), taken, "engagement: the holders taken");
        waits
    }

    /// A sender parked on a full gate's room is woken once by the delivered
    /// poll that frees a holder, and not by the completion that parks the
    /// value there: the holder stays until its waiter takes it (ADR-0177
    /// D4). Without the poll's wake the sender waits on a gate with room.
    #[test]
    fn a_delivered_poll_wakes_the_room_and_its_parked_value_does_not() {
        let gate = fabric_gate(8);
        let units = gate.borrow().units(8).expect("every unit");
        let mut waits = taken_holders(&gate, &units, 8);
        let mut sender = ParkedSender::park(&gate);
        let (key, mut wait) = waits.pop().expect("eight waits");
        assert_eq!(FabricGate::complete(&gate, key, 5, Nanos(1)), Delivery::Delivered);
        assert_eq!(gate.borrow().holders(), 8, "engagement: the value parks in its holder");
        assert_eq!(sender.woken(), (0, false), "a parked value frees no holder");
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(5));
        assert_eq!(gate.borrow().holders(), 7, "engagement: the poll frees the holder");
        assert_eq!(sender.woken(), (1, true), "room-wake-lost: the delivered poll");
    }

    /// A sender parked on a full gate's room is woken once by an orphaned
    /// holder's one completion, which frees it, and not by the drop that
    /// orphaned it: the slot waits for its request's completion (ADR-0177
    /// D4).
    #[test]
    fn an_orphans_completion_wakes_the_room_and_its_drop_does_not() {
        let gate = fabric_gate(8);
        let units = gate.borrow().units(8).expect("every unit");
        let mut waits = taken_holders(&gate, &units, 8);
        let mut sender = ParkedSender::park(&gate);
        let (key, wait) = waits.pop().expect("eight waits");
        drop(wait);
        assert_eq!(gate.borrow().holders(), 8, "engagement: the orphan keeps its slot");
        assert_eq!(sender.woken(), (0, false), "an orphaned holder frees nothing");
        assert_eq!(FabricGate::complete(&gate, key, 5, Nanos(1)), Delivery::Orphan);
        assert_eq!(gate.borrow().holders(), 7, "engagement: the completion frees the orphan");
        assert_eq!(sender.woken(), (1, true), "room-wake-lost: the orphan's completion");
    }

    /// A sender parked on a full gate's room is woken once by the drop of a
    /// wait whose value had arrived: that drop frees the holder at once.
    #[test]
    fn a_dropped_wait_that_held_its_value_wakes_the_room() {
        let gate = fabric_gate(8);
        let units = gate.borrow().units(8).expect("every unit");
        let mut waits = taken_holders(&gate, &units, 8);
        let mut sender = ParkedSender::park(&gate);
        let (key, wait) = waits.pop().expect("eight waits");
        assert_eq!(FabricGate::complete(&gate, key, 5, Nanos(1)), Delivery::Delivered);
        assert_eq!(sender.woken(), (0, false), "a parked value frees no holder");
        drop(wait);
        assert_eq!(gate.borrow().holders(), 7, "engagement: the drop frees the holder");
        assert_eq!(sender.woken(), (1, true), "room-wake-lost: the delivered wait's drop");
    }

    /// A sender parked on a gate's room is woken once by the drop of a permit
    /// that took the last holder and never registered: its slot returns at
    /// the drop, free at its next generation, and its unit with it (ADR-0194
    /// A2, ADR-0177 D4). The sender parks first, since a permit lives only
    /// inside its owner's borrow of the gate. The next permit reads the
    /// slot: the same index, one generation on, under the returned unit.
    #[test]
    fn a_dropped_unregistered_permit_wakes_the_room() {
        const INDEX: u64 = (1 << GATE_KEY_INDEX_BITS) - 1;
        let gate = fabric_gate(8);
        let units = gate.borrow().units(8).expect("every unit");
        let _waits = taken_holders(&gate, &units, 7);
        let mut sender = ParkedSender::park(&gate);
        let dropped = {
            let mut borrowed = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
                panic!("the last holder and unit are free");
            };
            assert_eq!(units.free(), 0, "engagement: the permit holds the last unit");
            let key = permit.key();
            drop(permit);
            key
        };
        assert_eq!(gate.borrow().holders(), 7, "engagement: the permit's slot returns");
        assert_eq!(units.free(), 1, "the dropped permit's unit returns");
        assert_eq!(sender.woken(), (1, true), "room-wake-lost: the unregistered permit's drop");
        let mut borrowed = gate.borrow_mut();
        let FabricAdmission::Granted(next) = borrowed.reserve(&units) else {
            panic!("the returned holder and unit are free");
        };
        assert_eq!(next.key() & INDEX, dropped & INDEX, "engagement: the dropped permit's slot");
        assert_eq!(
            next.key() - dropped,
            1 << GATE_KEY_INDEX_BITS,
            "the dropped permit left its slot free at the next generation"
        );
    }

    /// A keyed gate of `cap` on a census of its own.
    fn keyed_gate<V>(cap: Cap) -> KeyedGate<u64, V> {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(0));
        KeyedGate::build(cap, &census).expect("the test gate")
    }

    /// 1,000 keyed waits, each polled once, so its waker is stored, then
    /// dropped before any value. A dropped wait takes its route in the same
    /// step (ADR-0177 D4), so the hold reads no wait, the gate keeps no route
    /// and a gate of 8 never fills. Planted, the drop leaves the route and
    /// the holder with its waker.
    #[test]
    fn a_dropped_keyed_wait_leaves_no_route() {
        const WAITS: u64 = 1_000;
        let gate: KeyedGate<u64, u64> =
            keyed_gate(Cap::entries::<1_024>("test-keyed-holders", CapFill::Serving));
        let mut cx = Context::from_waker(Waker::noop());
        for key in 0..WAITS {
            let KeyedAdmission::Granted(permit) = gate.reserve(key) else {
                panic!("route-kept-on-drop: key {key} found no room after {key} dropped waits");
            };
            let (mut wait, hold) = permit.register_held();
            assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "engagement: the waker stored");
            drop(wait);
            assert!(!gate.waiting(&hold), "route-kept-on-drop: key {key}'s dropped wait waits");
            gate.release(hold);
        }
        assert_eq!(gate.routes(), 0, "route-kept-on-drop: routes after {WAITS} dropped waits");
        assert_eq!(gate.pending(), 0, "route-kept-on-drop: holders after {WAITS} dropped waits");
        let small: KeyedGate<u64, u64> =
            keyed_gate(Cap::entries::<8>("test-keyed-holders", CapFill::Serving));
        for key in 0..WAITS {
            if let KeyedAdmission::Granted(permit) = small.reserve(key) {
                let mut wait = permit.register();
                assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "engagement: stored");
            }
        }
        assert_eq!(small.full(), 0, "route-kept-on-drop: cold_gate_full at a cap of 8");
    }

    /// A model holder: what its wait awaits, whether a hold pins it.
    #[derive(Debug)]
    struct ModelHolder {
        state: ModelState,
        held: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ModelState {
        Awaiting,
        Delivered(u64),
        Orphaned,
    }

    /// The keyed gate's table (ADR-0149 D4 with ADR-0177 D4's hold) as a
    /// model of its own: holders by id, routes by key, no code shared.
    #[derive(Default)]
    struct Model {
        holders: BTreeMap<u32, ModelHolder>,
        routes: BTreeMap<u64, u32>,
        next: u32,
        full: u64,
    }

    impl Model {
        fn free(&mut self, id: u32) {
            self.holders.remove(&id);
        }

        /// A wait leaves: its route if it awaits, its holder unless held.
        fn drop_wait(&mut self, id: u32, key: u64) {
            let Some(holder) = self.holders.get_mut(&id) else { return };
            if holder.state == ModelState::Awaiting {
                self.routes.remove(&key);
            }
            holder.state = ModelState::Orphaned;
            if !holder.held {
                self.free(id);
            }
        }
    }

    /// Seeded operations on a keyed gate of 8 over 12 keys, against
    /// [`Model`]: every answer, and after every step the holders and routes
    /// held, equal the model's. A wait dropped under its hold keeps its
    /// slot until the release (ADR-0177 D4). Planted, that drop frees the
    /// slot, so the gate holds one slot fewer than the model, and the old
    /// hold could read the slot's next occupant. A granted permit
    /// dropped unregistered takes back the route and holder `reserve`
    /// published: the model holds neither.
    #[test]
    fn the_keyed_gate_holds_what_its_model_holds() {
        const CAP: usize = 8;
        const KEYS: u64 = 12;
        let gate: KeyedGate<u64, u64> =
            keyed_gate(Cap::entries::<{ CAP as u32 }>("test-keyed-holders", CapFill::Serving));
        let mut model = Model::default();
        let mut waits: Vec<(KeyedWait<u64, u64>, u32, u64)> = Vec::new();
        let mut holds: Vec<(GateHold, u32)> = Vec::new();
        let mut rng = inf_foundation::rng::SplitMix64::new(0x6B65_7965_6400_0013);
        let mut cx = Context::from_waker(Waker::noop());
        let mut held_drops = 0u32;
        let mut permit_drops = 0u32;
        for step in 0..20_000u32 {
            let pick = |rng: &mut inf_foundation::rng::SplitMix64, n: usize| {
                usize::try_from(rng.next_u64() % n as u64).expect("a small index")
            };
            match rng.next_u64() % 6 {
                0 => {
                    let key = rng.next_u64() % KEYS;
                    let held = rng.next_u64() & 1 == 1;
                    let unregistered = rng.next_u64().is_multiple_of(4);
                    let expected = if model.routes.contains_key(&key) {
                        "duplicate"
                    } else if model.holders.len() == CAP {
                        model.full += 1;
                        "full"
                    } else {
                        "granted"
                    };
                    match gate.reserve(key) {
                        KeyedAdmission::Granted(permit) if unregistered => {
                            assert_eq!(expected, "granted", "step {step}: key {key} granted");
                            drop(permit);
                            permit_drops += 1;
                        }
                        KeyedAdmission::Granted(permit) => {
                            assert_eq!(expected, "granted", "step {step}: key {key} granted");
                            let id = model.next;
                            model.next += 1;
                            model
                                .holders
                                .insert(id, ModelHolder { state: ModelState::Awaiting, held });
                            model.routes.insert(key, id);
                            if held {
                                let (wait, hold) = permit.register_held();
                                waits.push((wait, id, key));
                                holds.push((hold, id));
                            } else {
                                waits.push((permit.register(), id, key));
                            }
                        }
                        KeyedAdmission::Duplicate => {
                            assert_eq!(expected, "duplicate", "step {step}: key {key} a duplicate");
                        }
                        KeyedAdmission::Full => {
                            assert_eq!(expected, "full", "step {step}: key {key} full");
                        }
                    }
                }
                1 => {
                    let key = rng.next_u64() % KEYS;
                    let value = rng.next_u64();
                    let expected = match model.routes.remove(&key) {
                        Some(id) => {
                            let holder = model.holders.get_mut(&id).expect("a route's holder");
                            holder.state = ModelState::Delivered(value);
                            Delivery::Delivered
                        }
                        None => Delivery::Orphan,
                    };
                    assert_eq!(gate.complete(key, value), expected, "step {step}: key {key}");
                }
                2 if !waits.is_empty() => {
                    let at = pick(&mut rng, waits.len());
                    let id = waits[at].1;
                    let polled = Pin::new(&mut waits[at].0).poll(&mut cx);
                    let holder = model.holders.get_mut(&id).expect("a live wait's holder");
                    match core::mem::replace(&mut holder.state, ModelState::Orphaned) {
                        ModelState::Delivered(value) => {
                            assert_eq!(polled, Poll::Ready(value), "step {step}: delivered");
                            if !holder.held {
                                model.free(id);
                            }
                            drop(waits.swap_remove(at));
                        }
                        state => {
                            assert_eq!(polled, Poll::Pending, "step {step}: {state:?}");
                            holder.state = state;
                        }
                    }
                }
                3 if !waits.is_empty() => {
                    let at = pick(&mut rng, waits.len());
                    let (wait, id, key) = waits.swap_remove(at);
                    if model.holders.get(&id).is_some_and(|holder| holder.held) {
                        held_drops += 1;
                    }
                    drop(wait);
                    model.drop_wait(id, key);
                }
                4 if !holds.is_empty() => {
                    let (hold, id) = holds.swap_remove(pick(&mut rng, holds.len()));
                    gate.release(hold);
                    let holder = model.holders.get_mut(&id).expect("a hold's holder");
                    holder.held = false;
                    if holder.state == ModelState::Orphaned {
                        model.free(id);
                    }
                }
                5 if !holds.is_empty() => {
                    let (hold, id) = &holds[pick(&mut rng, holds.len())];
                    let awaits = model.holders[id].state == ModelState::Awaiting;
                    assert_eq!(gate.waiting(hold), awaits, "step {step}: holder {id} waiting");
                }
                _ => {}
            }
            assert_eq!(gate.pending(), model.holders.len(), "slots-held: step {step}");
            assert_eq!(gate.routes(), model.routes.len(), "routes-held: step {step}");
            assert_eq!(gate.full(), model.full, "full-counted: step {step}");
        }
        assert!(held_drops > 0, "VACUOUS: no wait was dropped under its hold");
        assert!(permit_drops > 0, "VACUOUS: no granted permit was dropped");
        assert!(model.full > 0, "VACUOUS: the gate never filled");
    }

    /// 1,000 waits on one key, each polled once, so it is queued on its node
    /// with its waker stored, then dropped before any wake; then 1,000 more,
    /// each the head a wake reaches and a poll reads, with the next queued
    /// behind it; then one woken and dropped unread. A wait's node is its
    /// future's, so a drop while queued unlinks it and gives the node back, a
    /// poll that reads its wake gives it back, and a drop after an unread
    /// wake passes the wake to the key's next wait (ADR-0177 D5): the pool is
    /// back at no node, no first poll starved, and the baton reached the wait
    /// behind.
    #[test]
    fn a_wait_dropped_while_queued_leaves_no_entry() {
        const WAITS: usize = 1_000;
        let (list, nodes) = pooled_list([7u32]);
        for _ in 0..WAITS {
            let mut wait = list.wait(7);
            assert!(!polled(&mut wait), "engagement: queued");
            drop(wait);
        }
        assert_eq!(nodes.len(), 0, "wait-node-kept: a dropped wait's node is not the pool's");
        assert_eq!(nodes.full(), 0, "wait-node-kept: a first poll found the pool empty");
        assert_eq!(list.waiting(), 0, "no dropped wait is a live waiter");
        let mut behind = list.wait(7);
        assert!(!polled(&mut behind), "engagement: a wait behind the woken ones");
        for _ in 0..WAITS {
            let mut woken = list.wait(7);
            assert!(!polled(&mut woken), "engagement: queued");
            // `behind` is the head: the wake reaches it and its poll reads
            // it, and the new wait is the head after it, for the next pass.
            assert!(list.wake_one(7), "engagement: the head is woken");
            assert!(polled(&mut behind), "the head's wake landed");
            behind = woken;
        }
        drop(behind);
        assert_eq!((list.waiting(), nodes.len()), (0, 0), "every node back in the pool");
        let mut first = list.wait(7);
        let mut second = list.wait(7);
        assert!(!polled(&mut first) && !polled(&mut second), "engagement: two queued");
        assert!(list.wake_one(7), "engagement: the first is woken");
        drop(first);
        assert!(polled(&mut second), "the unread wake passed to the next wait, once");
        assert_eq!((list.waiting(), nodes.len()), (0, 0), "the baton took no node");
    }

    /// The pool is one capped slab of its cap's nodes, its row on the census
    /// and every slot built before anything serves, and it holds nodes as a
    /// model of its live handles does: a seeded run of takes and releases,
    /// a take answered by a free slot below the cap and refused at it, a
    /// release by any live handle, and the pool's `len` the model's count
    /// after every step. Oracle: the model's own vector; the census row
    /// reads the cap's crossings and its high water.
    #[test]
    fn the_pool_holds_its_nodes_as_its_model_does() {
        const NODES: u32 = 100;
        const POOL: Cap = Cap::entries::<NODES>("test-pool-model", CapFill::Serving);
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let pool = WaitNodes::build(POOL, &census).expect("the test pool").nodes;
        let row = census.row("test-pool-model").expect("the pool's census row");
        assert_eq!((row.entries_max, row.capacity_now), (NODES, NODES), "built whole");
        assert_eq!((pool.len(), pool.full()), (0, 0), "a new pool holds no node");
        let mut live: Vec<NodeId> = Vec::new();
        let mut rng = inf_foundation::rng::SplitMix64::new(0x706F_6F6C_0000_00E4);
        let mut refused = 0u32;
        for _ in 0..10_000 {
            let mut nodes = pool.pool.nodes.borrow_mut();
            // Two takes for each release, so the run reaches the cap.
            if !rng.next_u64().is_multiple_of(3) {
                match nodes.reserve() {
                    SlabReserved::Slot(slot) => {
                        let node = WaitNode {
                            waker: None,
                            prev: None,
                            next: None,
                            position: 0,
                            state: NodeState::Queued,
                        };
                        live.push(slot.publish(node));
                    }
                    SlabReserved::Full => {
                        assert_eq!(live.len(), NODES as usize, "refused only at the cap");
                        refused += 1;
                    }
                    SlabReserved::Exhausted => panic!("a wrapping slab never retires a slot"),
                }
            } else if !live.is_empty() {
                let at = usize::try_from(rng.next_u64() % live.len() as u64).expect("an index");
                let id = live.swap_remove(at);
                assert_eq!(nodes.release(id), inf_foundation::bounded::Released::Dropped);
            }
            drop(nodes);
            assert_eq!(pool.len() as usize, live.len(), "the pool's len is the model's count");
        }
        assert!(refused > 0, "engagement: the run reached the cap");
        let row = census.row("test-pool-model").expect("the pool's census row");
        assert_eq!(row.high_water, NODES, "the high water is the cap");
        assert_eq!(row.crossings, u64::from(refused), "each refusal is one crossing");
        assert_eq!(pool.full(), 0, "no first poll ran, so none starved");
    }

    /// The pool builds its gate's entry array in its own steps, after its slab, and hands it to
    /// its seat whole, which a gate built on the seat keeps: one entry a node of the cap at one
    /// node, at one full chunk (2,730 entries of 24 B in 64 KiB), one past it (a directory and
    /// two chunks) and at the default node's 2,825, each inside its steps' bound. The slab is
    /// as a pool with no array leaves it: its row whole at the cap, with no high water and no
    /// crossing, no node held and no first poll counted, so minting the fill took no node; the
    /// build's own check holds that the fill names none. Oracle: the cap's count and the
    /// census row.
    #[test]
    fn the_pool_builds_its_gates_entry_array_in_its_own_steps() {
        const ONE: Cap = Cap::entries::<1>("test-entries-one", CapFill::Serving);
        const CHUNK: Cap = Cap::entries::<2_730>("test-entries-chunk", CapFill::Serving);
        const PAST_CHUNK: Cap = Cap::entries::<2_731>("test-entries-past", CapFill::Serving);
        const DEFAULT: Cap = Cap::entries::<2_825>("test-entries-default", CapFill::Serving);
        assert_eq!(WatermarkGate::ENTRY_BYTES, 24, "an entry is the 24 B the bill counts");
        for cap in [ONE, CHUNK, PAST_CHUNK, DEFAULT] {
            let (name, entries) = (cap.name(), cap.entries_max());
            let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
            let WaitPool { nodes, seat } = WaitNodes::build(cap, &census)
                .unwrap_or_else(|refused| panic!("{name}: the pool refused: {refused}"));
            assert_eq!(seat.entries.capacity(), entries, "{name}: one entry a node");
            let row = census.row(name).expect("the pool's census row");
            assert_eq!((row.entries_max, row.capacity_now), (entries, entries), "{name}: whole");
            assert_eq!((row.high_water, row.crossings), (0, 0), "{name}: the fill took no node");
            assert_eq!((nodes.len(), nodes.full()), (0, 0), "{name}: no node held or starved");
            let gate = WatermarkGate::build(seat);
            assert_eq!(gate.inner.entries.capacity(), entries, "{name}: the gate keeps the array");
            assert_eq!(gate.waiting(), 0, "{name}: nothing queued");
        }
    }

    /// A whole-list wake wakes every queued wait of its key, and the pool bounds them: with
    /// `T − 1` and `T` waits each takes a node and each is woken; the wait past `T` takes no
    /// node, its first poll counted (`wait_nodes_full`) and its task woken by itself, and the
    /// wake reaches the `T` that hold one.
    #[test]
    fn a_whole_list_wake_wakes_every_queued_wait_and_no_more_than_the_pool_holds() {
        const T: u32 = 8;
        const POOL: Cap = Cap::entries::<T>("test-pool-t", CapFill::Serving);
        for waits in [T - 1, T, T + 1] {
            let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
            let nodes = WaitNodes::build(POOL, &census).expect("the test pool").nodes;
            let list = WaitList::keyed(&nodes, Some(TEST_LIST_KEYS), &census, [7u32])
                .expect("the test list");
            let mut cx = Context::from_waker(Waker::noop());
            let mut parked: Vec<ListWait<u32>> = (0..waits).map(|_| list.wait(7)).collect();
            for wait in &mut parked {
                assert!(Pin::new(wait).poll(&mut cx).is_pending(), "a first poll parks");
            }
            let queued = waits.min(T);
            assert_eq!(
                list.wake_all(7),
                queued as usize,
                "{waits} waits: the wake reaches {queued}"
            );
            assert_eq!(nodes.full(), u64::from(waits - queued), "{waits} waits: starved past T");
            let mut read = 0;
            for wait in &mut parked {
                read += usize::from(Pin::new(wait).poll(&mut cx).is_ready());
            }
            assert_eq!(read, waits as usize, "{waits} waits: every wait resolves");
            assert_eq!(nodes.len(), 0, "{waits} waits: every node back in the pool");
        }
    }

    thread_local! {
        /// What a re-entrant waker runs when woken: the test's own step, on
        /// the cell's thread, which a `Send` waker cannot hold itself.
        static ON_WAKE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    /// A waker whose wake runs [`ON_WAKE`]'s step, inside the list's call.
    struct Reentrant;

    impl std::task::Wake for Reentrant {
        fn wake(self: std::sync::Arc<Self>) {
            ON_WAKE.with(|step| {
                if let Some(step) = step.borrow_mut().as_mut() {
                    step();
                }
            });
        }
    }

    /// One poll of `wait` on the re-entrant waker: whether it is Ready.
    fn polled_reentrant<K: ListKey>(wait: &mut ListWait<K>) -> bool {
        let waker = Waker::from(std::sync::Arc::new(Reentrant));
        Pin::new(wait).poll(&mut Context::from_waker(&waker)).is_ready()
    }

    /// A waker that reaches the pool inside `wake`: each wake
    /// first-polls a new wait on another list of the same pool, at
    /// `wake_one` and at `wake_all`. The wake runs after the pool's borrow
    /// has ended, so no borrow panics and each new wait queues on its list.
    #[test]
    fn a_waker_that_reaches_the_pool_inside_its_wake_finds_it_free() {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let nodes = wait_nodes(&census);
        let list = WaitList::keyed(&nodes, Some(TEST_LIST_KEYS), &census, [7u32]).expect("list");
        let other = Rc::new(WaitList::unit(&nodes));
        let made: Rc<RefCell<Vec<ListWait<()>>>> = Rc::new(RefCell::new(Vec::new()));
        let (step_other, step_made) = (Rc::clone(&other), Rc::clone(&made));
        ON_WAKE.with(|step| {
            *step.borrow_mut() = Some(Box::new(move || {
                let mut wait = step_other.wait(());
                assert!(!polled(&mut wait), "the new wait queues");
                step_made.borrow_mut().push(wait);
            }));
        });
        let mut first = list.wait(7);
        let mut second = list.wait(7);
        let mut third = list.wait(7);
        for wait in [&mut first, &mut second, &mut third] {
            assert!(!polled_reentrant(wait), "engagement: queued on the re-entrant waker");
        }
        assert!(list.wake_one(7), "wake_one ran its waker");
        assert_eq!(list.wake_all(7), 2, "wake_all ran both");
        ON_WAKE.with(|step| step.borrow_mut().take());
        assert_eq!(other.waiting(), 3, "each waker's new wait queued on its list");
        assert_eq!(nodes.len(), 6, "three woken and three queued, each on its node");
    }

    /// A waker that waits again on its own key inside `wake_all`: two waits on
    /// one key, each waker making and polling a new wait on that key. The
    /// call wakes the two its key held as it began, and the two new waits
    /// stay queued.
    #[test]
    fn a_whole_list_wake_wakes_the_count_its_key_held_as_it_began() {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let nodes = wait_nodes(&census);
        let list =
            Rc::new(WaitList::keyed(&nodes, Some(TEST_LIST_KEYS), &census, [7u32]).expect("list"));
        let made: Rc<RefCell<Vec<ListWait<u32>>>> = Rc::new(RefCell::new(Vec::new()));
        let (step_list, step_made) = (Rc::clone(&list), Rc::clone(&made));
        ON_WAKE.with(|step| {
            *step.borrow_mut() = Some(Box::new(move || {
                let mut wait = step_list.wait(7);
                assert!(!polled(&mut wait), "the new wait queues behind");
                step_made.borrow_mut().push(wait);
            }));
        });
        let mut first = list.wait(7);
        let mut second = list.wait(7);
        assert!(!polled_reentrant(&mut first) && !polled_reentrant(&mut second), "queued");
        assert_eq!(list.wake_all(7), 2, "the count at its entry, not the waits it made");
        ON_WAKE.with(|step| step.borrow_mut().take());
        assert_eq!(list.waiting(), 2, "the two new waits stay queued");
        assert!(polled(&mut first) && polled(&mut second), "the two read their wakes");
        assert_eq!(made.borrow().len(), 2, "engagement: each waker waited again");
    }

    /// `is_idle` takes no borrow of the pool and walks nothing: called under
    /// a held `borrow_mut` of the pool, and the room's
    /// wake step on an idle room under it, neither panics, and each answers
    /// what the count gives.
    #[test]
    fn is_idle_reads_the_count_under_a_held_borrow_of_the_pool() {
        let gate = fabric_gate(1);
        let room = gate.borrow().room.clone();
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let nodes = wait_nodes(&census);
        let list = WaitList::keyed(&nodes, Some(TEST_LIST_KEYS), &census, [7u32]).expect("list");
        let mut queued = list.wait(7);
        assert!(!polled(&mut queued), "engagement: one wait queued");
        let held = nodes.pool.nodes.borrow_mut();
        assert!(!list.is_idle(), "the count, read under the held borrow");
        drop(held);
        let pool = Rc::clone(&room.list.pool);
        let held = pool.nodes.borrow_mut();
        assert!(room.is_idle(), "the idle room, read under the held borrow");
        wake_room(&room);
        drop(held);
    }

    /// A wait made and dropped before its first poll takes no node and moves
    /// no count; a wait woken and dropped unread passes its wake to the next
    /// one, once, so the wait behind that one stays queued; 1,000 of each on
    /// one key leave the pool empty, and a wait polled after its Ready answers
    /// Ready again, with no node and no count.
    #[test]
    fn unpolled_and_unread_drops_and_a_poll_after_ready_take_no_node() {
        const WAITS: usize = 1_000;
        let (list, nodes) = pooled_list([7u32]);
        for _ in 0..WAITS {
            drop(list.wait(7));
        }
        assert_eq!((list.waiting(), nodes.len(), nodes.full()), (0, 0, 0), "nothing was taken");
        let mut batons = 0;
        for _ in 0..WAITS {
            let mut woken = list.wait(7);
            let mut next = list.wait(7);
            let mut last = list.wait(7);
            assert!(
                !polled(&mut woken) && !polled(&mut next) && !polled(&mut last),
                "engagement: three queued"
            );
            assert!(list.wake_one(7), "engagement: the head is woken");
            drop(woken);
            assert_eq!(list.waiting(), 1, "the baton woke one wait behind, not two");
            batons += usize::from(polled(&mut next));
            assert!(polled(&mut next), "a poll after Ready is Ready again");
            drop(last);
        }
        assert_eq!(batons, WAITS, "each unread wake passed once");
        assert_eq!((list.waiting(), nodes.len(), nodes.full()), (0, 0, 0), "the pool is empty");
    }

    /// A keyed list built with no map holds no key: its build with a key is
    /// refused past a cap of none, naming the keyless row, and with none it
    /// builds, and a wait on any key takes no node: its first poll starves
    /// (`wait_nodes_full`) and its next is Ready with no wake, which
    /// `starved` reads.
    #[test]
    fn a_keyless_list_refuses_a_key_and_starves_every_wait() {
        let (census, _mark) = CapCensus::new(inf_foundation::CellId(1));
        let nodes = wait_nodes(&census);
        let refused = WaitList::keyed(&nodes, None, &census, [1u32]).map(drop);
        let keyless = AssemblyFull::new(crate::limits::WAIT_LIST_KEYLESS);
        assert_eq!(refused, Err(KeysRefused::Full(keyless)), "a key past a cap of none");
        let list = WaitList::<u32>::keyed(&nodes, None, &census, []).expect("no key builds");
        let mut wait = list.wait(1);
        assert!(!polled(&mut wait), "the first poll starves");
        assert!(wait.starved(), "and says so");
        assert_eq!((nodes.len(), nodes.full(), list.waiting()), (0, 1, 0), "no node, counted");
        assert!(polled(&mut wait), "the next poll is Ready with no wake");
    }
}
