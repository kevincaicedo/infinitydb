//! `CappedMap<K, V>`: a map over exact, fixed-width keys (ADR-0151 D1–D3), a
//! compressed binary radix tree on fixed storage. A branch tests one key bit
//! and has two children, and split bits strictly increase toward the leaves,
//! so a lookup visits at most `K::BITS` branches and compares one whole key,
//! and an admission at most twice that, whatever the population or its
//! history: no hash, no resize, no rebalancing, no ambient state.
//!
//! `N` leaves and `N − 1` branches live in two slot stores, built and
//! retired one chunk or directory per step; once `Live` the map allocates
//! nothing. For `n > 0` entries exactly `n − 1` branches are published, so a
//! free leaf implies a free branch. Admission is `Occupied` (the key is
//! present: replacement is legal at the cap), `Vacant` (an exclusive slot
//! that fixes the key, its leaf and its split before the value exists, and
//! whose `publish` cannot allocate or refuse) or `Full` (no value taken, the
//! row's crossings up by one). The map holds one census row; its two stores
//! hold none and report only their own drop.
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
use core::mem;
use std::rc::Rc;

use super::census::{CapCensus, CapRowHandle};
use super::store::{BuildStep, FreeSlot, RetireStep as StoreRetire, Slot, SlotStore};
use super::{Cap, CapError, CapFill};
use crate::ids::CellId;

mod sealed {
    pub trait Sealed {}
}

/// A map key whose equality and bit access are total, constant-work and
/// exact (ADR-0151 D1): an unsigned integer word of at most
/// `MAP_KEY_BITS_MAX` bits, a pair of `u64`s, the unit key, or a foundation
/// id with an exact integer representation. Sealed: the set is closed, so no
/// key type can claim exact identity through a hash, and an owner's id
/// reaches a map through a lossless adapter at that owner's boundary.
pub trait ExactKey: sealed::Sealed + Copy + Eq + fmt::Debug {
    /// The key's width in bits, at most `MAP_KEY_BITS_MAX`: a lookup
    /// visits at most this many branches.
    const BITS: u32;

    /// Bit `index` of the key, counted from the most significant. The map
    /// asks only for `index < BITS`.
    fn bit(self, index: u32) -> bool;

    /// The first bit, from the most significant, at which two keys differ;
    /// `None` when they are equal.
    fn first_difference(self, other: Self) -> Option<u32>;
}

macro_rules! exact_word {
    ($($word:ty),*) => {$(
        impl sealed::Sealed for $word {}

        impl ExactKey for $word {
            const BITS: u32 = <$word>::BITS;

            #[inline]
            fn bit(self, index: u32) -> bool {
                // A left rotation by `index` brings that bit to the top.
                self.rotate_left(index) >= const { 1 << (<$word>::BITS - 1) }
            }

            #[inline]
            fn first_difference(self, other: Self) -> Option<u32> {
                let differing = self ^ other;
                if differing == 0 { None } else { Some(differing.leading_zeros()) }
            }
        }
    )*};
}
exact_word!(u8, u16, u32, u64, u128);

impl sealed::Sealed for (u64, u64) {}

/// The first word's bits, then the second's: 128 bits compared whole.
impl ExactKey for (u64, u64) {
    const BITS: u32 = 2 * u64::BITS;

    #[inline]
    fn bit(self, index: u32) -> bool {
        match index.checked_sub(u64::BITS) {
            None => self.0.bit(index),
            Some(low) => self.1.bit(low),
        }
    }

    #[inline]
    fn first_difference(self, other: Self) -> Option<u32> {
        match self.0.first_difference(other.0) {
            Some(bit) => Some(bit),
            None => self.1.first_difference(other.1).map(second_word_bit),
        }
    }
}

impl sealed::Sealed for () {}

/// The unit key: one key, no bits, so a map over it holds at most one
/// entry and never a branch.
impl ExactKey for () {
    const BITS: u32 = 0;

    #[inline]
    fn bit(self, _: u32) -> bool {
        false
    }

    #[inline]
    fn first_difference(self, (): Self) -> Option<u32> {
        None
    }
}

impl sealed::Sealed for CellId {}

/// All 16 bits of the cell's id.
impl ExactKey for CellId {
    const BITS: u32 = u16::BITS;

    #[inline]
    fn bit(self, index: u32) -> bool {
        self.0.bit(index)
    }

    #[inline]
    fn first_difference(self, other: Self) -> Option<u32> {
        self.0.first_difference(other.0)
    }
}

/// A link to a node: the variant names the store the index is in, so a
/// leaf's index is never read as a branch's (ADR-0151 D3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Node {
    Leaf(u32),
    Branch(u32),
}

/// A split: keys with `bit` clear are under `children[0]`, the rest under
/// `children[1]`, and `bit` is above every bit tested on the way here.
#[derive(Clone, Copy, Debug)]
struct Branch {
    bit: u32,
    children: [Node; 2],
}

/// A published entry.
struct Leaf<K, V> {
    key: K,
    value: V,
}

/// A live capped map: its census row handle with the fill byte first, the
/// tree's root link, its live count and high water, then the branch store
/// and the leaf store, so a walk's first loads (the root link and the branch
/// store's root) lie in the map's first line when the map starts on one.
#[repr(C)]
pub struct CappedMap<K: ExactKey, V> {
    row: CapRowHandle,
    root: Option<Node>,
    live: u32,
    high_water: u32,
    branches: SlotStore<Branch, ()>,
    leaves: SlotStore<Leaf<K, V>, ()>,
}

/// A map being built, one chunk or directory per step, its leaf store
/// first; it serves nothing.
pub struct MapBuilder<K: ExactKey, V> {
    row: CapRowHandle,
    branches: SlotStore<Branch, ()>,
    leaves: SlotStore<Leaf<K, V>, ()>,
    building: Building,
}

/// The store a builder's next step allocates in.
#[derive(Clone, Copy)]
enum Building {
    Leaves,
    Branches,
}

/// A map being retired, or a refused build's prefix: one chunk or directory
/// freed per step, the leaves with their occupants first, then the
/// branches.
pub struct MapRetiring<K: ExactKey, V> {
    branches: SlotStore<Branch, ()>,
    leaves: SlotStore<Leaf<K, V>, ()>,
}

/// The answer of a build step.
pub enum MapBuilt<K: ExactKey, V> {
    More(MapBuilder<K, V>),
    Live(CappedMap<K, V>),
    /// A step's allocation was refused: nothing was served, and what was
    /// built retires by steps.
    Refused(MapRetiring<K, V>, CapError),
}

/// The answer of a retire step.
pub enum MapRetired<K: ExactKey, V> {
    More(MapRetiring<K, V>),
    Done,
}

