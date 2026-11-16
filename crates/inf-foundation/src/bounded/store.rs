//! The storage of a capped slab and of a capped map's two stores (ADR-0194
//! D3 with A1, ADR-0151 D5): slots in chunks of `CHUNK_SLOTS`, the count
//! `chunk_slots_for` answers for the slot's bytes (64 up to 1,024 B), behind
//! at most `MAP_DIRECTORY_LEVELS_MAX` levels of directories of
//! `MAP_DIRECTORY_CHILDREN_MAX` children, the depth derived from the store's
//! own chunk, each chunk and each directory one allocation of at most
//! `MAP_BACKING_ALLOCATION_BYTES_MAX` bytes. A store is built one item per
//! step, a directory before its children, and retired one item per step,
//! children before their directory, so neither a build nor a retirement is
//! one large allocation or free. The free chain is intrusive in the slots,
//! so nothing is allocated once the store is built.
//!
//! The store knows indices only: no census row, no fill and no handle. The
//! slab's public layer compares a handle's instance and generation before
//! it reaches a slot, and reports to the row; a map walks its own links.
//! The one thing a store reports is its own drop: dropped holding a chunk or
//! a directory while its cell serves, it is a release no step sliced.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::mem::size_of;
use std::rc::Rc;

use super::CapError;
use super::census::{CapCensus, SlabInstance};
use super::slab::Generation;
use crate::limits::{
    MAP_BACKING_ALLOCATION_BYTES_MAX, MAP_CHUNK_SLOTS_MAX, MAP_DIRECTORY_CHILDREN_MAX,
    MAP_DIRECTORY_LEVELS_MAX, chunk_slots_for, directory_levels,
};

/// log2 of the children of one directory, a power of two (a const assertion
/// in `limits` holds that), so a slot index splits into a directory path by
/// shifts and a mask.
const CHILDREN_LOG2: u32 = MAP_DIRECTORY_CHILDREN_MAX.trailing_zeros();
const CHILD_MASK: u32 = (1 << CHILDREN_LOG2) - 1;

/// A chunk of `CHUNK_SLOTS` slots: a slot index splits into its chunk offset
/// by a shift and a mask, exact only for a power of two. Every count a store
/// is built at is one (`chunk_slots_for` answers no other), and an instance
/// at another fails to compile where it reads these, so each is a constant
/// of its instantiation.
struct ChunkShape<const CHUNK_SLOTS: usize>;

impl<const CHUNK_SLOTS: usize> ChunkShape<CHUNK_SLOTS> {
    const SLOTS_LOG2: u32 = {
        assert!(CHUNK_SLOTS.is_power_of_two(), "an instance's chunk slots are a power of two");
        CHUNK_SLOTS.trailing_zeros()
    };
    const SLOTS: u32 = 1 << Self::SLOTS_LOG2;
    const OFFSET_MASK: u32 = Self::SLOTS - 1;
}

/// The free chain's end: no slot index reaches it, since a store holds at
/// most `MAP_SLOT_COUNT_MAX` = `u32::MAX` slots.
const END: u32 = u32::MAX;

/// What a store's slot carries as its generation: `FIRST` is the value a
/// never-used slot takes, and the store copies it, nothing more. A slab's
/// `Generation` (`u32`, `u64`) is one, by the blanket impl; so is `()`, the
/// generation of a map's two stores, whose indices never leave the map, so
/// no handle carries one to compare. `()` is no `Generation`, so no slab and
/// no `SlotId` is built over a generation that cannot advance:
///
/// ```compile_fail
/// # use inf_foundation::CellId;
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill, CappedSlab, Retire};
/// let (census, _serve_mark) = CapCensus::new(CellId(0));
/// let cap = Cap::entries::<1>("unit", CapFill::Serving);
/// let _builder = CappedSlab::<u8, (), Retire>::builder(cap, &census);
/// ```
pub(super) trait SlotGeneration: Copy {
    const FIRST: Self;
}

