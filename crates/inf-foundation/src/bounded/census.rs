//! The cell's cap census (ADR-0163 D2): one row per cap name with the fill
//! it was declared with, its high water (the maximum over the row's
//! instances), its crossings (summed over them) and, on an `Assembly` row,
//! the live count of its one instance. One census per cell, built with that
//! cell's `CellId`; every capped constructor takes it, and the one source of
//! a slab's `SlabInstance` is this census (ADR-0171 A2.1), so a host keeps
//! node-wide identities by constructing it exactly once.
//!
//! Phases: `Assembling` from construction until the cell's loop takes the
//! serve mark at its first iteration, `Serving` after it, `Stopping` once
//! the loop has exited. At the mark the census walks its rows: an
//! `Assembly` row whose live count is below its cap is a violation, and so
//! is every publish, crossing or registration on an `Assembly` row from
//! then on. A violation is counted in `cap_assembly_violations`, which the
//! simulator reads as 0 on every seed, and is a debug assertion carrying the
//! token `cap-assembly-violated`.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::{Cap, CapError, CapFill};
use crate::ids::CellId;
use crate::limits::CAP_CENSUS_ROWS_MAX;

/// Where the cell is in its life, as the census sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CensusPhase {
    /// Before the loop's first iteration: every `Assembly` container fills.
    Assembling,
    /// The serve mark was taken; `Assembly` rows are frozen at their cap.
    Serving,
    /// The loop has exited: process teardown, where an unsliced drop is
    /// legal.
    Stopping,
}

/// One row of the census, read as a copy. `high_water` and `crossings` are
/// per cell, over every instance of the name; `assembly_live` is the live
/// count of an `Assembly` row's one instance and stays 0 on a `Serving` row,
/// which reports no publish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapRow {
    pub name: &'static str,
    pub entries_max: u32,
    pub fill: CapFill,
    pub high_water: u32,
    pub crossings: u64,
    pub assembly_live: u32,
}

/// The census of one cell's caps. Shared by `Rc` with every capped container
/// of the cell, so a capped container is `!Send`, as the cell's state is.
pub struct CapCensus {
    cell: CellId,
    phase: Cell<CensusPhase>,
    rows: RefCell<[Option<CapRow>; CAP_CENSUS_ROWS_MAX]>,
    cap_assembly_violations: Cell<u32>,
}

/// A container's handle on its `CapRow`: the index is fixed at registration
/// and the fill is kept beside it, so a report tests one byte.
pub(super) struct CapRowHandle {
    census: Rc<CapCensus>,
    index: usize,
    fill: CapFill,
}

impl CapCensus {
    /// The census of `cell`, `Assembling`, with no row. The one allocation is
    /// the `Rc`, at the cell's assembly, before it serves.
    #[must_use]
    pub fn new(cell: CellId) -> Rc<CapCensus> {
        Rc::new(CapCensus {
            cell,
            phase: Cell::new(CensusPhase::Assembling),
            rows: RefCell::new([None; CAP_CENSUS_ROWS_MAX]),
            cap_assembly_violations: Cell::new(0),
        })
    }

