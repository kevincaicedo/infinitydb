//! `CappedDeque<T, X>`: a FIFO over one fixed allocation of `entries × stride`
//! bytes (at most `DEQUE_BACKING_BYTES_MAX`), the one sanctioned holder of a
//! std container in cell code (ADR-0151 D6). `X` is the crossing at the cap:
//! `Reserve` (a slot, or `Full` with no value taken) or `Coalesce<M>` (`push`
//! merges into the back under `M` and counts). The deque is never
//! `Assembly`: no site fills one before serving, so `new` refuses that cap,
//! and a pop or a merge tests no fill. `len <= entries_max` is held by this
//! type's own compare, never by the backing's capacity, and no allocator
//! call follows construction: the backing is reserved whole, exactly, once.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use core::marker::PhantomData;
use core::mem::size_of;
use std::rc::Rc;

use super::census::{CapCensus, CapRowHandle};
use super::{Cap, CapError, CapFill, Coalesce, Lossy, Merge, Reserve};
use crate::limits::DEQUE_BACKING_BYTES_MAX;

/// A capped FIFO. Growth is `reserve` then `publish` (`Reserve`), or `push`
/// (`Coalesce`); there is no other way in, so a caller writes what happens
/// at the cap.
///
/// ```compile_fail
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill, CappedDeque, Reserve};
/// # use inf_foundation::CellId;
/// let census = CapCensus::new(CellId(0));
/// let Ok(mut deque) = CappedDeque::<u8, Reserve>::new(
///     Cap::entries::<4>("x", CapFill::Serving), &census) else { return };
/// deque.push_back(1u8); // no such method: growth needs a slot
/// ```
///
/// A `Reserved` is not a `Result`, so `.ok()` cannot discard it:
///
/// ```compile_fail
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill, CappedDeque, Reserve};
/// # use inf_foundation::CellId;
/// let census = CapCensus::new(CellId(0));
/// let Ok(mut deque) = CappedDeque::<u8, Reserve>::new(
///     Cap::entries::<4>("x", CapFill::Serving), &census) else { return };
/// deque.reserve().ok();
/// ```
///
/// A slot publishes once: a second publish is a use after move.
///
/// ```compile_fail
/// # use inf_foundation::bounded::{Cap, CapCensus, CapFill, CappedDeque, Reserve, Reserved};
/// # use inf_foundation::CellId;
/// let census = CapCensus::new(CellId(0));
/// let Ok(mut deque) = CappedDeque::<u8, Reserve>::new(
///     Cap::entries::<4>("x", CapFill::Serving), &census) else { return };
/// if let Reserved::Slot(slot) = deque.reserve() {
///     slot.publish(1);
///     slot.publish(2);
/// }
/// ```
#[allow(clippy::disallowed_types, reason = "container: capped-backing")]
pub struct CappedDeque<T, X> {
    backing: std::collections::VecDeque<T>,
    entries_max: u32,
    high_water: u32,
    row: CapRowHandle,
    crossing: PhantomData<X>,
}

/// An exclusive reservation of one entry of a `Reserve` deque: `publish`
/// consumes it exactly once and cannot allocate or refuse; dropping it
/// restores the capacity. It borrows the deque, so it cannot cross an
/// `await` (clippy's `await-holding-invalid-types` names it) or a reactor
/// iteration: a resumed step reserves again.
#[must_use = "a reservation publishes a value or is dropped; it carries none"]
pub struct DequeSlot<'a, T> {
    deque: &'a mut CappedDeque<T, Reserve>,
}

/// The answer to `reserve()`: a slot, or `Full`, which took no value and
/// changed nothing but the row's crossings. It carries the slot's borrow, so
/// it is matched in the step that reserved and cannot cross an `await`
/// (clippy's `await-holding-invalid-types` names it beside `DequeSlot`).
#[must_use = "a reservation publishes a value or is dropped; it carries none"]
pub enum Reserved<'a, T> {
    Slot(DequeSlot<'a, T>),
    Full,
}

