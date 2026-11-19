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
//! nothing but its census row's crossings. A chunk holds `CHUNK_SLOTS`
//! slots, 64 by default: the count `chunk_slots_for` answers for the slot's
//! bytes, a parameter because a count read from `size_of` cannot be an array
//! length on stable Rust, so a slab of wide slots names its count where it is
//! declared, and the builder refuses any other (ADR-0194 A1).
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
use super::store::{BuildStep, Chain, FreeSlot, RetireStep as StoreRetire, Slot, SlotStore};
use super::{Cap, CapError, CapFill};
use crate::limits::MAP_CHUNK_SLOTS_MAX;

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
    /// The generation's width in bits: a word's generation field is at most
    /// this wide (ADR-0194 D6).
    const BITS: u32;
    /// The next generation, none past `LAST`.
    fn after(self) -> Option<Self>;
    /// The next generation, `LAST` wrapping to `FIRST`.
    fn after_wrapped(self) -> Self;
    /// The generation zero-extended to a word, whose low bits a word's
    /// generation field carries.
    fn widened(self) -> u64;
}

macro_rules! generation {
    ($($width:ty),*) => {$(
        impl Generation for $width {
            const FIRST: Self = 0;
            const LAST: Self = <$width>::MAX;
            const BITS: u32 = <$width>::BITS;

            #[inline]
            fn after(self) -> Option<Self> {
                self.checked_add(1)
            }

            #[inline]
            fn after_wrapped(self) -> Self {
                // ADR-0165 D1 `bits`: a generation's wrap is its policy.
                self.wrapping_add(1)
            }

            #[inline]
            fn widened(self) -> u64 {
                u64::from(self)
            }
        }
    )*};
}
generation!(u32, u64);

/// What a slot's generation does at a `take` (ADR-0171 D2), a type
/// parameter of the slab so the advance is static.
pub trait GenerationPolicy: sealed::Sealed {
    /// Whether the generation wraps at its last value: a word may then
    /// carry fewer of its bits (ADR-0194 D6).
    const WRAPS: bool;
    /// The generation the slot's next occupant takes, or `None`: the slot
    /// retires and the slab serves one slot fewer.
    fn advance<G: Generation>(generation: G) -> Option<G>;
}

/// At the last generation the slot retires for the owner's lifetime, so a
/// handle that outlives its occupant can never name a new one; the slab's
/// capacity shrinks by one, and with none left `reserve` is `Exhausted`.
pub struct Retire;

impl GenerationPolicy for Retire {
    const WRAPS: bool = false;

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
    const WRAPS: bool = true;

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

/// The answer to `release`: the occupant dropped where it lay, or why none
/// (a `Stale` or `Vacant` handle as `take`'s, nothing changed).
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Released {
    Dropped,
    Stale,
    Vacant,
}

/// The answer to `get_mut_by_word`: the occupant a word names, with its
/// slot's whole handle, or why there is none. A word of another slab's
/// adapter, or one whose generation field is not the low bits of the slot's
/// occupant's generation (a free, retired or later-occupied slot), is
/// `Stale`; a word naming no slot of this slab is `Vacant`.
#[must_use]
#[derive(Debug)]
pub enum WordLookup<'a, T, G> {
    Live(SlotId<G>, &'a mut T),
    Stale,
    Vacant,
}

/// What `vacated` left of a slot's transition for its off-path step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vacated {
    Written,
    Assembling,
    Retiring,
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
///
/// It holds no value (ADR-0194 D1) and names its slab's row and counts by
/// their one address: a step that holds a reservation across a call keeps
/// each of its words through that call, so a pointer a fact would be a
/// store and a load a fact.
#[must_use = "a reservation publishes a value or is dropped; it carries none"]
pub struct SlotReservation<'a, T, G, P> {
    slot: FreeSlot<'a, T, G>,
    counts: &'a mut SlabCounts,
    instance: SlabInstance,
    policy: PhantomData<P>,
}

/// A reservation whose handle is known before its publish (ADR-0194 D1 with
/// A2), on a `WrapWithProof` slab alone: `id()` answers the handle `publish`
/// will return, every access by which is `Stale` until then. Dropped
/// unpublished, on any path, unwinding included, the slot's generation
/// advances by the policy and the slot stays free at the chain's head, so
/// that handle names no later occupant. It borrows the slab, as the
/// reservation it holds does, so it cannot cross an `await`.
#[must_use = "a named reservation publishes a value or is dropped, advancing its generation"]
pub struct NamedReservation<'a, T, G: Generation> {
    id: SlotId<G>,
    /// `Some` until `publish` takes it; the drop advances the generation of
    /// what is left.
    reservation: Option<SlotReservation<'a, T, G, WrapWithProof>>,
}

/// The adapter that carries one slab's handles in a word (ADR-0194 D6 with
/// A2): the low `INDEX_BITS` hold the slot's index, the next
/// `GENERATION_BITS` the low bits of its generation. It is built from the
/// slab it serves, and a word is decoded only by that slab's
/// `get_mut_by_word`, which compares the carried bits on the slot it walks
/// to, so no handle is ever built from a truncated generation. On a
/// `WrapWithProof` slab a narrower field aliases a later occupant only
/// after `k × 2^GENERATION_BITS` uses of a slot, which the slab's proof
/// makes a defect, never reuse.
///
/// The layout is the compiler's to check: the two fields fit one `u64`, the
/// generation field fits the generation, and a `Retire` slab's word carries
/// its whole generation (a retired slot's handle must never alias). A slab
/// of more slots than the index field numbers is refused `Layout` when its
/// adapter is built. A fabric key's layout, built and resolved:
///
/// ```
/// # use inf_foundation::CellId;
/// # use inf_foundation::bounded::{Built, Cap, CapCensus, CapFill, CappedSlab, SlabReserved};
/// # use inf_foundation::bounded::{SlotWord, WordLookup, WrapWithProof};
/// use inf_foundation::limits::{GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS};
/// # let (census, _serve_mark) = CapCensus::new(CellId(0));
/// # let cap = Cap::entries::<4>("holders", CapFill::Serving);
/// # let builder = CappedSlab::<u64, u32, WrapWithProof>::builder(cap, &census).expect("fits");
/// # let Built::Live(mut slab) = builder.step() else { panic!("one chunk holds four") };
/// type Key = SlotWord<u32, WrapWithProof, GATE_KEY_INDEX_BITS, GATE_KEY_GENERATION_BITS>;
/// let key = Key::for_slab(&slab).expect("four slots number in 24 bits");
/// let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
/// let id = slot.publish(7);
/// let bits = key.encode(id).expect("this slab's handle");
/// let found = slab.get_mut_by_word(&key, bits);
/// assert!(matches!(found, WordLookup::Live(handle, value) if handle == id && *value == 7));
/// ```
///
/// The index and generation fields of 33 and 32 bits pass one word:
///
/// ```compile_fail,E0080
/// # use inf_foundation::CellId;
/// # use inf_foundation::bounded::{Built, Cap, CapCensus, CapFill, CappedSlab};
/// # use inf_foundation::bounded::{SlotWord, WrapWithProof};
/// # let (census, _serve_mark) = CapCensus::new(CellId(0));
/// # let cap = Cap::entries::<1>("word", CapFill::Serving);
/// # let builder = CappedSlab::<u8, u32, WrapWithProof>::builder(cap, &census).expect("fits");
/// # let Built::Live(slab) = builder.step() else { panic!("one chunk") };
/// let _word = SlotWord::<u32, WrapWithProof, 33, 32>::for_slab(&slab);
/// ```
///
/// A generation field of 33 bits passes a `u32` generation:
///
/// ```compile_fail,E0080
/// # use inf_foundation::CellId;
/// # use inf_foundation::bounded::{Built, Cap, CapCensus, CapFill, CappedSlab};
/// # use inf_foundation::bounded::{SlotWord, WrapWithProof};
/// # let (census, _serve_mark) = CapCensus::new(CellId(0));
/// # let cap = Cap::entries::<1>("word", CapFill::Serving);
/// # let builder = CappedSlab::<u8, u32, WrapWithProof>::builder(cap, &census).expect("fits");
/// # let Built::Live(slab) = builder.step() else { panic!("one chunk") };
/// let _word = SlotWord::<u32, WrapWithProof, 24, 33>::for_slab(&slab);
/// ```
///
/// A `Retire` slab's word at 23 of its generation's 32 bits:
///
/// ```compile_fail,E0080
/// # use inf_foundation::CellId;
/// # use inf_foundation::bounded::{Built, Cap, CapCensus, CapFill, CappedSlab};
/// # use inf_foundation::bounded::{Retire, SlotWord};
/// # let (census, _serve_mark) = CapCensus::new(CellId(0));
/// # let cap = Cap::entries::<1>("word", CapFill::Serving);
/// # let builder = CappedSlab::<u8, u32, Retire>::builder(cap, &census).expect("fits");
/// # let Built::Live(slab) = builder.step() else { panic!("one chunk") };
/// let _word = SlotWord::<u32, Retire, 24, 23>::for_slab(&slab);
/// ```
pub struct SlotWord<G, P, const INDEX_BITS: u32, const GENERATION_BITS: u32> {
    instance: SlabInstance,
    /// The slab's slots: a word's index at or past it names no slot.
    slots: u32,
    layout: PhantomData<(G, P)>,
}

/// A slab's census row handle, the fill byte first, then its live count and
/// its high water: what a publish and a take count and report by. One part
/// of the slab, so a reservation borrows the three by one pointer beside
/// its borrow of the store. 24 bytes.
#[repr(C)]
struct SlabCounts {
    row: CapRowHandle,
    live: u32,
    high_water: u32,
}

/// A live capped slab: its row and counts, then its store, 64 bytes in all,
/// so a take's loads (the fill byte, the counts, the instance, the free head
/// and the root) lie in one line when the slab starts on one.
#[repr(C)]
pub struct CappedSlab<T, G, P, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    counts: SlabCounts,
    store: SlotStore<T, G, CHUNK_SLOTS>,
    policy: PhantomData<P>,
}

/// A slab being built, one chunk or directory per step; it serves nothing.
/// Its instance is minted, so a handle that must name its owner can read it
/// before the slab is live.
pub struct SlabBuilder<T, G, P, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    row: CapRowHandle,
    store: SlotStore<T, G, CHUNK_SLOTS>,
    /// The generation every slot a step builds is free at: `G::FIRST` but
    /// under the test hook `first_generation`, which no shipping build
    /// compiles, so a live slab's header is the same in every build.
    #[cfg(any(test, feature = "fault-points"))]
    first_generation: G,
    policy: PhantomData<P>,
}

/// A slab being retired, or a refused build's prefix: one chunk or
/// directory freed per step, children before their directory.
pub struct SlabRetiring<T, G, P, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    store: SlotStore<T, G, CHUNK_SLOTS>,
    policy: PhantomData<P>,
}

/// The answer of a build step.
pub enum Built<T, G, P, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    More(SlabBuilder<T, G, P, CHUNK_SLOTS>),
    Live(CappedSlab<T, G, P, CHUNK_SLOTS>),
    /// A step's allocation was refused: nothing was served, and what was
    /// built retires by steps.
    Refused(SlabRetiring<T, G, P, CHUNK_SLOTS>, CapError),
}

/// The answer of a retire step.
pub enum Retiring<T, G, P, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    More(SlabRetiring<T, G, P, CHUNK_SLOTS>),
    Done,
}

impl<T, G: Generation, P, const CHUNK_SLOTS: usize> CappedSlab<T, G, P, CHUNK_SLOTS> {
    /// The bytes of one slot of this slab, its value and its generation and
    /// state beside it: a site that builds a slab of wide slots reads its
    /// chunk count from this (`chunk_slots_for`) without naming the slot.
    pub const SLOT_BYTES: usize = mem::size_of::<Slot<T, G>>();
}

