//! An indexed minimum heap in fixed storage (ADR-0154 D3), and the fixed
//! array it is built on. The heap orders entries by their key; each move of
//! an entry is reported to the caller's hook with the entry's new index, so
//! the owner of an entry keeps its index beside it and removes the entry at
//! that index in one sift. A push, a removal and a pop each sift at most
//! `⌈log₂(capacity + 1)⌉` levels, whatever the order the keys arrive in, and
//! the minimum is read in constant time.
//!
//! The storage is fixed at its capacity and allocates nothing once built:
//! chunks of at most `MAP_BACKING_ALLOCATION_BYTES_MAX` bytes behind at most
//! two directory levels of the same bound, built one allocation per step, a
//! directory before its children. An array holds exactly its capacity, so
//! the heap never grows and a push past it is refused. The array is no
//! capped container of its own: it is built beside its owner's slab at the
//! slab's cap, and an element exists only with the occupant it is parallel
//! to, so it has no cap, crossing or census row.
//!
//! Safe Rust; every index is checked by the slice it reaches.

use core::mem::size_of;

use inf_foundation::bounded::CapError;
use inf_foundation::limits::MAP_BACKING_ALLOCATION_BYTES_MAX;

/// An entry of an [`IndexedHeap`]: a copy, ordered by its key.
pub trait HeapEntry: Copy {
    /// The order: the heap's minimum is the entry with the least key.
    type Key: Ord + Copy;

    /// The entry's place in the order.
    fn key(&self) -> Self::Key;
}

/// Chunks one directory holds: as many chunk pointers as one backing
/// allocation holds.
const DIRECTORY_CHUNKS: usize = MAP_BACKING_ALLOCATION_BYTES_MAX / size_of::<Box<[u8]>>();
/// Directories the top directory holds: as many directory headers as one
/// backing allocation holds.
const TOP_DIRECTORIES: usize = MAP_BACKING_ALLOCATION_BYTES_MAX / size_of::<Vec<Box<[u8]>>>();

// A chunk pointer is 16 B and a directory header 24 B: 4,096 and 2,730.
const _: () = assert!(DIRECTORY_CHUNKS == 4096, "a directory is one 64 KiB allocation");
const _: () = assert!(TOP_DIRECTORIES == 2730, "the top directory is one 64 KiB allocation");

/// A chunk of elements: one allocation.
type Chunk<E> = Box<[E]>;

/// A built array's storage by its directory depth. A directory is a `Vec`
/// reserved whole at its first build step, so a later step's push never
/// reallocates it.
enum Root<E> {
    /// One chunk: the capacity fits one allocation.
    Chunk(Chunk<E>),
    /// A directory of chunks.
    One(Vec<Chunk<E>>),
    /// A directory of directories of chunks.
    Two(Vec<Vec<Chunk<E>>>),
}

/// A fixed array of `capacity` elements, each set to a fill value when its
/// chunk is built. Built by steps ([`FixedArrayBuilder`]); once built it
/// allocates nothing. Its capacity is a `u32`, an owner's slot count.
pub struct FixedArray<E> {
    root: Root<E>,
    capacity: u32,
}

/// A fixed array being built, one allocation per step.
pub struct FixedArrayBuilder<E> {
    root: Option<Root<E>>,
    capacity: u32,
    fill: E,
    /// Chunks built: a prefix of the array's.
    chunks_built: usize,
}

/// The answer of a build step.
pub enum ArrayBuilt<E> {
    More(FixedArrayBuilder<E>),
    Done(FixedArray<E>),
    /// A step's allocation was refused; what was built drops with the
    /// answer, and nothing was served.
    Refused(CapError),
}

/// Elements per chunk for a non-zero-sized `E`: as many as one backing
/// allocation holds.
const fn chunk_elements<E>() -> usize {
    MAP_BACKING_ALLOCATION_BYTES_MAX / size_of::<E>()
}

