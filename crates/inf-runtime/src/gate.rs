//! Typed suspension primitives — the only ways a cell future may suspend
//! (ADR-0003). Each is a thin shape over one cell-local pattern: a future
//! registers its waker under a key; the event source completes the key and
//! wakes exactly the right task(s). No atomics, no locks (L1).
//!
//! - [`FabricGate`]: fabric replies. The gate mints each key, and a task's
//!   [`GateUnits`] bound the holders it has out (ADR-0177 D4).
//! - [`KeyedGate`] and [`IoGate`]: completions keyed by the caller's key (the
//!   cold gate's logical id, a driver token), behind one holder each.
//! - [`WaitList`]: key-keyed FIFO (M1 blocking ops: BLPOP, WAIT…).
//! - [`WatermarkGate`]: LSN-keyed (M2 fsync-gated acks).
//!
//! A holder wakes exactly one task. Completion may arrive before the waiter
//! first polls (same-iteration delivery): the value parks in the holder and
//! the first poll returns `Ready` without suspending. Every waiter future is
//! cancellation-safe: dropping one leaves no waker behind, and a value that
//! arrives for no waiter goes to its gate's [`Cleanup`].

use core::cell::{Cell, RefCell, RefMut};
use core::future::Future;
use core::hash::Hash;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
#[allow(clippy::disallowed_types, reason = "container: E")]
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

// Gate tables key by internally-generated integer tokens (fabric tokens,
// completion tokens, cell ids) — never attacker-chosen input — so they use
// the trusted-integer hasher: one folded multiply per lookup instead of
// SipHash rounds. On the remote hot path every fabric op pays three table
// operations (register, complete, waiter poll); the S21 cycle split
// attributed ~2.2% of natural-leg cycles to `DefaultHasher` (M2.5 Phase H).
use inf_foundation::BuildIntHasher;
use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapError, CappedSlab, LookupMut, NamedReservation, Retiring,
    SlabBuilder, SlabReserved, SlotId, SlotWord, Taken, WordLookup, WrapWithProof,
};
use inf_foundation::limits::{GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS};

use crate::driver::CompletionResult;
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

/// A holder's state (ADR-0177 D4). One holder implementation serves both
/// gates: the fabric gate reaches it by the key it minted, a keyed gate by
/// its caller's key.
enum HolderState<V> {
    /// Registered; the waiter has not suspended yet.
    Registered,
    /// The waiter suspended: wake this when the value arrives.
    Waiting(Waker),
    /// The value arrived before the waiter took it.
    Delivered(V),
    /// The waiter is gone and the slot still has a claim on it: a fabric
    /// request not yet answered, or a keyed queue hold not yet released.
    Orphaned,
}

/// One holder: its state, and whether a queue's [`GateHold`] pins it.
struct Holder<V> {
    state: HolderState<V>,
    held: bool,
}

impl<V> Holder<V> {
    const fn registered(held: bool) -> Holder<V> {
        Holder { state: HolderState::Registered, held }
    }
}

/// A gate's holders by key word, until its holder slab replaces the map.
#[allow(clippy::disallowed_types, reason = "container: E")]
struct HolderMap<V> {
    map: HashMap<u64, Holder<V>, BuildIntHasher>,
}

impl<V> Default for HolderMap<V> {
    fn default() -> Self {
        HolderMap { map: Default::default() }
    }
}