/// The take transition (ADR-0194 D1's take row), written once and compiled as
/// two bodies: `take`, which most takers call, and `take_inlined`, in a
/// delivered poll's own frame. A macro and not a function: a `take` that
/// only called `take_inlined` is folded by the compiler into each of its
/// callers, and every taker then holds the whole body.
macro_rules! take_transition {
    ($slab:ident, $id:ident) => {{
        let foreign = $slab.foreign(&$id);
        let lent = named(
            foreign,
            // Inlined always: a body of its own where two steps of one slab
            // take, the walk would be inline where one poll does, and a
            // bench would not be placed as the server is (ADR-0210 D1).
            #[inline(always)]
            || $slab.store.slot_chain_mut($id.index).map(|(slot, chain)| Vacate(slot, chain)),
            $id.generation,
        );
        match lent {
            Named::Live((value, slot, chain)) => {
                let left =
                    Self::vacated(slot, chain, $id, &mut $slab.counts.live, &$slab.counts.row);
                if left != Vacated::Written {
                    $slab.vacated_off_path($id, left);
                }
                Taken::Value(value)
            }
            Named::Stale => Taken::Stale,
            Named::Vacant => Taken::Vacant,
        }
    }};
}

impl<T, G: Generation, P: GenerationPolicy, const CHUNK_SLOTS: usize>
    CappedSlab<T, G, P, CHUNK_SLOTS>
{
    /// A slab of `cap` on `census`, to build by steps: the layout is checked
    /// (a chunk count other than the one `chunk_slots_for` answers for the
    /// slot's bytes, a chunk or a directory past one backing allocation, or
    /// a build of more chunks and directories than a `u32` counts, is
    /// `Layout`, and no row is registered), then the row is registered, then
    /// the instance minted (`Census` past its mints; the row stays). Nothing
    /// is allocated until the first step.
    pub fn builder(
        cap: Cap,
        census: &Rc<CapCensus>,
    ) -> Result<SlabBuilder<T, G, P, CHUNK_SLOTS>, CapError> {
        if !SlotStore::<T, G, CHUNK_SLOTS>::layout_fits(cap.entries_max()) {
            return Err(CapError::Layout);
        }
        let row = census.register(cap)?;
        let store = SlotStore::new(cap.entries_max(), census)?;
        Ok(SlabBuilder {
            row,
            store,
            #[cfg(any(test, feature = "fault-points"))]
            first_generation: G::FIRST,
            policy: PhantomData,
        })
    }

    /// Live occupants, at most `capacity_now`.
    #[must_use]
    #[inline]
    pub fn len(&self) -> u32 {
        self.counts.live
    }

    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.counts.live == 0
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
        self.counts.high_water
    }

    /// This slab's identity, stamped in every handle it mints.
    #[must_use]
    pub fn instance(&self) -> SlabInstance {
        self.store.instance()
    }

    /// A free slot when one can serve; `Full` when every slot that can
    /// serve is live (the row's crossings up by one); `Exhausted` when every
    /// slot has retired.
    // Inlined where it is called, so its answer stays in registers: out of
    // line it is returned through memory, a store and a load a word.
    #[inline(always)]
    pub fn reserve(&mut self) -> SlabReserved<'_, T, G, P> {
        if !self.has_room() {
            return if self.store.capacity_now() == 0 {
                SlabReserved::Exhausted
            } else {
                self.full()
            };
        }
        let (instance, capacity) = (self.store.instance(), self.store.capacity_now());
        let CappedSlab { counts, store, .. } = self;
        match store.reserve() {
            Some(slot) => {
                SlabReserved::Slot(SlotReservation { slot, counts, instance, policy: PhantomData })
            }
            // The chain holds `capacity_now - live` slots, so this arm is
            // never reached by a correct build.
            None => {
                debug_assert!(
                    counts.live >= capacity,
                    "the free chain is empty with live below capacity_now"
                );
                counts.row.report_full();
                SlabReserved::Full
            }
        }
    }

    /// The occupant `id` names, if it is this slab's and current.
    #[inline]
    pub fn get(&self, id: SlotId<G>) -> Lookup<'_, T> {
        match named(self.foreign(&id), || self.store.slot(id.index), id.generation) {
            Named::Live(value) => Lookup::Live(value),
            Named::Stale => Lookup::Stale,
            Named::Vacant => Lookup::Vacant,
        }
    }

    /// The occupant `id` names, mutably, if it is this slab's and current.
    #[inline]
    pub fn get_mut(&mut self, id: SlotId<G>) -> LookupMut<'_, T> {
        match named(self.foreign(&id), || self.store.slot_mut(id.index), id.generation) {
            Named::Live((_, value)) => LookupMut::Live(value),
            Named::Stale => LookupMut::Stale,
            Named::Vacant => LookupMut::Vacant,
        }
    }

    /// The occupant a word of `word` names, mutably, with its slot's whole
    /// handle: the one decode of a word (ADR-0194 D6 with A2). An adapter of
    /// another slab is `Stale` and nothing is walked; a word naming no slot
    /// (a bit set above its two fields, which no encode sets, or an index at
    /// or past the slab's slots) is `Vacant`; else one walk, `Live` when the
    /// slot's occupant took a generation whose low bits are the word's
    /// field, `Stale` for any other slot. Nothing is written.
    #[inline]
    pub fn get_mut_by_word<const INDEX_BITS: u32, const GENERATION_BITS: u32>(
        &mut self,
        word: &SlotWord<G, P, INDEX_BITS, GENERATION_BITS>,
        bits: u64,
    ) -> WordLookup<'_, T, G> {
        let instance = self.store.instance();
        if word.instance != instance {
            return WordLookup::Stale;
        }
        let Some((index, field)) = word.fields(bits) else { return WordLookup::Vacant };
        let mask = SlotWord::<G, P, INDEX_BITS, GENERATION_BITS>::GENERATION_MASK;
        match named(false, || self.store.slot_mut(index), WordField { field, mask }) {
            Named::Live((generation, value)) => {
                WordLookup::Live(SlotId { index, generation, instance }, value)
            }
            Named::Stale => WordLookup::Stale,
            Named::Vacant => WordLookup::Vacant,
        }
    }

    /// The occupant `id` names returns to its owner and the slot frees: its
    /// generation advances by the policy, or at the last it retires and the
    /// slab serves one slot fewer, reported to the row before any `Full` at
    /// the new count. An `Assembly` slab reports the removal (its fill byte,
    /// tested once). A stale or foreign handle changes nothing. `release`'s
    /// body with the value moved out first. The one called body of the
    /// transition: every taker calls it but a delivered poll.
    #[inline]
    pub fn take(&mut self, id: SlotId<G>) -> Taken<T> {
        take_transition!(self, id)
    }

    /// [`take`](Self::take) in its caller's frame (ADR-0194 D1's take row,
    /// a second entry to it): for the poll that takes a delivered value and
    /// reads it at once. Called, a take returns its occupant through memory
    /// and the caller copies it out again; in the caller's frame it does not.
    /// Each site pays the body's text, so no other taker enters here.
    #[inline(always)]
    pub fn take_inlined(&mut self, id: SlotId<G>) -> Taken<T> {
        take_transition!(self, id)
    }

    /// `take` with nothing returned (ADR-0194 D1's second note): the same
    /// transition and effects, the occupant dropped where it lies, after the
    /// counts and the free chain are written, so a site whose occupant is
    /// wide or already ended frees its slot without moving it, and an
    /// occupant whose drop panics leaves the slab as this leaves it. A stale
    /// or foreign handle changes nothing. One canary moves the occupant out
    /// as `take` does and drops it off its slot.
    #[inline]
    pub fn release(&mut self, id: SlotId<G>) -> Released {
        #[cfg(inf_canary_release_moves_occupant)]
        {
            match self.take(id) {
                Taken::Value(moved) => {
                    drop(moved);
                    Released::Dropped
                }
                Taken::Stale => Released::Stale,
                Taken::Vacant => Released::Vacant,
            }
        }
        #[cfg(not(inf_canary_release_moves_occupant))]
        {
            let foreign = self.foreign(&id);
            let lent = named(
                foreign,
                || self.store.slot_chain_mut(id.index).map(|(slot, chain)| InPlace(slot, chain)),
                id.generation,
            );
            match lent {
                Named::Live((slot, chain)) => {
                    let left =
                        Self::vacated(slot, chain, id, &mut self.counts.live, &self.counts.row);
                    if left != Vacated::Written {
                        self.vacated_off_path(id, left);
                    }
                    Released::Dropped
                }
                Named::Stale => Released::Stale,
                Named::Vacant => Released::Vacant,
            }
        }
    }

    /// Retirement of the whole slab, by steps.
    pub fn retire(self) -> SlabRetiring<T, G, P, CHUNK_SLOTS> {
        SlabRetiring { store: self.store, policy: PhantomData }
    }

    /// A test hook: `get_mut`'s lookup, then the live slot's generation and
    /// the handle's set to the last value, so the next `take` advances past
    /// it — the policy's last-value branch on the real path. It asserts
    /// nothing and computes nothing; `Stale` and `Vacant` answer as
    /// `get_mut` does.
    #[cfg(any(test, feature = "fault-points"))]
    pub fn place_generation_last(&mut self, id: &mut SlotId<G>) -> LookupMut<'_, T> {
        let index = id.index;
        match named(self.foreign(id), || self.store.slot_mut(index), id.generation) {
            Named::Live((generation, value)) => {
                *generation = G::LAST;
                id.generation = G::LAST;
                LookupMut::Live(value)
            }
            Named::Stale => LookupMut::Stale,
            Named::Vacant => LookupMut::Vacant,
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
            let serving = self.counts.row.fill() == CapFill::Serving;
            let short = capacity.checked_sub(1).unwrap_or_default();
            self.counts.live < if serving { short } else { capacity }
        }
        #[cfg(not(inf_canary_cap_minus_one))]
        {
            self.counts.live < capacity
        }
    }

    /// The crossing, off the hot path.
    #[cold]
    #[inline(never)]
    fn full(&mut self) -> SlabReserved<'_, T, G, P> {
        self.counts.row.report_full();
        SlabReserved::Full
    }

    /// D1's take transition on a slot `named` found live at `id`'s
    /// generation, the one body of `take` and `release`, in this order: the
    /// live count, an `Assembly` row's removal report, the policy's advance
    /// with the free head linked or the slot's retirement with the row's
    /// capacity report, and the slot's write last, which drops an occupant
    /// still in it (`release`'s) where it lies. A serving slab's advance is
    /// written here; a report and a retirement are left, the slot unwritten,
    /// for `vacated_off_path` once the lookup's borrow ends, so the serving
    /// path holds nothing across a call.
    #[inline(always)]
    fn vacated(
        slot: &mut Slot<T, G>,
        chain: Chain<'_>,
        id: SlotId<G>,
        live: &mut u32,
        row: &CapRowHandle,
    ) -> Vacated {
        *live = live_taken(*live);
        if row.fill() == CapFill::Assembly {
            return Vacated::Assembling;
        }
        Self::advanced(slot, chain, id)
    }

    /// The policy's advance with the free head linked and the slot's write,
    /// or, at the last generation, the retirement left to the caller. The arm
    /// writes the slot itself, so no slot-sized value is built apart.
    #[inline(always)]
    fn advanced(slot: &mut Slot<T, G>, chain: Chain<'_>, id: SlotId<G>) -> Vacated {
        match P::advance(id.generation) {
            Some(generation) => {
                let next = chain.free(id.index);
                *slot = Slot::Free { generation, next };
                Vacated::Written
            }
            None => Vacated::Retiring,
        }
    }

    /// The rest of a vacated slot off the serving path, the slot found again
    /// by its index once the lookup's borrow ended (the caller found it live
    /// a step ago): an `Assembly` row's removal reported, then the advance,
    /// and a retirement.
    #[cold]
    #[inline(never)]
    fn vacated_off_path(&mut self, id: SlotId<G>, left: Vacated) {
        let mut left = left;
        if left == Vacated::Assembling {
            self.counts.row.report_removal(self.counts.live);
            let found = self.store.slot_chain_mut(id.index);
            debug_assert!(found.is_some(), "a vacated slot is in its store");
            let Some((slot, chain)) = found else { return };
            left = Self::advanced(slot, chain, id);
        }
        if left == Vacated::Retiring {
            let CappedSlab { counts, store, .. } = self;
            let found = store.slot_chain_mut(id.index);
            debug_assert!(found.is_some(), "a vacated slot is in its store");
            let Some((slot, chain)) = found else { return };
            Self::retired(slot, chain, &counts.row);
        }
    }

    /// A slot retired at its last generation, off the hot path: the store
    /// serves one fewer, the row keeps the minimum capacity over its
    /// instances, and the slot is closed last.
    #[cold]
    #[inline(never)]
    fn retired(slot: &mut Slot<T, G>, chain: Chain<'_>, row: &CapRowHandle) {
        row.report_capacity(chain.close());
        *slot = Slot::Closed;
    }
}

