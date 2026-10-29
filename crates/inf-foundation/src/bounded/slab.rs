//! `CappedSlab<T, G, P>`: fixed generational slots for identities the owner
//! mints and later looks up (ADR-0171). A `SlotId` names its slot's index,
//! the generation its occupant took and the slab that minted it; an access
//! compares the slab first, then the generation, so another slab's handle
//! and a stale one are `Stale` and change nothing. `G` is the generation's
//! width and `P` its policy at the last value, chosen per instance by its
//! site's record: `Retire` takes the slot out for good, so capacity shrinks
//! and ends in `Exhausted`; `WrapWithProof` wraps, legal only where no
//! holder of an old handle exists when the slot frees.
//!
//! A slab is built and retired one chunk or directory per step
//! ([`SlabBuilder`], [`SlabRetiring`]); once `Live` it allocates nothing:
//! growth is `reserve` then `publish`, and `Full` holds no value and changes
//! nothing but its census row's crossings.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::fmt;
use core::marker::PhantomData;
use core::mem;
use std::rc::Rc;

use super::census::{CapCensus, CapRowHandle, SlabInstance};
use super::store::{BuildStep, FreeSlot, RetireStep as StoreRetire, Slot, SlotStore};
use super::{Cap, CapError, CapFill};

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
    impl Sealed for super::Retire {}
    impl Sealed for super::WrapWithProof {}
}

/// A slot's generation: `u32` or `u64`, the instance's choice. Its width is
/// the slot's uses before the policy acts.
pub trait Generation: sealed::Sealed + Copy + Eq + fmt::Debug {
    /// The generation a never-used slot hands out.
    const FIRST: Self;
    /// The last generation; past it the policy decides.
    const LAST: Self;
    /// The next generation, none past `LAST`.
    fn after(self) -> Option<Self>;
    /// The next generation, `LAST` wrapping to `FIRST`.
    fn after_wrapped(self) -> Self;
}

macro_rules! generation {
    ($($width:ty),*) => {$(
        impl Generation for $width {
            const FIRST: Self = 0;
            const LAST: Self = <$width>::MAX;

            #[inline]
            fn after(self) -> Option<Self> {
                self.checked_add(1)
            }

            #[inline]
            fn after_wrapped(self) -> Self {
                // ADR-0165 D1 `bits`: a generation's wrap is its policy.
                self.wrapping_add(1)
            }
        }
    )*};
}
generation!(u32, u64);

/// What a slot's generation does at a `take` (ADR-0171 D2), a type
/// parameter of the slab so the advance is static.
pub trait GenerationPolicy: sealed::Sealed {
    /// The generation the slot's next occupant takes, or `None`: the slot
    /// retires and the slab serves one slot fewer.
    fn advance<G: Generation>(generation: G) -> Option<G>;
}

/// At the last generation the slot retires for the owner's lifetime, so a
/// handle that outlives its occupant can never name a new one; the slab's
/// capacity shrinks by one, and with none left `reserve` is `Exhausted`.
pub struct Retire;

impl GenerationPolicy for Retire {
    #[inline]
    fn advance<G: Generation>(generation: G) -> Option<G> {
        generation.after()
    }
}

/// The generation wraps: legal only where the instance's record proves no
/// holder of an old handle exists when the slot frees, so a generation
/// catches defects, never reuse.
pub struct WrapWithProof;

impl GenerationPolicy for WrapWithProof {
    #[inline]
    fn advance<G: Generation>(generation: G) -> Option<G> {
        Some(generation.after_wrapped())
    }
}

/// A slot's identity: its index, the generation its occupant took and the
/// slab that minted it. Only a slab mints one, at `publish`; its parts read
/// out, so a model or an adapter can name them, but nothing outside this
/// module builds one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotId<G> {
    index: u32,
    generation: G,
    instance: SlabInstance,
}

impl<G: Copy> SlotId<G> {
    /// The slot's index in the slab that minted this handle.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The generation the occupant this handle names took.
    #[must_use]
    pub fn generation(&self) -> G {
        self.generation
    }

    /// The slab that minted this handle.
    #[must_use]
    pub fn instance(&self) -> SlabInstance {
        self.instance
    }
}

/// The answer to `get`: the occupant, or why there is none. A handle that
/// names a slot of this slab but not its occupant — another generation, a
/// free or retired slot — or that another slab minted is `Stale`, never
/// absence; `Vacant` is a handle naming no slot of this slab.
#[must_use]
#[derive(Debug)]
pub enum Lookup<'a, T> {
    Live(&'a T),
    Stale,
    Vacant,
}

/// The answer to `get_mut`, as [`Lookup`] with the occupant mutable.
#[must_use]
#[derive(Debug)]
pub enum LookupMut<'a, T> {
    Live(&'a mut T),
    Stale,
    Vacant,
}

/// The answer to `take`: the occupant returned to its owner, or why none.
#[must_use]
#[derive(PartialEq, Eq, Debug)]
pub enum Taken<T> {
    Value(T),
    Stale,
    Vacant,
}

/// The answer to `reserve()`: a slot, `Full` (every slot that can serve is
/// live: no value taken, the row's crossings up by one), or `Exhausted`
/// (every slot retired: the site's typed refusal). It carries the slot's
/// borrow, so it is matched in the step that reserved and cannot cross an
/// `await` (clippy's `await-holding-invalid-types` names it).
#[must_use = "a reservation publishes a value or is dropped; it carries none"]
pub enum SlabReserved<'a, T, G, P> {
    Slot(SlotReservation<'a, T, G, P>),
    Full,
    Exhausted,
}

/// An exclusive reservation of one free slot: `publish` consumes it once
/// and cannot allocate or refuse; dropping it leaves the slot free with its
/// generation. It borrows the slab, so it cannot cross an `await` or a
/// reactor iteration: a resumed step reserves again.
#[must_use = "a reservation publishes a value or is dropped; it carries none"]
pub struct SlotReservation<'a, T, G, P> {
    slot: FreeSlot<'a, T, G>,
    live: &'a mut u32,
    high_water: &'a mut u32,
    row: &'a CapRowHandle,
    instance: SlabInstance,
    policy: PhantomData<P>,
}