impl<G: Generation> SlotGeneration for G {
    const FIRST: Self = <G as Generation>::FIRST;
}

impl SlotGeneration for () {
    const FIRST: Self = ();
}

/// One slot. `Free` is on the free chain with the generation its next
/// occupant takes; `Live` holds its occupant; `Closed` never serves again:
/// retired at its last generation, or past the store's capacity in its last
/// chunk.
pub(super) enum Slot<T, G> {
    Free { generation: G, next: u32 },
    Live { generation: G, value: T },
    Closed,
}

type Chunk<T, G, const C: usize> = [Slot<T, G>; C];
type Directory<L> = [Option<Box<L>>; MAP_DIRECTORY_CHILDREN_MAX];
type One<T, G, const C: usize> = Directory<Chunk<T, G, C>>;
type Two<T, G, const C: usize> = Directory<One<T, G, C>>;
type Three<T, G, const C: usize> = Directory<Two<T, G, C>>;
type Four<T, G, const C: usize> = Directory<Three<T, G, C>>;

// The root's directory variants, `One` to `Four`, hold every level a `u32`
// cap needs at the smallest chunk, so at every chunk, and `Root::allocate`'s
// `Layout` arm is never reached.
const _: () =
    assert!(MAP_DIRECTORY_LEVELS_MAX == 4, "the root holds every directory level a u32 cap needs");

/// The root, by its directory levels: the variant is the level count, so an
/// access matches it once and then follows exactly that many links.
enum Root<T, G, const C: usize> {
    /// Nothing built yet, a capacity of zero, or everything retired.
    Empty,
    Chunk(Box<Chunk<T, G, C>>),
    One(Box<One<T, G, C>>),
    Two(Box<Two<T, G, C>>),
    Three(Box<Three<T, G, C>>),
    Four(Box<Four<T, G, C>>),
}

/// What one build step allocated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grown {
    Directory,
    Chunk,
    /// The chunk holding the store's last slot: the build is complete.
    LastChunk,
}

/// What one retire step under an item did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Retired {
    /// One descendant item was freed.
    Freed,
    /// The item holds no child: its parent frees it.
    Bare,
}

/// The answer of a store's build step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BuildStep {
    More,
    Done,
}

/// The answer of a store's retire step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RetireStep {
    More,
    Done,
}

/// One level of the tree: a chunk of slots, or a directory of the level
/// below. A slot index names one chunk offset and one child per directory
/// level; the generic levels are walked by nested calls of distinct types,
/// so the walk is a fixed depth, never a run-time recursion.
trait Level<T, G: SlotGeneration>: Sized {
    /// log2 of the slots one full item of this level covers.
    const SPAN_LOG2: u32;

    fn slot(&self, index: u32) -> Option<&Slot<T, G>>;

    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T, G>>;

    /// Slots `first` and `second`, two distinct indices, both mutably: the
    /// two paths are walked together while they share an item and apart
    /// below it, so neither borrow covers the other's slot.
    fn slot_pair_mut(&mut self, first: u32, second: u32) -> Option<[&mut Slot<T, G>; 2]>;

    /// The item whose slots start at `first`, in one allocation: a chunk
    /// with its slots below `capacity` free and chained in index order and
    /// the rest closed, or a directory with no child.
    fn allocate(first: u32, capacity: u32) -> Result<Box<Self>, CapError>;

    /// What allocating the item whose slots start at `first` grew.
    fn grown(first: u32, capacity: u32) -> Grown;

    /// One build step under this item, whose slots start at `first`: the
    /// next missing item in pre-order allocated, or `None` when this item
    /// is complete.
    fn build(&mut self, first: u32, capacity: u32) -> Result<Option<Grown>, CapError>;

    /// One retire step under this item: its last, deepest item freed, or
    /// `Bare` when it holds none.
    fn retire(&mut self) -> Retired;
}

