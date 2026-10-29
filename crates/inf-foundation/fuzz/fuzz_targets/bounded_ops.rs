//! Capped-container op sequences against std models (ADR-0151 D6, ADR-0171
//! D5). The first byte picks the family. Deques: a `Reserve` deque beside a
//! `VecDeque` the target caps by hand, a `Coalesce` deque beside one the
//! target merges by hand, equal after every op and at every place (`get`,
//! and `get_mut` edits at a place or the back); the census row's
//! `high_water` and `crossings` equal the models' at the end; `len` never
//! passes the cap; a layout past 64 KiB is refused. Slabs: a slab of either
//! generation policy beside a `BTreeMap<(index, generation), value>`, with
//! reserves, publishes, dropped reservations, reads, writes, takes of live
//! and stale handles and slots placed at their last generation, so `Retire`
//! shrinks the capacity to `Exhausted` and `WrapWithProof` wraps; every
//! handle read back against the model after every op, and the row's high
//! water, crossings and capacity equal the model's at the end; the slab is
//! built and retired one step per chunk or directory. The models share no
//! code with `inf_foundation::bounded`.
#![no_main]
#![allow(
    clippy::disallowed_types,
    reason = "fuzz target: the std models the capped types are checked against"
)]

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use inf_foundation::CellId;
use inf_foundation::bounded::{
    Built, Cap, CapCensus, CapError, CapFill, CappedDeque, CappedSlab, Coalesce, Generation,
    GenerationPolicy, Lookup, LookupMut, Lossy, Merge, Reserve, Reserved, Retire, Retiring,
    SlabReserved, SlotId, Taken, WrapWithProof,
};
use libfuzzer_sys::fuzz_target;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark(u16);

impl Lossy for Mark {}

struct Later;

impl Merge<Mark> for Later {
    fn merge(back: &mut Mark, new: Mark) {
        *back = new;
    }
}

/// Marks of one class share a value octave.
fn same_class(a: &Mark, b: &Mark) -> bool {
    a.0 >> 3 == b.0 >> 3
}

const CAPS: [Cap; 8] = [
    Cap::entries::<1>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<2>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<3>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<4>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<5>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<8>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<16>("fuzz-reserve", CapFill::Serving),
    Cap::entries::<32>("fuzz-reserve", CapFill::Serving),
];
const MARK_CAPS: [Cap; 8] = [
    Cap::entries::<1>("fuzz-marks", CapFill::Serving),
    Cap::entries::<2>("fuzz-marks", CapFill::Serving),
    Cap::entries::<3>("fuzz-marks", CapFill::Serving),
    Cap::entries::<4>("fuzz-marks", CapFill::Serving),
    Cap::entries::<5>("fuzz-marks", CapFill::Serving),
    Cap::entries::<8>("fuzz-marks", CapFill::Serving),
    Cap::entries::<16>("fuzz-marks", CapFill::Serving),
    Cap::entries::<32>("fuzz-marks", CapFill::Serving),
];
// 16 × 4 KiB is the backing bound exactly; 32 × 4 KiB passes it.
const PAGES_16: Cap = Cap::entries::<16>("fuzz-pages", CapFill::Serving);
const PAGES_32: Cap = Cap::entries::<32>("fuzz-pages-over", CapFill::Serving);

struct Model<T> {
    entries: VecDeque<T>,
    high_water: u32,
    crossings: u64,
}

impl<T> Model<T> {
    fn new() -> Model<T> {
        Model { entries: VecDeque::new(), high_water: 0, crossings: 0 }
    }

    fn note(&mut self) {
        self.high_water = self.high_water.max(self.entries.len() as u32);
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&head, ops)) = data.split_first() else { return };
    match (head >> 3) & 3 {
        0 | 1 => deques(head, ops),
        2 => slab::<u32, Retire>(head, ops),
        _ => slab::<u64, WrapWithProof>(head, ops),
    }
});