/// A live capped slab: its census row handle with the fill byte first, its
/// live count and high water, then its store, 64 bytes in all, so a take's
/// loads (the fill byte, the counts, the instance, the free head and the
/// root) lie in one line when the slab starts on one.
#[repr(C)]
pub struct CappedSlab<T, G, P> {
    row: CapRowHandle,
    live: u32,
    high_water: u32,
    store: SlotStore<T, G>,
    policy: PhantomData<P>,
}

/// A slab being built, one chunk or directory per step; it serves nothing.
/// Its instance is minted, so a handle that must name its owner can read it
/// before the slab is live.
pub struct SlabBuilder<T, G, P> {
    row: CapRowHandle,
    store: SlotStore<T, G>,
    policy: PhantomData<P>,
}

/// A slab being retired, or a refused build's prefix: one chunk or
/// directory freed per step, children before their directory.
pub struct SlabRetiring<T, G, P> {
    store: SlotStore<T, G>,
    policy: PhantomData<P>,
}

/// The answer of a build step.
pub enum Built<T, G, P> {
    More(SlabBuilder<T, G, P>),
    Live(CappedSlab<T, G, P>),
    /// A step's allocation was refused: nothing was served, and what was
    /// built retires by steps.
    Refused(SlabRetiring<T, G, P>, CapError),
}

/// The answer of a retire step.
pub enum Retiring<T, G, P> {
    More(SlabRetiring<T, G, P>),
    Done,
}

impl<T, G: Generation, P: GenerationPolicy> CappedSlab<T, G, P> {
    /// A slab of `cap` on `census`, to build by steps: the layout is checked
    /// (a chunk or a directory past one backing allocation is `Layout`, and
    /// no row is registered), then the row is registered, then the instance
    /// minted (`Census` past its mints; the row stays). Nothing is allocated
    /// until the first step.
    pub fn builder(cap: Cap, census: &Rc<CapCensus>) -> Result<SlabBuilder<T, G, P>, CapError> {
        if !SlotStore::<T, G>::layout_fits() {
            return Err(CapError::Layout);
        }
        let row = census.register(cap)?;
        let store = SlotStore::new(cap.entries_max(), census)?;
        Ok(SlabBuilder { row, store, policy: PhantomData })
    }

    /// Live occupants, at most `capacity_now`.
    #[must_use]
    #[inline]
    pub fn len(&self) -> u32 {
        self.live
    }

    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The slots that can still serve: the cap, less the slots retired.
    #[must_use]
    pub fn capacity_now(&self) -> u32 {
        self.store.capacity_now()
    }

    /// The most occupants this instance has held at once (a `Serving`
    /// slab's; an `Assembly` slab reports its every publish instead).
    #[must_use]
    pub fn high_water(&self) -> u32 {
        self.high_water
    }

    /// This slab's identity, stamped in every handle it mints.
    #[must_use]
    pub fn instance(&self) -> SlabInstance {
        self.store.instance()
    }

    /// A free slot when one can serve; `Full` when every slot that can
    /// serve is live (the row's crossings up by one); `Exhausted` when every
    /// slot has retired.
    #[inline]
    pub fn reserve(&mut self) -> SlabReserved<'_, T, G, P> {
        if !self.has_room() {
            return if self.store.capacity_now() == 0 {
                SlabReserved::Exhausted
            } else {
                self.full()
            };
        }
        let (instance, capacity) = (self.store.instance(), self.store.capacity_now());
        let CappedSlab { row, live, high_water, store, .. } = self;
        match store.reserve() {
            Some(slot) => SlabReserved::Slot(SlotReservation {
                slot,
                live,
                high_water,
                row,
                instance,
                policy: PhantomData,
            }),
            // The chain holds `capacity_now - live` slots, so this arm is
            // never reached by a correct build.
            None => {
                debug_assert!(*live >= capacity, "a free slot below capacity_now");
                row.report_full();
                SlabReserved::Full
            }
        }
    }

    /// The occupant `id` names, if it is this slab's and current.
    #[inline]
    pub fn get(&self, id: SlotId<G>) -> Lookup<'_, T> {
        if self.foreign(&id) {
            return Lookup::Stale;
        }
        match self.store.slot(id.index) {
            Some(Slot::Live { generation, value }) if *generation == id.generation => {
                Lookup::Live(value)
            }
            Some(Slot::Live { .. } | Slot::Free { .. } | Slot::Closed) => Lookup::Stale,
            None => Lookup::Vacant,
        }
    }

    /// The occupant `id` names, mutably, if it is this slab's and current.
    #[inline]
    pub fn get_mut(&mut self, id: SlotId<G>) -> LookupMut<'_, T> {
        if self.foreign(&id) {
            return LookupMut::Stale;
        }
        lookup_mut(self.store.slot_mut(id.index), id.generation)
    }

    /// The occupant `id` names returns to its owner and the slot frees: its
    /// generation advances by the policy, or at the last it retires and the
    /// slab serves one slot fewer, reported to the row before any `Full` at
    /// the new count. An `Assembly` slab reports the removal (its fill byte,
    /// tested once). A stale or foreign handle changes nothing.
    #[inline]
    pub fn take(&mut self, id: SlotId<G>) -> Taken<T> {
        if self.foreign(&id) {
            return Taken::Stale;
        }
        let head = self.store.free_head();
        let Some(slot) = self.store.slot_mut(id.index) else { return Taken::Vacant };
        let (value, retired) = match mem::replace(slot, Slot::Closed) {
            Slot::Live { generation, value } if generation == id.generation => {
                match P::advance(generation) {
                    Some(next) => {
                        *slot = Slot::Free { generation: next, next: head };
                        (value, false)
                    }
                    None => (value, true),
                }
            }
            other @ (Slot::Live { .. } | Slot::Free { .. } | Slot::Closed) => {
                *slot = other;
                return Taken::Stale;
            }
        };
        if retired {
            self.retired();
        } else {
            self.store.chain(id.index);
        }
        self.live = live_taken(self.live);
        if self.row.fill() == CapFill::Assembly {
            self.row.report_removal(self.live);
        }
        Taken::Value(value)
    }

    /// Retirement of the whole slab, by steps.
    pub fn retire(self) -> SlabRetiring<T, G, P> {
        SlabRetiring { store: self.store, policy: PhantomData }
    }

    /// A test hook: `get_mut`'s lookup, then the live slot's generation and
    /// the handle's set to the last value, so the next `take` advances past
    /// it — the policy's last-value branch on the real path. It asserts
    /// nothing and computes nothing; `Stale` and `Vacant` answer as
    /// `get_mut` does.
    #[cfg(any(test, feature = "fault-points"))]
    pub fn place_generation_last(&mut self, id: &mut SlotId<G>) -> LookupMut<'_, T> {
        if self.foreign(id) {
            return LookupMut::Stale;
        }
        match self.store.slot_mut(id.index) {
            Some(Slot::Live { generation, value }) if *generation == id.generation => {
                *generation = G::LAST;
                id.generation = G::LAST;
                LookupMut::Live(value)
            }
            Some(Slot::Live { .. } | Slot::Free { .. } | Slot::Closed) => LookupMut::Stale,
            None => LookupMut::Vacant,
        }
    }

    /// Whether `id` was minted by another slab: compared before anything
    /// else, so a foreign index is never walked. One canary skips it.
    #[inline]
    fn foreign(&self, id: &SlotId<G>) -> bool {
        #[cfg(inf_canary_slab_instance_blind)]
        {
            let _ = id;
            false
        }
        #[cfg(not(inf_canary_slab_instance_blind))]
        {
            id.instance != self.store.instance()
        }
    }

    /// The admission compare, `live < capacity_now`. The cap-minus-one
    /// canary admits one occupant fewer on a `Serving` row.
    #[inline]
    fn has_room(&self) -> bool {
        let capacity = self.store.capacity_now();
        #[cfg(inf_canary_cap_minus_one)]
        {
            let serving = self.row.fill() == CapFill::Serving;
            let short = capacity.checked_sub(1).unwrap_or_default();
            self.live < if serving { short } else { capacity }
        }
        #[cfg(not(inf_canary_cap_minus_one))]
        {
            self.live < capacity
        }
    }

    /// The crossing, off the hot path.
    #[cold]
    #[inline(never)]
    fn full(&mut self) -> SlabReserved<'_, T, G, P> {
        self.row.report_full();
        SlabReserved::Full
    }

    /// A slot retired at its last generation: the store serves one fewer,
    /// and the row keeps the minimum capacity over its instances.
    #[cold]
    #[inline(never)]
    fn retired(&mut self) {
        self.store.retire_slot();
        self.row.report_capacity(self.store.capacity_now());
    }
}