impl<T, G: SlotGeneration, const C: usize> Level<T, G> for Chunk<T, G, C> {
    const SPAN_LOG2: u32 = ChunkShape::<C>::SLOTS_LOG2;

    #[inline]
    fn slot(&self, index: u32) -> Option<&Slot<T, G>> {
        self.get((index & ChunkShape::<C>::OFFSET_MASK) as usize)
    }

    #[inline]
    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T, G>> {
        self.get_mut((index & ChunkShape::<C>::OFFSET_MASK) as usize)
    }

    #[inline]
    fn slot_pair_mut(&mut self, first: u32, second: u32) -> Option<[&mut Slot<T, G>; 2]> {
        let mask = ChunkShape::<C>::OFFSET_MASK;
        let offsets = [(first & mask) as usize, (second & mask) as usize];
        self.get_disjoint_mut(offsets).ok()
    }

    fn allocate(first: u32, capacity: u32) -> Result<Box<Self>, CapError> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(C).map_err(|_| CapError::Alloc)?;
        for offset in 0..ChunkShape::<C>::SLOTS {
            // `first` is a multiple of the chunk's slots, so the offset is
            // its low bits.
            let index = first | offset;
            slots.push(if index < capacity {
                let next = index.checked_add(1).filter(|next| *next < capacity).unwrap_or(END);
                Slot::Free { generation: G::FIRST, next }
            } else {
                Slot::Closed
            });
        }
        Box::<Self>::try_from(slots.into_boxed_slice()).map_err(|_| CapError::Layout)
    }

    fn grown(first: u32, capacity: u32) -> Grown {
        if capacity.checked_sub(first).is_some_and(|rest| rest <= ChunkShape::<C>::SLOTS) {
            Grown::LastChunk
        } else {
            Grown::Chunk
        }
    }

    fn build(&mut self, _: u32, _: u32) -> Result<Option<Grown>, CapError> {
        Ok(None)
    }

    fn retire(&mut self) -> Retired {
        Retired::Bare
    }
}

impl<T, G: SlotGeneration, L: Level<T, G>> Level<T, G> for Directory<L> {
    const SPAN_LOG2: u32 = L::SPAN_LOG2 + CHILDREN_LOG2;

    #[inline]
    fn slot(&self, index: u32) -> Option<&Slot<T, G>> {
        self.get(child(index, L::SPAN_LOG2))?.as_deref()?.slot(index)
    }

    #[inline]
    fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T, G>> {
        self.get_mut(child(index, L::SPAN_LOG2))?.as_deref_mut()?.slot_mut(index)
    }

    #[inline]
    fn slot_pair_mut(&mut self, first: u32, second: u32) -> Option<[&mut Slot<T, G>; 2]> {
        let children = [child(first, L::SPAN_LOG2), child(second, L::SPAN_LOG2)];
        if children[0] == children[1] {
            return self.get_mut(children[0])?.as_deref_mut()?.slot_pair_mut(first, second);
        }
        let [one, two] = self.get_disjoint_mut(children).ok()?;
        Some([one.as_deref_mut()?.slot_mut(first)?, two.as_deref_mut()?.slot_mut(second)?])
    }

    fn allocate(_: u32, _: u32) -> Result<Box<Self>, CapError> {
        let mut children = Vec::new();
        children.try_reserve_exact(MAP_DIRECTORY_CHILDREN_MAX).map_err(|_| CapError::Alloc)?;
        children.resize_with(MAP_DIRECTORY_CHILDREN_MAX, || None);
        Box::<Self>::try_from(children.into_boxed_slice()).map_err(|_| CapError::Layout)
    }

    fn grown(_: u32, _: u32) -> Grown {
        Grown::Directory
    }