/// The `Future` contract: a waiter is not polled after it was `Ready`.
#[cold]
fn polled_after_completion() -> ! {
    panic!("GateWait polled after completion")
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
        let free = self.free.get();
        if free == 0 {
            return false;
        }
        self.free.set(free - 1);
        true
    }

    fn give(&self) {
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

/// The fabric key's adapter: a holder's index and its generation's low
/// bits, built from the gate's own slab (ADR-0194 D6 with A2).
type GateWord = SlotWord<u32, WrapWithProof, GATE_KEY_INDEX_BITS, GATE_KEY_GENERATION_BITS>;

/// A fabric gate's holder slab and the one adapter its keys pass through.
struct FabricHolders<V> {
    slab: HolderSlab<V>,
    word: GateWord,
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
    /// (ADR-0177 D4); `None` builds no holder at a node of one cell. Built
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
    ) -> Result<FabricGate<V, C>, CapError> {
        let holders = match holders {
            None => None,
            Some(cap) => {
                debug_assert!(cap.entries_max() >= units_max, "H counts every unit");
                let slab = build_holders(CappedSlab::builder(cap, census)?)?;
                let word = GateWord::for_slab(&slab)?;
                Some(FabricHolders { slab, word })
            }
        };
        Ok(FabricGate {
            holders,
            high: (u64::from(origin) << GATE_KEY_ORIGIN_SHIFT) | (1 << GATE_KEY_TAG_BIT),
            units: Rc::new(UnitBook { issued: Cell::new(0), max: units_max }),
            stale: 0,
            full: 0,
            room: WaitList::new(),
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
        let reservation =
            match holders.as_mut().map(|holders| (holders.slab.reserve(), &holders.word)) {
                Some((SlabReserved::Slot(slot), word)) => (slot.named(), word),
                Some((SlabReserved::Full | SlabReserved::Exhausted, _)) | None => {
                    units.give();
                    *full += 1;
                    return FabricAdmission::Full;
                }
            };
        let (reservation, word) = reservation;
        let Some(fields) = word.encode(reservation.id()) else {
            unreachable!("the gate's adapter is built from its own slab")
        };
        FabricAdmission::Granted(GatePermit {
            reservation: Some(reservation),
            room,
            units,
            key: *high | fields,
            _cleanup: PhantomData,
        })
    }

    /// Delivers `value` to the holder `key` names, waking its waiter if it is
    /// suspended. A key of another origin, without the tag, of no live
    /// holder, or of a holder already delivered gives `value` to the cleanup
    /// and counts `stale`; an orphaned holder's one completion frees it.
    pub fn complete(gate: &RefCell<Self>, key: u64, value: V) -> Delivery {
        let (delivery, waker, orphan) = gate.borrow_mut().settle(key, value);
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
    fn settle(&mut self, key: u64, value: V) -> (Delivery, Option<Waker>, Option<V>) {
        let FabricGate { holders, high, stale, room, .. } = self;
        let own = key & !GATE_KEY_FIELDS == *high;
        let Some(FabricHolders { slab, word }) = holders.as_mut().filter(|_| own) else {
            *stale += 1;
            return (Delivery::Orphan, None, Some(value));
        };
        let (id, holder) = match slab.get_mut_by_word(word, key & GATE_KEY_FIELDS) {
            WordLookup::Live(id, holder) => (id, holder),
            WordLookup::Stale | WordLookup::Vacant => {
                *stale += 1;
                return (Delivery::Orphan, None, Some(value));
            }
        };
        match &holder.state {
            HolderState::Registered | HolderState::Waiting(_) => {}
            // A second completion: the parked value stays the waiter's.
            HolderState::Delivered(_) => {
                *stale += 1;
                return (Delivery::Orphan, None, Some(value));
            }
            // The waiter was dropped: this is its request's one completion.
            HolderState::Orphaned => {
                let _freed = slab.take(id);
                room.wake_one(());
                return (Delivery::Orphan, None, Some(value));
            }
        }
        match core::mem::replace(&mut holder.state, HolderState::Delivered(value)) {
            HolderState::Waiting(waker) => (Delivery::Delivered, Some(waker), None),
            HolderState::Registered | HolderState::Delivered(_) | HolderState::Orphaned => {
                (Delivery::Delivered, None, None)
            }
        }
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
}

/// Steps a holder slab's build to its end; a refused build retires its
/// prefix by steps before it answers.
fn build_holders<V>(
    mut builder: SlabBuilder<Holder<V>, u32, WrapWithProof>,
) -> Result<HolderSlab<V>, CapError> {
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

/// A dropped wait's holder: orphaned until its request's one completion
/// frees it, or freed now with the value that completion parked. Returns
/// the waker it stored and that value, both dropped outside the gate's
/// borrow.
fn orphan<V>(
    slab: &mut HolderSlab<V>,
    room: &WaitList<()>,
    slot: SlotId<u32>,
) -> (Option<Waker>, Option<V>) {
    let LookupMut::Live(holder) = slab.get_mut(slot) else { return (None, None) };
    let prior = core::mem::replace(&mut holder.state, HolderState::Orphaned);
    match prior {
        HolderState::Delivered(value) => {
            let _freed = slab.take(slot);
            room.wake_one(());
            (None, Some(value))
        }
        HolderState::Waiting(waker) => (Some(waker), None),
        HolderState::Registered | HolderState::Orphaned => (None, None),
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
pub struct GatePermit<'g, 'u, V, C: Cleanup<V> = DropValue> {
    /// The holder's slot until the permit registers or drops.
    reservation: Option<NamedReservation<'g, Holder<V>, u32>>,
    room: &'g WaitList<()>,
    units: &'u GateUnits,
    key: u64,
    _cleanup: PhantomData<fn(V) -> C>,
}

impl<'u, V, C: Cleanup<V>> GatePermit<'_, 'u, V, C> {
    /// The word the request carries: the fabric token it is sent under.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Publishes the holder once the request was sent: the key is routable.
    /// `gate` is the cell this permit's gate lives in.
    pub fn register(mut self, gate: &'u RefCell<FabricGate<V, C>>) -> GateWait<'u, V, C> {
        let Some(reservation) = self.reservation.take() else {
            unreachable!("a permit registers once, consuming itself")
        };
        let slot = reservation.publish(Holder::registered(false));
        GateWait { gate, units: self.units, slot, done: false }
    }
}

impl<V, C: Cleanup<V>> Drop for GatePermit<'_, '_, V, C> {
    fn drop(&mut self) {
        // Never registered (the send was refused): the named reservation's
        // drop advances its slot's generation, so the key names no later
        // occupant, and the holder and the unit return.
        if let Some(reservation) = self.reservation.take() {
            drop(reservation);
            self.units.give();
            self.room.wake_one(());
        }
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
        let Some(FabricHolders { slab, .. }) = holders.as_mut() else { polled_after_completion() };
        let LookupMut::Live(holder) = slab.get_mut(this.slot) else { polled_after_completion() };
        match &holder.state {
            HolderState::Delivered(_) => {}
            HolderState::Registered | HolderState::Waiting(_) => {
                holder.state = HolderState::Waiting(cx.waker().clone());
                return Poll::Pending;
            }
            HolderState::Orphaned => polled_after_completion(),
        }
        let Taken::Value(Holder { state: HolderState::Delivered(value), .. }) =
            slab.take(this.slot)
        else {
            polled_after_completion()
        };
        room.wake_one(());
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
            Some(FabricHolders { slab, .. }) => orphan(slab, room, self.slot),
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

struct KeyedCore<V> {
    holders: HolderMap<V>,
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
    /// Every holder is taken.
    Full,
}

impl<K: GateKey, V, C: Cleanup<V>> Default for KeyedGate<K, V, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: GateKey, V, C: Cleanup<V>> KeyedGate<K, V, C> {
    pub fn new() -> KeyedGate<K, V, C> {
        KeyedGate {
            core: Rc::new(RefCell::new(KeyedCore { holders: HolderMap::default() })),
            _key: PhantomData,
        }
    }

    /// Reserves `key`'s route and a holder. The permit holds the gate until
    /// it registers, one synchronous step.
    pub fn reserve(&self, key: K) -> KeyedAdmission<'_, K, V, C> {
        let core = self.core.borrow_mut();
        if core.holders.map.contains_key(&key.word()) {
            return KeyedAdmission::Duplicate;
        }
        KeyedAdmission::Granted(KeyedPermit { gate: self, core, key })
    }

    /// Delivers `value` to `key`'s waiter, waking it if it is suspended. No
    /// waiter registered under `key`, or one delivered already, gives
    /// `value` to the cleanup.
    pub fn complete(&self, key: K, value: V) -> Delivery {
        let mut core = self.core.borrow_mut();
        let Some(holder) = core.holders.map.get_mut(&key.word()) else {
            drop(core);
            C::orphan(value);
            return Delivery::Orphan;
        };
        match &holder.state {
            HolderState::Registered | HolderState::Waiting(_) => {}
            HolderState::Delivered(_) | HolderState::Orphaned => {
                drop(core);
                C::orphan(value);
                return Delivery::Orphan;
            }
        }
        match core::mem::replace(&mut holder.state, HolderState::Delivered(value)) {
            HolderState::Waiting(waker) => {
                drop(core);
                waker.wake();
            }
            HolderState::Registered | HolderState::Delivered(_) | HolderState::Orphaned => {}
        }
        Delivery::Delivered
    }

    /// Whether the wait `hold` pins is still registered or waiting: one read
    /// of the holder, never another key's.
    pub fn waiting(&self, hold: &GateHold) -> bool {
        matches!(
            self.core.borrow().holders.map.get(&hold.word).map(|holder| &holder.state),
            Some(HolderState::Registered | HolderState::Waiting(_))
        )
    }

    /// Releases `hold` when its request leaves the queue: an orphaned holder
    /// frees now, a live one when its wait is done.
    pub fn release(&self, hold: GateHold) {
        let mut core = self.core.borrow_mut();
        let Some(holder) = core.holders.map.get_mut(&hold.word) else { return };
        debug_assert!(holder.held, "a hold pins its holder");
        holder.held = false;
        if matches!(holder.state, HolderState::Orphaned) {
            core.holders.map.remove(&hold.word);
        }
    }

    /// Holders taken: registered, waiting, delivered or held (tests, gauges).
    pub fn pending(&self) -> usize {
        self.core.borrow().holders.map.len()
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

/// A reserved route and holder for one key, holding the gate until it
/// registers: it cannot cross an `await` (ADR-0144 A2's list).
pub struct KeyedPermit<'g, K: GateKey, V, C: Cleanup<V> = DropValue> {
    gate: &'g KeyedGate<K, V, C>,
    core: RefMut<'g, KeyedCore<V>>,
    key: K,
}

impl<K: GateKey, V, C: Cleanup<V>> KeyedPermit<'_, K, V, C> {
    /// Publishes the route and its holder: the key is routable.
    pub fn register(mut self) -> KeyedWait<K, V, C> {
        let word = self.key.word();
        self.core.holders.map.insert(word, Holder::registered(false));
        KeyedWait { core: Rc::clone(&self.gate.core), word, done: false, _key: PhantomData }
    }

    /// [`register`](Self::register) for a request its producer queues before
    /// issuing: the [`GateHold`] is a second claim on the holder, kept with
    /// the queued request and given back by [`KeyedGate::release`].
    pub fn register_held(mut self) -> (KeyedWait<K, V, C>, GateHold) {
        let word = self.key.word();
        self.core.holders.map.insert(word, Holder::registered(true));
        let wait =
            KeyedWait { core: Rc::clone(&self.gate.core), word, done: false, _key: PhantomData };
        (wait, GateHold { word })
    }
}

/// A queue's claim on a keyed holder (ADR-0177 D4): move-only, consumed by
/// [`KeyedGate::release`]. While it is out the holder stays, so
/// [`KeyedGate::waiting`] reads that holder and no other.
#[must_use = "a hold is released when its request leaves the queue"]
#[derive(Debug)]
pub struct GateHold {
    word: u64,
}

/// Future returned by a keyed registration. Dropped before its value, the
/// route returns at once and the holder with it, or at its hold's release.
pub struct KeyedWait<K: GateKey, V, C: Cleanup<V> = DropValue> {
    core: Rc<RefCell<KeyedCore<V>>>,
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
        let Some(holder) = core.holders.map.get_mut(&this.word) else { polled_after_completion() };
        match &holder.state {
            HolderState::Delivered(_) => {}
            HolderState::Registered | HolderState::Waiting(_) => {
                holder.state = HolderState::Waiting(cx.waker().clone());
                return Poll::Pending;
            }
            HolderState::Orphaned => polled_after_completion(),
        }
        let value = if holder.held {
            // The hold keeps the holder until its release; the route leaves.
            core::mem::replace(&mut holder.state, HolderState::Orphaned)
        } else {
            let Some(Holder { state, .. }) = core.holders.map.remove(&this.word) else {
                polled_after_completion()
            };
            state
        };
        let HolderState::Delivered(value) = value else { polled_after_completion() };
        this.done = true;
        Poll::Ready(value)
    }
}

impl<K: GateKey, V, C: Cleanup<V>> Drop for KeyedWait<K, V, C> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut core = self.core.borrow_mut();
        let Some(holder) = core.holders.map.get_mut(&self.word) else { return };
        let prior = core::mem::replace(&mut holder.state, HolderState::Orphaned);
        if !holder.held {
            core.holders.map.remove(&self.word);
        }
        drop(core);
        match prior {
            HolderState::Delivered(value) => C::orphan(value),
            HolderState::Registered | HolderState::Waiting(_) | HolderState::Orphaned => {}
        }
    }
}

/// Completion-keyed gate for suspending on backend I/O (M7 disk reads; the
/// seam exists from M0 so the executor contract is complete).
pub type IoGate = KeyedGate<CompletionToken, CompletionResult>;

// ---- WaitList ---------------------------------------------------------------

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

/// Key-keyed FIFO wait list for blocking ops (M1+: BLPOP, XREAD BLOCK…).
/// Multiple tasks may wait on one key; `wake_one` hands the key to the
/// longest-waiting live task, `wake_all` to everyone.
#[allow(
    clippy::type_complexity,
    reason = "an alias of a banned container is refused (ADR-0163 D2)"
)]
#[allow(clippy::disallowed_types, reason = "container: E")]
pub struct WaitList<K: Eq + Hash + Copy> {
    queues: Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
}

impl<K: Eq + Hash + Copy> Default for WaitList<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Copy> WaitList<K> {
    #[allow(clippy::disallowed_types, reason = "container: E")]
    pub fn new() -> WaitList<K> {
        WaitList { queues: Rc::new(RefCell::new(HashMap::default())) }
    }

    /// Join the FIFO for `key`. The future resolves when a mutation wakes
    /// this waiter; dropping it (timeout, disconnect) cancels in place and
    /// — if the wake already landed — passes the baton to the next waiter,
    /// so a wake is never silently lost.
    pub fn wait(&self, key: K) -> ListWait<K> {
        let waiter =
            Rc::new(Waiter { state: Cell::new(WaiterState::Queued), waker: RefCell::new(None) });
        self.queues.borrow_mut().entry(key).or_default().push_back(Rc::clone(&waiter));
        ListWait { queues: Rc::clone(&self.queues), key, waiter, done: false }
    }

    /// Wake the longest-waiting live task on `key`. Returns whether one was
    /// woken.
    pub fn wake_one(&self, key: K) -> bool {
        wake_one_in(&self.queues, key)
    }

    /// Wake every task waiting on `key`. Returns how many.
    pub fn wake_all(&self, key: K) -> usize {
        let Some(queue) = self.queues.borrow_mut().remove(&key) else { return 0 };
        let mut woken = 0;
        for waiter in queue {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
                woken += 1;
            }
        }
        woken
    }

    /// Whether any live waiter is queued, on any key: the emptiness test,
    /// which stops at the first one where [`waiting`](Self::waiting) counts
    /// them all. It passes over only the cancelled entries queued ahead of
    /// that waiter.
    pub fn any_waiting(&self) -> bool {
        self.queues
            .borrow()
            .values()
            .flat_map(|q| q.iter())
            .any(|w| w.state.get() == WaiterState::Queued)
    }

    /// Live waiters across all keys (gauges, tests, leak asserts): a walk
    /// of every queued entry. Cancelled entries pending lazy cleanup are
    /// not counted.
    pub fn waiting(&self) -> usize {
        self.queues
            .borrow()
            .values()
            .flat_map(|q| q.iter())
            .filter(|w| w.state.get() == WaiterState::Queued)
            .count()
    }
}

impl<K: Eq + Hash + Copy> Clone for WaitList<K> {
    /// Shared handle: mutation sites and waiting futures share the queues.
    fn clone(&self) -> Self {
        WaitList { queues: Rc::clone(&self.queues) }
    }
}

impl<K: Eq + Hash + Copy> core::fmt::Debug for WaitList<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WaitList {{ waiting: {} }}", self.waiting())
    }
}