impl<T, G: Generation, P> SlotReservation<'_, T, G, P> {
    /// The value takes the slot, and its handle is returned. Cannot allocate
    /// or refuse: the slot was free and the reservation held it.
    #[inline]
    pub fn publish(self, value: T) -> SlotId<G> {
        let (index, generation) = self.slot.occupy(value);
        let live = live_published(*self.live);
        *self.live = live;
        // An `Assembly` slab keeps its own high water at 0, so this one
        // compare reports its every publish and the row keeps its live count.
        if live > *self.high_water {
            if self.row.fill() == CapFill::Serving {
                *self.high_water = live;
            }
            self.row.report_publish(live);
        }
        SlotId { index, generation, instance: self.instance }
    }
}

impl<T, G: Generation, P: GenerationPolicy> SlabBuilder<T, G, P> {
    /// The identity the slab will stamp in its handles, minted when the
    /// builder was made.
    #[must_use]
    pub fn instance(&self) -> SlabInstance {
        self.store.instance()
    }

    /// One build step: one chunk or one directory allocated. `Live` once
    /// the last chunk is built; a refused allocation retires what was built.
    pub fn step(mut self) -> Built<T, G, P> {
        match self.store.build_step() {
            Ok(BuildStep::More) => Built::More(self),
            Ok(BuildStep::Done) => Built::Live(CappedSlab {
                row: self.row,
                live: 0,
                high_water: 0,
                store: self.store,
                policy: PhantomData,
            }),
            Err(refused) => {
                Built::Refused(SlabRetiring { store: self.store, policy: PhantomData }, refused)
            }
        }
    }
}

impl<T, G: Generation, P> SlabRetiring<T, G, P> {
    /// One retire step: the last chunk or directory freed, with the
    /// occupants of a chunk. One canary drops the rest whole at the first
    /// step instead, an unsliced drop.
    pub fn step(mut self) -> Retiring<T, G, P> {
        #[cfg(inf_canary_unsliced_drop)]
        {
            drop(self);
            Retiring::Done
        }
        #[cfg(not(inf_canary_unsliced_drop))]
        match self.store.retire_step() {
            StoreRetire::More => Retiring::More(self),
            StoreRetire::Done => Retiring::Done,
        }
    }
}

/// `get_mut`'s answer for the slot at a handle's index.
#[inline]
fn lookup_mut<T, G: Generation>(slot: Option<&mut Slot<T, G>>, expected: G) -> LookupMut<'_, T> {
    match slot {
        Some(Slot::Live { generation, value }) if *generation == expected => LookupMut::Live(value),
        Some(Slot::Live { .. } | Slot::Free { .. } | Slot::Closed) => LookupMut::Stale,
        None => LookupMut::Vacant,
    }
}

/// `live` after a publish.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: live < capacity_now <= u32::MAX by the admission compare before the \
              reservation, so + 1 stays within u32 (ADR-0165 D1's bound row)"
)]
fn live_published(live: u32) -> u32 {
    live + 1
}