    fn build(&mut self, first: u32, capacity: u32) -> Result<Option<Grown>, CapError> {
        // The children built are a prefix: the last one may still be
        // incomplete, and the next one starts after it.
        let held = self.partition_point(Option::is_some);
        if let Some(last) = held.checked_sub(1)
            && let Some(child_first) = child_first(first, last, L::SPAN_LOG2, capacity)
            && let Some(child) = self.get_mut(last).and_then(|entry| entry.as_deref_mut())
            && let Some(grown) = child.build(child_first, capacity)?
        {
            return Ok(Some(grown));
        }
        let Some(child_first) = child_first(first, held, L::SPAN_LOG2, capacity) else {
            return Ok(None);
        };
        let Some(entry) = self.get_mut(held) else { return Ok(None) };
        *entry = Some(L::allocate(child_first, capacity)?);
        Ok(Some(L::grown(child_first, capacity)))
    }

    fn retire(&mut self) -> Retired {
        let held = self.partition_point(Option::is_some);
        let Some(last) = held.checked_sub(1) else { return Retired::Bare };
        let Some(entry) = self.get_mut(last) else { return Retired::Bare };
        if let Some(child) = entry.as_deref_mut()
            && child.retire() == Retired::Freed
        {
            return Retired::Freed;
        }
        *entry = None;
        Retired::Freed
    }
}

/// The child of a directory whose children cover `2^span_log2` slots each
/// that holds slot `index`; a span as wide as the index holds it in child 0.
#[inline]
fn child(index: u32, span_log2: u32) -> usize {
    (index.checked_shr(span_log2).unwrap_or(0) & CHILD_MASK) as usize
}

/// The first slot of child `position` of a directory whose slots start at
/// `first`, if that child holds a slot below `capacity`: `first` is a
/// multiple of the directory's span, so the child's offset is its high bits.
fn child_first(first: u32, position: usize, span_log2: u32, capacity: u32) -> Option<u32> {
    let position = u64::try_from(position).ok().filter(|p| *p <= u64::from(CHILD_MASK))?;
    let start = u64::from(first) | (position << span_log2);
    u32::try_from(start).ok().filter(|start| *start < capacity)
}

/// Whether slot `index` lies under a root covering `2^span_log2` slots.
#[inline]
fn covers(index: u32, span_log2: u32) -> bool {
    u64::from(index) >> span_log2 == 0
}

impl<T, G: SlotGeneration, const C: usize> Root<T, G, C> {
    /// The root of a store of `capacity` slots, allocated empty, with what
    /// it grew; its levels are those its own chunk needs, and a capacity
    /// whose levels pass the root's is `Layout`.
    fn allocate(capacity: u32) -> Result<(Root<T, G, C>, Grown), CapError> {
        Ok(match directory_levels(u64::from(capacity), C) {
            0 => (Root::Chunk(Chunk::allocate(0, capacity)?), Chunk::<T, G, C>::grown(0, capacity)),
            1 => (Root::One(One::allocate(0, capacity)?), Grown::Directory),
            2 => (Root::Two(Two::allocate(0, capacity)?), Grown::Directory),
            3 => (Root::Three(Three::allocate(0, capacity)?), Grown::Directory),
            4 => (Root::Four(Four::allocate(0, capacity)?), Grown::Directory),
            _ => return Err(CapError::Layout),
        })
    }
}

/// A free slot the store's head names, borrowed for one step: `occupy`
/// makes it live and moves the head to the next free slot. It borrows the
/// slot and the head together, so nothing changes between the two.
pub(super) struct FreeSlot<'a, T, G> {
    slot: &'a mut Slot<T, G>,
    free_head: &'a mut u32,
    index: u32,
    generation: G,
    next: u32,
}

impl<'a, T, G: SlotGeneration> FreeSlot<'a, T, G> {
    /// The slot holds `value` at the generation it was free with; its index
    /// and that generation are returned. Cannot allocate or refuse.
    #[inline]
    pub(super) fn occupy(self, value: T) -> (u32, G) {
        let (index, generation, _) = self.occupy_mut(value);
        (index, generation)
    }