impl<E: Copy> FixedArray<E> {
    /// A builder of an array of `capacity` elements, each `fill`; nothing is
    /// allocated until its first step. A zero-sized element, one wider than
    /// one allocation, or a capacity past two directory levels is `Layout`,
    /// with nothing allocated.
    pub fn builder(capacity: u32, fill: E) -> Result<FixedArrayBuilder<E>, CapError> {
        let fits = size_of::<E>() > 0 && size_of::<E>() <= MAP_BACKING_ALLOCATION_BYTES_MAX;
        if !fits || levels::<E>(capacity).is_none() {
            return Err(CapError::Layout);
        }
        Ok(FixedArrayBuilder { root: None, capacity, fill, chunks_built: 0 })
    }

    /// Elements the array holds.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The element at `index`. An index at or past the capacity panics, as
    /// a slice index does: an index is its owner's own position.
    #[must_use]
    #[inline]
    pub fn get(&self, index: u32) -> E {
        match &self.root {
            Root::Chunk(only) => only[index as usize],
            Root::One(directory) => {
                let (chunk, offset) = split::<E>(index);
                directory[chunk][offset]
            }
            Root::Two(top) => {
                let (chunk, offset) = split::<E>(index);
                let (upper, lower) = split_chunk(chunk);
                top[upper][lower][offset]
            }
        }
    }

    /// Sets the element at `index`, under [`Self::get`]'s rule.
    #[inline]
    pub fn set(&mut self, index: u32, value: E) {
        let place = match &mut self.root {
            Root::Chunk(only) => &mut only[index as usize],
            Root::One(directory) => {
                let (chunk, offset) = split::<E>(index);
                &mut directory[chunk][offset]
            }
            Root::Two(top) => {
                let (chunk, offset) = split::<E>(index);
                let (upper, lower) = split_chunk(chunk);
                &mut top[upper][lower][offset]
            }
        };
        *place = value;
    }
}

impl<E: Copy> FixedArrayBuilder<E> {
    /// One build step: the next directory or chunk allocated, a directory
    /// before its children. `Done` once the chunk holding the last element
    /// is built.
    pub fn step(mut self) -> ArrayBuilt<E> {
        let chunks = chunk_count::<E>(self.capacity);
        if let Err(refused) = self.grow(chunks) {
            return ArrayBuilt::Refused(refused);
        }
        if self.chunks_built == chunks
            && let Some(root) = self.root.take()
        {
            return ArrayBuilt::Done(FixedArray { root, capacity: self.capacity });
        }
        ArrayBuilt::More(self)
    }

    /// Allocates one item of an array of `chunks` chunks.
    fn grow(&mut self, chunks: usize) -> Result<(), CapError> {
        let (capacity, fill, built) = (self.capacity, self.fill, self.chunks_built);
        match &mut self.root {
            None => {
                self.root = Some(match levels::<E>(capacity) {
                    Some(0) => {
                        self.chunks_built = chunks;
                        Root::Chunk(allocate(capacity as usize, fill)?)
                    }
                    Some(1) => Root::One(reserve(chunks)?),
                    Some(_) | None => Root::Two(reserve(chunks.div_ceil(DIRECTORY_CHUNKS))?),
                });
            }
            Some(Root::Chunk(_)) => {}
            Some(Root::One(directory)) => {
                directory.push(allocate(chunk_len::<E>(capacity, built), fill)?);
                self.chunks_built = built + 1;
            }
            Some(Root::Two(top)) => {
                let (upper, lower) = split_chunk(built);
                if lower == 0 && top.len() == upper {
                    top.push(reserve((chunks - built).min(DIRECTORY_CHUNKS))?);
                } else {
                    top[upper].push(allocate(chunk_len::<E>(capacity, built), fill)?);
                    self.chunks_built = built + 1;
                }
            }
        }
        Ok(())
    }
}

/// One allocation of `len` copies of `fill`, refused as `Alloc`.
fn allocate<T: Copy>(len: usize, fill: T) -> Result<Box<[T]>, CapError> {
    let mut items = Vec::new();
    items.try_reserve_exact(len).map_err(|_| CapError::Alloc)?;
    items.resize(len, fill);
    Ok(items.into_boxed_slice())
}

/// An empty directory reserved whole for `len` children: one allocation.
fn reserve<T>(len: usize) -> Result<Vec<T>, CapError> {
    let mut directory = Vec::new();
    directory.try_reserve_exact(len).map_err(|_| CapError::Alloc)?;
    Ok(directory)
}