    #[must_use]
    pub fn cell(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub fn phase(&self) -> CensusPhase {
        self.phase.get()
    }

    /// Violations of the `Assembly` contract so far: an `Assembly` row under
    /// its cap at the serve mark, or a publish, crossing or registration on
    /// one after it. Cell scope. A correct host reads 0 on every seed.
    #[must_use]
    pub fn cap_assembly_violations(&self) -> u32 {
        self.cap_assembly_violations.get()
    }

    /// The serve mark, taken once by the cell's loop as its first iteration
    /// begins: every step before it was the cell's assembly. One walk of the
    /// rows: each `Assembly` row whose live count is below its cap is counted
    /// and asserted. The live count, never `high_water`: a host that fills 4
    /// of 4 and removes one before the mark leaves `high_water = 4` with 3
    /// live.
    pub fn mark_serving(&self) {
        debug_assert!(
            self.phase.get() == CensusPhase::Assembling,
            "the serve mark is taken once, by the cell's loop ({})",
            self.cell
        );
        self.phase.set(CensusPhase::Serving);
        let mut short: u32 = 0;
        let mut first = 0;
        for (index, row) in self.rows.borrow().iter().enumerate() {
            let Some(row) = row else { continue };
            if row.fill == CapFill::Assembly && row.assembly_live < row.entries_max {
                if short == 0 {
                    first = index;
                }
                short = short.saturating_add(1);
            }
        }
        self.note_violations(short, "under-filled at the serve mark", first);
    }

    /// After the loop has exited: an unsliced drop is process teardown now.
    pub fn mark_stopping(&self) {
        self.phase.set(CensusPhase::Stopping);
    }

    /// Every row, copied out in registration order (at most
    /// `CAP_CENSUS_ROWS_MAX`): the INFO render and the oracles read this.
    pub fn rows(&self) -> impl Iterator<Item = CapRow> {
        (*self.rows.borrow()).into_iter().flatten()
    }

    /// The row registered under `name`, if any.
    #[must_use]
    pub fn row(&self, name: &str) -> Option<CapRow> {
        self.rows().find(|row| row.name == name)
    }

    /// The row for `cap`: shared when `name` is registered with the same cap
    /// value and fill and is `Serving` (per-namespace instances share one
    /// row); refused as `CapError::Census` for a second value or fill under
    /// one name, an `Assembly` name a second time, or a name past
    /// `CAP_CENSUS_ROWS_MAX`. Registration is O(rows), at construction. An
    /// `Assembly` registration after the serve mark is a violation.
    pub(super) fn register(self: &Rc<Self>, cap: Cap) -> Result<CapRowHandle, CapError> {
        let index = {
            let mut rows = self.rows.borrow_mut();
            let found = rows.iter().position(|row| row.is_some_and(|row| row.name == cap.name()));
            match found {
                Some(index) => {
                    let Some(row) = rows[index] else { return Err(CapError::Census) };
                    let same = row.entries_max == cap.entries_max() && row.fill == cap.fill();
                    if !same || cap.fill() == CapFill::Assembly {
                        return Err(CapError::Census);
                    }
                    index
                }
                None => {
                    let Some(index) = rows.iter().position(Option::is_none) else {
                        return Err(CapError::Census);
                    };
                    rows[index] = Some(CapRow {
                        name: cap.name(),
                        entries_max: cap.entries_max(),
                        fill: cap.fill(),
                        high_water: 0,
                        crossings: 0,
                        assembly_live: 0,
                    });
                    index
                }
            }
        };
        let late = cap.fill() == CapFill::Assembly && self.phase.get() != CensusPhase::Assembling;
        self.note_violations(u32::from(late), "a registration after the serve mark", index);
        Ok(CapRowHandle { census: Rc::clone(self), index, fill: cap.fill() })
    }

    /// `count` violations of one kind on the row at `index`: counted first,
    /// so the count is read after the assertion unwinds, then asserted in a
    /// debug build. The row's name is read here alone, on a violation: the
    /// reports stay direct on their common path (`count == 0`).
    fn note_violations(&self, count: u32, what: &str, index: usize) {
        if count == 0 {
            return;
        }
        let total = self.cap_assembly_violations.get().saturating_add(count);
        self.cap_assembly_violations.set(total);
        debug_assert!(
            count == 0,
            "cap-assembly-violated: {what} on cap `{}` of {} ({count} row(s))",
            self.name(index),
            self.cell
        );
    }

    fn with_row(&self, index: usize, edit: impl FnOnce(&mut CapRow)) {
        if let Some(row) = self.rows.borrow_mut()[index].as_mut() {
            edit(row);
        }
    }

    fn name(&self, index: usize) -> &'static str {
        self.rows.borrow()[index].map_or("", |row| row.name)
    }
}