    /// `occupy`, with the occupant lent for the slot's borrow, at the
    /// address it lies at until a take moves it out (ADR-0194 D1's note).
    #[inline]
    pub(super) fn occupy_mut(self, value: T) -> (u32, G, &'a mut T) {
        let FreeSlot { slot, free_head, index, generation, next } = self;
        *slot = Slot::Live { generation, value };
        *free_head = next;
        match slot {
            Slot::Live { value, .. } => (index, generation, value),
            Slot::Free { .. } | Slot::Closed => unreachable!("a slot just made live is live"),
        }
    }
}

/// A free slot borrowed with another slot of the same store, for one step.
pub(super) type FreeBeside<'a, T, G> = (FreeSlot<'a, T, G>, &'a mut Slot<T, G>);

/// The slot storage of one slab, or of one store of a map, in chunks of
/// `CHUNK_SLOTS` slots: hot fields first (the instance, the free head and the
/// root, which every reserve, take and lookup loads), then the cold counts
/// and the census its drop reports to. 40 bytes.
#[repr(C)]
pub(super) struct SlotStore<T, G, const CHUNK_SLOTS: usize = MAP_CHUNK_SLOTS_MAX> {
    instance: SlabInstance,
    free_head: u32,
    root: Root<T, G, CHUNK_SLOTS>,
    /// Slots that can still serve: the capacity built, less the slots
    /// retired at their last generation.
    capacity_now: u32,
    /// Chunks and directories allocated and not yet freed.
    items_held: u32,
    census: Rc<CapCensus>,
}

impl<T, G: SlotGeneration, const CHUNK_SLOTS: usize> SlotStore<T, G, CHUNK_SLOTS> {
    /// Whether a store of `capacity` slots has this layout: its chunk holds
    /// the slots `chunk_slots_for` answers for its slot's bytes, no more and
    /// no fewer (ADR-0194 A1), a chunk and a directory each fit one backing
    /// allocation, and a `u32` counts the chunks and directories its build
    /// allocates (`items_for`). Checked before anything is registered or
    /// minted.
    pub(super) fn layout_fits(capacity: u32) -> bool {
        let slot = size_of::<Slot<T, G>>();
        let chunk = slot.checked_mul(CHUNK_SLOTS);
        let directory = size_of::<Option<Box<Chunk<T, G, CHUNK_SLOTS>>>>()
            .checked_mul(MAP_DIRECTORY_CHILDREN_MAX);
        let fits =
            |bytes: Option<usize>| bytes.is_some_and(|b| b <= MAP_BACKING_ALLOCATION_BYTES_MAX);
        chunk_slots_for(slot) == Some(CHUNK_SLOTS)
            && fits(chunk)
            && fits(directory)
            && Self::items_for(capacity).is_some()
    }

    /// The chunks and directories the build of `capacity` slots allocates,
    /// when a `u32` counts them (the store's `items_held`): ⌈N ÷ c⌉ chunks
    /// and, per directory level above them, ⌈chunks ÷ 256^k⌉ directories. At
    /// a chunk of 64 a `u32` cap's build is at most 2^26 chunks and
    /// 2^18 + 2^10 + 2^2 + 1 directories, at a chunk of 2 at most 2^31
    /// chunks and 8,421,505 directories; at a chunk of one slot more than
    /// 4,278,190,079 slots pass `u32::MAX` items, so such a cap is `None`.
    fn items_for(capacity: u32) -> Option<u32> {
        let chunk = u64::try_from(CHUNK_SLOTS).ok().filter(|chunk| *chunk > 0)?;
        let chunks = u64::from(capacity).div_ceil(chunk);
        let (mut items, mut level) = (chunks, chunks);
        // At most `MAP_DIRECTORY_LEVELS_MAX` levels for a chunk of one slot or more.
        for _ in 0..directory_levels(u64::from(capacity), CHUNK_SLOTS) {
            level = level.div_ceil(MAP_DIRECTORY_CHILDREN_MAX as u64);
            items = items.checked_add(level)?;
        }
        u32::try_from(items).ok()
    }