/// The answer to `admit(key)`: the present entry, a vacant slot for the
/// key, or `Full` (no value taken, the row's crossings up by one). It
/// borrows the map, so it is matched in the step that admitted and cannot
/// cross an `await` (clippy's `await-holding-invalid-types` names it).
#[must_use = "an admission is published, replaced or dropped; it carries no value"]
pub enum Admission<'a, K: ExactKey, V> {
    Occupied(OccupiedSlot<'a, K, V>),
    Vacant(VacantSlot<'a, K, V>),
    Full,
}

/// The present entry of an admitted key, borrowed exclusively for one
/// step: its value may be read, edited or replaced, at any count, the cap
/// included (ADR-0151 D2).
pub struct OccupiedSlot<'a, K: ExactKey, V> {
    key: K,
    value: &'a mut V,
}

/// An exclusive reservation for an absent key: its leaf slot and, in a
/// nonempty tree, its branch slot and the link the branch takes are fixed
/// before the value exists. `publish` consumes it once, cannot allocate or
/// refuse, and looks nothing up again; dropping it leaves the map as it
/// was. It borrows the map, so it cannot cross an `await` or a reactor
/// iteration: a resumed step admits again.
#[must_use = "a vacant slot publishes a value or is dropped; it carries none"]
pub struct VacantSlot<'a, K: ExactKey, V> {
    key: K,
    leaf: FreeSlot<'a, Leaf<K, V>, ()>,
    place: Place<'a>,
    live: &'a mut u32,
    high_water: &'a mut u32,
    row: &'a CapRowHandle,
}

/// Where a vacant slot's leaf joins the tree.
enum Place<'a> {
    /// The tree is empty: the leaf becomes the root.
    Root(&'a mut Option<Node>),
    /// A new branch at `bit` splits above `displaced`: the branch takes it
    /// and the leaf as its children, and `link`, which named `displaced`,
    /// names the branch.
    Split { branch: FreeSlot<'a, Branch, ()>, bit: u32, displaced: Node, link: Link<'a> },
}

/// The one link a publish rewrites: the root, or a branch's child.
enum Link<'a> {
    Root(&'a mut Option<Node>),
    Child(&'a mut Node),
}

impl<K: ExactKey, V> CappedMap<K, V> {
    /// A map of `cap` on `census`, to build by steps: both stores' layouts
    /// are checked (a chunk or a directory past one backing allocation is
    /// `Layout`, and no row is registered), then the map's one row is
    /// registered, then the leaf store of `N` slots and the branch store of
    /// `N − 1` are constructed, each minting its instance (`Census` past the
    /// census's mints; the row stays). Nothing is allocated until the first
    /// step.
    pub fn builder(cap: Cap, census: &Rc<CapCensus>) -> Result<MapBuilder<K, V>, CapError> {
        if !SlotStore::<Leaf<K, V>, ()>::layout_fits() || !SlotStore::<Branch, ()>::layout_fits() {
            return Err(CapError::Layout);
        }
        let row = census.register(cap)?;
        let leaves = SlotStore::new(cap.entries_max(), census)?;
        let branches = SlotStore::new(branches_for(cap.entries_max()), census)?;
        Ok(MapBuilder { row, branches, leaves, building: Building::Leaves })
    }

    /// Live entries, at most `entries_max`.
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

    /// The most entries the map holds at once, its cap.
    #[must_use]
    pub fn entries_max(&self) -> u32 {
        self.leaves.capacity_now()
    }

    /// The most entries this instance has held at once (a `Serving` map's;
    /// an `Assembly` map reports its every publish instead).
    #[must_use]
    pub fn high_water(&self) -> u32 {
        self.high_water
    }

    /// The value under `key`, if present.
    #[must_use]
    #[inline]
    pub fn get(&self, key: K) -> Option<&V> {
        let leaf = self.leaf(self.nearest(self.root?, key));
        if leaf.key == key { Some(&leaf.value) } else { None }
    }

    /// The value under `key`, mutably, if present.
    #[must_use]
    #[inline]
    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let nearest = self.nearest(self.root?, key);
        let leaf = live_mut(self.leaves.slot_mut(nearest));
        if leaf.key == key { Some(&mut leaf.value) } else { None }
    }

    /// The entry for `key`: `Occupied` when present, at any count; for an
    /// absent key `Vacant` while the map holds fewer than its cap, else
    /// `Full`, answered before any slot is reserved.
    #[inline]
    pub fn admit(&mut self, key: K) -> Admission<'_, K, V> {
        let split = match self.root {
            None => None,
            Some(root) => {
                let nearest = self.nearest(root, key);
                match key.first_difference(self.leaf(nearest).key) {
                    None => return self.occupied(nearest, key),
                    Some(bit) => Some((root, bit)),
                }
            }
        };
        if !self.has_room() {
            return self.full();
        }
        match split {
            None => self.vacant_root(key),
            Some((root, bit)) => self.vacant_split(root, key, bit),
        }
    }

    /// The value under `key` returns to its owner: its leaf and the branch
    /// above it are unlinked, the sibling takes the branch's place, and both
    /// slots return to their chains. An `Assembly` map reports the removal
    /// (its fill byte, tested once). An absent key changes nothing.
    #[inline]
    pub fn remove(&mut self, key: K) -> Option<V> {
        let mut node = self.root?;
        let (mut parent, mut grandparent) = (None, None);
        let leaf = loop {
            match node {
                Node::Leaf(index) => break index,
                Node::Branch(index) => {
                    let branch = self.branch(index);
                    let side = usize::from(key.bit(branch.bit));
                    grandparent = parent;
                    parent = Some((index, side));
                    node = branch.children[side];
                }
            }
        };
        if self.leaf(leaf).key != key {
            return None;
        }
        if let Some((index, side)) = parent {
            let sibling = self.branch(index).children[side ^ 1];
            match grandparent {
                None => self.root = Some(sibling),
                Some((above, above_side)) => {
                    live_mut(self.branches.slot_mut(above)).children[above_side] = sibling;
                }
            }
            vacate(&mut self.branches, index);
        } else {
            self.root = None;
        }
        let Leaf { value, .. } = vacate(&mut self.leaves, leaf);
        self.live = live_removed(self.live);
        if self.row.fill() == CapFill::Assembly {
            self.row.report_removal(self.live);
        }
        Some(value)
    }

    /// Retirement of the whole map, by steps.
    pub fn retire(self) -> MapRetiring<K, V> {
        MapRetiring { branches: self.branches, leaves: self.leaves }
    }

    /// The leaf whose key shares the longest prefix with `key`: at each
    /// branch the side `key`'s bit names. Split bits strictly increase toward
    /// the leaves, so the walk visits at most `K::BITS` branches.
    #[inline]
    fn nearest(&self, root: Node, key: K) -> u32 {
        let mut node = root;
        loop {
            match node {
                Node::Leaf(index) => return index,
                Node::Branch(index) => {
                    let branch = self.branch(index);
                    node = branch.children[usize::from(key.bit(branch.bit))];
                }
            }
        }
    }

    /// Where an absent `key` that first differs from its nearest leaf at
    /// `bit` splits off: the first node on its path that is a leaf or a
    /// branch at a later bit, and the branch and side whose link names it
    /// (`None`: the root names it). At most `K::BITS` visits.
    fn split_point(&self, root: Node, key: K, bit: u32) -> (Option<(u32, usize)>, Node) {
        let (mut parent, mut node) = (None, root);
        while let Node::Branch(index) = node {
            let branch = self.branch(index);
            // The nearest leaf agrees with `key` above `bit`, so no branch
            // on the shared path tests `bit` itself.
            debug_assert_ne!(branch.bit, bit, "a split bit on the shared path");
            if branch.bit > bit {
                break;
            }
            let side = usize::from(key.bit(branch.bit));
            parent = Some((index, side));
            node = branch.children[side];
        }
        (parent, node)
    }

    /// The admission compare, `live < N`: the slab's form, on the map's own
    /// live count. The cap-minus-one canary admits one entry fewer on a
    /// `Serving` row.
    #[inline]
    fn has_room(&self) -> bool {
        let capacity = self.leaves.capacity_now();
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

    fn occupied(&mut self, nearest: u32, key: K) -> Admission<'_, K, V> {
        let leaf = live_mut(self.leaves.slot_mut(nearest));
        Admission::Occupied(OccupiedSlot { key, value: &mut leaf.value })
    }

    /// The crossing, off the hot path.
    #[cold]
    #[inline(never)]
    fn full(&self) -> Admission<'_, K, V> {
        self.row.report_full();
        Admission::Full
    }

    /// An empty tree's admission: one leaf slot, no branch.
    fn vacant_root(&mut self, key: K) -> Admission<'_, K, V> {
        let capacity = self.leaves.capacity_now();
        let CappedMap { row, root, live, high_water, leaves, .. } = self;
        let Some(leaf) = leaves.reserve() else { return refused(row, *live, capacity) };
        Admission::Vacant(VacantSlot { key, leaf, place: Place::Root(root), live, high_water, row })
    }

    /// A nonempty tree's admission: one leaf slot, one branch slot and the
    /// link the branch takes, all fixed now, so the publish walks nothing.
    fn vacant_split(&mut self, root: Node, key: K, bit: u32) -> Admission<'_, K, V> {
        let (parent, displaced) = self.split_point(root, key, bit);
        let capacity = self.leaves.capacity_now();
        let CappedMap { row, root: root_link, live, high_water, branches, leaves } = self;
        let Some(leaf) = leaves.reserve() else { return refused(row, *live, capacity) };
        let (branch, link) = match parent {
            None => match branches.reserve() {
                Some(branch) => (branch, Link::Root(root_link)),
                None => return refused(row, *live, capacity),
            },
            Some((index, side)) => match branches.reserve_beside(index) {
                Some((branch, above)) => {
                    (branch, Link::Child(&mut live_mut(Some(above)).children[side]))
                }
                None => return refused(row, *live, capacity),
            },
        };
        let place = Place::Split { branch, bit, displaced, link };
        Admission::Vacant(VacantSlot { key, leaf, place, live, high_water, row })
    }

    #[inline]
    fn branch(&self, index: u32) -> &Branch {
        live(self.branches.slot(index))
    }

    #[inline]
    fn leaf(&self, index: u32) -> &Leaf<K, V> {
        live(self.leaves.slot(index))
    }
}