fn deques(head: u8, ops: &[u8]) {
    let which = usize::from(head & 7);
    let (census, _serve_mark) = CapCensus::new(CellId(u16::from(head)));
    let mut reserve = CappedDeque::<u16, Reserve>::new(CAPS[which], &census).expect("fits");
    let mut marks =
        CappedDeque::<Mark, Coalesce<Later>>::new(MARK_CAPS[which], &census).expect("fits");
    let cap = usize::from(CAPS[which].entries_max() as u16);
    let mut reserve_model = Model::<u16>::new();
    let mut marks_model = Model::<Mark>::new();
    assert!(CappedDeque::<[u8; 4096], Reserve>::new(PAGES_16, &census).is_ok());
    assert!(matches!(
        CappedDeque::<[u8; 4096], Reserve>::new(PAGES_32, &census),
        Err(CapError::Layout)
    ));
    for &byte in ops {
        let value = u16::from(byte >> 3);
        match byte & 7 {
            0 | 1 => match reserve.reserve() {
                Reserved::Slot(slot) => {
                    assert!(reserve_model.entries.len() < cap, "a slot past the cap");
                    slot.publish(value);
                    reserve_model.entries.push_back(value);
                    reserve_model.note();
                }
                Reserved::Full => {
                    assert_eq!(reserve_model.entries.len(), cap, "Full below the cap");
                    reserve_model.crossings += 1;
                }
            },
            2 => assert_eq!(reserve.pop_front(), reserve_model.entries.pop_front()),
            3 => {
                marks.push(Mark(value));
                if marks_model.entries.len() < cap {
                    marks_model.entries.push_back(Mark(value));
                    marks_model.note();
                } else {
                    *marks_model.entries.back_mut().expect("a cap holds one") = Mark(value);
                    marks_model.crossings += 1;
                }
            }
            4 => assert_eq!(marks.pop_front(), marks_model.entries.pop_front()),
            5 => {
                marks.merge_adjacent(same_class);
                let mut merged: VecDeque<Mark> = VecDeque::new();
                for mark in marks_model.entries.drain(..) {
                    match merged.back_mut() {
                        Some(back) if same_class(back, &mark) => *back = mark,
                        _ => merged.push_back(mark),
                    }
                }
                marks_model.entries = merged;
            }
            // An edit in place: odd values name a place from the front
            // (one past the last included: no entry there), even ones the
            // back.
            6 if value & 1 == 1 => {
                let index = usize::from(value >> 1) % (reserve_model.entries.len() + 1);
                assert_eq!(
                    reserve.get_mut(index).is_some(),
                    index < reserve_model.entries.len(),
                    "a place exists exactly below the length"
                );
                if let Some(entry) = reserve.get_mut(index) {
                    *entry = value;
                }
                if let Some(entry) = reserve_model.entries.get_mut(index) {
                    *entry = value;
                }
            }
            6 => {
                if let Some(back) = reserve.back_mut() {
                    *back = value;
                }
                if let Some(back) = reserve_model.entries.back_mut() {
                    *back = value;
                }
            }
            _ if value & 1 == 1 => {
                let index = usize::from(value >> 1) % (marks_model.entries.len() + 1);
                if let Some(entry) = marks.get_mut(index) {
                    *entry = Mark(value);
                }
                if let Some(entry) = marks_model.entries.get_mut(index) {
                    *entry = Mark(value);
                }
            }
            _ => {
                if let Some(back) = marks.back_mut() {
                    *back = Mark(value);
                }
                if let Some(back) = marks_model.entries.back_mut() {
                    *back = Mark(value);
                }
            }
        }
        assert!(reserve.len() <= cap);
        assert!(marks.len() <= cap);
        assert!(reserve.iter().eq(reserve_model.entries.iter()));
        assert!(marks.iter().eq(marks_model.entries.iter()));
        for index in 0..=cap {
            assert_eq!(reserve.get(index), reserve_model.entries.get(index));
            assert_eq!(marks.get(index), marks_model.entries.get(index));
        }
        assert_eq!(
            (reserve.front(), reserve.back()),
            (reserve_model.entries.front(), reserve_model.entries.back())
        );
        assert_eq!(
            (marks.front(), marks.back()),
            (marks_model.entries.front(), marks_model.entries.back())
        );
    }
    let row = census.row("fuzz-reserve").expect("row");
    assert_eq!(
        (row.high_water, row.crossings),
        (reserve_model.high_water, reserve_model.crossings)
    );
    assert_eq!(reserve.high_water(), reserve_model.high_water);
    let row = census.row("fuzz-marks").expect("row");
    assert_eq!((row.high_water, row.crossings), (marks_model.high_water, marks_model.crossings));
    assert_eq!(marks.high_water(), marks_model.high_water);
}

// A chunk holds 64 slots and a directory 256 chunks: one chunk, a chunk
// edge, and a directory over three chunks.
// Ops a slab run reads: every op reads back every handle minted, so the run
// is quadratic in this bound.
const SLAB_OPS_MAX: usize = 512;
const SLAB_CAPS: [Cap; 8] = [
    Cap::entries::<1>("fuzz-slab", CapFill::Serving),
    Cap::entries::<2>("fuzz-slab", CapFill::Serving),
    Cap::entries::<3>("fuzz-slab", CapFill::Serving),
    Cap::entries::<5>("fuzz-slab", CapFill::Serving),
    Cap::entries::<63>("fuzz-slab", CapFill::Serving),
    Cap::entries::<64>("fuzz-slab", CapFill::Serving),
    Cap::entries::<65>("fuzz-slab", CapFill::Serving),
    Cap::entries::<130>("fuzz-slab", CapFill::Serving),
];

/// Builds a slab by steps: the steps it took.
fn built<G: Generation, P: GenerationPolicy>(
    cap: Cap,
    census: &Rc<CapCensus>,
) -> (CappedSlab<u16, G, P>, u32) {
    let mut builder = CappedSlab::<u16, G, P>::builder(cap, census).expect("fits");
    let mut steps = 0;
    loop {
        steps += 1;
        match builder.step() {
            Built::More(next) => builder = next,
            Built::Live(slab) => return (slab, steps),
            Built::Refused(_, refused) => panic!("{refused}"),
        }
    }
}

