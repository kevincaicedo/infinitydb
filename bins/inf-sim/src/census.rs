//! The cap census oracle (ADR-0163 D2), read for every cell at the end of
//! every node life, after the cell's last loop iteration:
//!
//! - **The cap bound**, per row: `high_water ≤ entries_max` — no capped
//!   container held more than its cap. Violation token `cap-high-water-over`.
//! - **The crossing rule**, per row that crossed: `crossings > 0` implies
//!   `capacity_now ≤ high_water` — a container answered `Full` (or merged)
//!   only once full at the capacity it had then. Violation token
//!   `cap-crossed-short`; the rows it judged are summed on the report line
//!   (`cap_rows_crossed`), so a leg that never crossed is visible.
//! - **The assembly rule**: `cap_assembly_violations = 0` (token
//!   `cap-assembly-violated`), and the census left `Assembling`: a census
//!   never marked means the cell's loop took no serve mark, or marked
//!   another census (token `census-never-marked`).
//! - **The window's turn rule**: a cell's durable plane counts no turn
//!   that found the extent-seal window full (`turn_refused = 0`, the
//!   release form of `len + w ≤ 16`; token `turn-refused`).
//! - Liveness: a cell holding a tiered namespace has the reclaim marks'
//!   row, and a cell whose durable plane is built has the extent-seal
//!   window's row (token `cap-row-missing`); the rows checked are summed on
//!   the report line (`cap_rows_checked`), so a run whose census read
//!   nothing is visible.
//!
//! The rules read the product's own counts: wiring checks. The independent
//! evidence is each site's model test and the DST rows' disk-side oracles.
//! Each violation's token is whitespace-free, so a canary row can expect it.

use inf_foundation::bounded::{CapCensus, CapRow, CensusPhase};

/// The reclaim marks' census row, which every cell with a tiered namespace
/// registers.
const EPOCH_MARKS_ROW: &str = "epoch-marks";
/// The extent-seal window's census row, which every cell's durable plane
/// registers when it is built.
const EXTENT_WINDOW_ROW: &str = "extent-window";

/// What a cell's planes hold at the end of a life, beside its census.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CellFacts {
    /// Tiered namespaces the cell's tier plane serves (none before the
    /// cell's recovery has built that plane).
    pub(crate) tiered_tables: usize,
    /// The cell's durable plane, once built: its turns that found the
    /// extent-seal window full.
    pub(crate) turn_refused: Option<u64>,
}

/// The census verdict of one run: every cell of every life it judged.
#[derive(Clone, Debug, Default)]
pub(crate) struct CensusVerdict {
    /// Rows the cap bound read, summed over the cells and lives (node sum).
    pub(crate) rows_checked: u64,
    /// Rows the crossing rule judged, those with `crossings > 0`, summed over
    /// the cells and lives (node sum): the rule's engagement.
    pub(crate) rows_crossed: u64,
    pub(crate) violations: Vec<String>,
}

impl CensusVerdict {
    /// Judges one cell's census at the end of a life, beside what its
    /// planes hold (`facts`).
    pub(crate) fn judge(&mut self, cell: usize, census: &CapCensus, facts: CellFacts) {
        if census.phase() == CensusPhase::Assembling {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: census-never-marked — the census its \
                 containers registered with is still assembling after the cell's last iteration"
            ));
        }
        let assembly = census.cap_assembly_violations();
        if assembly > 0 {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: cap-assembly-violated — {assembly} \
                 assembly violation(s) counted"
            ));
        }
        for row in census.rows() {
            self.judge_row(cell, &row);
        }
        let tiered_tables = facts.tiered_tables;
        if tiered_tables > 0 && census.row(EPOCH_MARKS_ROW).is_none() {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — {tiered_tables} tiered \
                 namespace(s) and no `{EPOCH_MARKS_ROW}` row"
            ));
        }
        let Some(refused) = facts.turn_refused else { return };
        if census.row(EXTENT_WINDOW_ROW).is_none() {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — a durable plane and no \
                 `{EXTENT_WINDOW_ROW}` row"
            ));
        }
        if refused > 0 {
            self.violations.push(format!(
                "EXTENT WINDOW VIOLATION cell {cell}: turn-refused — {refused} turn(s) found the \
                 extent-seal window full: a unit handed out twice"
            ));
        }
    }

    /// One row's clauses: the cap bound, and the crossing rule on a row
    /// that crossed.
    fn judge_row(&mut self, cell: usize, row: &CapRow) {
        self.rows_checked += 1;
        self.violations.extend(over_cap(cell, row));
        if row.crossings > 0 {
            self.rows_crossed += 1;
            self.violations.extend(crossed_short(cell, row));
        }
    }

    /// Folds another verdict into this one.
    pub(crate) fn absorb(&mut self, other: CensusVerdict) {
        self.rows_checked += other.rows_checked;
        self.rows_crossed += other.rows_crossed;
        self.violations.extend(other.violations);
    }
}