/// A reserve no correct build reaches: the leaf chain holds `N − live`
/// slots and the admission compare passed, and a free leaf implies a free
/// branch (ADR-0151 D3). A debug build asserts; a release build answers
/// `Full`, every reservation taken dropped (nothing was published, so
/// nothing changed), and reports the crossing, which the census reads as a
/// crossing below the cap.
#[cold]
#[inline(never)]
fn refused<'a, K: ExactKey, V>(
    row: &CapRowHandle,
    live: u32,
    capacity: u32,
) -> Admission<'a, K, V> {
    debug_assert!(live >= capacity, "a map's chain is empty with live below its cap");
    row.report_full();
    Admission::Full
}

/// The occupant of a slot a tree link names. The tree links only live
/// slots: a publish links a slot after occupying it, and a removal unlinks
/// a leaf and its branch before freeing either.
#[inline]
fn live<T>(slot: Option<&Slot<T, ()>>) -> &T {
    match slot {
        Some(Slot::Live { value, .. }) => value,
        Some(Slot::Free { .. } | Slot::Closed) | None => {
            unreachable!("a map's tree links only live slots")
        }
    }
}

/// As [`live`], mutably.
#[inline]
fn live_mut<T>(slot: Option<&mut Slot<T, ()>>) -> &mut T {
    match slot {
        Some(Slot::Live { value, .. }) => value,
        Some(Slot::Free { .. } | Slot::Closed) | None => {
            unreachable!("a map's tree links only live slots")
        }
    }
}

/// The occupant of slot `index`, just unlinked, moves out, and the slot
/// rejoins its chain at the head.
fn vacate<T>(store: &mut SlotStore<T, ()>, index: u32) -> T {
    let head = store.free_head();
    let free = Slot::Free { generation: (), next: head };
    match store.slot_mut(index).map(|slot| mem::replace(slot, free)) {
        Some(Slot::Live { value, .. }) => {
            store.chain(index);
            value
        }
        Some(Slot::Free { .. } | Slot::Closed) | None => {
            unreachable!("a map's tree links only live slots")
        }
    }
}

impl Link<'_> {
    fn name(self, node: Node) {
        match self {
            Link::Root(root) => *root = Some(node),
            Link::Child(child) => *child = node,
        }
    }
}

impl<K: ExactKey, V> OccupiedSlot<'_, K, V> {
    #[must_use]
    pub fn key(&self) -> K {
        self.key
    }

    #[must_use]
    pub fn get(&self) -> &V {
        self.value
    }

    #[must_use]
    pub fn get_mut(&mut self) -> &mut V {
        self.value
    }

    /// The entry takes `value`; the value it held returns to its owner. The
    /// map's count does not move.
    #[must_use = "the replaced value returns to its owner"]
    pub fn replace(self, value: V) -> V {
        mem::replace(self.value, value)
    }
}

impl<K: ExactKey, V> VacantSlot<'_, K, V> {
    #[must_use]
    pub fn key(&self) -> K {
        self.key
    }

    /// The value takes the slot: the leaf, and in a nonempty tree its
    /// branch, are written, then one link names them. Cannot allocate or
    /// refuse: the slots were free and the reservation held them.
    #[inline]
    pub fn publish(self, value: V) {
        let VacantSlot { key, leaf, place, live, high_water, row } = self;
        let (leaf, ()) = leaf.occupy(Leaf { key, value });
        let leaf = Node::Leaf(leaf);
        match place {
            Place::Root(root) => *root = Some(leaf),
            Place::Split { branch, bit, displaced, link } => {
                let children = if key.bit(bit) { [displaced, leaf] } else { [leaf, displaced] };
                let (branch, ()) = branch.occupy(Branch { bit, children });
                link.name(Node::Branch(branch));
            }
        }
        let count = live_published(*live);
        *live = count;
        // An `Assembly` map keeps its own high water at 0, so this one
        // compare reports its every publish and the row keeps its live count.
        if count > *high_water {
            if row.fill() == CapFill::Serving {
                *high_water = count;
            }
            row.report_publish(count);
        }
    }
}