impl<T, X> CappedDeque<T, X> {
    /// A deque of `cap` on `census`: the fill is checked (an `Assembly` cap is
    /// `CapError::Census`, and no row is registered), then the layout
    /// (`entries × stride` at most `DEQUE_BACKING_BYTES_MAX`, else
    /// `CapError::Layout`), then the row is registered, then the one
    /// allocation is made (`CapError::Alloc` if refused). Nothing is built on
    /// any refusal.
    pub fn new(cap: Cap, census: &Rc<CapCensus>) -> Result<Self, CapError> {
        if cap.fill() == CapFill::Assembly {
            return Err(CapError::Census);
        }
        let entries = usize::try_from(cap.entries_max()).map_err(|_| CapError::Layout)?;
        let bytes = entries.checked_mul(size_of::<T>()).ok_or(CapError::Layout)?;
        if bytes > DEQUE_BACKING_BYTES_MAX {
            return Err(CapError::Layout);
        }
        let row = census.register(cap)?;
        let mut deque = CappedDeque {
            backing: Default::default(),
            entries_max: cap.entries_max(),
            high_water: 0,
            row,
            crossing: PhantomData,
        };
        deque.backing.try_reserve_exact(entries).map_err(|_| CapError::Alloc)?;
        Ok(deque)
    }

    /// Published entries, at most `entries_max`.
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.backing.len()
    }

    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.backing.is_empty()
    }

    #[must_use]
    pub fn entries_max(&self) -> u32 {
        self.entries_max
    }

    /// The most entries this instance has held at once.
    #[must_use]
    pub fn high_water(&self) -> u32 {
        self.high_water
    }

    #[must_use]
    #[inline]
    pub fn front(&self) -> Option<&T> {
        self.backing.front()
    }

    #[must_use]
    #[inline]
    pub fn back(&self) -> Option<&T> {
        self.backing.back()
    }

    #[must_use]
    #[inline]
    pub fn back_mut(&mut self) -> Option<&mut T> {
        self.backing.back_mut()
    }

    /// The front entry returns to its owner; capacity is restored.
    #[inline]
    pub fn pop_front(&mut self) -> Option<T> {
        self.backing.pop_front()
    }

    /// Front to back, at most `entries_max` entries.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.backing.iter()
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "bound: len <= entries_max, a u32, held by every growth path's compare"
    )]
    #[inline]
    fn published_count(&self) -> u32 {
        self.backing.len() as u32
    }

    /// After a publish: the one compare a publish pays. A rise is reported
    /// to the row, at most `entries_max` times over the instance's life.
    #[inline]
    fn published(&mut self) {
        let live = self.published_count();
        if live > self.high_water {
            self.high_water_rose(live);
        }
    }

    /// A new high water, off the hot path: kept here and reported.
    #[cold]
    #[inline(never)]
    fn high_water_rose(&mut self, live: u32) {
        self.high_water = live;
        self.row.report_publish(live);
    }
}

impl<T> CappedDeque<T, Reserve> {
    /// A slot when `len < entries_max`; else `Full`, with no value taken, and
    /// the row's crossings up by one.
    #[inline]
    pub fn reserve(&mut self) -> Reserved<'_, T> {
        if self.published_count() < self.entries_max {
            Reserved::Slot(DequeSlot { deque: self })
        } else {
            self.full()
        }
    }

    /// The pacing crossing, off the hot path.
    #[cold]
    #[inline(never)]
    fn full(&mut self) -> Reserved<'_, T> {
        self.row.report_full();
        Reserved::Full
    }
}

impl<T> DequeSlot<'_, T> {
    /// The entry is appended at the back. Cannot allocate or refuse: the
    /// backing was reserved whole at construction and the slot proved room.
    #[inline]
    pub fn publish(self, value: T) {
        self.deque.backing.push_back(value);
        self.deque.published();
    }
}

impl<T: Lossy, M: Merge<T>> CappedDeque<T, Coalesce<M>> {
    /// Appended when `len < entries_max`; at the cap `M::merge(back, value)`
    /// folds it into the last entry and the row's crossings go up by one.
    #[inline]
    pub fn push(&mut self, value: T) {
        if self.published_count() < self.entries_max {
            self.backing.push_back(value);
            self.published();
        } else {
            self.merge_at_cap(value);
        }
    }