impl<'a, T, G: Generation, P> SlotReservation<'a, T, G, P> {
    /// The value takes the slot, and its handle is returned. Cannot allocate
    /// or refuse: the slot was free and the reservation held it.
    #[inline]
    pub fn publish(self, value: T) -> SlotId<G> {
        self.publish_mut(value).0
    }

    /// `publish`, with the occupant lent for the reservation's borrow
    /// (ADR-0194 D1's note): a site that uses the value in the step that
    /// published it walks no directory, and the lend and each later
    /// `get_mut` of the handle name one address. Inlined always, as
    /// `reserve` is: out of line, its answer would return through memory.
    #[must_use = "the handle is the slot's one name"]
    #[inline(always)]
    pub fn publish_mut(self, value: T) -> (SlotId<G>, &'a mut T) {
        let SlotReservation { slot, counts, instance, .. } = self;
        let (index, generation, occupant) = slot.occupy_mut(value);
        let live = live_published(counts.live);
        counts.live = live;
        // An `Assembly` slab keeps its own high water at 0, so this one
        // compare reports its every publish and the row keeps its live count.
        if live > counts.high_water {
            if counts.row.fill() == CapFill::Serving {
                counts.high_water = live;
            }
            counts.row.report_publish(live);
        }
        (SlotId { index, generation, instance }, occupant)
    }
}

impl<'a, T, G: Generation> SlotReservation<'a, T, G, WrapWithProof> {
    /// This reservation with its handle known before its publish (ADR-0194
    /// D1 with A2): the handle a request must carry before the value it
    /// will name is published. Dropped unpublished, it advances the slot's
    /// generation; an unnamed reservation's drop keeps it.
    #[inline]
    pub fn named(self) -> NamedReservation<'a, T, G> {
        let id = SlotId {
            index: self.slot.index(),
            generation: self.slot.generation(),
            instance: self.instance,
        };
        NamedReservation { id, reservation: Some(self) }
    }

    /// The slot left free at the chain's head at its next generation, the
    /// policy's wrap included, so a handle read from this reservation names
    /// no later occupant. One canary leaves the generation unchanged.
    #[inline]
    fn advanced_unpublished(self) {
        #[cfg(inf_canary_named_drop_keeps_generation)]
        {
            drop(self);
        }
        #[cfg(not(inf_canary_named_drop_keeps_generation))]
        {
            let next = self.slot.generation().after_wrapped();
            self.slot.free_at(next);
        }
    }
}

impl<T, G: Generation> NamedReservation<'_, T, G> {
    /// The handle `publish` will return: the slot's index, the generation it
    /// is free at, and the slab's identity. Every access by it is `Stale`
    /// until the publish; after a drop, at every later occupant.
    #[must_use]
    #[inline]
    pub fn id(&self) -> SlotId<G> {
        self.id
    }

    /// The value takes the slot, and `id()` is returned. Cannot allocate or
    /// refuse: the slot was free and the reservation held it.
    #[inline]
    pub fn publish(mut self, value: T) -> SlotId<G> {
        let reservation = self.reservation.take();
        let named = self.id;
        // What is left has nothing to advance, so its drop would do nothing.
        // Forgotten before any step that can unwind, it is named by no
        // unwind path either: such a path would hand its address to the
        // drop, and the reservation would live in its caller's memory.
        core::mem::forget(self);
        let Some(reservation) = reservation else {
            unreachable!("a named reservation holds its slot until it publishes or drops")
        };
        let id = reservation.publish(value);
        debug_assert_eq!(id, named, "a named reservation publishes the handle it named");
        id
    }
}

impl<T, G: Generation> Drop for NamedReservation<'_, T, G> {
    /// Unpublished, the slot's generation advances: on every end but
    /// `publish`, which took the reservation.
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            reservation.advanced_unpublished();
        }
    }
}

impl<G: Generation, P: GenerationPolicy, const INDEX_BITS: u32, const GENERATION_BITS: u32>
    SlotWord<G, P, INDEX_BITS, GENERATION_BITS>
{
    /// The layout's rules, one assertion a clause, evaluated where a build
    /// names them (`for_slab`), so a layout that breaks one fails to compile.
    const FITS: () = {
        assert!(
            match INDEX_BITS.checked_add(GENERATION_BITS) {
                Some(bits) => bits <= u64::BITS,
                None => false,
            },
            "a word's index and generation fields fit one u64"
        );
        assert!(GENERATION_BITS <= G::BITS, "a word's generation field fits its generation");
        assert!(
            P::WRAPS || GENERATION_BITS == G::BITS,
            "a Retire slab's word carries its whole generation"
        );
    };
    const INDEX_MASK: u64 = low_bits_mask(INDEX_BITS);
    const GENERATION_MASK: u64 = low_bits_mask(GENERATION_BITS);

    /// The adapter of `slab`: `Layout` when the slab has more slots than the
    /// index field numbers (`2^INDEX_BITS`), checked when it is built, at a
    /// site's boot; every other layout fault is a compile error.
    pub fn for_slab<T, const CHUNK_SLOTS: usize>(
        slab: &CappedSlab<T, G, P, CHUNK_SLOTS>,
    ) -> Result<Self, CapError> {
        let () = Self::FITS;
        let slots = slab.counts.row.entries_max();
        // Every index below `slots` fits the field unless `slots > 2^INDEX_BITS`.
        if 1u64.checked_shl(INDEX_BITS).is_some_and(|span| u64::from(slots) > span) {
            return Err(CapError::Layout);
        }
        Ok(SlotWord { instance: slab.instance(), slots, layout: PhantomData })
    }

    /// The word of `id`: its index, and its generation's low
    /// `GENERATION_BITS` above it; `None` for another slab's handle, which no
    /// word of this adapter may carry.
    #[must_use]
    #[inline]
    pub fn encode(&self, id: SlotId<G>) -> Option<u64> {
        if id.instance != self.instance {
            return None;
        }
        debug_assert!(id.index < self.slots, "a handle of this slab names one of its slots");
        let generation = id.generation.widened() & Self::GENERATION_MASK;
        Some(u64::from(id.index) | generation.checked_shl(INDEX_BITS).unwrap_or(0))
    }

    /// A word's slot index and generation field, or `None` when it names no
    /// slot: a bit set above the two fields, or an index at or past the
    /// slab's slots, so no walk passes the slab's bound.
    #[inline]
    fn fields(&self, bits: u64) -> Option<(u32, u64)> {
        let carried = bits.checked_shr(INDEX_BITS).unwrap_or(0);
        if carried.checked_shr(GENERATION_BITS).unwrap_or(0) != 0 {
            return None;
        }
        let index = u32::try_from(bits & Self::INDEX_MASK).ok().filter(|i| *i < self.slots)?;
        Some((index, carried & Self::GENERATION_MASK))
    }
}

/// The mask of a word's low `bits` bits, all of them at 64 or more.
const fn low_bits_mask(bits: u32) -> u64 {
    match u64::MAX.checked_shl(bits) {
        Some(high) => !high,
        None => u64::MAX,
    }
}

impl<T, G: Generation, P: GenerationPolicy, const CHUNK_SLOTS: usize>
    SlabBuilder<T, G, P, CHUNK_SLOTS>
{
    /// The identity the slab will stamp in its handles, minted when the
    /// builder was made.
    #[must_use]
    pub fn instance(&self) -> SlabInstance {
        self.store.instance()
    }

    /// A test hook: every slot a later step builds is free at `generation`,
    /// so a test reaches a generation's high values, a word's field width
    /// among them, without that many uses of a slot. Called before the
    /// first step, every slot is.
    #[cfg(any(test, feature = "fault-points"))]
    #[must_use]
    pub fn first_generation(mut self, generation: G) -> Self {
        self.first_generation = generation;
        self
    }

    /// The generation the slots a step builds are free at.
    #[inline]
    fn built_generation(&self) -> G {
        #[cfg(any(test, feature = "fault-points"))]
        {
            self.first_generation
        }
        #[cfg(not(any(test, feature = "fault-points")))]
        {
            G::FIRST
        }
    }

    /// One build step: one chunk or one directory allocated. `Live` once
    /// the last chunk is built; a refused allocation retires what was built.
    pub fn step(mut self) -> Built<T, G, P, CHUNK_SLOTS> {
        let generation = self.built_generation();
        match self.store.build_step_from(generation) {
            Ok(BuildStep::More) => Built::More(self),
            Ok(BuildStep::Done) => Built::Live(CappedSlab {
                counts: SlabCounts { row: self.row, live: 0, high_water: 0 },
                store: self.store,
                policy: PhantomData,
            }),
            Err(refused) => {
                Built::Refused(SlabRetiring { store: self.store, policy: PhantomData }, refused)
            }
        }
    }
}

impl<T, G: Generation, P, const CHUNK_SLOTS: usize> SlabRetiring<T, G, P, CHUNK_SLOTS> {
    /// One retire step: the last chunk or directory freed, with the
    /// occupants of a chunk. One canary drops the rest whole at the first
    /// step instead, an unsliced drop.
    pub fn step(mut self) -> Retiring<T, G, P, CHUNK_SLOTS> {
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

/// What a handle names in this slab, decided once, by [`named`], for
/// `get`, `get_mut`, `take`, `release` and the test hook (ADR-0171 D1, A2.2).
enum Named<O> {
    /// The slot's occupant, live at the handle's generation.
    Live(O),
    /// Another slab's handle, or a slot of this slab holding no occupant at
    /// the handle's generation: live at another, free at any (a wrap brings
    /// a free slot back to an old handle's), or retired.
    Stale,
    /// The handle names no slot of this slab.
    Vacant,
}

/// A borrow of one slot that yields its occupant while the slot is live at
/// a generation, and nothing otherwise. It sees neither the handle's slab
/// nor whether the slot exists, so no state of a slot can answer `Vacant`.
trait SlotBorrow<G> {
    type Occupant;

    fn occupant(self, generation: G) -> Option<Self::Occupant>;
}

/// A handle against this slab: another slab's is `Stale` and `slot`, the
/// store's walk to the handle's index, never runs for it; no slot is
/// `Vacant`; a slot's occupant at `generation` is `Live`; any other slot is
/// `Stale`.
// Folded into each operation, so a benchmark's step and the server's call
// sites compile the lookup in one shape and a count read on the bench is the
// server's code (ADR-0210 D1).
#[inline(always)]
fn named<G, S: SlotBorrow<G>>(
    foreign: bool,
    slot: impl FnOnce() -> Option<S>,
    generation: G,
) -> Named<S::Occupant> {
    if foreign {
        return Named::Stale;
    }
    let Some(slot) = slot() else { return Named::Vacant };
    match slot.occupant(generation) {
        Some(occupant) => Named::Live(occupant),
        None => Named::Stale,
    }
}

impl<'s, T, G: Generation> SlotBorrow<G> for &'s Slot<T, G> {
    type Occupant = &'s T;

    #[inline]
    fn occupant(self, at: G) -> Option<&'s T> {
        match self {
            Slot::Live { generation, value } if *generation == at => Some(value),
            Slot::Live { .. } | Slot::Free { .. } | Slot::Closed => None,
        }
    }
}

/// The occupant with its generation, which the test hook sets.
impl<'s, T, G: Generation> SlotBorrow<G> for &'s mut Slot<T, G> {
    type Occupant = (&'s mut G, &'s mut T);