impl CapRowHandle {
    /// The container's live count rose to `live`. A `Serving` container
    /// reports only when its own high water rises; an `Assembly` one never
    /// raises its own, so it reports every publish, and the row keeps its
    /// live count. On an `Assembly` row after the serve mark it is a
    /// violation. The report is cold and out of line, and it takes the
    /// census and two scalars, never a pointer into the container: the rare
    /// report stays apart from the one compare a publish pays, and borrows
    /// nothing of the container that made it.
    #[cold]
    #[inline(never)]
    pub(super) fn report_publish(&self, live: u32) {
        let (census, index, assembly) = (&*self.census, self.index, self.fill == CapFill::Assembly);
        census.with_row(index, |row| {
            row.high_water = row.high_water.max(live);
            if assembly {
                row.assembly_live = live;
            }
        });
        let late = assembly && census.phase.get() != CensusPhase::Assembling;
        census.note_violations(u32::from(late), "a publish after the serve mark", index);
    }

    /// An `Assembly` container's live count fell to `live` (a slab's `take`,
    /// a map's `remove`); a `Serving` container never calls this, and the
    /// deque, never `Assembly`, has no caller: the slab's `take` is its first.
    #[allow(
        dead_code,
        reason = "the slab's and the map's removal report; the deque is never Assembly"
    )]
    #[cold]
    #[inline(never)]
    pub(super) fn report_removal(&self, live: u32) {
        if self.fill == CapFill::Assembly {
            self.census.with_row(self.index, |row| row.assembly_live = live);
        }
    }

    /// A crossing: `Full` answered, or a merge at the cap. On an `Assembly`
    /// row after the serve mark it is a violation.
    #[cold]
    #[inline(never)]
    pub(super) fn report_full(&self) {
        let (census, index) = (&*self.census, self.index);
        census.with_row(index, |row| row.crossings = row.crossings.saturating_add(1));
        let late = self.fill == CapFill::Assembly && census.phase.get() != CensusPhase::Assembling;
        census.note_violations(u32::from(late), "a crossing after the serve mark", index);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    const CELL: CellId = CellId(3);
    const FOUR: Cap = Cap::entries::<4>("timer-owners", CapFill::Assembly);
    const FOUR_SERVING: Cap = Cap::entries::<4>("timer-owners", CapFill::Serving);
    const THREE: Cap = Cap::entries::<3>("timer-owners", CapFill::Assembly);
    const MARKS: Cap = Cap::entries::<16>("epoch-marks", CapFill::Serving);

    /// Runs `plant` and requires both halves of a violation: the count, and
    /// in a debug build the assertion carrying `cap-assembly-violated`, the
    /// cell and the violated row's name (every plant's row is `timer-owners`).
    fn expect_violation(census: &Rc<CapCensus>, before: u32, plant: impl FnOnce()) {
        let outcome = catch_unwind(AssertUnwindSafe(plant));
        if cfg!(debug_assertions) {
            let payload = outcome.expect_err("a debug build asserts the violation");
            let text = panic_text(payload.as_ref());
            assert!(text.contains("cap-assembly-violated"), "{text}");
            assert!(text.contains("cell3"), "{text}");
            assert!(text.contains("`timer-owners`"), "{text}");
        } else {
            outcome.expect("a release build counts without asserting");
        }
        assert_eq!(census.cap_assembly_violations(), before + 1, "the violation is counted");
    }

    fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .expect("a panic message")
    }

    fn fill(row: &CapRowHandle, count: u32) {
        for live in 1..=count {
            row.report_publish(live);
        }
    }

    #[test]
    fn a_census_is_born_assembling_with_its_cell_and_no_row() {
        let census = CapCensus::new(CELL);
        assert_eq!(census.cell(), CELL);
        assert_eq!(census.phase(), CensusPhase::Assembling);
        assert_eq!(census.rows().count(), 0);
        assert_eq!(census.row("epoch-marks"), None);
        assert_eq!(census.cap_assembly_violations(), 0);
    }

    #[test]
    fn phases_go_assembling_serving_stopping() {
        let census = CapCensus::new(CELL);
        census.mark_serving();
        assert_eq!(census.phase(), CensusPhase::Serving);
        census.mark_stopping();
        assert_eq!(census.phase(), CensusPhase::Stopping);
        // A cell whose loop never ran still stops.
        let never_served = CapCensus::new(CELL);
        never_served.mark_stopping();
        assert_eq!(never_served.phase(), CensusPhase::Stopping);
        assert_eq!(census.cap_assembly_violations(), 0);
    }

    #[test]
    fn one_name_with_one_cap_and_fill_shares_a_row() {
        let census = CapCensus::new(CELL);
        let first = census.register(MARKS).expect("first instance");
        let second = census.register(MARKS).expect("a per-namespace second instance");
        assert_eq!(first.index, second.index);
        assert_eq!(census.rows().count(), 1);
        // high_water is the maximum over the instances, crossings the sum.
        fill(&first, 5);
        fill(&second, 9);
        first.report_full();
        second.report_full();
        second.report_full();
        let row = census.row("epoch-marks").expect("row");
        assert_eq!(row.high_water, 9);
        assert_eq!(row.crossings, 3);
        assert_eq!(row.assembly_live, 0, "a Serving row keeps no live count");
        assert_eq!(row.entries_max, 16);
        assert_eq!(row.fill, CapFill::Serving);
    }

    #[test]
    fn a_second_cap_value_under_one_name_is_census() {
        let census = CapCensus::new(CELL);
        census.register(MARKS).expect("first");
        const OTHER: Cap = Cap::entries::<17>("epoch-marks", CapFill::Serving);
        assert_eq!(census.register(OTHER).map(|_| ()), Err(CapError::Census));
        assert_eq!(census.rows().count(), 1, "nothing was registered");
    }

    #[test]
    fn a_second_fill_under_one_name_is_census() {
        let census = CapCensus::new(CELL);
        census.register(FOUR).expect("the Assembly instance");
        assert_eq!(census.register(FOUR_SERVING).map(|_| ()), Err(CapError::Census));
        let other = CapCensus::new(CELL);
        other.register(FOUR_SERVING).expect("the Serving instance");
        assert_eq!(other.register(FOUR).map(|_| ()), Err(CapError::Census));
    }

    #[test]
    fn an_assembly_name_registered_twice_is_census() {
        let census = CapCensus::new(CELL);
        census.register(FOUR).expect("one instance per cell");
        assert_eq!(census.register(FOUR).map(|_| ()), Err(CapError::Census));
        assert_eq!(census.rows().count(), 1);
    }

    #[test]
    fn the_name_past_the_row_bound_is_census() {
        let one = core::num::NonZeroU32::MIN;
        let census = CapCensus::new(CELL);
        for name in &NAME_TABLE[..CAP_CENSUS_ROWS_MAX] {
            let cap = Cap { name, entries: one, fill: CapFill::Serving };
            census.register(cap).expect("within the bound");
        }
        assert_eq!(census.rows().count(), CAP_CENSUS_ROWS_MAX);
        let over =
            Cap { name: NAME_TABLE[CAP_CENSUS_ROWS_MAX], entries: one, fill: CapFill::Serving };
        assert_eq!(census.register(over).map(|_| ()), Err(CapError::Census));
        // A known name still shares its row at the bound.
        let known = Cap { name: NAME_TABLE[0], entries: one, fill: CapFill::Serving };
        census.register(known).expect("shares row 0");
        assert_eq!(census.rows().count(), CAP_CENSUS_ROWS_MAX);
    }

    const NAME_TABLE: [&str; CAP_CENSUS_ROWS_MAX + 1] = [
        "n00", "n01", "n02", "n03", "n04", "n05", "n06", "n07", "n08", "n09", "n10", "n11", "n12",
        "n13", "n14", "n15", "n16", "n17", "n18", "n19", "n20", "n21", "n22", "n23", "n24", "n25",
        "n26", "n27", "n28", "n29", "n30", "n31", "n32", "n33", "n34", "n35", "n36", "n37", "n38",
        "n39", "n40", "n41", "n42", "n43", "n44", "n45", "n46", "n47", "n48", "n49", "n50", "n51",
        "n52", "n53", "n54", "n55", "n56", "n57", "n58", "n59", "n60", "n61", "n62", "n63", "n64",
    ];

    // The serve mark's plants (I13). Each requires the assertion and the
    // count; the full row is the control.

    #[test]
    fn an_assembly_row_full_at_the_mark_is_the_control() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 4);
        census.mark_serving();
        assert_eq!(census.cap_assembly_violations(), 0);
        let read = census.row("timer-owners").expect("row");
        assert_eq!((read.assembly_live, read.high_water), (4, 4));
    }

    #[test]
    fn an_assembly_row_under_filled_at_the_mark_is_asserted_and_counted() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 3);
        expect_violation(&census, 0, || census.mark_serving());
        assert_eq!(census.phase(), CensusPhase::Serving);
    }

    #[test]
    fn an_assembly_row_filled_then_one_removed_is_caught_by_the_live_count() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 4);
        row.report_removal(3);
        let read = census.row("timer-owners").expect("row");
        assert_eq!(read.high_water, 4, "a high-water check would pass this host");
        assert_eq!(read.assembly_live, 3);
        expect_violation(&census, 0, || census.mark_serving());
    }

    #[test]
    fn the_mark_names_the_short_row_behind_a_serving_one() {
        // The name is resolved from the short row's index at the violation:
        // row 0 is a full Serving row, so a text naming row 0 is wrong.
        let census = CapCensus::new(CELL);
        let marks = census.register(MARKS).expect("row 0");
        let row = census.register(FOUR).expect("row 1");
        fill(&marks, 16);
        fill(&row, 3);
        let outcome = catch_unwind(AssertUnwindSafe(|| census.mark_serving()));
        if cfg!(debug_assertions) {
            let payload = outcome.expect_err("a debug build asserts the violation");
            let text = panic_text(payload.as_ref());
            assert!(text.contains("`timer-owners`"), "{text}");
            assert!(!text.contains("`epoch-marks`"), "{text}");
        }
        assert_eq!(census.cap_assembly_violations(), 1);
    }

    #[test]
    fn two_short_assembly_rows_count_two_at_the_mark() {
        let census = CapCensus::new(CELL);
        let timers = census.register(FOUR).expect("row");
        const CLASSES: Cap = Cap::entries::<2>("task-classes", CapFill::Assembly);
        let classes = census.register(CLASSES).expect("row");
        fill(&timers, 3);
        fill(&classes, 1);
        let outcome = catch_unwind(AssertUnwindSafe(|| census.mark_serving()));
        assert_eq!(outcome.is_err(), cfg!(debug_assertions));
        assert_eq!(census.cap_assembly_violations(), 2);
    }

    #[test]
    fn an_assembly_publish_after_the_mark_is_asserted_and_counted() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 4);
        census.mark_serving();
        row.report_removal(3);
        expect_violation(&census, 0, || row.report_publish(4));
    }

    #[test]
    fn an_assembly_crossing_after_the_mark_is_asserted_and_counted() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 4);
        census.mark_serving();
        expect_violation(&census, 0, || row.report_full());
        assert_eq!(census.row("timer-owners").expect("row").crossings, 1);
    }

    #[test]
    fn an_assembly_registration_after_the_mark_is_asserted_and_counted() {
        let census = CapCensus::new(CELL);
        census.mark_serving();
        expect_violation(&census, 0, || {
            census.register(THREE).expect("registered, and counted");
        });
        assert_eq!(census.rows().count(), 1);
    }

    #[test]
    fn a_serving_row_is_outside_the_assembly_checks() {
        let census = CapCensus::new(CELL);
        let row = census.register(MARKS).expect("row");
        fill(&row, 3);
        census.mark_serving();
        row.report_publish(4);
        row.report_full();
        census.register(MARKS).expect("a Serving registration while serving");
        assert_eq!(census.cap_assembly_violations(), 0);
        let read = census.row("epoch-marks").expect("row");
        assert_eq!((read.high_water, read.crossings, read.assembly_live), (4, 1, 0));
    }

    #[test]
    fn violations_accumulate_across_plants() {
        let census = CapCensus::new(CELL);
        let row = census.register(FOUR).expect("row");
        fill(&row, 2);
        expect_violation(&census, 0, || census.mark_serving());
        expect_violation(&census, 1, || row.report_publish(3));
        expect_violation(&census, 2, || row.report_full());
        assert_eq!(census.cap_assembly_violations(), 3);
    }
}