    /// The coalescing crossing, off the hot path: `value` folds into the back.
    #[cold]
    #[inline(never)]
    fn merge_at_cap(&mut self, value: T) {
        // At the cap the deque holds an entry (the cap is nonzero), so the
        // back is there. Were it not, `value` drops: a `Lossy` payload may be
        // dropped at a cap, while a push here would be the one path past it
        // (`len <= entries_max` is this type's own compare, not the backing's).
        debug_assert!(!self.backing.is_empty(), "a Coalesce deque at its cap has a back");
        if let Some(back) = self.backing.back_mut() {
            M::merge(back, value);
        }
        self.row.report_full();
    }
}

impl<T: Lossy, X> CappedDeque<T, X> {
    /// One shrinking pass of at most `entries_max` steps: of each adjacent
    /// pair `same` calls one, the later entry stays and the earlier is
    /// dropped. Exact by the payload's own rule, so not a crossing. Order is
    /// kept: each step pops the front and either folds it into the last
    /// entry re-pushed or re-pushes it.
    pub fn merge_adjacent(&mut self, mut same: impl FnMut(&T, &T) -> bool) {
        let steps = self.backing.len();
        let mut kept = false;
        for _ in 0..steps {
            let Some(entry) = self.backing.pop_front() else { break };
            match self.backing.back_mut() {
                Some(back) if kept && same(back, &entry) => *back = entry,
                _ => {
                    self.backing.push_back(entry);
                    kept = true;
                }
            }
        }
    }
}