    /// A store of `capacity` slots on `census`, which mints its instance
    /// now; nothing is allocated until its first build step. A shape no
    /// allocation can hold, or whose items a `u32` does not count, is
    /// `Layout`; a census past its mints is `Census`.
    pub(super) fn new(capacity: u32, census: &Rc<CapCensus>) -> Result<Self, CapError> {
        if !Self::layout_fits(capacity) {
            return Err(CapError::Layout);
        }
        let instance = census.mint_instance()?;
        let free_head = if capacity == 0 { END } else { 0 };
        Ok(SlotStore {
            instance,
            free_head,
            root: Root::Empty,
            capacity_now: capacity,
            items_held: 0,
            census: Rc::clone(census),
        })
    }

    /// One build step: the next chunk or directory allocated, a directory
    /// before its children. `Done` once the chunk holding the last slot is
    /// built; a store of no capacity is done with nothing built. A refused
    /// allocation leaves what was built, which retires by steps.
    pub(super) fn build_step(&mut self) -> Result<BuildStep, CapError> {
        let capacity = self.capacity_now;
        let grown = match &mut self.root {
            Root::Empty if capacity == 0 => return Ok(BuildStep::Done),
            Root::Empty => {
                let (root, grown) = Root::allocate(capacity)?;
                self.root = root;
                Some(grown)
            }
            Root::Chunk(_) => None,
            Root::One(directory) => directory.build(0, capacity)?,
            Root::Two(directory) => directory.build(0, capacity)?,
            Root::Three(directory) => directory.build(0, capacity)?,
            Root::Four(directory) => directory.build(0, capacity)?,
        };
        let Some(grown) = grown else { return Ok(BuildStep::Done) };
        self.items_held = items_grown(self.items_held);
        Ok(match grown {
            Grown::LastChunk => BuildStep::Done,
            Grown::Directory | Grown::Chunk => BuildStep::More,
        })
    }

    /// One retire step: the last, deepest chunk or directory freed, with
    /// the occupants of a chunk, so a directory goes after its children.
    /// `Done` once nothing is held.
    pub(super) fn retire_step(&mut self) -> RetireStep {
        let retired = match &mut self.root {
            Root::Empty => return RetireStep::Done,
            Root::Chunk(_) => Retired::Bare,
            Root::One(directory) => directory.retire(),
            Root::Two(directory) => directory.retire(),
            Root::Three(directory) => directory.retire(),
            Root::Four(directory) => directory.retire(),
        };
        if retired == Retired::Bare {
            self.root = Root::Empty;
        }
        self.items_held = items_freed(self.items_held);
        match self.root {
            Root::Empty => RetireStep::Done,
            Root::Chunk(_) | Root::One(_) | Root::Two(_) | Root::Three(_) | Root::Four(_) => {
                RetireStep::More
            }
        }
    }

    #[inline]
    pub(super) fn instance(&self) -> SlabInstance {
        self.instance
    }

    #[inline]
    pub(super) fn capacity_now(&self) -> u32 {
        self.capacity_now
    }

    /// Chunks and directories held: a map retires its two stores one after
    /// the other, so it asks which still holds an item.
    #[inline]
    pub(super) fn items_held(&self) -> u32 {
        self.items_held
    }