/// The cap bound on one row: its high water above its cap is red.
fn over_cap(cell: usize, row: &CapRow) -> Option<String> {
    (row.high_water > row.entries_max).then(|| {
        format!(
            "CAP CENSUS VIOLATION cell {cell}: cap-high-water-over — row `{}` held {} entries, \
             above its cap of {}",
            row.name, row.high_water, row.entries_max
        )
    })
}

/// The crossing rule on a row that crossed: a `Full` or a merge is
/// answered only at a live count a publish raised the high water to, and a
/// retirement lowers `capacity_now` before any `Full` at the new count, so
/// `capacity_now` above the high water is a crossing short of the cap.
/// `high_water <= entries_max` is the cap bound's, so no row reports twice.
fn crossed_short(cell: usize, row: &CapRow) -> Option<String> {
    (row.capacity_now > row.high_water).then(|| {
        format!(
            "CAP CENSUS VIOLATION cell {cell}: cap-crossed-short — row `{}` crossed {} time(s) \
             at a high water of {}, below its capacity of {} now (cap {})",
            row.name, row.crossings, row.high_water, row.capacity_now, row.entries_max
        )
    })
}

#[cfg(test)]
mod tests {
    use inf_foundation::CellId;
    use inf_foundation::bounded::{
        Cap, CapFill, CappedDeque, Coalesce, Lossy, Merge, Reserve, Reserved,
    };

    use super::*;