/// `live` after a take.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: live >= 1, the live slot the take found and vacated, so - 1 stays at or \
              above 0 (ADR-0165 D1's bound row)"
)]
fn live_taken(live: u32) -> u32 {
    live - 1
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::bounded::{CensusPhase, ServeMark};
    use crate::ids::CellId;
    use crate::limits::{
        CAP_CENSUS_SLABS_MAX, MAP_CHUNK_SLOTS_MAX, MAP_DIRECTORY_CHILDREN_MAX,
        MAP_DIRECTORY_LEVELS_MAX,
    };
    use crate::rng::{Entropy, SplitMix64};

    const CELL: CellId = CellId(7);

    fn census() -> (Rc<CapCensus>, ServeMark) {
        CapCensus::new(CELL)
    }

    /// A slab of `cap`, built by steps; the steps it took.
    fn built<T, G: Generation, P: GenerationPolicy>(
        cap: Cap,
        census: &Rc<CapCensus>,
    ) -> (CappedSlab<T, G, P>, u32) {
        let mut builder = CappedSlab::builder(cap, census).expect("registered");
        let mut steps = 0;
        loop {
            steps += 1;
            match builder.step() {
                Built::More(next) => builder = next,
                Built::Live(slab) => return (slab, steps),
                Built::Refused(_, refused) => panic!("a build step refused: {refused}"),
            }
        }
    }

    /// Retires a slab by steps; the steps it took.
    fn retired<T, G: Generation, P>(mut retiring: SlabRetiring<T, G, P>) -> u32 {
        let mut steps = 0;
        loop {
            steps += 1;
            match retiring.step() {
                Retiring::More(next) => retiring = next,
                Retiring::Done => return steps,
            }
        }
    }

    fn publish<T, G: Generation, P: GenerationPolicy>(
        slab: &mut CappedSlab<T, G, P>,
        value: T,
    ) -> Option<SlotId<G>> {
        match slab.reserve() {
            SlabReserved::Slot(slot) => Some(slot.publish(value)),
            SlabReserved::Full | SlabReserved::Exhausted => None,
        }
    }

    fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .expect("a panic message")
    }

    #[test]
    fn the_backing_constants_are_powers_of_two_covering_every_u32_index() {
        assert!(MAP_CHUNK_SLOTS_MAX.is_power_of_two());
        assert!(MAP_DIRECTORY_CHILDREN_MAX.is_power_of_two());
        assert_eq!(MAP_DIRECTORY_LEVELS_MAX, 4);
        assert_eq!(crate::limits::directory_levels(64), 0);
        assert_eq!(crate::limits::directory_levels(65), 1);
        assert_eq!(crate::limits::directory_levels(16_384), 1);
        assert_eq!(crate::limits::directory_levels(16_385), 2);
        assert_eq!(crate::limits::directory_levels(u64::from(u32::MAX)), 4);
    }

    #[test]
    fn the_headers_fit_one_line() {
        assert_eq!(core::mem::size_of::<CapRowHandle>(), 16);
        assert_eq!(core::mem::size_of::<SlotStore<u64, u32>>(), 40);
        assert_eq!(core::mem::size_of::<CappedSlab<u64, u32, Retire>>(), 64);
        assert_eq!(core::mem::size_of::<CappedSlab<[u8; 200], u64, WrapWithProof>>(), 64);
        assert_eq!(core::mem::size_of::<SlotId<u32>>(), 12);
        assert_eq!(core::mem::size_of::<SlotId<u64>>(), 16);
    }

    #[test]
    fn a_build_and_a_retirement_take_one_item_per_step() {
        // One chunk and no directory up to 64 slots; a directory and its
        // chunks to 16,384 (10^3: 16 chunks and one directory); two levels
        // past that (16,385: 257 chunks, two directories and the root).
        let (census, _mark) = census();
        for (cap, steps) in [
            (Cap::entries::<1>("s-1", CapFill::Serving), 1),
            (Cap::entries::<64>("s-64", CapFill::Serving), 1),
            (Cap::entries::<65>("s-65", CapFill::Serving), 3),
            (Cap::entries::<1000>("s-1000", CapFill::Serving), 17),
            (Cap::entries::<16_385>("s-16385", CapFill::Serving), 260),
        ] {
            let (slab, built) = built::<u64, u32, Retire>(cap, &census);
            assert_eq!(built, steps, "{cap:?}");
            assert_eq!(slab.capacity_now(), cap.entries_max());
            assert_eq!(retired(slab.retire()), steps, "{cap:?}");
        }
        assert_eq!(census.unsliced_drops(), 0);
    }

    #[test]
    fn a_chunk_past_one_backing_allocation_is_layout_and_registers_nothing() {
        let (census, _mark) = census();
        let cap = Cap::entries::<4>("wide", CapFill::Serving);
        let refused = CappedSlab::<[u8; 1100], u32, Retire>::builder(cap, &census);
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout));
        assert_eq!(census.rows().count(), 0);
    }

    #[test]
    fn zero_one_cap_minus_one_cap_and_cap_plus_one() {
        let (census, _mark) = census();
        let cap = Cap::entries::<3>("three", CapFill::Serving);
        let (mut slab, _) = built::<u32, u32, Retire>(cap, &census);
        assert!(slab.is_empty());
        let a = publish(&mut slab, 10).expect("one");
        let b = publish(&mut slab, 20).expect("cap minus one");
        let c = publish(&mut slab, 30).expect("reaching the cap is legal");
        assert!(matches!(slab.reserve(), SlabReserved::Full), "cap plus one is Full");
        assert_eq!((slab.len(), slab.high_water()), (3, 3));
        let row = census.row("three").expect("row");
        assert_eq!((row.high_water, row.crossings, row.capacity_now), (3, 1, 3));
        assert_eq!(slab.take(b), Taken::Value(20));
        assert!(matches!(slab.get(b), Lookup::Stale), "a taken handle is stale");
        assert_eq!(slab.take(b), Taken::Stale);
        let d = publish(&mut slab, 40).expect("a take frees a slot");
        assert_eq!(d.index, b.index, "the freed slot is reused");
        assert_ne!(d.generation, b.generation, "at its next generation");
        for (id, value) in [(a, 10), (c, 30), (d, 40)] {
            assert!(matches!(slab.get(id), Lookup::Live(v) if *v == value));
        }
        if let LookupMut::Live(value) = slab.get_mut(a) {
            *value = 11;
        }
        assert_eq!(slab.take(a), Taken::Value(11));
    }

    #[test]
    fn a_dropped_reservation_publishes_nothing() {
        let (census, _mark) = census();
        let cap = Cap::entries::<1>("one", CapFill::Serving);
        let (mut slab, _) = built::<u8, u32, Retire>(cap, &census);
        match slab.reserve() {
            SlabReserved::Slot(slot) => drop(slot),
            SlabReserved::Full | SlabReserved::Exhausted => panic!("room for one"),
        }
        assert!(slab.is_empty());
        let id = publish(&mut slab, 1).expect("the slot is still free");
        assert_eq!(id.generation, 0, "a dropped reservation keeps the generation");
        assert_eq!(census.row("one").expect("row").crossings, 0);
    }

    /// The model's own generation arithmetic, sharing no code with
    /// `Generation`: a wrong `FIRST`, `LAST` or step there disagrees with it.
    trait ModelGeneration: Copy + Ord + fmt::Debug {
        const ZERO: Self;
        const MAX: Self;
        fn successor(self) -> Option<Self>;
    }

    macro_rules! model_generation {
        ($($width:ty),*) => {$(
            impl ModelGeneration for $width {
                const ZERO: Self = 0;
                const MAX: Self = <$width>::MAX;

                fn successor(self) -> Option<Self> {
                    self.checked_add(1)
                }
            }
        )*};
    }
    model_generation!(u32, u64);

    /// What the model expects of a policy when a slot's last generation is
    /// taken (ADR-0171 D2).
    #[derive(Clone, Copy)]
    enum AtLast {
        /// The slot never serves again; the slab serves one slot fewer.
        Retires,
        /// The slot's next occupant takes the first generation.
        Wraps,
    }

    trait ModelPolicy {
        const AT_LAST: AtLast;
    }

    impl ModelPolicy for Retire {
        const AT_LAST: AtLast = AtLast::Retires;
    }

    impl ModelPolicy for WrapWithProof {
        const AT_LAST: AtLast = AtLast::Wraps;
    }

    /// The four accesses that answer a handle against its slot.
    #[derive(Clone, Copy)]
    enum Access {
        Get,
        GetMut,
        Take,
        Hook,
    }

    /// Per access, the ops that named a handle whose slot is free at the
    /// handle's generation (a wrap brought it back) or retired: the states
    /// an answer for a slot not live at the handle's generation is read on.
    /// A zero means the model checked nothing of that state on that access.
    #[derive(Default)]
    struct Reach {
        free_at_generation: [u64; 4],
        retired: [u64; 4],
    }

    /// The model of one slab: its occupants by `(index, generation)`, by
    /// index the generation the slot's next occupant takes (`None`: the slot
    /// retired), the slots left to serve, the row's high water and
    /// crossings, and what its ops reached.
    struct Model<G> {
        live: BTreeMap<(u32, G), u64>,
        next: Vec<Option<G>>,
        capacity: u32,
        high_water: u32,
        crossings: u64,
        reach: Reach,
    }

    impl<G: ModelGeneration> Model<G> {
        /// The generation slot `index`'s next occupant takes, `None` once
        /// the slot retired.
        fn next_generation(&self, index: u32) -> Option<G> {
            self.next[index as usize]
        }

        /// Counts an access to `id` on the model's state before the op.
        fn note(&mut self, id: &SlotId<G>, access: Access) {
            let access = access as usize;
            match self.next_generation(id.index) {
                None => self.reach.retired[access] += 1,
                Some(next) if next == id.generation => {
                    let range = (id.index, G::ZERO)..=(id.index, G::MAX);
                    if self.live.range(range).next().is_none() {
                        self.reach.free_at_generation[access] += 1;
                    }
                }
                Some(_) => {}
            }
        }

        /// The occupant at `(index, generation)` left: the next generation,
        /// or at the last the policy's answer.
        fn vacate(&mut self, index: u32, generation: G, at_last: AtLast) {
            let next = &mut self.next[index as usize];
            assert_eq!(*next, Some(generation), "the occupant took the slot's generation");
            match (generation.successor(), at_last) {
                (Some(after), AtLast::Retires | AtLast::Wraps) => *next = Some(after),
                (None, AtLast::Retires) => {
                    *next = None;
                    self.capacity -= 1;
                }
                (None, AtLast::Wraps) => *next = Some(G::ZERO),
            }
        }
    }

    /// A handle for an op to name, from the limits of a generation: any
    /// handle the run minted, a live occupant's (so the hook and a take
    /// reach the last generation), or one at the first generation (the
    /// handle a wrap brings a free slot back to).
    fn pick<G: ModelGeneration>(
        rng: &mut SplitMix64,
        handles: &[SlotId<G>],
        model: &Model<G>,
    ) -> usize {
        let uniform = rng.next_below(handles.len() as u64) as usize;
        let live = |id: &SlotId<G>| model.live.contains_key(&(id.index, id.generation));
        let first = |id: &SlotId<G>| id.generation == G::ZERO;
        let wanted: &dyn Fn(&SlotId<G>) -> bool = match rng.next_below(3) {
            0 => return uniform,
            1 => &live,
            _ => &first,
        };
        let count = handles.iter().filter(|id| wanted(id)).count();
        if count == 0 {
            return uniform;
        }
        let nth = rng.next_below(count as u64) as usize;
        handles.iter().enumerate().filter(|(_, id)| wanted(id)).nth(nth).map_or(uniform, |(i, _)| i)
    }

    fn model_publish<G: Generation + ModelGeneration, P: GenerationPolicy>(
        slab: &mut CappedSlab<u64, G, P>,
        model: &mut Model<G>,
        handles: &mut Vec<SlotId<G>>,
        value: u64,
    ) {
        let instance = slab.instance();
        match slab.reserve() {
            SlabReserved::Slot(slot) => {
                assert!((model.live.len() as u64) < u64::from(model.capacity), "a slot to serve");
                let id = slot.publish(value);
                assert_eq!(id.instance, instance);
                assert!((id.index as usize) < model.next.len(), "an index past the cap");
                let next = model.next_generation(id.index).expect("a retired slot served again");
                assert_eq!(id.generation, next, "the slot's next generation");
                assert!(model.live.insert((id.index, id.generation), value).is_none());
                model.high_water = model.high_water.max(model.live.len() as u32);
                handles.push(id);
            }
            SlabReserved::Full => {
                assert!(model.capacity > 0, "Full with no slot left");
                assert_eq!(model.live.len() as u64, u64::from(model.capacity), "Full");
                model.crossings += 1;
            }
            SlabReserved::Exhausted => assert_eq!(model.capacity, 0, "Exhausted with a slot left"),
        }
    }

    fn model_take<G: Generation + ModelGeneration, P: GenerationPolicy + ModelPolicy>(
        slab: &mut CappedSlab<u64, G, P>,
        model: &mut Model<G>,
        id: SlotId<G>,
    ) {
        model.note(&id, Access::Take);
        match model.live.remove(&(id.index, id.generation)) {
            Some(expected) => {
                assert_eq!(slab.take(id), Taken::Value(expected));
                model.vacate(id.index, id.generation, P::AT_LAST);
            }
            None => assert_eq!(slab.take(id), Taken::Stale, "{id:?}: a stale take"),
        }
    }

    fn model_write<G: Generation + ModelGeneration, P: GenerationPolicy>(
        slab: &mut CappedSlab<u64, G, P>,
        model: &mut Model<G>,
        id: SlotId<G>,
        value: u64,
    ) {
        model.note(&id, Access::GetMut);
        match (slab.get_mut(id), model.live.get_mut(&(id.index, id.generation))) {
            (LookupMut::Live(found), Some(expected)) => {
                assert_eq!(*found, *expected);
                *found = value;
                *expected = value;
            }
            (LookupMut::Stale, None) => {}
            (found, expected) => panic!("{id:?}: get_mut {found:?}, model {expected:?}"),
        }
    }

    /// The test hook on `handles[at]`: a live occupant moves to the model's
    /// last generation with its handle; any other handle is `Stale` and
    /// keeps its generation.
    fn model_place_last<G: Generation + ModelGeneration, P: GenerationPolicy>(
        slab: &mut CappedSlab<u64, G, P>,
        model: &mut Model<G>,
        handles: &mut [SlotId<G>],
        at: usize,
    ) {
        let before = handles[at];
        model.note(&before, Access::Hook);
        let held = model.live.remove(&(before.index, before.generation));
        match (slab.place_generation_last(&mut handles[at]), held) {
            (LookupMut::Live(found), Some(expected)) => assert_eq!(*found, expected),
            (LookupMut::Stale, None) => {}
            (found, expected) => panic!("{before:?}: the hook {found:?}, model {expected:?}"),
        }
        let after = handles[at];
        assert_eq!((after.index, after.instance), (before.index, before.instance));
        match held {
            Some(value) => {
                assert_eq!(after.generation, G::MAX, "the hook re-stamps a live handle");
                assert!(model.live.insert((after.index, G::MAX), value).is_none());
                model.next[after.index as usize] = Some(G::MAX);
            }
            None => assert_eq!(after.generation, before.generation, "a stale handle stays"),
        }
    }

    /// The model: a `BTreeMap<(index, generation), value>` per instance with
    /// the slots' next generations, the retired slots and the capacity left,
    /// sharing no code with the slab. Publishes, takes, writes through
    /// `get_mut` and the test hook's placements at the last generation are
    /// each checked against it, so a slot retires (`Retire`) or wraps
    /// (`WrapWithProof`) under random sequences; every handle the run minted
    /// is read back through `get` after every op, and the row's high water,
    /// crossings and capacity equal the model's at the end. Returns what the
    /// run's ops reached.
    fn model_run<G, P>(cap: Cap, seed: u64, ops: u32) -> Reach
    where
        G: Generation + ModelGeneration,
        P: GenerationPolicy + ModelPolicy,
    {
        let (census, _mark) = census();
        let (mut slab, _) = built::<u64, G, P>(cap, &census);
        let mut model = Model {
            live: BTreeMap::new(),
            next: vec![Some(G::ZERO); cap.entries_max() as usize],
            capacity: cap.entries_max(),
            high_water: 0,
            crossings: 0,
            reach: Reach::default(),
        };
        let mut handles: Vec<SlotId<G>> = Vec::new();
        let mut rng = SplitMix64::new(seed);
        for op in 0..ops {
            let value = u64::from(op);
            let kind = rng.next_below(16);
            if kind < 8 || handles.is_empty() {
                model_publish(&mut slab, &mut model, &mut handles, value);
            } else {
                let at = pick(&mut rng, &handles, &model);
                match kind {
                    8..=10 => model_take(&mut slab, &mut model, handles[at]),
                    11 | 12 => model_write(&mut slab, &mut model, handles[at], value),
                    13 => model_place_last(&mut slab, &mut model, &mut handles, at),
                    _ => {}
                }
            }
            assert_eq!(slab.len() as usize, model.live.len());
            assert_eq!(slab.capacity_now(), model.capacity);
            for id in &handles {
                model.note(id, Access::Get);
                match (model.live.get(&(id.index, id.generation)), slab.get(*id)) {
                    (Some(expected), Lookup::Live(found)) => assert_eq!(found, expected),
                    (None, Lookup::Stale) => {}
                    (expected, found) => panic!("{id:?}: get {found:?}, model {expected:?}"),
                }
            }
        }
        let row = census.row(cap.name()).expect("row");
        assert_eq!(
            (row.high_water, row.crossings, row.capacity_now),
            (model.high_water, model.crossings, model.capacity)
        );
        census.mark_stopping();
        model.reach
    }

    fn add(sum: &mut [u64; 4], run: [u64; 4]) {
        for (total, count) in sum.iter_mut().zip(run) {
            *total += count;
        }
    }

    /// Both policies at both widths, across the chunk and directory edges.
    /// Engagement: each of the four accesses named a handle whose slot had
    /// retired (`Retire` runs) and one whose slot had wrapped back to the
    /// handle's generation while free (`WrapWithProof` runs).
    #[test]
    fn the_slab_equals_its_model_across_chunk_and_directory_edges() {
        let (mut retired, mut wrapped) = ([0; 4], [0; 4]);
        for (index, cap) in [
            Cap::entries::<1>("m-1", CapFill::Serving),
            Cap::entries::<2>("m-2", CapFill::Serving),
            Cap::entries::<63>("m-63", CapFill::Serving),
            Cap::entries::<64>("m-64", CapFill::Serving),
            Cap::entries::<65>("m-65", CapFill::Serving),
            Cap::entries::<300>("m-300", CapFill::Serving),
        ]
        .into_iter()
        .enumerate()
        {
            let seed = 0x51AB + 4 * index as u64;
            add(&mut retired, model_run::<u32, Retire>(cap, seed, 3_000).retired);
            add(&mut retired, model_run::<u64, Retire>(cap, seed + 1, 3_000).retired);
            let wraps = model_run::<u32, WrapWithProof>(cap, seed + 2, 3_000);
            add(&mut wrapped, wraps.free_at_generation);
            let wraps = model_run::<u64, WrapWithProof>(cap, seed + 3, 3_000);
            add(&mut wrapped, wraps.free_at_generation);
        }
        // Get, GetMut, Take, Hook.
        assert!(retired.iter().all(|count| *count > 0), "a retired slot unread: {retired:?}");
        assert!(wrapped.iter().all(|count| *count > 0), "a wrapped slot unread: {wrapped:?}");
    }

    /// Two instances on one census, built and populated by the same ops,
    /// hold the same index and generation pairs; each one's live handle is
    /// `Stale` on the other, for `get`, `get_mut` and `take`, and neither
    /// changes. Without the instance compare a foreign `take` removes the
    /// other's occupant.
    #[test]
    fn another_slabs_live_handle_is_stale_and_changes_neither() {
        let (census, _mark) = census();
        let cap = Cap::entries::<8>("twins", CapFill::Serving);
        let (mut first, _) = built::<u64, u32, Retire>(cap, &census);
        let (mut second, _) = built::<u64, u32, Retire>(cap, &census);
        assert_ne!(first.instance(), second.instance());
        for value in 0..5 {
            let mine = publish(&mut first, value).expect("room");
            let theirs = publish(&mut second, value + 100).expect("room");
            assert_eq!((mine.index, mine.generation), (theirs.index, theirs.generation));
            assert!(matches!(first.get(theirs), Lookup::Stale), "slab-instance-blind: get");
            assert!(matches!(first.get_mut(theirs), LookupMut::Stale), "slab-instance-blind");
            assert_eq!(first.take(theirs), Taken::Stale, "slab-instance-blind: take");
            assert!(matches!(second.get(theirs), Lookup::Live(v) if *v == value + 100));
            assert!(matches!(first.get(mine), Lookup::Live(v) if *v == value));
        }
        assert_eq!((first.len(), second.len()), (5, 5));
    }

    #[test]
    fn a_census_refuses_its_store_past_its_mints_and_builds_nothing() {
        let (census, _mark) = census();
        for _ in 0..CAP_CENSUS_SLABS_MAX {
            census.mint_instance().expect("within the ordinals");
        }
        let cap = Cap::entries::<4>("late", CapFill::Serving);
        let refused = CappedSlab::<u8, u32, Retire>::builder(cap, &census);
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Census));
        assert_eq!(census.row("late").expect("the row stays").high_water, 0);
    }

    #[test]
    fn instances_name_their_cell_and_count_from_one() {
        let (census, _mark) = census();
        let (first, second) = (census.mint_instance(), census.mint_instance());
        let (first, second) = (first.expect("one"), second.expect("two"));
        assert_eq!((first.cell(), first.ordinal()), (CELL, 1));
        assert_eq!((second.cell(), second.ordinal()), (CELL, 2));
    }

    /// `Retire` at the last generation: the slot leaves for good and the
    /// slab serves one fewer, reported to the row; with every remaining slot
    /// live `reserve` is `Full` until a take, and `Exhausted` only once the
    /// last slot retired (ADR-0171 A1).
    #[test]
    fn retire_shrinks_the_capacity_to_exhausted_and_reports_it() {
        let (census, _mark) = census();
        let cap = Cap::entries::<2>("retiring", CapFill::Serving);
        let (mut slab, _) = built::<u8, u32, Retire>(cap, &census);
        let mut first = publish(&mut slab, 1).expect("room");
        let second = publish(&mut slab, 2).expect("room");
        assert!(matches!(slab.place_generation_last(&mut first), LookupMut::Live(1)));
        assert_eq!(slab.take(first), Taken::Value(1));
        assert_eq!(slab.capacity_now(), 1);
        assert_eq!(census.row("retiring").expect("row").capacity_now, 1);
        assert!(matches!(slab.reserve(), SlabReserved::Full), "the one slot left is live");
        let mut second = match slab.take(second) {
            Taken::Value(2) => publish(&mut slab, 3).expect("a take frees the slot"),
            other => panic!("{other:?}"),
        };
        assert!(matches!(slab.get(first), Lookup::Stale), "a retired slot's handle");
        assert!(matches!(slab.place_generation_last(&mut second), LookupMut::Live(3)));
        assert_eq!(slab.take(second), Taken::Value(3));
        assert_eq!((slab.capacity_now(), slab.len()), (0, 0));
        assert!(matches!(slab.reserve(), SlabReserved::Exhausted));
        let row = census.row("retiring").expect("row");
        assert_eq!((row.capacity_now, row.crossings), (0, 1), "Exhausted is no crossing");
    }

    /// `WrapWithProof` at the last generation wraps to the first, and the
    /// old holder's handle, planted, is caught stale.
    #[test]
    fn wrap_with_proof_wraps_and_the_planted_stale_holder_is_stale() {
        let (census, _mark) = census();
        let cap = Cap::entries::<1>("wrapping", CapFill::Serving);
        let (mut slab, _) = built::<u8, u32, WrapWithProof>(cap, &census);
        let mut held = publish(&mut slab, 1).expect("room");
        assert!(matches!(slab.place_generation_last(&mut held), LookupMut::Live(1)));
        let stale = held;
        assert_eq!(slab.take(held), Taken::Value(1));
        let fresh = publish(&mut slab, 2).expect("the slot wrapped and serves");
        assert_eq!((fresh.index, fresh.generation), (stale.index, 0));
        assert!(matches!(slab.get(stale), Lookup::Stale));
        assert_eq!(slab.take(stale), Taken::Stale);
        assert_eq!(slab.capacity_now(), 1, "a wrap never shrinks");
    }

    /// A wrap brings a free slot back to the first generation, the one its
    /// first occupant's handle names: that handle names no occupant now, so
    /// it is `Stale`, as every handle of this slab that is not its slot's
    /// live occupant; once the slot is published again it names the new
    /// occupant, the reuse a `WrapWithProof` site's proof rules out.
    #[test]
    fn a_handle_from_before_a_wrap_is_stale_while_its_slot_is_free() {
        let (census, _mark) = census();
        let cap = Cap::entries::<1>("wrapped", CapFill::Serving);
        let (mut slab, _) = built::<u8, u32, WrapWithProof>(cap, &census);
        let first = publish(&mut slab, 1).expect("room");
        assert_eq!(slab.take(first), Taken::Value(1));
        let mut second = publish(&mut slab, 2).expect("room");
        assert!(matches!(slab.place_generation_last(&mut second), LookupMut::Live(2)));
        assert_eq!(slab.take(second), Taken::Value(2));
        assert!(matches!(slab.get(first), Lookup::Stale), "a free slot at its generation");
        assert!(matches!(slab.get_mut(first), LookupMut::Stale));
        assert_eq!(slab.take(first), Taken::Stale);
        let third = publish(&mut slab, 3).expect("room");
        assert_eq!(third, first, "the wrap reissues the first handle");
        assert!(matches!(slab.get(first), Lookup::Live(3)));
    }

    #[test]
    fn the_hook_answers_like_get_mut_for_a_stale_or_foreign_handle() {
        let (census, _mark) = census();
        let cap = Cap::entries::<2>("hooked", CapFill::Serving);
        let (mut slab, _) = built::<u8, u32, Retire>(cap, &census);
        let (mut other, _) = built::<u8, u32, Retire>(cap, &census);
        let mut id = publish(&mut slab, 1).expect("room");
        let mut foreign = publish(&mut other, 1).expect("room");
        assert!(matches!(slab.place_generation_last(&mut foreign), LookupMut::Stale));
        assert_eq!(slab.take(id), Taken::Value(1));
        assert!(matches!(slab.place_generation_last(&mut id), LookupMut::Stale));
        assert_eq!(id.generation, 0, "a stale handle is not re-stamped");
    }

    // The fill byte's removal leg: an `Assembly` slab filled to its cap and
    // one taken before the mark is caught by the live count at the mark;
    // full at the mark is the control.

    #[test]
    fn an_assembly_slab_full_at_the_mark_is_the_control() {
        let (census, mark) = census();
        let cap = Cap::entries::<3>("owners", CapFill::Assembly);
        let (mut slab, _) = built::<u8, u32, Retire>(cap, &census);
        for value in 0..3 {
            publish(&mut slab, value).expect("room");
        }
        mark.take();
        assert_eq!(census.cap_assembly_violations(), 0);
        let row = census.row("owners").expect("row");
        assert_eq!((row.assembly_live, row.high_water), (3, 3));
        census.mark_stopping();
    }

    #[test]
    fn an_assembly_slab_with_one_taken_before_the_mark_is_asserted_and_counted() {
        let (census, mark) = census();
        let cap = Cap::entries::<3>("owners", CapFill::Assembly);
        let (mut slab, _) = built::<u8, u32, Retire>(cap, &census);
        let ids: Vec<_> = (0..3).map(|value| publish(&mut slab, value).expect("room")).collect();
        assert_eq!(slab.take(ids[1]), Taken::Value(1));
        assert_eq!(census.row("owners").expect("row").assembly_live, 2, "the removal reported");
        let outcome = catch_unwind(AssertUnwindSafe(|| mark.take()));
        if cfg!(debug_assertions) {
            let text = panic_text(outcome.expect_err("asserted").as_ref());
            assert!(text.contains("cap-assembly-violated"), "{text}");
        }
        assert_eq!(census.cap_assembly_violations(), 1);
        census.mark_stopping();
    }

    // The unsliced drops: a live slab, a builder at `Built::More` and a
    // retiring prefix, each dropped holding backing while the cell serves,
    // are counted and asserted; the same once stopping, or while
    // assembling, are not.

    fn expect_unsliced(census: &Rc<CapCensus>, drop_it: impl FnOnce()) {
        let before = census.unsliced_drops();
        let outcome = catch_unwind(AssertUnwindSafe(drop_it));
        if cfg!(debug_assertions) {
            let text = panic_text(outcome.expect_err("unsliced-drop: asserted").as_ref());
            assert!(text.contains("unsliced-drop"), "{text}");
        }
        assert_eq!(census.unsliced_drops(), before + 1, "unsliced-drop: counted");
    }

    #[test]
    fn a_live_slab_dropped_while_serving_is_an_unsliced_drop() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("dropped", CapFill::Serving);
        let (slab, _) = built::<u8, u32, Retire>(cap, &census);
        mark.take();
        expect_unsliced(&census, || drop(slab));
    }

    #[test]
    fn a_builder_abandoned_at_more_while_serving_is_an_unsliced_drop() {
        let (census, mark) = census();
        mark.take();
        let cap = Cap::entries::<100>("abandoned", CapFill::Serving);
        let builder = CappedSlab::<u8, u32, Retire>::builder(cap, &census).expect("registered");
        let Built::More(builder) = builder.step() else { panic!("a directory, then chunks") };
        expect_unsliced(&census, || drop(builder));
    }

    #[test]
    fn a_retiring_prefix_dropped_mid_steps_while_serving_is_an_unsliced_drop() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("halfway", CapFill::Serving);
        let (slab, _) = built::<u8, u32, Retire>(cap, &census);
        mark.take();
        let Retiring::More(retiring) = slab.retire().step() else { panic!("three items") };
        expect_unsliced(&census, || drop(retiring));
    }

    #[test]
    fn a_slab_dropped_once_stopping_or_while_assembling_is_not_counted() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("teardown", CapFill::Serving);
        let (assembling, _) = built::<u8, u32, Retire>(cap, &census);
        drop(assembling);
        let (stopping, _) = built::<u8, u32, Retire>(cap, &census);
        mark.take();
        census.mark_stopping();
        assert_eq!(census.phase(), CensusPhase::Stopping);
        drop(stopping);
        assert_eq!(census.unsliced_drops(), 0);
    }

    /// The retire path every site uses, on a serving cell: one item per
    /// step, so nothing is dropped whole. A step that dropped the rest
    /// whole is counted, and asserted in a debug build.
    #[test]
    fn a_live_slab_retires_by_steps_with_no_unsliced_drop() {
        let (census, mark) = census();
        let cap = Cap::entries::<1000>("retired", CapFill::Serving);
        let (mut slab, _) = built::<u64, u32, Retire>(cap, &census);
        for value in 0..10 {
            publish(&mut slab, value).expect("room");
        }
        mark.take();
        assert_eq!(retired(slab.retire()), 17);
        assert_eq!(
            census.unsliced_drops(),
            0,
            "unsliced-drop: a retire step dropped backing whole"
        );
    }
}