#[allow(
    clippy::type_complexity,
    reason = "an alias of a banned container is refused (ADR-0163 D2)"
)]
#[allow(clippy::disallowed_types, reason = "container: E")]
fn wake_one_in<K: Eq + Hash + Copy>(
    queues: &Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
    key: K,
) -> bool {
    let mut map = queues.borrow_mut();
    // Hot-path guard: credit wakes probe this per fabric reply and the list
    // is empty unless a sender actually ran out of credits — skip the hash.
    if map.is_empty() {
        return false;
    }
    let Some(queue) = map.get_mut(&key) else { return false };
    // Skip lazily-cancelled entries.
    let woken = loop {
        match queue.pop_front() {
            None => break None,
            Some(w) if w.state.get() == WaiterState::Cancelled => continue,
            Some(w) => break Some(w),
        }
    };
    if queue.is_empty() {
        map.remove(&key);
    }
    drop(map);
    match woken {
        None => false,
        Some(waiter) => {
            waiter.state.set(WaiterState::Woken);
            if let Some(w) = waiter.waker.borrow_mut().take() {
                w.wake();
            }
            true
        }
    }
}

/// Future returned by [`WaitList::wait`].
#[allow(
    clippy::type_complexity,
    reason = "an alias of a banned container is refused (ADR-0163 D2)"
)]
#[allow(clippy::disallowed_types, reason = "container: E")]
pub struct ListWait<K: Eq + Hash + Copy> {
    queues: Rc<RefCell<HashMap<K, VecDeque<Rc<Waiter>>, BuildIntHasher>>>,
    key: K,
    waiter: Rc<Waiter>,
    done: bool,
}

