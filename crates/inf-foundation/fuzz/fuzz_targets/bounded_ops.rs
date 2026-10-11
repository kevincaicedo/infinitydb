//! Capped-container op sequences against std models (ADR-0151 D6): a
//! `Reserve` deque beside a `VecDeque` the target caps by hand, a `Coalesce`
//! deque beside one the target merges by hand, equal after every op; the
//! census row's `high_water` and `crossings` equal the models' at the end;
//! `len` never passes the cap; a layout past 64 KiB is refused. The models
//! share no code with `inf_foundation::bounded`.
#![no_main]
#![allow(
    clippy::disallowed_types,
    reason = "fuzz target: the std models the capped types are checked against"
)]

use std::collections::VecDeque;

use inf_foundation::CellId;
use inf_foundation::bounded::{
    Cap, CapCensus, CapError, CapFill, CappedDeque, Coalesce, Lossy, Merge, Reserve, Reserved,
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
    let which = usize::from(head & 7);
    let census = CapCensus::new(CellId(u16::from(head)));
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
            6 => {
                if let Some(back) = reserve.back_mut() {
                    *back = value;
                }
                if let Some(back) = reserve_model.entries.back_mut() {
                    *back = value;
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
});