/// The directory levels an array of `capacity` needs: 0 for one chunk, at
/// most 2, each directory one allocation; `None` past that.
fn levels<E>(capacity: u32) -> Option<u32> {
    let chunks = chunk_count::<E>(capacity);
    if chunks <= 1 {
        Some(0)
    } else if chunks <= DIRECTORY_CHUNKS {
        Some(1)
    } else if chunks.div_ceil(DIRECTORY_CHUNKS) <= TOP_DIRECTORIES {
        Some(2)
    } else {
        None
    }
}

/// Chunks of an array of `capacity`.
fn chunk_count<E>(capacity: u32) -> usize {
    (capacity as usize).div_ceil(chunk_elements::<E>())
}

/// The elements of chunk `chunk` of an array of `capacity`: a full chunk
/// but for the last, which holds the rest, so the array is exact.
fn chunk_len<E>(capacity: u32, chunk: usize) -> usize {
    let start = chunk * chunk_elements::<E>();
    (capacity as usize - start).min(chunk_elements::<E>())
}

/// Element `index`'s chunk and its offset in it.
#[inline]
fn split<E>(index: u32) -> (usize, usize) {
    let index = index as usize;
    (index / chunk_elements::<E>(), index % chunk_elements::<E>())
}

/// A chunk's directory under the top one, and its place there.
#[inline]
fn split_chunk(chunk: usize) -> (usize, usize) {
    (chunk / DIRECTORY_CHUNKS, chunk % DIRECTORY_CHUNKS)
}

/// An indexed minimum heap of at most `capacity` entries.
pub struct IndexedHeap<E> {
    entries: FixedArray<E>,
    len: u32,
}

/// A push past the heap's capacity: nothing changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapFull;

impl<E: HeapEntry> IndexedHeap<E> {
    /// A heap over a built array, empty: the array's elements are its
    /// storage, whatever their fill.
    #[must_use]
    pub fn new(entries: FixedArray<E>) -> IndexedHeap<E> {
        IndexedHeap { entries, len: 0 }
    }

    /// Entries held.
    #[must_use]
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The most entries the heap holds.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.entries.capacity()
    }

    /// The minimum, read in constant time.
    #[must_use]
    #[inline]
    pub fn peek(&self) -> Option<E> {
        (self.len > 0).then(|| self.entries.get(0))
    }

    /// The entry at `index`, if the heap holds one there.
    #[must_use]
    pub fn get(&self, index: u32) -> Option<E> {
        (index < self.len).then(|| self.entries.get(index))
    }

    /// Adds `entry`, one upward sift; `moved` hears every entry the sift
    /// placed, the new one included, with its index. `HeapFull` at the
    /// capacity, nothing changed.
    pub fn push(&mut self, entry: E, mut moved: impl FnMut(E, u32)) -> Result<(), HeapFull> {
        if self.len == self.entries.capacity() {
            return Err(HeapFull);
        }
        let index = self.len;
        self.len = index + 1;
        self.sift_up(index, entry, &mut moved);
        Ok(())
    }

    /// Removes the minimum, one downward sift; `moved` hears every entry it
    /// placed.
    pub fn pop(&mut self, moved: impl FnMut(E, u32)) -> Option<E> {
        self.remove(0, moved)
    }

    /// Removes the entry at `index`, the last entry taking its place with
    /// one sift, up or down; `moved` hears every entry placed. `None` when
    /// the heap holds no entry there, nothing changed.
    pub fn remove(&mut self, index: u32, mut moved: impl FnMut(E, u32)) -> Option<E> {
        if index >= self.len {
            return None;
        }
        let removed = self.entries.get(index);
        let last = self.len - 1;
        self.len = last;
        if index < last {
            let entry = self.entries.get(last);
            let parent_key = (index > 0).then(|| self.entries.get(parent(index)).key());
            if parent_key.is_some_and(|key| entry.key() < key) {
                self.sift_up(index, entry, &mut moved);
            } else {
                self.sift_down(index, entry, &mut moved);
            }
        }
        Some(removed)
    }

    fn sift_up(&mut self, mut index: u32, entry: E, moved: &mut impl FnMut(E, u32)) {
        while index > 0 {
            let above = parent(index);
            let parent_entry = self.entries.get(above);
            if entry.key() >= parent_entry.key() {
                break;
            }
            self.entries.set(index, parent_entry);
            moved(parent_entry, index);
            index = above;
        }
        self.entries.set(index, entry);
        moved(entry, index);
    }

    fn sift_down(&mut self, mut index: u32, entry: E, moved: &mut impl FnMut(E, u32)) {
        while let Some(left) = child(index, 0).filter(|left| *left < self.len) {
            let right = child(index, 1).filter(|right| *right < self.len);
            let least = match right {
                Some(right) if self.entries.get(right).key() < self.entries.get(left).key() => {
                    right
                }
                Some(_) | None => left,
            };
            let least_entry = self.entries.get(least);
            if least_entry.key() >= entry.key() {
                break;
            }
            self.entries.set(index, least_entry);
            moved(least_entry, index);
            index = least;
        }
        self.entries.set(index, entry);
        moved(entry, index);
    }
}