impl<K: ExactKey, V> MapBuilder<K, V> {
    /// One build step: one chunk or one directory allocated, the leaf store
    /// first. `Live` once the branch store's last chunk is built; a map of
    /// one entry has no branch slot, so it is `Live` with its leaf store. A
    /// refused allocation retires what was built.
    pub fn step(mut self) -> MapBuilt<K, V> {
        let built = match self.building {
            Building::Leaves => self.leaves.build_step(),
            Building::Branches => self.branches.build_step(),
        };
        match (built, self.building) {
            (Err(refused), Building::Leaves | Building::Branches) => {
                let prefix = MapRetiring { branches: self.branches, leaves: self.leaves };
                MapBuilt::Refused(prefix, refused)
            }
            (Ok(BuildStep::More), Building::Leaves | Building::Branches) => MapBuilt::More(self),
            (Ok(BuildStep::Done), Building::Leaves) if self.branches.capacity_now() > 0 => {
                self.building = Building::Branches;
                MapBuilt::More(self)
            }
            (Ok(BuildStep::Done), Building::Leaves | Building::Branches) => {
                MapBuilt::Live(CappedMap {
                    row: self.row,
                    root: None,
                    live: 0,
                    high_water: 0,
                    branches: self.branches,
                    leaves: self.leaves,
                })
            }
        }
    }
}

impl<K: ExactKey, V> MapRetiring<K, V> {
    /// One retire step: the last chunk or directory of the leaf store freed
    /// with its occupants, then, once it holds nothing, of the branch store.
    /// `Done` once neither holds an item.
    pub fn step(mut self) -> MapRetired<K, V> {
        let more = if self.leaves.items_held() > 0 {
            match self.leaves.retire_step() {
                StoreRetire::More => true,
                StoreRetire::Done => self.branches.items_held() > 0,
            }
        } else {
            match self.branches.retire_step() {
                StoreRetire::More => true,
                StoreRetire::Done => false,
            }
        };
        if more { MapRetired::More(self) } else { MapRetired::Done }
    }
}

/// The branch store's slots for a map of `entries`: `entries − 1`, so a map
/// of one entry has none (ADR-0151 D3).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: entries >= 1, a Cap's NonZeroU32, so - 1 stays at or above 0 (ADR-0165 \
              D1's bound row)"
)]
fn branches_for(entries: u32) -> u32 {
    entries - 1
}

/// A pair key's bit index in its second word: after the first word's 64.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: bit < 64, a u64's leading zeros below its width, so 64 + bit < 128 (ADR-0165 \
              D1's bound row)"
)]
fn second_word_bit(bit: u32) -> u32 {
    u64::BITS + bit
}

/// `live` after a publish.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: live < entries_max <= u32::MAX by the admission compare before the \
              reservation, so + 1 stays within u32 (ADR-0165 D1's bound row)"
)]
fn live_published(live: u32) -> u32 {
    live + 1
}

/// `live` after a removal.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bound: live >= 1, the leaf the removal found and unlinked, so - 1 stays at or above \
              0 (ADR-0165 D1's bound row)"
)]
fn live_removed(live: u32) -> u32 {
    live - 1
}

#[cfg(test)]
mod tests {
    use core::mem::{offset_of, size_of};
    use std::collections::BTreeMap;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::bounded::{CapRow, CensusPhase, ServeMark};
    use crate::limits::{CAP_CENSUS_SLABS_MAX, MAP_KEY_BITS_MAX};
    use crate::rng::{Entropy, SplitMix64};

    const CELL: CellId = CellId(9);

    fn census() -> (Rc<CapCensus>, ServeMark) {
        CapCensus::new(CELL)
    }

    /// A map of `cap`, built by steps; the steps it took.
    fn built<K: ExactKey, V>(cap: Cap, census: &Rc<CapCensus>) -> (CappedMap<K, V>, u32) {
        let mut builder = CappedMap::builder(cap, census).expect("registered");
        let mut steps = 0;
        loop {
            steps += 1;
            match builder.step() {
                MapBuilt::More(next) => builder = next,
                MapBuilt::Live(map) => return (map, steps),
                MapBuilt::Refused(_, refused) => panic!("a build step refused: {refused}"),
            }
        }
    }

    /// Retires a map by steps; the steps it took.
    fn retired<K: ExactKey, V>(mut retiring: MapRetiring<K, V>) -> u32 {
        let mut steps = 0;
        loop {
            steps += 1;
            match retiring.step() {
                MapRetired::More(next) => retiring = next,
                MapRetired::Done => return steps,
            }
        }
    }