    #[inline]
    fn occupant(self, at: G) -> Option<(&'s mut G, &'s mut T)> {
        match self {
            Slot::Live { generation, value } if *generation == at => Some((generation, value)),
            Slot::Live { .. } | Slot::Free { .. } | Slot::Closed => None,
        }
    }
}

/// A word's generation field and the mask of its width: a slot answers it
/// when its occupant's generation's low bits are the field (ADR-0194 D6
/// with A2). One canary compares the whole generation with the field.
#[derive(Clone, Copy)]
struct WordField {
    field: u64,
    mask: u64,
}

impl WordField {
    #[inline]
    fn answers<G: Generation>(self, generation: G) -> bool {
        #[cfg(inf_canary_word_resolve_unmasked)]
        {
            let _ = self.mask;
            generation.widened() == self.field
        }
        #[cfg(not(inf_canary_word_resolve_unmasked))]
        {
            generation.widened() & self.mask == self.field
        }
    }
}

/// The occupant with its whole generation, for a word's walk.
impl<'s, T, G: Generation> SlotBorrow<WordField> for &'s mut Slot<T, G> {
    type Occupant = (G, &'s mut T);

    #[inline]
    fn occupant(self, at: WordField) -> Option<(G, &'s mut T)> {
        match self {
            Slot::Live { generation, value } if at.answers(*generation) => {
                Some((*generation, value))
            }
            Slot::Live { .. } | Slot::Free { .. } | Slot::Closed => None,
        }
    }
}

/// A slot whose occupant at a generation moves out, leaving the slot
/// `Closed` for `take` to free or retire, lent with its chain; any other
/// slot is not written.
struct Vacate<'s, T, G>(&'s mut Slot<T, G>, Chain<'s>);

impl<'s, T, G: Generation> SlotBorrow<G> for Vacate<'s, T, G> {
    type Occupant = (T, &'s mut Slot<T, G>, Chain<'s>);

    #[inline]
    fn occupant(self, at: G) -> Option<(T, &'s mut Slot<T, G>, Chain<'s>)> {
        let Vacate(slot, chain) = self;
        if !matches!(slot, Slot::Live { generation, .. } if *generation == at) {
            return None;
        }
        match mem::replace(slot, Slot::Closed) {
            Slot::Live { value, .. } => Some((value, slot, chain)),
            other @ (Slot::Free { .. } | Slot::Closed) => {
                *slot = other;
                None
            }
        }
    }
}

/// A slot whose occupant at a generation stays where it lies, lent with its
/// chain for `release` to drop it there; any other slot is not written.
struct InPlace<'s, T, G>(&'s mut Slot<T, G>, Chain<'s>);

impl<'s, T, G: Generation> SlotBorrow<G> for InPlace<'s, T, G> {
    type Occupant = (&'s mut Slot<T, G>, Chain<'s>);