// No self-references — state lives behind the shared `Rc` queues.
impl<K: Eq + Hash + Copy> Unpin for ListWait<K> {}

impl<K: Eq + Hash + Copy> Future for ListWait<K> {
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
            WaiterState::Cancelled => panic!("ListWait polled after cancellation"),
        }
    }
}

impl<K: Eq + Hash + Copy> Drop for ListWait<K> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        match self.waiter.state.get() {
            // Still queued: cancel in place; `wake_one` skips us lazily.
            WaiterState::Queued => self.waiter.state.set(WaiterState::Cancelled),
            // Woken but never observed (e.g. timed out in the same
            // iteration): pass the baton so the wake is not lost.
            WaiterState::Woken => {
                wake_one_in(&self.queues, self.key);
            }
            WaiterState::Cancelled => {}
        }
    }
}

// ---- WatermarkGate ----------------------------------------------------------

struct WatermarkInner {
    watermark: Cell<u64>,
    waiters: RefCell<BTreeMap<u64, Vec<Rc<Waiter>>>>,
}

/// LSN-keyed gate: tasks wait for the durability watermark to reach their
/// LSN; `advance` wakes every task at or below the new watermark (M2 group
/// commit acks). A waiter at or below the current watermark is ready on its
/// first poll.
#[derive(Clone)]
pub struct WatermarkGate {
    inner: Rc<WatermarkInner>,
}