    /// The free slot at the head of the chain, if any, borrowed with the
    /// head for one step.
    #[inline]
    pub(super) fn reserve(&mut self) -> Option<FreeSlot<'_, T, G>> {
        let index = self.free_head;
        let SlotStore { root, free_head, .. } = self;
        let slot = root_slot_mut(root, index)?;
        let Slot::Free { generation, next } = *slot else { return None };
        Some(FreeSlot { slot, free_head, index, generation, next })
    }

    /// As `reserve`, with slot `other` borrowed beside the free one for the
    /// same step: a map's split occupies a free branch slot and rewrites
    /// the link its parent branch holds in one publish. `None` when the
    /// chain is empty, or `other` is the head or no slot of this store.
    #[inline]
    pub(super) fn reserve_beside(&mut self, other: u32) -> Option<FreeBeside<'_, T, G>> {
        let index = self.free_head;
        let SlotStore { root, free_head, .. } = self;
        let [slot, beside] = root_slot_pair_mut(root, index, other)?;
        let Slot::Free { generation, next } = *slot else { return None };
        Some((FreeSlot { slot, free_head, index, generation, next }, beside))
    }

    #[inline]
    pub(super) fn slot(&self, index: u32) -> Option<&Slot<T, G>> {
        match &self.root {
            Root::Empty => None,
            Root::Chunk(level) => walk(&**level, index),
            Root::One(level) => walk(&**level, index),
            Root::Two(level) => walk(&**level, index),
            Root::Three(level) => walk(&**level, index),
            Root::Four(level) => walk(&**level, index),
        }
    }

    #[inline]
    pub(super) fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T, G>> {
        root_slot_mut(&mut self.root, index)
    }

    /// Slot `index`, just vacated, rejoins the free chain at its head; its
    /// state is already `Free` with its next generation.
    #[inline]
    pub(super) fn chain(&mut self, index: u32) {
        self.free_head = index;
    }

    /// The free head, for a vacated slot to link to.
    #[inline]
    pub(super) fn free_head(&self) -> u32 {
        self.free_head
    }

    /// A slot left the chain for good at its last generation: the store
    /// serves one slot fewer.
    #[cold]
    pub(super) fn retire_slot(&mut self) {
        self.capacity_now = capacity_retired(self.capacity_now);
    }
}

impl<T, G, const CHUNK_SLOTS: usize> Drop for SlotStore<T, G, CHUNK_SLOTS> {
    /// The store's one `Drop`, under every phase of a slab and each store of
    /// a map: a store dropped holding a chunk or a directory while its cell
    /// serves is a release no step sliced, which the census counts and a
    /// debug build asserts.
    fn drop(&mut self) {
        self.census.note_store_drop(self.items_held);
    }
}

/// Slot `index` under `root`, mutably.
#[inline]
fn root_slot_mut<T, G: SlotGeneration, const C: usize>(
    root: &mut Root<T, G, C>,
    index: u32,
) -> Option<&mut Slot<T, G>> {
    match root {
        Root::Empty => None,
        Root::Chunk(level) => walk_mut(&mut **level, index),
        Root::One(level) => walk_mut(&mut **level, index),
        Root::Two(level) => walk_mut(&mut **level, index),
        Root::Three(level) => walk_mut(&mut **level, index),
        Root::Four(level) => walk_mut(&mut **level, index),
    }
}

/// Slots `first` and `second` under `root`, both mutably.
#[inline]
fn root_slot_pair_mut<T, G: SlotGeneration, const C: usize>(
    root: &mut Root<T, G, C>,
    first: u32,
    second: u32,
) -> Option<[&mut Slot<T, G>; 2]> {
    match root {
        Root::Empty => None,
        Root::Chunk(level) => walk_pair_mut(&mut **level, first, second),
        Root::One(level) => walk_pair_mut(&mut **level, first, second),
        Root::Two(level) => walk_pair_mut(&mut **level, first, second),
        Root::Three(level) => walk_pair_mut(&mut **level, first, second),
        Root::Four(level) => walk_pair_mut(&mut **level, first, second),
    }
}

/// Slot `index` under a root level, if the root covers it: an index past
/// the root's span is never walked.
#[inline]
fn walk<T, G: SlotGeneration, L: Level<T, G>>(level: &L, index: u32) -> Option<&Slot<T, G>> {
    if covers(index, L::SPAN_LOG2) { level.slot(index) } else { None }
}