/// The parent of a nonzero index.
#[inline]
fn parent(index: u32) -> u32 {
    (index - 1) / 2
}

/// Child `side` (0 left, 1 right) of `index`, `None` past `u32`.
#[inline]
fn child(index: u32, side: u32) -> Option<u32> {
    index.checked_mul(2)?.checked_add(1 + side)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use inf_foundation::rng::{Entropy, SplitMix64};

    /// Builds an array by steps; the steps it took.
    fn built<E: Copy>(capacity: u32, fill: E) -> (FixedArray<E>, u32) {
        let mut builder = FixedArray::builder(capacity, fill).expect("a layout that fits");
        let mut steps = 0;
        loop {
            steps += 1;
            builder = match builder.step() {
                ArrayBuilt::More(next) => next,
                ArrayBuilt::Done(array) => return (array, steps),
                ArrayBuilt::Refused(refused) => panic!("a step refused: {refused}"),
            };
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Entry {
        key: u64,
        tag: u32,
    }

    impl HeapEntry for Entry {
        type Key = (u64, u32);

        fn key(&self) -> (u64, u32) {
            (self.key, self.tag)
        }
    }

    /// One chunk up to a chunk's elements, then a directory and a chunk per
    /// 4,096 elements of 16 B: the steps and the exact capacity.
    #[test]
    fn an_array_builds_one_allocation_per_step_and_holds_its_capacity() {
        for (capacity, steps) in [(1u32, 1u32), (4, 1), (4096, 1), (4097, 3), (65_536, 17)] {
            let (mut array, taken) = built(capacity, Entry { key: 7, tag: 0 });
            assert_eq!((array.capacity(), taken), (capacity, steps), "capacity {capacity}");
            assert_eq!(array.get(capacity - 1), Entry { key: 7, tag: 0 }, "the fill");
            for index in [0, capacity / 2, capacity - 1] {
                array.set(index, Entry { key: u64::from(index), tag: index });
            }
            for index in [0, capacity / 2, capacity - 1] {
                assert_eq!(array.get(index).tag, index);
            }
        }
    }

    /// Past one directory of chunks the array takes a second level: the top
    /// directory, then each directory before its chunks. Elements of 64 KiB
    /// make a chunk of one, so 4,097 of them need two directories.
    #[test]
    fn a_second_directory_level_builds_before_its_chunks() {
        let (mut array, steps) = built(4097, [0u8; 65_536]);
        assert_eq!(steps, 1 + 2 + 4097, "the top, two directories, a chunk per element");
        for index in [0u32, 4095, 4096] {
            let mut value = [0u8; 65_536];
            value[0] = u8::try_from(index % 251).expect("fits");
            array.set(index, value);
            assert_eq!(array.get(index)[0], value[0], "element {index}");
        }
    }

    #[test]
    fn a_shape_no_allocation_holds_is_refused_before_allocating() {
        assert!(matches!(FixedArray::builder(1, [0u8; 65_537]), Err(CapError::Layout)));
        assert!(matches!(FixedArray::builder(1, ()), Err(CapError::Layout)));
        // Chunks of one element: past 4,096 directories of 4,096 chunks the
        // top directory passes one allocation.
        assert!(matches!(FixedArray::builder(4096 * 2731, [0u8; 65_536]), Err(CapError::Layout)));
        assert!(FixedArray::builder(4096 * 2730, [0u8; 65_536]).is_ok());
        assert!(FixedArray::builder(u32::MAX, Entry { key: 0, tag: 0 }).is_ok());
    }

    /// A heap of `capacity` with a reverse index the hook keeps, checked
    /// against a `BTreeSet` model across seeded pushes, pops and removals at
    /// every index: after every operation the heap's entries and the model
    /// agree, the minimum is the model's, and every entry's recorded index
    /// names it.
    #[test]
    fn the_heap_matches_a_sorted_model_and_its_hook_keeps_every_index() {
        for capacity in [1u32, 2, 3, 64, 1025] {
            let (array, _) = built(capacity, Entry { key: 0, tag: 0 });
            let mut heap = IndexedHeap::new(array);
            let mut model = BTreeSet::new();
            let mut index_of = vec![u32::MAX; capacity as usize];
            let mut rng = SplitMix64::new(u64::from(capacity));
            for _ in 0..20_000 {
                let roll = rng.next_u64() % 10;
                if roll < 5 {
                    let tag = (rng.next_u64() % u64::from(capacity)) as u32;
                    if index_of[tag as usize] != u32::MAX {
                        continue;
                    }
                    let entry = Entry { key: rng.next_u64() % 64, tag };
                    let pushed =
                        heap.push(entry, |moved, index| index_of[moved.tag as usize] = index);
                    assert_eq!(pushed, Ok(()), "a tag absent from the heap has room");
                    model.insert((entry.key, entry.tag));
                } else if roll < 7 {
                    let popped = heap.pop(|moved, index| index_of[moved.tag as usize] = index);
                    assert_eq!(popped.map(|e| (e.key, e.tag)), model.pop_first());
                    if let Some(entry) = popped {
                        index_of[entry.tag as usize] = u32::MAX;
                    }
                } else {
                    let tag = (rng.next_u64() % u64::from(capacity)) as usize;
                    let at = index_of[tag];
                    if at == u32::MAX {
                        assert_eq!(heap.remove(heap.len(), |_, _| panic!("nothing moves")), None);
                        continue;
                    }
                    let removed =
                        heap.remove(at, |moved, index| index_of[moved.tag as usize] = index);
                    let removed = removed.expect("a recorded index names its entry");
                    assert!(model.remove(&(removed.key, removed.tag)));
                    index_of[tag] = u32::MAX;
                }
                assert_eq!(heap.len() as usize, model.len());
                assert_eq!(heap.peek().map(|e| (e.key, e.tag)), model.first().copied());
                for (tag, at) in index_of.iter().enumerate() {
                    if *at != u32::MAX {
                        assert_eq!(heap.get(*at).map(|e| e.tag as usize), Some(tag));
                    }
                }
            }
        }
    }

    #[test]
    fn a_push_past_the_capacity_is_refused_and_changes_nothing() {
        let (array, _) = built(2, Entry { key: 0, tag: 0 });
        let mut heap = IndexedHeap::new(array);
        for tag in 0..2 {
            assert_eq!(heap.push(Entry { key: 9, tag }, |_, _| {}), Ok(()));
        }
        assert_eq!(
            heap.push(Entry { key: 1, tag: 2 }, |_, _| panic!("nothing moves")),
            Err(HeapFull)
        );
        assert_eq!(heap.len(), 2);
        assert_eq!(heap.peek(), Some(Entry { key: 9, tag: 0 }));
    }

    /// The sift bound: a heap of 65,535 entries is 16 levels deep, so a
    /// push of a new minimum moves 16 entries and itself.
    #[test]
    fn a_sift_moves_at_most_one_entry_a_level() {
        let (array, _) = built(65_536, Entry { key: 0, tag: 0 });
        let mut heap = IndexedHeap::new(array);
        for tag in 0..65_535u32 {
            assert_eq!(heap.push(Entry { key: u64::from(tag) + 1, tag }, |_, _| {}), Ok(()));
        }
        let mut moves = 0;
        assert_eq!(heap.push(Entry { key: 0, tag: 65_535 }, |_, _| moves += 1), Ok(()));
        assert_eq!(moves, 17, "16 parents moved down and the new entry placed");
    }
}