impl Default for WatermarkGate {
    fn default() -> Self {
        Self::new()
    }
}

impl WatermarkGate {
    pub fn new() -> WatermarkGate {
        WatermarkGate {
            inner: Rc::new(WatermarkInner {
                watermark: Cell::new(0),
                waiters: RefCell::new(BTreeMap::new()),
            }),
        }
    }

    pub fn watermark(&self) -> u64 {
        self.inner.watermark.get()
    }

    /// Wait until the watermark reaches `lsn`.
    pub fn waiter(&self, lsn: u64) -> WatermarkWait {
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
    /// every waiter at or below it. Returns how many tasks were woken.
    pub fn advance(&self, to: u64) -> usize {
        if to <= self.inner.watermark.get() {
            return 0;
        }
        self.inner.watermark.set(to);
        let due = {
            let mut waiters = self.inner.waiters.borrow_mut();
            let still_waiting = waiters.split_off(&(to + 1));
            core::mem::replace(&mut *waiters, still_waiting)
        };
        let mut woken = 0;
        for waiter in due.into_values().flatten() {
            if waiter.state.get() == WaiterState::Queued {
                waiter.state.set(WaiterState::Woken);
                if let Some(w) = waiter.waker.borrow_mut().take() {
                    w.wake();
                }
                woken += 1;
            }
        }
        woken
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
            "WatermarkGate {{ watermark: {}, waiting: {} }}",
            self.watermark(),
            self.waiting()
        )
    }
}

