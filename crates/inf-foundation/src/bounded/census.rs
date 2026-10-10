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

/// A container's handle on its row: the index is fixed at registration and
/// the fill is kept beside it, so a report tests one byte.
pub(super) struct CensusRow {
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
        let mut first = "";
        for row in self.rows.borrow().iter().flatten() {
            if row.fill == CapFill::Assembly && row.assembly_live < row.entries_max {
                short = short.saturating_add(1);
                if first.is_empty() {
                    first = row.name;
                }
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
    pub(super) fn register(self: &Rc<Self>, cap: Cap) -> Result<CensusRow, CapError> {
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
        self.note_violations(u32::from(late), "a registration after the serve mark", cap.name());
        Ok(CensusRow { census: Rc::clone(self), index, fill: cap.fill() })
    }

    /// `count` violations of one kind on `name`: counted first, so the count
    /// is read after the assertion unwinds, then asserted in a debug build.
    fn note_violations(&self, count: u32, what: &str, name: &str) {
        if count == 0 {
            return;
        }
        let total = self.cap_assembly_violations.get().saturating_add(count);
        self.cap_assembly_violations.set(total);
        debug_assert!(
            count == 0,
            "cap-assembly-violated: {what} on cap `{name}` of {} ({count} row(s))",
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

impl CensusRow {
    /// The container's live count rose to `live`. A `Serving` container
    /// reports only when its own high water rises; an `Assembly` one never
    /// raises its own, so it reports every publish, and the row keeps its
    /// live count. On an `Assembly` row after the serve mark it is a
    /// violation. Off the hot path: the call takes the census and two
    /// scalars, never a pointer into the container, so the container's
    /// fields stay in registers around it.
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
        census.note_violations(
            u32::from(late),
            "a publish after the serve mark",
            census.name(index),
        );
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
        census.note_violations(
            u32::from(late),
            "a crossing after the serve mark",
            census.name(index),
        );
    }
}