    #[inline]
    fn occupant(self, at: G) -> Option<(&'s mut Slot<T, G>, Chain<'s>)> {
        let InPlace(slot, chain) = self;
        if !matches!(slot, Slot::Live { generation, .. } if *generation == at) {
            return None;
        }
        Some((slot, chain))
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

/// `live` after a take or a release.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: live >= 1, the live slot the take or release found and vacated, so - 1 \
              stays at or above 0 (ADR-0165 D1's bound row)"
)]
fn live_taken(live: u32) -> u32 {
    live - 1
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::bounded::{CensusPhase, ServeMark};
    use crate::ids::CellId;
    use crate::limits::{CAP_CENSUS_SLABS_MAX, GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS};
    use crate::rng::{Entropy, SplitMix64};

    const CELL: CellId = CellId(7);

    fn census() -> (Rc<CapCensus>, ServeMark) {
        CapCensus::new(CELL)
    }

    /// A slab of `cap` in chunks of 64, built by steps; the steps it took.
    fn built<T, G: Generation, P: GenerationPolicy>(
        cap: Cap,
        census: &Rc<CapCensus>,
    ) -> (CappedSlab<T, G, P>, u32) {
        built_in(cap, census)
    }

    /// A slab of `cap` in chunks of `C`, built by steps; the steps it took.
    fn built_in<T, G: Generation, P: GenerationPolicy, const C: usize>(
        cap: Cap,
        census: &Rc<CapCensus>,
    ) -> (CappedSlab<T, G, P, C>, u32) {
        stepped(CappedSlab::builder(cap, census).expect("registered"))
    }

    /// `builder` stepped to its live slab; the steps it took.
    fn stepped<T, G: Generation, P: GenerationPolicy, const C: usize>(
        mut builder: SlabBuilder<T, G, P, C>,
    ) -> (CappedSlab<T, G, P, C>, u32) {
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
    fn retired<T, G: Generation, P, const C: usize>(mut retiring: SlabRetiring<T, G, P, C>) -> u32 {
        let mut steps = 0;
        loop {
            steps += 1;
            match retiring.step() {
                Retiring::More(next) => retiring = next,
                Retiring::Done => return steps,
            }
        }
    }

    fn publish<T, G: Generation, P: GenerationPolicy, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
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

    /// The level thresholds in the store's own chunk: one chunk, then each
    /// directory level's span, `256 × c` slots under one level; at a chunk of
    /// one slot four levels cover every `u32` index (256⁴ = 2³²).
    #[test]
    fn directory_levels_cover_every_u32_index() {
        use crate::limits::{MAP_DIRECTORY_LEVELS_MAX, directory_levels as levels};
        for (slots, chunk, expected) in [
            (64, 64, 0),
            (65, 64, 1),
            (16_384, 64, 1),
            (16_385, 64, 2),
            (u64::from(u32::MAX), 64, 4),
            (16, 16, 0),
            (17, 16, 1),
            (4_096, 16, 1),
            (4_097, 16, 2),
            (5_008, 16, 2),
            (1, 1, 0),
            (2, 1, 1),
            (256, 1, 1),
            (257, 1, 2),
            (1 << 24, 1, 3),
            ((1 << 24) + 1, 1, 4),
            (u64::from(u32::MAX), 1, 4),
            (0, 0, 0),
        ] {
            assert_eq!(levels(slots, chunk), expected, "{slots} slots in chunks of {chunk}");
        }
        assert!(levels(1, 0) > MAP_DIRECTORY_LEVELS_MAX, "a chunk of no slots covers none");
        assert_eq!(MAP_DIRECTORY_LEVELS_MAX, 4);
    }

    #[test]
    fn the_headers_fit_one_line() {
        assert_eq!(core::mem::size_of::<CapRowHandle>(), 16);
        assert_eq!(core::mem::size_of::<SlabCounts>(), 24);
        assert_eq!(core::mem::size_of::<SlotStore<u64, u32>>(), 40);
        assert_eq!(core::mem::size_of::<CappedSlab<u64, u32, Retire>>(), 64);
        assert_eq!(core::mem::size_of::<CappedSlab<[u8; 200], u64, WrapWithProof>>(), 64);
        assert_eq!(core::mem::size_of::<SlotId<u32>>(), 12);
        assert_eq!(core::mem::size_of::<SlotId<u64>>(), 16);
    }

    /// `take_inlined` is `take`'s transition entered in the caller's frame
    /// (ADR-0194 D1): by either entry the occupant returns, the slab holds
    /// one fewer, the handle is stale, a second take of it changes nothing,
    /// and the slot's next occupant is one generation on.
    #[test]
    fn take_inlined_is_takes_transition() {
        type Twin = CappedSlab<u64, u32, WrapWithProof>;
        type Entry = fn(&mut Twin, SlotId<u32>) -> Taken<u64>;
        let entries: [(&str, Entry); 2] =
            [("take", Twin::take), ("take_inlined", Twin::take_inlined)];
        for (entry, take) in entries {
            let (census, _mark) = census();
            let cap = Cap::entries::<1>("twin", CapFill::Serving);
            let (mut slab, _) = built::<u64, u32, WrapWithProof>(cap, &census);
            let SlabReserved::Slot(slot) = slab.reserve() else { panic!("{entry}: a free slot") };
            let id = slot.publish(7);
            assert!(matches!(take(&mut slab, id), Taken::Value(7)), "{entry}: the occupant");
            assert_eq!(slab.len(), 0, "{entry}: one fewer");
            assert!(matches!(slab.get(id), Lookup::Stale), "{entry}: the handle is stale");
            assert!(matches!(take(&mut slab, id), Taken::Stale), "{entry}: a second take");
            assert_eq!(slab.len(), 0, "{entry}: a stale take changes nothing");
            let SlabReserved::Slot(slot) = slab.reserve() else {
                panic!("{entry}: the slot again")
            };
            let next = slot.publish(8);
            assert_eq!(
                (next.index, next.generation),
                (id.index, id.generation + 1),
                "{entry}: the slot's next occupant, one generation on"
            );
        }
    }

    #[test]
    fn a_reservation_names_its_slabs_row_and_counts_by_one_pointer() {
        // The free slot with the head it moves (32 bytes at either
        // generation's width), one pointer to the row and counts, and the
        // instance: 48 bytes. A pointer each for the row, the live count
        // and the high water makes it 64, each word of them kept through
        // any call a step makes while it holds the reservation.
        assert_eq!(core::mem::size_of::<SlotReservation<'_, u64, u32, WrapWithProof>>(), 48);
        assert_eq!(core::mem::size_of::<SlotReservation<'_, [u8; 200], u64, Retire>>(), 48);
    }

    #[test]
    fn a_build_and_a_retirement_take_one_item_per_step() {
        // At 64 slots a chunk, a slot of at most 1,024 B: one chunk and no
        // directory up to 64 slots; a directory and its chunks to 16,384
        // (10^3: 16 chunks and one directory); two levels past that (16,385:
        // 257 chunks, two directories and the root).
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

    /// A wide payload of `K` words, `value` in the first and the last.
    fn words<const K: usize>(value: u64) -> [u64; K] {
        let mut words = [0; K];
        words[0] = value;
        words[K - 1] = value;
        words
    }

    /// The bytes one backing allocation holds, written here as the rule's
    /// arithmetic reads them, not taken from the limits the code reads.
    const ALLOCATION_BYTES: usize = 65_536;

    /// A slab of `[u64; K]` payloads at a `u64` generation, in chunks of `C`,
    /// which the rule's own arithmetic picks: its slot is the payload and
    /// 16 B, `C` of them fit one allocation, and under 64 twice `C` do not.
    /// It builds `cap`, `steps` items, and every slot serves and reads back.
    fn a_wide_slab_serves_every_slot<const K: usize, const C: usize>(
        cap: Cap,
        steps: u32,
        census: &Rc<CapCensus>,
    ) {
        let slot = mem::size_of::<Slot<[u64; K], u64>>();
        assert_eq!(slot, 8 * K + 16, "a slot is its payload and 16 B");
        assert_eq!(CappedSlab::<[u64; K], u64, Retire, C>::SLOT_BYTES, slot);
        assert!(C * slot <= ALLOCATION_BYTES, "{slot} B: {C} slots pass one allocation");
        if C < 64 {
            assert!(2 * C * slot > ALLOCATION_BYTES, "{slot} B: twice {C} slots fit one");
        }
        let mut builder = match CappedSlab::<[u64; K], u64, Retire, C>::builder(cap, census) {
            Ok(builder) => builder,
            Err(refused) => {
                panic!("wide-slot-refused: a {slot} B slot in chunks of {C}: {refused}")
            }
        };
        let mut built = 1;
        let mut slab = loop {
            match builder.step() {
                Built::More(next) => builder = next,
                Built::Live(slab) => break slab,
                Built::Refused(_, refused) => panic!("{slot} B: a build step refused: {refused}"),
            }
            built += 1;
        };
        assert_eq!(built, steps, "{slot} B, {cap:?}: one item a step");
        let mut ids = Vec::new();
        for value in 0..u64::from(cap.entries_max()) {
            match slab.reserve() {
                SlabReserved::Slot(free) => ids.push(free.publish(words::<K>(value))),
                SlabReserved::Full | SlabReserved::Exhausted => {
                    panic!("{slot} B, {cap:?}: slot {value} unreachable")
                }
            }
        }
        assert!(matches!(slab.reserve(), SlabReserved::Full), "{slot} B: past the cap");
        for (value, id) in (0..).zip(&ids) {
            let found = matches!(slab.get(*id), Lookup::Live(words) if words[0] == value);
            assert!(found, "{slot} B: slot {} reads another value", id.index);
            assert!(matches!(slab.get(*id), Lookup::Live(words) if words[K - 1] == value));
        }
        assert_eq!(retired(slab.retire()), steps, "{slot} B: one item a retire step");
    }

    /// A slab of `[u64; K]` payloads at a `u64` generation in chunks of `C`
    /// is refused `Layout` and registers nothing: `C` is not the chunk its
    /// slot takes.
    fn a_wide_slab_at_another_chunk_is_refused<const K: usize, const C: usize>(
        census: &Rc<CapCensus>,
    ) {
        let rows = census.rows().count();
        let cap = Cap::entries::<4>("wide-refused", CapFill::Serving);
        let refused = CappedSlab::<[u64; K], u64, Retire, C>::builder(cap, census);
        let slot = mem::size_of::<Slot<[u64; K], u64>>();
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout), "{slot} B at {C}");
        assert_eq!(census.rows().count(), rows, "{slot} B at {C}: a row registered");
    }

    /// ADR-0194 A1: a slot of `s` bytes takes the largest power-of-two chunk,
    /// at most 64 slots, that one 64 KiB allocation holds, and its builder
    /// takes no other count. Each width builds at two chunks (a directory
    /// and two chunks: three steps) and serves every slot; the 2,992 B slot
    /// also at 5,008 slots, two levels at 16 a chunk (one level spans 4,096),
    /// 313 chunks under two directories and the root. A slot past 64 KiB has
    /// no chunk. Planted, every width takes 64 and a wide slot is refused.
    #[test]
    fn a_slot_takes_the_largest_chunk_one_allocation_holds() {
        let (census, _mark) = census();
        let fill = CapFill::Serving;
        a_wide_slab_serves_every_slot::<126, 64>(Cap::entries::<128>("w-1024", fill), 3, &census);
        a_wide_slab_serves_every_slot::<127, 32>(Cap::entries::<64>("w-1032", fill), 3, &census);
        a_wide_slab_serves_every_slot::<372, 16>(Cap::entries::<32>("w-2992", fill), 3, &census);
        a_wide_slab_serves_every_slot::<510, 16>(Cap::entries::<32>("w-4096", fill), 3, &census);
        a_wide_slab_serves_every_slot::<511, 8>(Cap::entries::<16>("w-4104", fill), 3, &census);
        a_wide_slab_serves_every_slot::<8190, 1>(Cap::entries::<2>("w-65536", fill), 3, &census);
        // Each threshold and the slot one word past it: chunks of 32, 16, 8, 4, 2 and 1.
        a_wide_slab_serves_every_slot::<254, 32>(Cap::entries::<64>("w-2048", fill), 3, &census);
        a_wide_slab_serves_every_slot::<255, 16>(Cap::entries::<32>("w-2056", fill), 3, &census);
        a_wide_slab_serves_every_slot::<1022, 8>(Cap::entries::<16>("w-8192", fill), 3, &census);
        a_wide_slab_serves_every_slot::<1023, 4>(Cap::entries::<8>("w-8200", fill), 3, &census);
        a_wide_slab_serves_every_slot::<2046, 4>(Cap::entries::<8>("w-16384", fill), 3, &census);
        a_wide_slab_serves_every_slot::<2047, 2>(Cap::entries::<4>("w-16392", fill), 3, &census);
        a_wide_slab_serves_every_slot::<4094, 2>(Cap::entries::<4>("w-32768", fill), 3, &census);
        a_wide_slab_serves_every_slot::<4095, 1>(Cap::entries::<2>("w-32776", fill), 3, &census);
        let two_levels = Cap::entries::<5008>("w-2992-5008", fill);
        a_wide_slab_serves_every_slot::<372, 16>(two_levels, 316, &census);
        a_wide_slab_at_another_chunk_is_refused::<126, 32>(&census);
        a_wide_slab_at_another_chunk_is_refused::<127, 64>(&census);
        a_wide_slab_at_another_chunk_is_refused::<127, 16>(&census);
        a_wide_slab_at_another_chunk_is_refused::<372, 32>(&census);
        a_wide_slab_at_another_chunk_is_refused::<372, 8>(&census);
        a_wide_slab_at_another_chunk_is_refused::<8190, 2>(&census);
        a_wide_slab_at_another_chunk_is_refused::<1023, 8>(&census);
        a_wide_slab_at_another_chunk_is_refused::<2047, 4>(&census);
        a_wide_slab_at_another_chunk_is_refused::<4095, 2>(&census);
        // 65,544 B: past one allocation at a chunk of one slot.
        a_wide_slab_at_another_chunk_is_refused::<8191, 1>(&census);
        assert_eq!(census.unsliced_drops(), 0);
    }

    /// The rule at each power-of-two threshold of the slot's bytes, as ADR-0194 A1 writes it,
    /// `min(64, 2^⌊log₂(65,536 ÷ s)⌋)`: a threshold keeps its chunk and one byte past it halves.
    #[test]
    fn chunk_slots_for_halves_one_byte_past_each_threshold() {
        use crate::limits::chunk_slots_for;
        for (bytes, chunk) in [
            (0, Some(64)),
            (1, Some(64)),
            (1_024, Some(64)),
            (1_025, Some(32)),
            (2_048, Some(32)),
            (2_049, Some(16)),
            (4_096, Some(16)),
            (4_097, Some(8)),
            (8_192, Some(8)),
            (8_193, Some(4)),
            (16_384, Some(4)),
            (16_385, Some(2)),
            (32_768, Some(2)),
            (32_769, Some(1)),
            (65_536, Some(1)),
            (65_537, None),
            (usize::MAX, None),
        ] {
            assert_eq!(chunk_slots_for(bytes), chunk, "{bytes} B a slot");
        }
    }

    /// The chunks and directories a store of `slots` slots in chunks of `chunk` builds, by the
    /// layout's own arithmetic, written here: ⌈slots ÷ chunk⌉ chunks, and per directory level
    /// above them ⌈chunks ÷ 256^k⌉ directories, up to the one root.
    fn items(slots: u64, chunk: u64) -> u64 {
        let chunks = slots.div_ceil(chunk);
        let (mut total, mut level, mut covered) = (chunks, chunks, chunk);
        while covered < slots {
            covered *= 256;
            level = level.div_ceil(256);
            total += level;
        }
        total
    }

    /// A store counts its chunks and directories in a `u32`, so a cap whose layout builds more
    /// than `u32::MAX` items is `Layout` at its builder, before a row registers. At a chunk of
    /// one slot (a slot past 32 KiB) a `u32::MAX` cap builds 2^32 − 1 chunks and 16,843,009
    /// directories; the largest that fits is 4,278,190,079 slots. At 64 slots a chunk a
    /// `u32::MAX` cap builds 67,372,037 items, at two slots 2,155,905,153: both build.
    #[test]
    fn a_cap_whose_chunks_and_directories_pass_a_u32_is_layout() {
        const FITS: u32 = 4_278_190_079;
        assert!(items(u64::from(FITS), 1) <= u64::from(u32::MAX));
        assert!(items(u64::from(FITS) + 1, 1) > u64::from(u32::MAX));
        assert!(items(u64::from(u32::MAX), 2) <= u64::from(u32::MAX));
        let (census, _mark) = census();
        let fill = CapFill::Serving;
        let over = Cap::entries::<{ FITS + 1 }>("items-over", fill);
        let refused = CappedSlab::<[u64; 8190], u64, Retire, 1>::builder(over, &census);
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout), "items-overflow-admitted");
        let refused = CappedSlab::<[u64; 8190], u64, Retire, 1>::builder(
            Cap::entries::<{ u32::MAX }>("items-max", fill),
            &census,
        );
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout), "items-overflow-admitted");
        assert_eq!(census.rows().count(), 0, "a refused layout registered a row");
        // Nothing is allocated until a step, so these builders only check and mint.
        let fits = Cap::entries::<FITS>("items-fit", fill);
        assert!(CappedSlab::<[u64; 8190], u64, Retire, 1>::builder(fits, &census).is_ok());
        let wide = Cap::entries::<{ u32::MAX }>("items-two", fill);
        assert!(CappedSlab::<[u64; 4094], u64, Retire, 2>::builder(wide, &census).is_ok());
        let narrow = Cap::entries::<{ u32::MAX }>("items-64", fill);
        assert!(CappedSlab::<u64, u64, Retire>::builder(narrow, &census).is_ok());
        census.mark_stopping();
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
        /// The generation as a `u128`, whose low bits the model reads by
        /// remainders, not the slab's masks.
        fn wide(self) -> u128;
    }

    macro_rules! model_generation {
        ($($width:ty),*) => {$(
            impl ModelGeneration for $width {
                const ZERO: Self = 0;
                const MAX: Self = <$width>::MAX;

                fn successor(self) -> Option<Self> {
                    self.checked_add(1)
                }

                fn wide(self) -> u128 {
                    u128::from(self)
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

    /// What the model expects of a policy, and its word and reservation
    /// ops (ADR-0194 D6 and D1 with A2): a `WrapWithProof` slab's word
    /// carries two generation bits, so a stale handle's word aliases a later
    /// occupant within a run, and its reservations are named; a `Retire`
    /// slab's word carries its whole generation (at a `u64`, beside an index
    /// field of no bits: one slot), and its reservations drop unnamed.
    trait ModelPolicy<G: Generation + ModelGeneration>: GenerationPolicy + Sized {
        const AT_LAST: AtLast;

        fn word<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, G, Self, C>,
            model: &mut Model<G>,
            handles: &[SlotId<G>],
            rng: &mut SplitMix64,
        );

        fn reservation<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, G, Self, C>,
            model: &mut Model<G>,
            handles: &mut Vec<SlotId<G>>,
            value: u64,
        );
    }

    impl ModelPolicy<u32> for Retire {
        const AT_LAST: AtLast = AtLast::Retires;

        fn word<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, u32, Self, C>,
            model: &mut Model<u32>,
            handles: &[SlotId<u32>],
            rng: &mut SplitMix64,
        ) {
            model_word::<T, u32, Self, 32, 32, C>(slab, model, handles, rng);
        }

        fn reservation<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, u32, Self, C>,
            model: &mut Model<u32>,
            _: &mut Vec<SlotId<u32>>,
            _: u64,
        ) {
            model_unnamed_drop(slab, model);
        }
    }

    impl ModelPolicy<u64> for Retire {
        const AT_LAST: AtLast = AtLast::Retires;

        fn word<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, u64, Self, C>,
            model: &mut Model<u64>,
            handles: &[SlotId<u64>],
            rng: &mut SplitMix64,
        ) {
            model_word::<T, u64, Self, 0, 64, C>(slab, model, handles, rng);
        }

        fn reservation<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, u64, Self, C>,
            model: &mut Model<u64>,
            _: &mut Vec<SlotId<u64>>,
            _: u64,
        ) {
            model_unnamed_drop(slab, model);
        }
    }