/// Future returned by [`WatermarkGate::waiter`].
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

    /// 1,000 waits on one key, each polled once, so it is queued with its
    /// waker stored, then dropped before any wake. A wait's entry is its
    /// future's, so its drop unlinks it and the list is empty after the
    /// last (ADR-0177 D5). At HEAD a dropped wait is only marked cancelled,
    /// and its entry stays queued until a later wake of the key reaches it.
    #[test]
    #[ignore = "a wait dropped while queued is marked cancelled and stays in its key's queue \
                until a later wake of that key walks past it, so a key nobody wakes keeps every \
                dropped entry; red until a wait's node is unlinked by its own drop"]
    fn a_wait_dropped_while_queued_leaves_no_entry() {
        const WAITS: usize = 1_000;
        let list: WaitList<u32> = WaitList::new();
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..WAITS {
            let mut wait = list.wait(7);
            assert!(Pin::new(&mut wait).poll(&mut cx).is_pending(), "engagement: queued");
            drop(wait);
        }
        assert_eq!(list.waiting(), 0, "no dropped wait is a live waiter");
        let entries: usize = list.queues.borrow().values().map(|queue| queue.len()).sum();
        assert_eq!(entries, 0, "{entries} entries stay queued after {WAITS} waits were dropped");
    }
}