    /// The liveness red: a cell with a tiered namespace and an empty census
    /// (no reclaim marks' row) fails, though neither rule has a row to read.
    #[test]
    fn an_empty_census_beside_a_tiered_namespace_is_red() {
        let (census, serve_mark) = CapCensus::new(CellId(0));
        serve_mark.take();
        let mut verdict = CensusVerdict::default();
        verdict.judge(0, &census, CellFacts { tiered_tables: 1, turn_refused: None });
        assert_eq!(verdict.rows_checked, 0);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].contains("cap-row-missing"), "{:?}", verdict.violations);
        let mut durable = CensusVerdict::default();
        durable.judge(0, &census, CellFacts { tiered_tables: 0, turn_refused: Some(0) });
        assert_eq!(durable.violations.len(), 1, "{:?}", durable.violations);
        assert!(durable.violations[0].contains("extent-window"), "{:?}", durable.violations);
        let mut control = CensusVerdict::default();
        control.judge(0, &census, CellFacts::default());
        assert!(control.violations.is_empty(), "no tiered namespace, no row owed");
    }

    /// The window's turn rule: a refused turn is red by its token; none is
    /// green.
    #[test]
    fn a_refused_turn_is_red() {
        const WINDOW: Cap = Cap::entries::<16>("extent-window", CapFill::Serving);
        let (census, serve_mark) = CapCensus::new(CellId(3));
        serve_mark.take();
        let _window = CappedDeque::<u64, Reserve>::new(WINDOW, &census).expect("built");
        let mut red = CensusVerdict::default();
        red.judge(3, &census, CellFacts { tiered_tables: 0, turn_refused: Some(1) });
        assert_eq!(red.violations.len(), 1, "{:?}", red.violations);
        assert!(red.violations[0].contains("turn-refused"), "{:?}", red.violations);
        let mut green = CensusVerdict::default();
        green.judge(3, &census, CellFacts { tiered_tables: 0, turn_refused: Some(0) });
        assert!(green.violations.is_empty(), "{:?}", green.violations);
    }

    /// The assembly rule's liveness: a census its cell's loop never marked
    /// is red, by the token only that violation carries.
    #[test]
    fn a_census_never_marked_is_red() {
        let (census, _serve_mark) = CapCensus::new(CellId(2));
        let mut verdict = CensusVerdict::default();
        verdict.judge(2, &census, CellFacts::default());
        assert_eq!(verdict.violations.len(), 1);
        assert!(verdict.violations[0].contains("census-never-marked"));
        assert!(!verdict.violations[0].contains("cap-assembly-violated"));
    }

    /// The cap bound reads each row; a full row is green and a row past its
    /// cap red.
    #[test]
    fn the_cap_bound_reads_every_row_and_is_red_past_a_cap() {
        struct Mark;
        impl Lossy for Mark {}
        struct Later;
        impl Merge<Mark> for Later {
            fn merge(back: &mut Mark, new: Mark) {
                *back = new;
            }
        }
        const MARKS: Cap = Cap::entries::<16>("epoch-marks", CapFill::Serving);
        const PAIRS: Cap = Cap::entries::<2>("pairs", CapFill::Serving);
        let (census, serve_mark) = CapCensus::new(CellId(1));
        serve_mark.take();
        let mut marks = CappedDeque::<Mark, Coalesce<Later>>::new(MARKS, &census).expect("built");
        let mut pairs = CappedDeque::<u8, Reserve>::new(PAIRS, &census).expect("built");
        for _ in 0..20 {
            marks.push(Mark);
        }
        for _ in 0..3 {
            if let Reserved::Slot(slot) = pairs.reserve() {
                slot.publish(1);
            }
        }
        let mut verdict = CensusVerdict::default();
        verdict.judge(1, &census, CellFacts { tiered_tables: 1, turn_refused: None });
        assert_eq!(verdict.rows_checked, 2);
        assert!(verdict.violations.is_empty(), "full rows are within their caps: {verdict:?}");
        // A row past its cap: no correct container writes one, so a row
        // read with a planted high water stands in for the cap-plus-one plant.
        let over = CapRow {
            name: "pairs",
            entries_max: 2,
            fill: CapFill::Serving,
            high_water: 3,
            crossings: 0,
            assembly_live: 0,
            capacity_now: 2,
        };
        let text = over_cap(1, &over).expect("red past the cap");
        assert!(text.contains("cap-high-water-over"), "{text}");
        assert_eq!(over_cap(1, &CapRow { high_water: 2, ..over }), None, "at the cap is legal");
    }

    /// The crossing rule on planted rows: a row that crossed holds its high
    /// water at or above the capacity it has now, whatever a retirement
    /// lowered it to. Each row is `(entries_max, capacity_now, high_water,
    /// crossings)` with the verdict's tokens and whether it counts as
    /// crossed. The rule reads `capacity_now <= high_water` alone: a high
    /// water past the cap is the cap bound's red, so no row reports twice.
    #[test]
    fn a_row_that_crossed_below_its_capacity_now_is_red() {
        const SHORT: &str = "cap-crossed-short";
        const OVER: &str = "cap-high-water-over";
        type Shape = (u32, u32, u32, u64);
        let planted: [(Shape, Option<&str>, u64); 7] = [
            ((4, 4, 3, 1), Some(SHORT), 1),
            ((4, 4, 4, 1), None, 1),
            ((4, 2, 1, 1), Some(SHORT), 1),
            ((4, 2, 2, 1), None, 1),
            // The high water predates the shrink.
            ((4, 2, 3, 1), None, 1),
            ((4, 4, 3, 0), None, 0),
            ((4, 4, 5, 1), Some(OVER), 1),
        ];
        for ((entries_max, capacity_now, high_water, crossings), token, crossed) in planted {
            let row = CapRow {
                name: "planted",
                entries_max,
                fill: CapFill::Serving,
                high_water,
                crossings,
                assembly_live: 0,
                capacity_now,
            };
            let mut verdict = CensusVerdict::default();
            verdict.judge_row(5, &row);
            let shape = (entries_max, capacity_now, high_water, crossings);
            assert_eq!(verdict.rows_crossed, crossed, "{shape:?}");
            match token {
                Some(token) => {
                    let found = &verdict.violations;
                    assert_eq!(found.len(), 1, "{shape:?} reads {token}: {found:?}");
                    assert!(found[0].contains(token), "{shape:?} reads {token}: {found:?}");
                }
                None => assert!(verdict.violations.is_empty(), "{shape:?}: {verdict:?}"),
            }
        }
    }
}