    impl<G: Generation + ModelGeneration> ModelPolicy<G> for WrapWithProof {
        const AT_LAST: AtLast = AtLast::Wraps;

        fn word<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, G, Self, C>,
            model: &mut Model<G>,
            handles: &[SlotId<G>],
            rng: &mut SplitMix64,
        ) {
            model_word::<T, G, Self, 16, 2, C>(slab, model, handles, rng);
        }

        fn reservation<T: ModelValue, const C: usize>(
            slab: &mut CappedSlab<T, G, Self, C>,
            model: &mut Model<G>,
            handles: &mut Vec<SlotId<G>>,
            value: u64,
        ) {
            model_named(slab, model, handles, value);
        }
    }

    /// The five accesses that answer a handle against its slot.
    #[derive(Clone, Copy)]
    enum Access {
        Get,
        GetMut,
        Take,
        Hook,
        Release,
    }

    /// Per access, the ops that named a handle whose slot is free at the
    /// handle's generation (a wrap brought it back) or retired: the states
    /// an answer for a slot not live at the handle's generation is read on.
    /// A zero means the model checked nothing of that state on that access.
    #[derive(Default)]
    struct Reach {
        free_at_generation: [u64; 5],
        retired: [u64; 5],
        /// Publishes through `publish_mut` whose lend wrote a second value.
        lent: u64,
        /// Live occupants freed by `release`.
        released: u64,
        /// Word ops by answer: `Live`, `Stale`, `Vacant`, and the adapter
        /// refused `Layout` (a slab whose slots pass its index field).
        words: [u64; 4],
        /// Words of a handle that resolved a later occupant at the same low
        /// generation bits: the alias a narrow field allows.
        aliased: u64,
        /// Named reservations dropped, then published.
        named: [u64; 2],
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

    /// A value the model holds as its `u64`: a narrow slot's own, or a wide
    /// slot's payload.
    trait ModelValue: PartialEq + fmt::Debug {
        fn of(value: u64) -> Self;
        fn value(&self) -> u64;
    }

    impl ModelValue for u64 {
        fn of(value: u64) -> u64 {
            value
        }

        fn value(&self) -> u64 {
            *self
        }
    }

    /// A payload of `K` words carrying its value in the first and the last,
    /// so a slot of `8 × K + 16` bytes at a `u64` generation.
    #[derive(PartialEq, Debug)]
    struct Wide<const K: usize>([u64; K]);

    impl<const K: usize> ModelValue for Wide<K> {
        fn of(value: u64) -> Wide<K> {
            Wide(words(value))
        }

        fn value(&self) -> u64 {
            assert_eq!(self.0[0], self.0[K - 1], "a payload's two words disagree");
            self.0[0]
        }
    }

    /// A publish; through `publish_mut` when `lend`, whose lend then writes
    /// a second value in the same step, which the model holds.
    fn model_publish<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        handles: &mut Vec<SlotId<G>>,
        value: u64,
        lend: bool,
    ) where
        G: Generation + ModelGeneration,
        P: GenerationPolicy,
    {
        let instance = slab.instance();
        match slab.reserve() {
            SlabReserved::Slot(slot) => {
                assert!((model.live.len() as u64) < u64::from(model.capacity), "a slot to serve");
                let (id, value) = if lend {
                    let (id, occupant) = slot.publish_mut(T::of(value));
                    assert_eq!(occupant.value(), value, "the lend is the occupant");
                    let second = value | 1 << 20;
                    *occupant = T::of(second);
                    model.reach.lent += 1;
                    (id, second)
                } else {
                    (slot.publish(T::of(value)), value)
                };
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

    fn model_take<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        id: SlotId<G>,
    ) where
        G: Generation + ModelGeneration,
        P: ModelPolicy<G>,
    {
        model.note(&id, Access::Take);
        match model.live.remove(&(id.index, id.generation)) {
            Some(expected) => {
                assert_eq!(slab.take(id), Taken::Value(T::of(expected)));
                model.vacate(id.index, id.generation, P::AT_LAST);
            }
            None => assert_eq!(slab.take(id), Taken::Stale, "{id:?}: a stale take"),
        }
    }

    /// `release` by a held handle: a live occupant's slot frees as a take's
    /// does, nothing returned; any other handle is `Stale`.
    fn model_release<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        id: SlotId<G>,
    ) where
        G: Generation + ModelGeneration,
        P: ModelPolicy<G>,
    {
        model.note(&id, Access::Release);
        match model.live.remove(&(id.index, id.generation)) {
            Some(_) => {
                assert_eq!(slab.release(id), Released::Dropped, "{id:?}: a live release");
                model.vacate(id.index, id.generation, P::AT_LAST);
                model.reach.released += 1;
            }
            None => assert_eq!(slab.release(id), Released::Stale, "{id:?}: a stale release"),
        }
    }

    fn model_write<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        id: SlotId<G>,
        value: u64,
    ) where
        G: Generation + ModelGeneration,
        P: GenerationPolicy,
    {
        model.note(&id, Access::GetMut);
        match (slab.get_mut(id), model.live.get_mut(&(id.index, id.generation))) {
            (LookupMut::Live(found), Some(expected)) => {
                assert_eq!(found.value(), *expected);
                *found = T::of(value);
                *expected = value;
            }
            (LookupMut::Stale, None) => {}
            (found, expected) => panic!("{id:?}: get_mut {found:?}, model {expected:?}"),
        }
    }

    /// The test hook on `handles[at]`: a live occupant moves to the model's
    /// last generation with its handle; any other handle is `Stale` and
    /// keeps its generation.
    fn model_place_last<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        handles: &mut [SlotId<G>],
        at: usize,
    ) where
        G: Generation + ModelGeneration,
        P: GenerationPolicy,
    {
        let before = handles[at];
        model.note(&before, Access::Hook);
        let held = model.live.remove(&(before.index, before.generation));
        match (slab.place_generation_last(&mut handles[at]), held) {
            (LookupMut::Live(found), Some(expected)) => assert_eq!(found.value(), expected),
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

    /// A word op: an encoded handle's word (drawn as `pick` draws) or a
    /// peer's (any `u64`, or one
    /// whose index and field are drawn within and just past the slab),
    /// decoded against the model's own reading of the layout, by remainders
    /// of `u128`s: a bit above the two fields or an index at or past the
    /// slots is `Vacant`; the slot's live occupant whose generation's low
    /// bits are the field is `Live`, its whole handle answered, and written
    /// through; any other is `Stale`. A slab whose slots pass the index
    /// field refuses the adapter `Layout`.
    fn model_word<T: ModelValue, G, P, const I: u32, const B: u32, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
        handles: &[SlotId<G>],
        rng: &mut SplitMix64,
    ) where
        G: Generation + ModelGeneration,
        P: GenerationPolicy,
    {
        let slots = model.next.len() as u128;
        let word = match SlotWord::<G, P, I, B>::for_slab(slab) {
            Ok(word) => word,
            Err(refused) => {
                assert_eq!(refused, CapError::Layout);
                assert!(slots > 1 << I, "the adapter refused slots its index field numbers");
                model.reach.words[3] += 1;
                return;
            }
        };
        assert!(slots <= 1 << I, "an adapter built past its index field");
        let (bits, from) = match rng.next_below(4) {
            0 | 1 => {
                let id = handles[pick(rng, handles, model)];
                (word.encode(id).expect("this slab's handle"), Some(id))
            }
            2 => (rng.next_u64(), None),
            _ => {
                let index = u128::from(rng.next_below(slots as u64 + 2)) % (1 << I);
                let field = u128::from(rng.next_u64()) % (1 << B);
                ((index + (field << I)) as u64, None)
            }
        };
        let (index, carried) = (u128::from(bits) % (1 << I), u128::from(bits) >> I);
        let expected = (carried >> B == 0 && index < slots).then(|| {
            let index = index as u32;
            let mut live = model.live.range((index, G::ZERO)..=(index, G::MAX));
            let field = carried % (1 << B);
            live.next().filter(|((_, generation), _)| generation.wide() % (1 << B) == field)
        });
        let expected = expected.map(|live| live.map(|(key, value)| (*key, *value)));
        let instance = slab.instance();
        match (slab.get_mut_by_word(&word, bits), expected) {
            (WordLookup::Live(id, found), Some(Some(((index, generation), value)))) => {
                assert_eq!((id.index, id.generation, id.instance), (index, generation, instance));
                assert_eq!(found.value(), value, "{bits:#x}: the occupant the word names");
                let written = value ^ 1 << 21;
                *found = T::of(written);
                model.live.insert((index, generation), written);
                model.reach.words[0] += 1;
                if from.is_some_and(|from| from.generation != generation) {
                    model.reach.aliased += 1;
                }
            }
            (WordLookup::Stale, Some(None)) => model.reach.words[1] += 1,
            (WordLookup::Vacant, None) => model.reach.words[2] += 1,
            (found, expected) => panic!("{bits:#x}: the word {found:?}, model {expected:?}"),
        }
    }

    /// A named reservation: its `id()` is the slot's index and the model's
    /// next generation for it, naming no occupant; published at odd values,
    /// the publish returns it; else dropped, and the slot's next generation
    /// advances, wrapping at the last. Its handle joins the run's, so every
    /// later op reads it, at every later occupant.
    fn model_named<T: ModelValue, G, const C: usize>(
        slab: &mut CappedSlab<T, G, WrapWithProof, C>,
        model: &mut Model<G>,
        handles: &mut Vec<SlotId<G>>,
        value: u64,
    ) where
        G: Generation + ModelGeneration,
    {
        let instance = slab.instance();
        let named = match slab.reserve() {
            SlabReserved::Slot(slot) => slot.named(),
            SlabReserved::Full => {
                assert_eq!(model.live.len() as u64, u64::from(model.capacity), "Full");
                model.crossings += 1;
                return;
            }
            SlabReserved::Exhausted => panic!("a wrapping slab answered Exhausted"),
        };
        let id = named.id();
        assert_eq!(id.instance, instance);
        let next = model.next_generation(id.index).expect("a wrapping slot never retires");
        assert_eq!(id.generation, next, "a named reservation names the slot's next generation");
        assert!(!model.live.contains_key(&(id.index, id.generation)), "it names no occupant");
        if value & 1 == 1 {
            assert_eq!(named.publish(T::of(value)), id, "the publish returns the named handle");
            assert!(model.live.insert((id.index, id.generation), value).is_none());
            model.high_water = model.high_water.max(model.live.len() as u32);
            model.reach.named[1] += 1;
        } else {
            drop(named);
            model.next[id.index as usize] = Some(id.generation.successor().unwrap_or(G::ZERO));
            model.reach.named[0] += 1;
        }
        handles.push(id);
    }

    /// A reservation dropped unnamed: nothing changes, the slot's generation
    /// kept (a `Retire` slab names none).
    fn model_unnamed_drop<T: ModelValue, G, P, const C: usize>(
        slab: &mut CappedSlab<T, G, P, C>,
        model: &mut Model<G>,
    ) where
        G: Generation + ModelGeneration,
        P: GenerationPolicy,
    {
        match slab.reserve() {
            SlabReserved::Slot(slot) => drop(slot),
            SlabReserved::Full => {
                assert!(model.capacity > 0, "Full with no slot left");
                assert_eq!(model.live.len() as u64, u64::from(model.capacity), "Full");
                model.crossings += 1;
            }
            SlabReserved::Exhausted => assert_eq!(model.capacity, 0, "Exhausted with a slot left"),
        }
    }

    /// The model: a `BTreeMap<(index, generation), value>` per instance with
    /// the slots' next generations, the retired slots and the capacity left,
    /// sharing no code with the slab. Publishes, takes, releases, writes
    /// through `get_mut`, words decoded by `get_mut_by_word`, reservations
    /// named or dropped, and the test hook's placements at the last
    /// generation are each checked against it, so a slot retires (`Retire`) or wraps
    /// (`WrapWithProof`) under random sequences; every handle the run minted
    /// is read back through `get` after every op, and the row's high water,
    /// crossings and capacity equal the model's at the end. Returns what the
    /// run's ops reached.
    fn model_run<G, P>(cap: Cap, seed: u64, ops: u32) -> Reach
    where
        G: Generation + ModelGeneration,
        P: ModelPolicy<G>,
    {
        model_run_in::<u64, G, P, MAP_CHUNK_SLOTS_MAX>(cap, seed, ops)
    }

    /// `model_run` on a slab of `T` in chunks of `C`.
    fn model_run_in<T, G, P, const C: usize>(cap: Cap, seed: u64, ops: u32) -> Reach
    where
        T: ModelValue,
        G: Generation + ModelGeneration,
        P: ModelPolicy<G>,
    {
        let (census, _mark) = census();
        let (mut slab, _) = built_in::<T, G, P, C>(cap, &census);
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
            let kind = rng.next_below(18);
            if kind < 8 || handles.is_empty() {
                model_publish(&mut slab, &mut model, &mut handles, value, false);
            } else {
                let at = pick(&mut rng, &handles, &model);
                match kind {
                    8..=10 => model_take(&mut slab, &mut model, handles[at]),
                    11 | 12 => model_write(&mut slab, &mut model, handles[at], value),
                    13 => model_place_last(&mut slab, &mut model, &mut handles, at),
                    14 => model_publish(&mut slab, &mut model, &mut handles, value, true),
                    15 => model_release(&mut slab, &mut model, handles[at]),
                    16 => P::word(&mut slab, &mut model, &handles, &mut rng),
                    _ => P::reservation(&mut slab, &mut model, &mut handles, value),
                }
            }
            assert_eq!(slab.len() as usize, model.live.len());
            assert_eq!(slab.capacity_now(), model.capacity);
            for id in &handles {
                model.note(id, Access::Get);
                match (model.live.get(&(id.index, id.generation)), slab.get(*id)) {
                    (Some(expected), Lookup::Live(found)) => assert_eq!(found.value(), *expected),
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

    /// `run`, its publishes through `publish_mut` added to `totals[0]` and
    /// its releases of a live occupant to `totals[1]`.
    fn lent(totals: &mut [u64; 2], run: Reach) -> Reach {
        totals[0] += run.lent;
        totals[1] += run.released;
        run
    }

    /// `run`'s words and named reservations added to `totals`: the word
    /// answers `Live`, `Stale`, `Vacant` and `Layout`, the aliased `Live`s,
    /// the named drops and the named publishes.
    fn worded(totals: &mut [u64; 7], run: Reach) -> Reach {
        let reached = [run.words, [run.aliased, run.named[0], run.named[1], 0]].concat();
        for (total, count) in totals.iter_mut().zip(reached) {
            *total += count;
        }
        run
    }

    fn add(sum: &mut [u64; 5], run: [u64; 5]) {
        for (total, count) in sum.iter_mut().zip(run) {
            *total += count;
        }
    }

    /// Both policies at both widths, across the chunk and directory edges.
    /// Engagement: each of the five accesses named a handle whose slot had
    /// retired (`Retire` runs) and one whose slot had wrapped back to the
    /// handle's generation while free (`WrapWithProof` runs).
    #[test]
    fn the_slab_equals_its_model_across_chunk_and_directory_edges() {
        let (mut retired, mut wrapped, mut lends) = ([0; 5], [0; 5], [0; 2]);
        let (mut retire_words, mut wrap_words) = ([0; 7], [0; 7]);
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
            let retires = model_run::<u32, Retire>(cap, seed, 3_000);
            add(&mut retired, worded(&mut retire_words, lent(&mut lends, retires)).retired);
            let retires = model_run::<u64, Retire>(cap, seed + 1, 3_000);
            add(&mut retired, worded(&mut retire_words, lent(&mut lends, retires)).retired);
            let wraps = model_run::<u32, WrapWithProof>(cap, seed + 2, 3_000);
            let wraps = worded(&mut wrap_words, lent(&mut lends, wraps));
            add(&mut wrapped, wraps.free_at_generation);
            let wraps = model_run::<u64, WrapWithProof>(cap, seed + 3, 3_000);
            let wraps = worded(&mut wrap_words, lent(&mut lends, wraps));
            add(&mut wrapped, wraps.free_at_generation);
        }
        // Get, GetMut, Take, Hook, Release.
        assert!(retired.iter().all(|count| *count > 0), "a retired slot unread: {retired:?}");
        assert!(wrapped.iter().all(|count| *count > 0), "a wrapped slot unread: {wrapped:?}");
        assert!(lends[0] > 0, "no publish through publish_mut");
        assert!(lends[1] > 0, "no live occupant released");
        // Live, Stale, Vacant and Layout words; aliased, named drops and
        // publishes: a `Retire` word carries its whole generation and names
        // none, and a `u64` one refuses every slab past one slot.
        assert!(retire_words[..4].iter().all(|count| *count > 0), "{retire_words:?}");
        assert!(wrap_words[..3].iter().chain(&wrap_words[4..]).all(|count| *count > 0));
        assert_eq!(wrap_words[3], 0, "a 16-bit index field numbers every cap here");
    }

    /// The model at a wide slot (ADR-0194 A1): 2,992 B at a `u64` generation,
    /// 16 slots a chunk, across that chunk's edges and a directory over 19
    /// chunks, under both policies, with the same engagement.
    #[test]
    fn a_wide_slab_equals_its_model_across_its_chunk_and_directory_edges() {
        type Payload = Wide<372>;
        assert_eq!(mem::size_of::<Slot<Payload, u64>>(), 2_992);
        let (mut retired, mut wrapped, mut lends) = ([0; 5], [0; 5], [0; 2]);
        let (mut retire_words, mut wrap_words) = ([0; 7], [0; 7]);
        for (index, cap) in [
            Cap::entries::<1>("w-1", CapFill::Serving),
            Cap::entries::<2>("w-2", CapFill::Serving),
            Cap::entries::<15>("w-15", CapFill::Serving),
            Cap::entries::<16>("w-16", CapFill::Serving),
            Cap::entries::<17>("w-17", CapFill::Serving),
            Cap::entries::<300>("w-300", CapFill::Serving),
        ]
        .into_iter()
        .enumerate()
        {
            let seed = 0x3D1D + 2 * index as u64;
            let retires = model_run_in::<Payload, u64, Retire, 16>(cap, seed, 3_000);
            add(&mut retired, worded(&mut retire_words, lent(&mut lends, retires)).retired);
            let wraps = model_run_in::<Payload, u64, WrapWithProof, 16>(cap, seed + 1, 3_000);
            let wraps = worded(&mut wrap_words, lent(&mut lends, wraps));
            add(&mut wrapped, wraps.free_at_generation);
        }
        // Get, GetMut, Take, Hook, Release.
        assert!(retired.iter().all(|count| *count > 0), "a retired slot unread: {retired:?}");
        assert!(wrapped.iter().all(|count| *count > 0), "a wrapped slot unread: {wrapped:?}");
        assert!(lends[0] > 0, "no publish through publish_mut");
        assert!(lends[1] > 0, "no live occupant released");
        // A `u64` `Retire` word numbers one slot, whose hook retires it within
        // a few ops: `Stale` there, `Layout` past it, never `Vacant` (no bit
        // lies above its fields); its `Live` is the peer-word test's.
        let retire_reached = [retire_words[1], retire_words[3]];
        assert!(retire_reached.iter().all(|count| *count > 0), "{retire_words:?}");
        assert_eq!(retire_words[2], 0, "a whole-word field leaves no bit above it");
        assert!(wrap_words[..3].iter().chain(&wrap_words[4..]).all(|count| *count > 0));
    }

    /// Records its own address, where it is dropped.
    struct DroppedAt(Rc<Cell<usize>>);

    impl Drop for DroppedAt {
        fn drop(&mut self) {
            self.0.set(core::ptr::from_ref::<Self>(self) as usize);
        }
    }

    /// A drop that panics.
    struct PanicsOnDrop;

    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            panic!("the occupant's drop panics");
        }
    }

    /// `release` drops the occupant where `get` lends it (else
    /// `release-moved`), advances the slot's generation and counts one
    /// fewer; a stale, foreign or vacant handle changes nothing; a `Retire`
    /// slot at its last generation closes and the slab serves one fewer; an
    /// occupant whose drop panics leaves the slab as the release leaves it.
    #[test]
    fn release_drops_the_occupant_where_it_lies() {
        let (census, _mark) = census();
        let cap = Cap::entries::<2>("released", CapFill::Serving);
        let (mut slab, _) = built::<DroppedAt, u32, Retire>(cap, &census);
        let dropped_at = Rc::new(Cell::new(0));
        let mut id = publish(&mut slab, DroppedAt(Rc::clone(&dropped_at))).expect("room");
        let Lookup::Live(occupant) = slab.get(id) else { panic!("a live occupant") };
        let lies_at = core::ptr::from_ref(occupant) as usize;
        assert_eq!(slab.release(id), Released::Dropped);
        assert_eq!(dropped_at.get(), lies_at, "release-moved: dropped off its slot");
        assert_eq!(slab.len(), 0, "one fewer live");
        assert!(matches!(slab.get(id), Lookup::Stale));
        let next = publish(&mut slab, DroppedAt(Rc::clone(&dropped_at))).expect("room");
        assert_eq!((next.index, next.generation), (id.index, id.generation + 1), "advanced");

        // Refusals: nothing changes.
        let (mut other, _) = built::<DroppedAt, u32, Retire>(cap, &census);
        let foreign = publish(&mut other, DroppedAt(Rc::new(Cell::new(0)))).expect("room");
        let past = SlotId { index: 1 << 20, generation: 0, instance: slab.instance() };
        dropped_at.set(0);
        for (handle, refused) in
            [(id, Released::Stale), (foreign, Released::Stale), (past, Released::Vacant)]
        {
            assert_eq!(slab.release(handle), refused, "{handle:?}");
            assert_eq!((slab.len(), slab.capacity_now(), dropped_at.get()), (1, 2, 0));
            assert!(matches!(slab.get(next), Lookup::Live(_)), "the occupant stays");
        }
        assert_eq!(other.len(), 1, "a foreign release changes the other slab neither");

        // The last generation of a `Retire` slot: closed, the slab serves one fewer.
        id = next;
        assert!(matches!(slab.place_generation_last(&mut id), LookupMut::Live(_)));
        assert_eq!(slab.release(id), Released::Dropped);
        assert_eq!((slab.len(), slab.capacity_now()), (0, 1));
        assert_eq!(census.row("released").expect("row").capacity_now, 1);
        assert!(matches!(slab.get(id), Lookup::Stale), "a closed slot's handle");

        // An `Assembly` slab's release reports the removal to its row.
        let assembling = Cap::entries::<2>("released-assembly", CapFill::Assembly);
        let (mut owners, _) = built::<DroppedAt, u32, Retire>(assembling, &census);
        let kept = publish(&mut owners, DroppedAt(Rc::new(Cell::new(0)))).expect("room");
        let _also = publish(&mut owners, DroppedAt(Rc::new(Cell::new(0)))).expect("room");
        assert_eq!(owners.release(kept), Released::Dropped);
        let row = census.row("released-assembly").expect("row");
        assert_eq!((owners.len(), row.assembly_live), (1, 1), "the removal reported");

        // A drop that panics runs last: the counts and the chain are written.
        let (mut panics, _) = built::<PanicsOnDrop, u32, Retire>(cap, &census);
        let id = publish(&mut panics, PanicsOnDrop).expect("room");
        let outcome = catch_unwind(AssertUnwindSafe(|| panics.release(id)));
        assert!(outcome.is_err(), "the occupant's drop panicked");
        assert_eq!(panics.len(), 0, "counted out before the drop");
        let again = publish(&mut panics, PanicsOnDrop).expect("the slot freed before the drop");
        assert_eq!((again.index, again.generation), (id.index, id.generation + 1));
        assert!(catch_unwind(AssertUnwindSafe(|| panics.release(again))).is_err());
        assert!(panics.is_empty(), "nothing left to drop with the slab");
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

    /// The fabric key's adapter: 24 index bits, the low 23 of a `u32`
    /// generation (ADR-0177 D4).
    type Key = SlotWord<u32, WrapWithProof, GATE_KEY_INDEX_BITS, GATE_KEY_GENERATION_BITS>;

    /// The first generation past the key's field, whose field reads 0.
    const PAST_FIELD: u32 = 1 << GATE_KEY_GENERATION_BITS;

    /// A word resolves `id`'s occupant, `value`, answering its whole handle.
    fn resolves(slab: &mut CappedSlab<u64, u32, WrapWithProof>, key: &Key, id: SlotId<u32>) {
        let bits = key.encode(id).expect("this slab's handle");
        match slab.get_mut_by_word(key, bits) {
            WordLookup::Live(found, _) => assert_eq!(found, id, "the word's whole handle"),
            other => panic!(
                "key-stale-past-width: the word {bits:#x} of generation {:#x} is {other:?}",
                id.generation
            ),
        }
    }

    /// A word that names no occupant now is `Stale`.
    fn stale(slab: &mut CappedSlab<u64, u32, WrapWithProof>, key: &Key, id: SlotId<u32>) {
        let bits = key.encode(id).expect("this slab's handle");
        let found = slab.get_mut_by_word(key, bits);
        assert!(matches!(found, WordLookup::Stale), "{bits:#x}: a past occupant's word {found:?}");
    }

    /// A word at the key's field edge (ADR-0194 D6 with A2): slots built
    /// free at generation 2²³ − 1, one published and resolved by its word,
    /// taken (its word now `Stale`), published again at 2²³, whose field
    /// reads 0: that word resolves the occupant with its whole generation,
    /// and the old word, field 2²³ − 1, is `Stale`. Compared on the whole
    /// generation, the second word is `Stale` and its waiter never wakes.
    #[test]
    fn a_word_resolves_its_slot_past_the_field_width() {
        let (census, _mark) = census();
        let cap = Cap::entries::<2>("key-edge", CapFill::Serving);
        let builder = CappedSlab::builder(cap, &census).expect("registered");
        let (mut slab, _) =
            stepped::<u64, u32, WrapWithProof, 64>(builder.first_generation(PAST_FIELD - 1));
        let key = Key::for_slab(&slab).expect("two slots number in 24 bits");
        let first = publish(&mut slab, 1).expect("room");
        assert_eq!(first.generation, PAST_FIELD - 1);
        resolves(&mut slab, &key, first);
        assert_eq!(slab.take(first), Taken::Value(1));
        stale(&mut slab, &key, first);
        let second = publish(&mut slab, 2).expect("the slot freed");
        assert_eq!((second.index, second.generation), (first.index, PAST_FIELD));
        assert_eq!(key.encode(second), Some(u64::from(second.index)), "its field reads 0");
        resolves(&mut slab, &key, second);
        stale(&mut slab, &key, first);
        assert!(matches!(slab.get(second), Lookup::Live(2)), "the word wrote nothing");
    }

    /// A word across the generation's last value: an occupant placed at
    /// `u32::MAX` (field 2²³ − 1) resolves; taken, the slot wraps to 0; the
    /// next occupant's word (field 0) resolves it, and the old word is
    /// `Stale`.
    #[test]
    fn a_word_resolves_its_slot_across_the_last_generation() {
        let (census, _mark) = census();
        let cap = Cap::entries::<1>("key-last", CapFill::Serving);
        let (mut slab, _) = built::<u64, u32, WrapWithProof>(cap, &census);
        let key = Key::for_slab(&slab).expect("one slot numbers in 24 bits");
        let mut last = publish(&mut slab, 1).expect("room");
        assert!(matches!(slab.place_generation_last(&mut last), LookupMut::Live(1)));
        assert_eq!(key.encode(last), Some(u64::from(PAST_FIELD - 1) << GATE_KEY_INDEX_BITS));
        resolves(&mut slab, &key, last);
        assert_eq!(slab.take(last), Taken::Value(1));
        let wrapped = publish(&mut slab, 2).expect("the slot wrapped");
        assert_eq!(wrapped.generation, 0);
        resolves(&mut slab, &key, wrapped);
        stale(&mut slab, &key, last);
    }

    /// A peer's word (ADR-0194 D6 with A2): any `u64` answers `Live`,
    /// `Stale` or `Vacant` against the oracle, which reads the layout by
    /// remainders, and writes nothing; an index at or past the slab's slots,
    /// or a bit above the two fields, is `Vacant`; another slab's adapter is
    /// `Stale`; another slab's handle encodes to `None`; a slab past the
    /// index field's span refuses its adapter `Layout`.
    #[test]
    fn a_peer_word_answers_by_its_bits_and_writes_nothing() {
        let (census, _mark) = census();
        let cap = Cap::entries::<5>("peer", CapFill::Serving);
        let (mut slab, _) = built::<u64, u32, WrapWithProof>(cap, &census);
        let (mut other, _) = built::<u64, u32, WrapWithProof>(cap, &census);
        let key = Key::for_slab(&slab).expect("five slots number in 24 bits");
        let theirs_key = Key::for_slab(&other).expect("five slots number in 24 bits");
        let ids: Vec<_> = (0..4).map(|value| publish(&mut slab, value).expect("room")).collect();
        let theirs = publish(&mut other, 9).expect("room");
        let mut rng = SplitMix64::new(0x0B17);
        for draw in 0..20_000u32 {
            let bits = match draw % 3 {
                0 => rng.next_u64(),
                1 => rng.next_u64() % (1 << 47),
                _ => rng.next_below(7) + (rng.next_below(2) << GATE_KEY_INDEX_BITS),
            };
            let (index, carried) = (bits % (1 << 24), bits >> 24);
            let expected = (carried >> 23 == 0 && index < 5).then(|| {
                let field = |id: &&SlotId<u32>| u64::from(id.generation) % (1 << 23) == carried;
                ids.iter().find(|id| u64::from(id.index) == index).filter(field)
            });
            match (slab.get_mut_by_word(&key, bits), expected) {
                (WordLookup::Live(found, value), Some(Some(id))) => {
                    assert_eq!(found, *id);
                    assert_eq!(*value, u64::from(id.index));
                }
                (WordLookup::Stale, Some(None)) | (WordLookup::Vacant, None) => {}
                (found, expected) => panic!("{bits:#x}: the word {found:?}, oracle {expected:?}"),
            }
        }
        for (index, id) in (0..).zip(&ids) {
            assert!(matches!(slab.get(*id), Lookup::Live(value) if *value == index));
        }
        assert_eq!(slab.len(), 4, "no word wrote the slab");
        let past = [5, (1 << 24) - 1, (1 << 47) | 1, u64::MAX];
        for bits in past {
            assert!(matches!(slab.get_mut_by_word(&key, bits), WordLookup::Vacant), "{bits:#x}");
        }
        let mine = key.encode(ids[1]).expect("this slab's handle");
        assert!(matches!(slab.get_mut_by_word(&theirs_key, mine), WordLookup::Stale));
        assert_eq!(key.encode(theirs), None, "another slab's handle");
        type Narrow = SlotWord<u32, WrapWithProof, 2, 23>;
        assert_eq!(Narrow::for_slab(&slab).map(|_| ()).err(), Some(CapError::Layout));
        let four = Cap::entries::<4>("four", CapFill::Serving);
        assert!(Narrow::for_slab(&built::<u64, u32, WrapWithProof>(four, &census).0).is_ok());
        // A `Retire` word carries the whole `u64`: one slot, every bit its
        // generation's, so its occupant resolves and, retired, is `Stale`.
        type Whole = SlotWord<u64, Retire, 0, 64>;
        let one = Cap::entries::<1>("one", CapFill::Serving);
        let mut single = built::<u64, u64, Retire>(one, &census).0;
        let whole = Whole::for_slab(&single).expect("one slot numbers in no bits");
        let mut last = publish(&mut single, 3).expect("room");
        assert!(matches!(single.place_generation_last(&mut last), LookupMut::Live(3)));
        assert_eq!(whole.encode(last), Some(u64::MAX), "the word is the generation");
        let found = single.get_mut_by_word(&whole, u64::MAX);
        assert!(matches!(found, WordLookup::Live(id, 3) if id == last), "{found:?}");
        assert_eq!(single.take(last), Taken::Value(3));
        assert!(matches!(single.get_mut_by_word(&whole, u64::MAX), WordLookup::Stale));
        let two = Cap::entries::<2>("two", CapFill::Serving);
        let refused = Whole::for_slab(&built::<u64, u64, Retire>(two, &census).0);
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout));
    }

    /// A named reservation (ADR-0194 D1 with A2): its `id()` is `Stale`
    /// until its publish; dropped unpublished, the next named reservation of
    /// that slot names the next generation, so the first word is `Stale` at
    /// the new occupant; at the last generation the drop wraps; and the
    /// drop's advance runs on an unwind. Kept, the dropped handle is reissued.
    #[test]
    fn a_dropped_named_reservation_names_no_later_occupant() {
        let (census, _mark) = census();
        let cap = Cap::entries::<1>("named", CapFill::Serving);
        let builder = CappedSlab::builder(cap, &census).expect("registered");
        let (mut slab, _) =
            stepped::<u64, u32, WrapWithProof, 64>(builder.first_generation(u32::MAX - 1));
        let key = Key::for_slab(&slab).expect("one slot numbers in 24 bits");
        let mut dropped = Vec::new();
        for expected in [u32::MAX - 1, u32::MAX, 0] {
            let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
            let named = slot.named();
            let id = named.id();
            assert_eq!(id.generation, expected, "id-reused: {id:?} after {dropped:?}");
            drop(named);
            assert!(matches!(slab.get(id), Lookup::Stale), "an unpublished handle");
            dropped.push(id);
        }
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
            let named = slot.named();
            assert_eq!(named.id().generation, 1, "id-reused: the drops advanced to 1");
            panic!("unwinding with {:?} held", named.id());
        }));
        assert!(unwound.is_err(), "the closure unwound");
        let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
        let named = slot.named();
        let id = named.id();
        assert_eq!(id.generation, 2, "id-reused: the unwind's drop advanced past 1");
        assert_eq!(named.publish(7), id, "the publish returns the named handle");
        assert_eq!(slab.len(), 1);
        for past in dropped {
            stale(&mut slab, &key, past);
            assert!(matches!(slab.get(past), Lookup::Stale), "{past:?}");
        }
        resolves(&mut slab, &key, id);
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