    /// What an admission answered.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Answer {
        Occupied,
        Vacant,
        Full,
    }

    /// Admits `key`: a vacant slot publishes `value`, a present entry takes
    /// it.
    fn insert<K: ExactKey, V>(map: &mut CappedMap<K, V>, key: K, value: V) -> Answer {
        match map.admit(key) {
            Admission::Occupied(slot) => {
                drop(slot.replace(value));
                Answer::Occupied
            }
            Admission::Vacant(slot) => {
                slot.publish(value);
                Answer::Vacant
            }
            Admission::Full => Answer::Full,
        }
    }

    /// The free chain's length, followed slot by slot from its head.
    fn chain_len<T>(store: &SlotStore<T, ()>) -> u32 {
        let (mut next, mut count) = (store.free_head(), 0);
        while let Some(Slot::Free { next: after, .. }) = store.slot(next) {
            count += 1;
            next = *after;
        }
        count
    }

    /// The tree read whole by an explicit stack, apart from the map's own
    /// walks: every split's bit is below `K::BITS` and above its parent's;
    /// each leaf's key takes the side named at every split above it; the
    /// entries equal the model's; the leaves are the live count and the
    /// branches one fewer; each store's free chain holds the rest.
    fn judge<K: ExactKey + Ord, V: Copy + PartialEq + fmt::Debug>(
        map: &CappedMap<K, V>,
        model: &BTreeMap<K, V>,
    ) {
        let mut found = BTreeMap::new();
        let mut branches = 0u32;
        // A node, the bit of the split above it, and the (bit, side) pairs
        // on its path.
        type Visit = (Node, Option<u32>, Vec<(u32, bool)>);
        let mut stack: Vec<Visit> =
            map.root.into_iter().map(|node| (node, None, Vec::new())).collect();
        while let Some((node, above, path)) = stack.pop() {
            match node {
                Node::Leaf(index) => {
                    let leaf = map.leaf(index);
                    for &(bit, side) in &path {
                        assert_eq!(leaf.key.bit(bit), side, "{:?} off its side of {bit}", leaf.key);
                    }
                    assert!(found.insert(leaf.key, leaf.value).is_none(), "a key twice");
                }
                Node::Branch(index) => {
                    let branch = map.branch(index);
                    assert!(branch.bit < K::BITS, "a split past the key's width");
                    if let Some(above) = above {
                        assert!(branch.bit > above, "split bits increase toward the leaves");
                    }
                    branches += 1;
                    for (side, child) in branch.children.into_iter().enumerate() {
                        let mut path = path.clone();
                        path.push((branch.bit, side == 1));
                        stack.push((child, Some(branch.bit), path));
                    }
                }
            }
        }
        assert_eq!(found, *model, "the tree's entries");
        assert_eq!(map.len() as usize, model.len());
        assert_eq!(branches, map.len().saturating_sub(1), "n - 1 branches for n leaves");
        let cap = map.entries_max();
        assert_eq!(chain_len(&map.leaves), cap - map.len(), "the leaf chain holds the rest");
        let branch_free = (cap - 1) - map.len().saturating_sub(1);
        assert_eq!(chain_len(&map.branches), branch_free, "the branch chain holds the rest");
    }

    /// Branches the path to `key` tests, read apart from the map's walk.
    fn depth<K: ExactKey, V>(map: &CappedMap<K, V>, key: K) -> u32 {
        let (mut node, mut visits) = (map.root.expect("a nonempty map"), 0);
        while let Node::Branch(index) = node {
            let branch = map.branch(index);
            visits += 1;
            node = branch.children[usize::from(key.bit(branch.bit))];
        }
        visits
    }

    /// What a model run's ops reached.
    #[derive(Default, Debug)]
    struct Reach {
        occupied: u64,
        vacant: u64,
        full: u64,
        removed: u64,
    }

    /// The model: a `BTreeMap<K, V>` sharing no code with the map. Admits
    /// with publish or replace, removals, writes through `get_mut` and
    /// dropped admissions on keys drawn from `keys`, each checked against
    /// the model; after every op the tree is judged whole and every key of
    /// the pool read back; at the end the row's high water, crossings and
    /// capacity equal the model's.
    fn model_run<K: ExactKey + Ord>(cap: Cap, keys: &[K], seed: u64, ops: u32) -> Reach {
        let (census, _mark) = census();
        let (mut map, _) = built::<K, u64>(cap, &census);
        let mut model: BTreeMap<K, u64> = BTreeMap::new();
        let (mut high_water, mut crossings, mut reach) = (0u32, 0u64, Reach::default());
        let mut rng = SplitMix64::new(seed);
        for op in 0..ops {
            let key = keys[rng.next_below(keys.len() as u64) as usize];
            let value = u64::from(op);
            match rng.next_below(10) {
                0..=5 => match map.admit(key) {
                    Admission::Occupied(mut slot) => {
                        assert_eq!(slot.key(), key);
                        assert_eq!(Some(slot.get()), model.get(&key));
                        *slot.get_mut() ^= 1;
                        let before = model.insert(key, value).expect("present") ^ 1;
                        assert_eq!(slot.replace(value), before, "{key:?}: the replaced value");
                        reach.occupied += 1;
                    }
                    Admission::Vacant(slot) => {
                        assert_eq!(slot.key(), key);
                        assert!(!model.contains_key(&key), "{key:?}: vacant while present");
                        assert!((model.len() as u32) < cap.entries_max(), "vacant at the cap");
                        slot.publish(value);
                        model.insert(key, value);
                        high_water = high_water.max(model.len() as u32);
                        reach.vacant += 1;
                    }
                    Admission::Full => {
                        assert!(!model.contains_key(&key), "{key:?}: Full while present");
                        assert_eq!(model.len() as u32, cap.entries_max(), "Full below the cap");
                        crossings += 1;
                        reach.full += 1;
                    }
                },
                6 | 7 => {
                    let removed = map.remove(key);
                    assert_eq!(removed, model.remove(&key), "{key:?}: remove");
                    reach.removed += u64::from(removed.is_some());
                }
                8 => match (map.get_mut(key), model.get_mut(&key)) {
                    (Some(found), Some(expected)) => {
                        *found = value;
                        *expected = value;
                    }
                    (None, None) => {}
                    (found, expected) => panic!("{key:?}: get_mut {found:?}, model {expected:?}"),
                },
                _ => match map.admit(key) {
                    Admission::Vacant(slot) => drop(slot),
                    Admission::Full => crossings += 1,
                    Admission::Occupied(_) => {}
                },
            }
            judge(&map, &model);
            for key in keys {
                assert_eq!(map.get(*key), model.get(key), "{key:?}: get");
            }
        }
        let row = census.row(cap.name()).expect("row");
        let expected = (high_water, crossings, cap.entries_max());
        assert_eq!((row.high_water, row.crossings, row.capacity_now), expected);
        assert_eq!(map.high_water(), high_water);
        reach
    }

    /// Distinct random words, each with a one-bit neighbour (a key sharing
    /// all but one bit with it), and the word's two extremes.
    fn pool_u64(seed: u64, count: usize) -> Vec<u64> {
        let mut rng = SplitMix64::new(seed);
        let mut keys = vec![0, u64::MAX];
        while keys.len() < count {
            let key = rng.next_u64();
            keys.push(key);
            keys.push(key ^ (1 << rng.next_below(64)));
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    #[test]
    fn the_map_equals_its_model_across_chunk_and_directory_edges() {
        let mut total = Reach::default();
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
            let seed = 0x3A9 + index as u64;
            let keys = pool_u64(seed, 2 * cap.entries_max() as usize + 3);
            let reach = model_run(cap, &keys, seed, 3_000);
            assert!(reach.full > 0, "{cap:?} never reached its cap: {reach:?}");
            total.occupied += reach.occupied;
            total.vacant += reach.vacant;
            total.full += reach.full;
            total.removed += reach.removed;
        }
        assert!(total.occupied > 0 && total.vacant > 0 && total.removed > 0, "{total:?}");
    }

    /// Every key type of the sealed set under the model: the widest words
    /// with neighbours across both of a pair's words, a narrow word whose
    /// whole universe fits the pool, the cell's id and the unit key.
    #[test]
    fn every_key_type_equals_its_model() {
        let words = pool_u64(0xC1, 160);
        let wide: Vec<u128> =
            words.chunks(2).map(|w| u128::from(w[0]) << 64 | u128::from(w[1])).collect();
        let mut wide_pool = wide.clone();
        wide_pool.extend(wide.iter().map(|key| key ^ 1 << 127));
        let pairs: Vec<(u64, u64)> =
            wide_pool.iter().map(|w| ((w >> 64) as u64, *w as u64)).collect();
        let narrow: Vec<u8> = (0..=u8::MAX).collect();
        let cells: Vec<CellId> = (0..120).map(|id| CellId(id * 547)).collect();
        let cap = Cap::entries::<64>("keys", CapFill::Serving);
        assert!(model_run(cap, &wide_pool, 1, 2_000).full > 0);
        assert!(model_run(cap, &pairs, 2, 2_000).full > 0);
        assert!(model_run(cap, &narrow, 3, 2_000).full > 0);
        assert!(model_run(cap, &cells, 4, 2_000).full > 0);
        let reach = model_run(Cap::entries::<2>("unit", CapFill::Serving), &[()], 5, 500);
        assert_eq!(reach.full, 0, "the unit key is one key");
        assert!(reach.occupied > 0 && reach.vacant > 0 && reach.removed > 0, "{reach:?}");
    }

    /// Zero and every single-bit key: the path to zero tests every bit of
    /// the key, the lookup's ceiling (ADR-0151 D3), on the widest word and
    /// on a pair, whose second word's bits follow its first's.
    #[test]
    fn a_full_width_one_bit_ladder_reaches_the_lookup_ceiling() {
        let (census, _mark) = census();
        let cap = Cap::entries::<129>("ladder", CapFill::Serving);
        let wide: Vec<u128> = core::iter::once(0).chain((0..128).map(|b| 1u128 << b)).collect();
        let pair = |w: u128| ((w >> 64) as u64, w as u64);
        let (mut words, _) = built::<u128, u32>(cap, &census);
        let (mut pairs, _) = built::<(u64, u64), u32>(cap, &census);
        let (mut word_model, mut pair_model) = (BTreeMap::new(), BTreeMap::new());
        for (value, key) in (0u32..).zip(wide.iter().copied()) {
            assert_eq!(insert(&mut words, key, value), Answer::Vacant);
            assert_eq!(insert(&mut pairs, pair(key), value), Answer::Vacant);
            word_model.insert(key, value);
            pair_model.insert(pair(key), value);
        }
        judge(&words, &word_model);
        judge(&pairs, &pair_model);
        assert_eq!(depth(&words, 0), MAP_KEY_BITS_MAX);
        assert_eq!(depth(&pairs, (0, 0)), MAP_KEY_BITS_MAX);
        for key in &wide {
            assert_eq!(words.get(*key), word_model.get(key));
            assert_eq!(pairs.get(pair(*key)), pair_model.get(&pair(*key)));
            assert!(depth(&words, *key) <= MAP_KEY_BITS_MAX);
        }
        for key in wide.iter().step_by(2) {
            assert_eq!(words.remove(*key), word_model.remove(key));
            assert_eq!(pairs.remove(pair(*key)), pair_model.remove(&pair(*key)));
            judge(&words, &word_model);
            judge(&pairs, &pair_model);
        }
    }

    #[test]
    fn each_key_type_reads_its_bits_from_the_most_significant() {
        let key = 0b1010_0000_u8;
        assert_eq!(
            (0..8).map(|i| key.bit(i)).collect::<Vec<_>>(),
            [true, false, true, false, false, false, false, false]
        );
        let word = 0x8000_0000_0000_0001_u64;
        assert!(word.bit(0) && word.bit(63) && !word.bit(1));
        assert_eq!(word.first_difference(word), None);
        assert_eq!(word.first_difference(1), Some(0));
        assert_eq!(1u64.first_difference(0), Some(63));
        assert_eq!((1u128 << 127).first_difference(0), Some(0));
        assert_eq!(1u128.first_difference(0), Some(127));
        let pair = (0x8000_0000_0000_0000_u64, 1_u64);
        assert!(pair.bit(0) && !pair.bit(64) && pair.bit(127));
        assert_eq!(pair.first_difference((0, 1)), Some(0), "the first word's top bit");
        assert_eq!(pair.first_difference((pair.0, 0)), Some(127), "the second word's last bit");
        assert_eq!(pair.first_difference(pair), None);
        assert_eq!(CellId(0x8001).first_difference(CellId(0x8000)), Some(15));
        assert!(CellId(0x8000).bit(0));
        assert_eq!(().first_difference(()), None);
        for bits in [u8::BITS, u16::BITS, u32::BITS, u64::BITS, u128::BITS] {
            assert!(bits <= MAP_KEY_BITS_MAX);
        }
        for bits in [<(u64, u64)>::BITS, <()>::BITS, CellId::BITS, <u128>::BITS] {
            assert!(bits <= MAP_KEY_BITS_MAX, "a sealed key past the bound");
        }
    }

    /// Zero, one, the cap less one, the cap and one past it, then the
    /// replacement at the cap and a removal that frees a place.
    #[test]
    fn zero_one_cap_minus_one_cap_and_cap_plus_one() {
        let (census, _mark) = census();
        let cap = Cap::entries::<3>("three", CapFill::Serving);
        let (mut map, _) = built::<u32, u32>(cap, &census);
        assert!(map.is_empty());
        assert_eq!(map.get(1), None);
        assert_eq!(map.remove(1), None, "an empty map removes nothing");
        assert_eq!(insert(&mut map, 1, 10), Answer::Vacant, "one");
        assert_eq!(insert(&mut map, 2, 20), Answer::Vacant, "cap minus one");
        assert_eq!(insert(&mut map, 3, 30), Answer::Vacant, "reaching the cap is legal");
        assert_eq!(insert(&mut map, 4, 40), Answer::Full, "cap plus one is Full");
        assert_eq!(insert(&mut map, 2, 21), Answer::Occupied, "replacement at the cap");
        assert_eq!((map.len(), map.high_water()), (3, 3));
        let row = census.row("three").expect("row");
        assert_eq!((row.high_water, row.crossings, row.capacity_now), (3, 1, 3));
        assert_eq!(map.remove(2), Some(21));
        assert_eq!(map.remove(2), None, "a removed key is absent");
        assert_eq!(insert(&mut map, 4, 40), Answer::Vacant, "a removal frees a place");
        let model = BTreeMap::from([(1, 10), (3, 30), (4, 40)]);
        judge(&map, &model);
    }

    #[test]
    fn a_dropped_vacant_slot_changes_nothing() {
        let (census, _mark) = census();
        let cap = Cap::entries::<2>("two", CapFill::Serving);
        let (mut map, _) = built::<u64, u64>(cap, &census);
        for key in [5, 6] {
            match map.admit(key) {
                Admission::Vacant(slot) => drop(slot),
                Admission::Occupied(_) | Admission::Full => panic!("room for {key}"),
            }
            assert!(map.is_empty());
        }
        assert_eq!(insert(&mut map, 5, 50), Answer::Vacant);
        match map.admit(6) {
            Admission::Vacant(slot) => drop(slot),
            Admission::Occupied(_) | Admission::Full => panic!("room for 6"),
        }
        judge(&map, &BTreeMap::from([(5, 50)]));
        assert_eq!(census.row("two").expect("row").crossings, 0);
    }

    /// A cap of one: one leaf slot and no branch store to build, one row and
    /// one crossing at a high water of one, its capacity now (the branch
    /// store's chain is never asked: a nonempty one-entry map is full).
    #[test]
    fn a_cap_one_map_holds_one_row_and_no_branch() {
        let (census, mark) = census();
        let cap = Cap::entries::<1>("map-one", CapFill::Serving);
        let (mut map, steps) = built::<u64, u64>(cap, &census);
        assert_eq!(steps, 1, "the leaf chunk; the branch store has nothing to build");
        assert_eq!(map.branches.capacity_now(), 0);
        mark.take();
        let mut model = BTreeMap::new();
        assert_eq!(insert(&mut map, 7, 70), Answer::Vacant);
        model.insert(7, 70);
        judge(&map, &model);
        assert_eq!(insert(&mut map, 7, 71), Answer::Occupied);
        model.insert(7, 71);
        judge(&map, &model);
        assert_eq!(insert(&mut map, 8, 80), Answer::Full);
        judge(&map, &model);
        assert_eq!(map.remove(7), Some(71));
        model.remove(&7);
        judge(&map, &model);
        assert_eq!(insert(&mut map, 8, 80), Answer::Vacant);
        model.insert(8, 80);
        judge(&map, &model);
        assert_eq!(census.rows().count(), 1, "one row: a map's stores register none");
        let row = census.row("map-one").expect("row");
        assert_eq!((row.high_water, row.crossings, row.capacity_now), (1, 1, 1));
        census.mark_stopping();
    }

    #[test]
    fn an_assembly_cap_one_map_full_at_the_mark_is_the_control() {
        let (census, mark) = census();
        let cap = Cap::entries::<1>("one-gate", CapFill::Assembly);
        let (mut map, _) = built::<CellId, u8>(cap, &census);
        assert_eq!(insert(&mut map, CellId(1), 1), Answer::Vacant);
        mark.take();
        assert_eq!(census.cap_assembly_violations(), 0);
        let row = census.row("one-gate").expect("row");
        assert_eq!((row.assembly_live, row.high_water), (1, 1));
        census.mark_stopping();
    }

    // The fill byte's removal leg: an `Assembly` map filled to its cap and
    // one removed before the mark is caught by the live count at the mark;
    // full at the mark is the control.

    #[test]
    fn an_assembly_map_full_at_the_mark_is_the_control() {
        let (census, mark) = census();
        let cap = Cap::entries::<3>("waiters", CapFill::Assembly);
        let (mut map, _) = built::<u32, u8>(cap, &census);
        for key in 0..3 {
            assert_eq!(insert(&mut map, key, 1), Answer::Vacant);
        }
        mark.take();
        assert_eq!(census.cap_assembly_violations(), 0);
        let row = census.row("waiters").expect("row");
        assert_eq!((row.assembly_live, row.high_water), (3, 3));
        census.mark_stopping();
    }

    #[test]
    fn an_assembly_map_with_one_removed_before_the_mark_is_asserted_and_counted() {
        let (census, mark) = census();
        let cap = Cap::entries::<3>("waiters", CapFill::Assembly);
        let (mut map, _) = built::<u32, u8>(cap, &census);
        for key in 0..3 {
            assert_eq!(insert(&mut map, key, 1), Answer::Vacant);
        }
        assert_eq!(map.remove(1), Some(1));
        assert_eq!(census.row("waiters").expect("row").assembly_live, 2, "the removal reported");
        let outcome = catch_unwind(AssertUnwindSafe(|| mark.take()));
        if cfg!(debug_assertions) {
            let text = panic_text(outcome.expect_err("asserted").as_ref());
            assert!(text.contains("cap-assembly-violated"), "{text}");
        }
        assert_eq!(census.cap_assembly_violations(), 1);
        census.mark_stopping();
    }

    /// One name, one row: a second cap value or a second fill under a
    /// name, and an `Assembly` name a second time, are `Census` and build
    /// nothing; a map and a slab of one name, cap and `Serving` fill share
    /// the row.
    #[test]
    fn a_name_keeps_one_cap_and_one_fill() {
        let (census, _mark) = census();
        let four = Cap::entries::<4>("routes", CapFill::Serving);
        let (_map, _) = built::<u64, u8>(four, &census);
        let five = Cap::entries::<5>("routes", CapFill::Serving);
        let refused = CappedMap::<u64, u8>::builder(five, &census).map(|_| ()).err();
        assert_eq!(refused, Some(CapError::Census), "a second cap value");
        let assembly = Cap::entries::<4>("routes", CapFill::Assembly);
        let refused = CappedMap::<u64, u8>::builder(assembly, &census).map(|_| ()).err();
        assert_eq!(refused, Some(CapError::Census), "a second fill");
        let gate = Cap::entries::<2>("gate", CapFill::Assembly);
        let (_gate, _) = built::<u64, u8>(gate, &census);
        let refused = CappedMap::<u64, u8>::builder(gate, &census).map(|_| ()).err();
        assert_eq!(refused, Some(CapError::Census), "an Assembly name a second time");
        let shared =
            crate::bounded::CappedSlab::<u8, u32, crate::bounded::Retire>::builder(four, &census);
        assert!(shared.is_ok(), "a slab shares the map's row");
        assert_eq!(census.rows().count(), 2);
    }

    #[test]
    fn a_chunk_past_one_backing_allocation_is_layout_and_registers_nothing() {
        let (census, _mark) = census();
        let cap = Cap::entries::<4>("wide", CapFill::Serving);
        let refused = CappedMap::<u64, [u8; 1100]>::builder(cap, &census);
        assert_eq!(refused.map(|_| ()).err(), Some(CapError::Layout));
        assert_eq!(census.rows().count(), 0);
    }

    /// A census that has minted all but one ordinal: the leaf store takes
    /// the last and the branch store's mint is `Census`, so nothing is
    /// built and the row stays; with two left the stores take them in turn.
    #[test]
    fn a_map_mints_one_ordinal_per_store_and_none_past_the_census() {
        let (census, _mark) = census();
        for _ in 0..CAP_CENSUS_SLABS_MAX - 1 {
            census.mint_instance().expect("within the ordinals");
        }
        let cap = Cap::entries::<4>("late", CapFill::Serving);
        let refused = CappedMap::<u64, u8>::builder(cap, &census).map(|_| ()).err();
        assert_eq!(refused, Some(CapError::Census));
        assert!(census.row("late").is_some(), "the row stays");
        assert_eq!(census.unsliced_drops(), 0, "the leaf store built nothing");
        let (census, _mark) = super::tests::census();
        for _ in 0..CAP_CENSUS_SLABS_MAX - 2 {
            census.mint_instance().expect("within the ordinals");
        }
        let builder = CappedMap::<u64, u8>::builder(cap, &census).expect("two ordinals left");
        let ordinals = (builder.leaves.instance().ordinal(), builder.branches.instance().ordinal());
        assert_eq!(ordinals, (CAP_CENSUS_SLABS_MAX - 1, CAP_CENSUS_SLABS_MAX));
    }

    #[test]
    fn a_build_and_a_retirement_take_one_item_per_step() {
        // Leaves then branches: one chunk each up to 64 entries (a map of
        // one has no branch slot); a directory over 64 slots; two levels
        // past 16,384.
        let (census, _mark) = census();
        for (cap, steps) in [
            (Cap::entries::<1>("s-1", CapFill::Serving), 1),
            (Cap::entries::<2>("s-2", CapFill::Serving), 2),
            (Cap::entries::<64>("s-64", CapFill::Serving), 2),
            (Cap::entries::<65>("s-65", CapFill::Serving), 4),
            (Cap::entries::<1000>("s-1000", CapFill::Serving), 34),
            (Cap::entries::<16_385>("s-16385", CapFill::Serving), 517),
        ] {
            let (map, built) = built::<u64, u64>(cap, &census);
            assert_eq!(built, steps, "{cap:?}");
            assert_eq!(map.entries_max(), cap.entries_max());
            assert_eq!(retired(map.retire()), steps, "{cap:?}");
        }
        assert_eq!(census.unsliced_drops(), 0);
    }

    /// The occupants a retirement frees are dropped by its steps, once
    /// each; a replaced or removed value returned to its owner is not.
    #[test]
    fn a_retirement_drops_each_occupant_once() {
        let (census, _mark) = census();
        let cap = Cap::entries::<100>("held", CapFill::Serving);
        let (mut map, _) = built::<u64, Rc<()>>(cap, &census);
        let token = Rc::new(());
        for key in 0..100 {
            assert_eq!(insert(&mut map, key * 3, Rc::clone(&token)), Answer::Vacant);
        }
        let Admission::Occupied(slot) = map.admit(0) else { panic!("present") };
        let returned = slot.replace(Rc::clone(&token));
        let removed = map.remove(3).expect("present");
        assert_eq!(Rc::strong_count(&token), 102);
        drop((returned, removed));
        // Each store: a directory and two chunks.
        assert_eq!(retired(map.retire()), 6);
        assert_eq!(Rc::strong_count(&token), 1, "every occupant dropped once");
    }

    #[test]
    fn the_map_header_starts_with_its_walk() {
        type Map = CappedMap<u64, u64>;
        assert_eq!(size_of::<Map>(), 112);
        assert_eq!(offset_of!(Map, row), 0);
        assert_eq!(offset_of!(Map, root), 16);
        assert_eq!(offset_of!(Map, live), 24);
        assert_eq!(offset_of!(Map, high_water), 28);
        assert_eq!(offset_of!(Map, branches), 32);
        assert_eq!(offset_of!(Map, leaves), 72);
        // A store's root follows its instance and free head (8 bytes) and
        // spans 16: the branch store's lies in bytes 40 to 56 of the first
        // line.
        assert_eq!(size_of::<SlotStore<Branch, ()>>(), 40);
        assert!(offset_of!(Map, branches) + 8 + 16 <= 64);
    }

    // The unsliced drops: a live map (two stores), a builder at its `More`
    // (its leaf store alone holds an item) and a retiring prefix, each
    // dropped holding backing while the cell serves, are counted and
    // asserted; the same once stopping, or while assembling, are not.

    fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .expect("a panic message")
    }

    fn expect_unsliced(census: &Rc<CapCensus>, stores: u32, drop_it: impl FnOnce()) {
        let before = census.unsliced_drops();
        let outcome = catch_unwind(AssertUnwindSafe(drop_it));
        if cfg!(debug_assertions) {
            let text = panic_text(outcome.expect_err("unsliced-drop: asserted").as_ref());
            assert!(text.contains("unsliced-drop"), "{text}");
        }
        assert_eq!(census.unsliced_drops(), before + stores, "unsliced-drop: counted");
    }

    #[test]
    fn a_live_map_dropped_while_serving_is_two_unsliced_drops() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("dropped", CapFill::Serving);
        let (mut map, _) = built::<u64, u8>(cap, &census);
        assert_eq!(insert(&mut map, 1, 1), Answer::Vacant);
        mark.take();
        expect_unsliced(&census, 2, || drop(map));
    }

    #[test]
    fn a_builder_abandoned_at_more_while_serving_is_an_unsliced_drop() {
        let (census, mark) = census();
        mark.take();
        let cap = Cap::entries::<100>("abandoned", CapFill::Serving);
        let builder = CappedMap::<u64, u8>::builder(cap, &census).expect("registered");
        let MapBuilt::More(builder) = builder.step() else { panic!("a directory, then chunks") };
        expect_unsliced(&census, 1, || drop(builder));
    }

    #[test]
    fn a_retiring_prefix_dropped_mid_steps_while_serving_is_an_unsliced_drop() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("halfway", CapFill::Serving);
        let (map, _) = built::<u64, u8>(cap, &census);
        mark.take();
        let MapRetired::More(retiring) = map.retire().step() else { panic!("six items") };
        expect_unsliced(&census, 2, || drop(retiring));
    }

    #[test]
    fn a_map_dropped_once_stopping_or_while_assembling_is_not_counted() {
        let (census, mark) = census();
        let cap = Cap::entries::<100>("teardown", CapFill::Serving);
        let (assembling, _) = built::<u64, u8>(cap, &census);
        drop(assembling);
        let (stopping, _) = built::<u64, u8>(cap, &census);
        mark.take();
        census.mark_stopping();
        assert_eq!(census.phase(), CensusPhase::Stopping);
        drop(stopping);
        assert_eq!(census.unsliced_drops(), 0);
    }

    /// The retire path a serving site uses: one item per step, so nothing
    /// is dropped whole.
    #[test]
    fn a_live_map_retires_by_steps_with_no_unsliced_drop() {
        let (census, mark) = census();
        let cap = Cap::entries::<1000>("retired", CapFill::Serving);
        let (mut map, _) = built::<u64, u64>(cap, &census);
        for key in 0..10 {
            assert_eq!(insert(&mut map, key << 40, key), Answer::Vacant);
        }
        mark.take();
        assert_eq!(retired(map.retire()), 34);
        assert_eq!(census.unsliced_drops(), 0, "unsliced-drop: a retire step dropped backing");
    }

    #[test]
    fn a_census_row_reads_the_map_alone() {
        let (census, _mark) = census();
        let cap = Cap::entries::<8>("alone", CapFill::Serving);
        let (_map, _) = built::<u64, u8>(cap, &census);
        let rows: Vec<CapRow> = census.rows().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].name, rows[0].entries_max, rows[0].capacity_now), ("alone", 8, 8));
    }
}