#[inline]
fn walk_mut<T, G: SlotGeneration, L: Level<T, G>>(
    level: &mut L,
    index: u32,
) -> Option<&mut Slot<T, G>> {
    if covers(index, L::SPAN_LOG2) { level.slot_mut(index) } else { None }
}

#[inline]
fn walk_pair_mut<T, G: SlotGeneration, L: Level<T, G>>(
    level: &mut L,
    first: u32,
    second: u32,
) -> Option<[&mut Slot<T, G>; 2]> {
    if covers(first, L::SPAN_LOG2) && covers(second, L::SPAN_LOG2) {
        level.slot_pair_mut(first, second)
    } else {
        None
    }
}

/// One store's slots, read through a root level fixed for the reader's
/// life: a walk matches the root once and reads every visit through this.
pub(super) trait SlotAccess<T, G> {
    fn slot(&self, index: u32) -> Option<&Slot<T, G>>;
}

/// A walk over one store, run on the store's root level as a type.
pub(super) trait LevelWalk<T, G> {
    type Output;
    fn walk<A: SlotAccess<T, G>>(self, access: &A) -> Self::Output;
}

struct RootAccess<'a, L>(&'a L);

struct NoSlots;

impl<T, G> SlotAccess<T, G> for NoSlots {
    #[inline]
    fn slot(&self, _: u32) -> Option<&Slot<T, G>> {
        None
    }
}

impl<T, G: SlotGeneration, L: Level<T, G>> SlotAccess<T, G> for RootAccess<'_, L> {
    #[inline]
    fn slot(&self, index: u32) -> Option<&Slot<T, G>> {
        walk(self.0, index)
    }
}

impl<T, G: SlotGeneration, const CHUNK_SLOTS: usize> SlotStore<T, G, CHUNK_SLOTS> {
    /// `walker` on this store's root level, matched once: the walk borrows
    /// the store, so its root, and with it the level every visit reads,
    /// cannot change until the walk returns. Inlined into its caller, so
    /// the caller's own attribute places the walk.
    #[inline(always)]
    pub(super) fn walk_with<W: LevelWalk<T, G>>(&self, walker: W) -> W::Output {
        match &self.root {
            Root::Empty => walker.walk(&NoSlots),
            Root::Chunk(level) => walker.walk(&RootAccess(&**level)),
            Root::One(level) => walker.walk(&RootAccess(&**level)),
            Root::Two(level) => walker.walk(&RootAccess(&**level)),
            Root::Three(level) => walker.walk(&RootAccess(&**level)),
            Root::Four(level) => walker.walk(&RootAccess(&**level)),
        }
    }
}

/// `items_held` after a build step allocated one item.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: items_held is below the layout's item count before the step, and `new` \
              admits a capacity only when that count fits a u32 (`SlotStore::items_for`: at \
              a chunk of 64 at most 2^26 chunks and 2^18 + 2^10 + 2^2 + 1 directories, at a \
              chunk of one slot a refusal past 4,278,190,079 slots), so + 1 stays within u32 \
              (ADR-0165 D1's bound row)"
)]
fn items_grown(items_held: u32) -> u32 {
    items_held + 1
}

/// `items_held` after a retire step freed one item.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: items_held >= 1, the item the retire step just freed, so - 1 stays at or \
              above 0 (ADR-0165 D1's bound row)"
)]
fn items_freed(items_held: u32) -> u32 {
    items_held - 1
}

/// `capacity_now` after a slot retired at its last generation.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: capacity_now >= live >= 1 when a take retires a slot, the live slot it took \
              counted, so - 1 stays at or above 0 (ADR-0165 D1's bound row)"
)]
fn capacity_retired(capacity_now: u32) -> u32 {
    capacity_now - 1
}