#[allow(clippy::disallowed_types, reason = "test-only: the std model the deque is checked against")]
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::ids::CellId;
    use crate::rng::{Entropy, SplitMix64};

    const CELL: CellId = CellId(1);
    const FOUR: Cap = Cap::entries::<4>("four", CapFill::Serving);
    const ONE: Cap = Cap::entries::<1>("one", CapFill::Serving);
    const ASSEMBLY: Cap = Cap::entries::<4>("assembly", CapFill::Assembly);

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Mark {
        seq: u64,
        epoch: u64,
    }

    impl Lossy for Mark {}

    struct LaterMark;

    impl Merge<Mark> for LaterMark {
        fn merge(back: &mut Mark, new: Mark) {
            *back = new;
        }
    }

    fn mark(seq: u64) -> Mark {
        Mark { seq, epoch: seq }
    }

    fn deque(cap: Cap) -> (Rc<CapCensus>, CappedDeque<u32, Reserve>) {
        let census = CapCensus::new(CELL);
        let deque = CappedDeque::new(cap, &census).expect("built");
        (census, deque)
    }

    fn publish(deque: &mut CappedDeque<u32, Reserve>, value: u32) -> bool {
        match deque.reserve() {
            Reserved::Slot(slot) => {
                slot.publish(value);
                true
            }
            Reserved::Full => false,
        }
    }

    #[test]
    fn zero_one_cap_minus_one_cap_and_cap_plus_one() {
        let (census, mut deque) = deque(FOUR);
        assert!(deque.is_empty());
        assert_eq!((deque.len(), deque.entries_max(), deque.high_water()), (0, 4, 0));
        assert!(publish(&mut deque, 1));
        assert_eq!((deque.len(), deque.high_water()), (1, 1));
        assert!(publish(&mut deque, 2));
        assert!(publish(&mut deque, 3));
        assert_eq!((deque.len(), deque.high_water()), (3, 3));
        assert!(publish(&mut deque, 4), "reaching the cap is legal");
        assert_eq!((deque.len(), deque.high_water()), (4, 4));
        assert!(!publish(&mut deque, 5), "the fifth is Full");
        assert_eq!(deque.len(), 4);
        assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
        let row = census.row("four").expect("row");
        assert_eq!((row.high_water, row.crossings), (4, 1));
        assert_eq!(deque.pop_front(), Some(1));
        assert!(publish(&mut deque, 5), "a pop restores capacity");
        assert_eq!((deque.front(), deque.back()), (Some(&2), Some(&5)));
        assert_eq!(census.row("four").expect("row").high_water, 4);
    }

    #[test]
    fn a_cap_of_one_holds_one() {
        let (census, mut deque) = deque(ONE);
        assert!(publish(&mut deque, 7));
        assert!(!publish(&mut deque, 8));
        assert_eq!(deque.pop_front(), Some(7));
        assert_eq!(deque.pop_front(), None);
        assert!(publish(&mut deque, 8));
        assert_eq!(census.row("one").expect("row").crossings, 1);
    }

    #[test]
    fn a_dropped_slot_restores_capacity_and_publishes_nothing() {
        let (census, mut deque) = deque(ONE);
        match deque.reserve() {
            Reserved::Slot(slot) => drop(slot),
            Reserved::Full => panic!("room for one"),
        }
        assert!(deque.is_empty());
        assert!(publish(&mut deque, 1));
        assert_eq!(census.row("one").expect("row").crossings, 0);
    }

    #[test]
    fn discard_spellings_lose_nothing() {
        // ADR-0144 A2's spellings applied to a reservation discard only an
        // unused slot: no value was taken. Applied to a publish they do not
        // build under `-D warnings` (`let _ =` and `_ =` draw
        // `let_unit_value`, `drop` draws `dropping_copy_types`), and `.ok()`
        // has no receiver (the `compile_fail` doctest above): the lint-scopes
        // probe plants each of those.
        let (census, mut deque) = deque(FOUR);
        let _ = deque.reserve();
        _ = deque.reserve();
        drop(deque.reserve());
        match deque.reserve() {
            Reserved::Slot(_unused) => {}
            Reserved::Full => {}
        }
        let unused_binding = deque.reserve();
        drop(unused_binding);
        assert!(deque.is_empty(), "no spelling took a value");
        if let Reserved::Slot(slot) = deque.reserve() {
            slot.publish(1);
        }
        assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(census.row("four").expect("row").crossings, 0);
    }

    #[test]
    fn back_mut_edits_the_last_entry() {
        let (_census, mut deque) = deque(FOUR);
        assert_eq!(deque.back_mut(), None);
        assert!(publish(&mut deque, 1));
        assert!(publish(&mut deque, 2));
        if let Some(back) = deque.back_mut() {
            *back = 20;
        }
        assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [1, 20]);
    }

    #[test]
    fn an_assembly_cap_is_refused_and_registers_no_row() {
        let census = CapCensus::new(CELL);
        let refused = CappedDeque::<u32, Reserve>::new(ASSEMBLY, &census);
        assert!(matches!(refused, Err(CapError::Census)));
        assert_eq!(census.rows().count(), 0);
        let refused = CappedDeque::<Mark, Coalesce<LaterMark>>::new(ASSEMBLY, &census);
        assert!(matches!(refused, Err(CapError::Census)));
        assert_eq!(census.rows().count(), 0);
    }

    #[test]
    fn a_second_cap_under_one_name_is_census() {
        let (census, _deque) = deque(FOUR);
        const FOUR_AGAIN: Cap = Cap::entries::<5>("four", CapFill::Serving);
        let refused = CappedDeque::<u32, Reserve>::new(FOUR_AGAIN, &census);
        assert!(matches!(refused, Err(CapError::Census)));
        let shared = CappedDeque::<u32, Reserve>::new(FOUR, &census).expect("shares the row");
        assert_eq!(shared.entries_max(), 4);
        assert_eq!(census.rows().count(), 1);
    }

    #[test]
    fn a_layout_past_the_backing_bound_is_refused() {
        let census = CapCensus::new(CELL);
        const PAGES_64: Cap = Cap::entries::<64>("pages", CapFill::Serving);
        const PAGES_65: Cap = Cap::entries::<65>("pages-over", CapFill::Serving);
        let fits = CappedDeque::<[u8; 1024], Reserve>::new(PAGES_64, &census);
        assert!(fits.is_ok(), "64 × 1 KiB is the bound exactly");
        let over = CappedDeque::<[u8; 1024], Reserve>::new(PAGES_65, &census);
        assert!(matches!(over, Err(CapError::Layout)));
        assert_eq!(census.row("pages-over"), None, "refused before registration");
    }

    #[test]
    fn coalesce_appends_below_the_cap_and_merges_at_it() {
        let census = CapCensus::new(CELL);
        const MARKS: Cap = Cap::entries::<3>("marks", CapFill::Serving);
        let mut deque =
            CappedDeque::<Mark, Coalesce<LaterMark>>::new(MARKS, &census).expect("built");
        deque.push(mark(1));
        deque.push(mark(2));
        deque.push(mark(3));
        assert_eq!(deque.len(), 3);
        deque.push(mark(4));
        assert_eq!(deque.len(), 3, "the fourth merged into the back");
        assert_eq!(deque.back(), Some(&mark(4)));
        let row = census.row("marks").expect("row");
        assert_eq!((row.high_water, row.crossings), (3, 1));
        assert_eq!(deque.pop_front(), Some(mark(1)));
        deque.push(mark(5));
        assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [mark(2), mark(4), mark(5)]);
        assert_eq!(census.row("marks").expect("row").crossings, 1);
    }

    #[test]
    fn merge_adjacent_keeps_the_later_entry_of_each_pair() {
        let census = CapCensus::new(CELL);
        const MARKS: Cap = Cap::entries::<8>("marks", CapFill::Serving);
        let mut deque =
            CappedDeque::<Mark, Coalesce<LaterMark>>::new(MARKS, &census).expect("built");
        for seq in [1, 2, 3, 10, 11, 20, 30, 31] {
            deque.push(mark(seq));
        }
        // Marks of one class share a decade.
        deque.merge_adjacent(|a, b| a.seq / 10 == b.seq / 10);
        let seqs: Vec<u64> = deque.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [3, 11, 20, 31]);
        deque.merge_adjacent(|a, b| a.seq / 10 == b.seq / 10);
        assert_eq!(deque.iter().map(|m| m.seq).collect::<Vec<_>>(), [3, 11, 20, 31], "idempotent");
        deque.merge_adjacent(|_, _| true);
        assert_eq!(deque.iter().map(|m| m.seq).collect::<Vec<_>>(), [31], "all into the last");
        let mut empty =
            CappedDeque::<Mark, Coalesce<LaterMark>>::new(MARKS, &census).expect("built");
        empty.merge_adjacent(|_, _| true);
        assert!(empty.is_empty());
        assert_eq!(
            census.row("marks").expect("row").crossings,
            0,
            "a merge pass is not a crossing"
        );
    }

    /// The seeded model test (ADR-0151 D6): op sequences on both crossings
    /// against `std::collections::VecDeque`, which shares no code with this
    /// module, equal after every op.
    #[test]
    fn a_seeded_history_equals_the_std_model_after_every_op() {
        for seed in 0..64 {
            model_run(seed, 400);
        }
    }

    fn model_run(seed: u64, ops: u32) {
        const CAP: Cap = Cap::entries::<6>("model", CapFill::Serving);
        let census = CapCensus::new(CellId(2));
        let mut deque = CappedDeque::<u32, Reserve>::new(CAP, &census).expect("built");
        let mut model: VecDeque<u32> = VecDeque::new();
        let (mut full, mut high) = (0u64, 0u32);
        let mut rng = SplitMix64::new(seed);
        for value in 0..ops {
            match rng.next_below(4) {
                0 | 1 => {
                    let took = publish(&mut deque, value);
                    if model.len() < 6 {
                        model.push_back(value);
                        assert!(took);
                    } else {
                        full += 1;
                        assert!(!took);
                    }
                }
                2 => assert_eq!(deque.pop_front(), model.pop_front()),
                _ => {
                    let edit = rng.next_below(1000) as u32;
                    if let Some(back) = model.back_mut() {
                        *back = edit;
                    }
                    if let Some(back) = deque.back_mut() {
                        *back = edit;
                    }
                }
            }
            high = high.max(model.len() as u32);
            assert_eq!(deque.len(), model.len(), "seed {seed}");
            assert!(deque.iter().eq(model.iter()), "seed {seed}");
            assert_eq!(deque.front(), model.front());
            assert_eq!(deque.back(), model.back());
            assert!(deque.len() <= 6);
        }
        let row = census.row("model").expect("row");
        assert_eq!((row.high_water, row.crossings, deque.high_water()), (high, full, high));
    }
}