/// The slot generations a slab's model expects: the generation each index's
/// next occupant takes (absent: never used, the first), and the indices
/// retired at their last generation.
struct Slots<G> {
    next: BTreeMap<u32, G>,
    retired: Vec<u32>,
}

fn slab<G: Generation + Ord, P: GenerationPolicy>(head: u8, ops: &[u8]) {
    let cap = SLAB_CAPS[usize::from(head & 7)];
    let retires = (head >> 3) & 3 == 2;
    let (census, _serve_mark) = CapCensus::new(CellId(u16::from(head)));
    let (mut slab, build_steps) = built::<G, P>(cap, &census);
    let mut model: BTreeMap<(u32, G), u16> = BTreeMap::new();
    let mut slots = Slots { next: BTreeMap::new(), retired: Vec::new() };
    // Every handle minted, live or stale; bounded by the ops read.
    let mut handles: Vec<SlotId<G>> = Vec::new();
    let mut capacity = cap.entries_max();
    let (mut high_water, mut crossings) = (0u32, 0u64);
    for &byte in ops.iter().take(SLAB_OPS_MAX) {
        let value = u16::from(byte >> 3);
        let pick = usize::from(value);
        match byte & 7 {
            0 | 1 => match slab.reserve() {
                SlabReserved::Slot(slot) => {
                    assert!((model.len() as u32) < capacity, "a slot past the capacity");
                    let id = slot.publish(value);
                    let (index, generation) = (id.index(), id.generation());
                    assert!(index < cap.entries_max(), "an index past the cap");
                    assert!(!slots.retired.contains(&index), "a retired slot served again");
                    let expected = slots.next.get(&index).copied().unwrap_or(G::FIRST);
                    assert_eq!(generation, expected, "the slot's next generation");
                    assert!(model.insert((index, generation), value).is_none(), "one occupant");
                    handles.push(id);
                    high_water = high_water.max(model.len() as u32);
                }
                SlabReserved::Full => {
                    assert!(capacity > 0, "Full with no capacity left");
                    assert_eq!(model.len() as u32, capacity, "Full below the capacity");
                    crossings += 1;
                }
                SlabReserved::Exhausted => assert_eq!(capacity, 0, "Exhausted with capacity"),
            },
            2 => match slab.reserve() {
                SlabReserved::Slot(slot) => drop(slot),
                SlabReserved::Full => crossings += 1,
                SlabReserved::Exhausted => assert_eq!(capacity, 0),
            },
            3 | 4 if !handles.is_empty() => {
                let id = handles[pick % handles.len()];
                let (index, generation) = (id.index(), id.generation());
                let Some(expected) = model.remove(&(index, generation)) else {
                    assert_eq!(slab.take(id), Taken::Stale, "a stale handle");
                    continue;
                };
                assert_eq!(slab.take(id), Taken::Value(expected));
                let next =
                    if retires { generation.after() } else { Some(generation.after_wrapped()) };
                match next {
                    Some(next) => {
                        slots.next.insert(index, next);
                    }
                    None => {
                        slots.retired.push(index);
                        capacity -= 1;
                    }
                }
            }
            5 if !handles.is_empty() => {
                let at = pick % handles.len();
                let mut id = handles[at];
                let (index, generation) = (id.index(), id.generation());
                let placed = slab.place_generation_last(&mut id);
                match model.remove(&(index, generation)) {
                    Some(held) => {
                        assert!(matches!(placed, LookupMut::Live(_)), "a live slot is placed");
                        model.insert((index, G::LAST), held);
                        assert_eq!(id.generation(), G::LAST);
                        handles[at] = id;
                    }
                    None => assert!(matches!(placed, LookupMut::Stale), "a stale handle"),
                }
            }
            6 if !handles.is_empty() => {
                let id = handles[pick % handles.len()];
                match (slab.get_mut(id), model.get_mut(&(id.index(), id.generation()))) {
                    (LookupMut::Live(found), Some(expected)) => {
                        *found = value;
                        *expected = value;
                    }
                    (LookupMut::Stale, None) => {}
                    (LookupMut::Live(_) | LookupMut::Stale | LookupMut::Vacant, _) => {
                        panic!("get_mut disagrees with the model")
                    }
                }
            }
            _ => {}
        }
        assert_eq!(slab.len() as usize, model.len());
        assert_eq!(slab.capacity_now(), capacity);
        for id in &handles {
            match (slab.get(*id), model.get(&(id.index(), id.generation()))) {
                (Lookup::Live(found), Some(expected)) => assert_eq!(found, expected),
                (Lookup::Stale, None) => {}
                (Lookup::Live(_) | Lookup::Stale | Lookup::Vacant, _) => {
                    panic!("get disagrees with the model")
                }
            }
        }
    }
    let row = census.row("fuzz-slab").expect("row");
    assert_eq!((row.high_water, row.crossings), (high_water, crossings));
    assert_eq!(row.capacity_now, capacity);
    let mut retiring = slab.retire();
    let mut steps = 0;
    loop {
        steps += 1;
        match retiring.step() {
            Retiring::More(next) => retiring = next,
            Retiring::Done => break,
        }
    }
    assert_eq!(steps, build_steps, "one retire step per item built");
}
