//! The cap census oracle (ADR-0163 D2), read for every cell at the end of
//! every node life, after the cell's last loop iteration:
//!
//! - **H1**, per row: `high_water ≤ entries_max` — no capped container held
//!   more than its cap. Violation token `cap-high-water-over`.
//! - **H5**: `cap_assembly_violations = 0` (token `cap-assembly-violated`),
//!   and the census left `Assembling`: a census never marked means the
//!   cell's loop took no serve mark, or marked another census (token
//!   `census-never-marked`).
//! - Liveness: a cell holding a tiered namespace has the reclaim marks'
//!   row (token `cap-row-missing`), and the rows checked are summed on the
//!   report line (`cap_rows_checked`), so a run whose census read nothing is
//!   visible.
//!
//! H1 and H5 read the product's own counts: wiring checks. The independent
//! evidence is each site's model test and the DST rows' disk-side oracles.
//! Each violation's token is whitespace-free, so a canary row can expect it.

use inf_foundation::bounded::{CapCensus, CapRow, CensusPhase};

/// The reclaim marks' census row, which every cell with a tiered namespace
/// registers.
const EPOCH_MARKS_ROW: &str = "epoch-marks";

/// The census verdict of one run: every cell of every life it judged.
#[derive(Clone, Debug, Default)]
pub(crate) struct CensusVerdict {
    /// Rows H1 read, summed over the cells and lives (node sum).
    pub(crate) rows_checked: u64,
    pub(crate) violations: Vec<String>,
}

impl CensusVerdict {
    /// Judges one cell's census at the end of a life: `tiered_tables` is the
    /// number of tiered namespaces the cell holds.
    pub(crate) fn judge(&mut self, cell: usize, census: &CapCensus, tiered_tables: usize) {
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
            self.rows_checked += 1;
            self.violations.extend(over_cap(cell, &row));
        }
        if tiered_tables > 0 && census.row(EPOCH_MARKS_ROW).is_none() {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — {tiered_tables} tiered \
                 namespace(s) and no `{EPOCH_MARKS_ROW}` row"
            ));
        }
    }

    /// Folds another verdict into this one.
    pub(crate) fn absorb(&mut self, other: CensusVerdict) {
        self.rows_checked += other.rows_checked;
        self.violations.extend(other.violations);
    }
}

/// H1 on one row: its high water above its cap is red.
fn over_cap(cell: usize, row: &CapRow) -> Option<String> {
    (row.high_water > row.entries_max).then(|| {
        format!(
            "CAP CENSUS VIOLATION cell {cell}: cap-high-water-over — row `{}` held {} entries, \
             above its cap of {}",
            row.name, row.high_water, row.entries_max
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
    /// (no reclaim marks' row) fails, though H1 and H5 have nothing to read.
    #[test]
    fn an_empty_census_beside_a_tiered_namespace_is_red() {
        let (census, serve_mark) = CapCensus::new(CellId(0));
        serve_mark.take();
        let mut verdict = CensusVerdict::default();
        verdict.judge(0, &census, 1);
        assert_eq!(verdict.rows_checked, 0);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].contains("cap-row-missing"), "{:?}", verdict.violations);
        let mut control = CensusVerdict::default();
        control.judge(0, &census, 0);
        assert!(control.violations.is_empty(), "no tiered namespace, no row owed");
    }

    /// H5's liveness: a census its cell's loop never marked is red, by the
    /// token only that violation carries.
    #[test]
    fn a_census_never_marked_is_red() {
        let (census, _serve_mark) = CapCensus::new(CellId(2));
        let mut verdict = CensusVerdict::default();
        verdict.judge(2, &census, 0);
        assert_eq!(verdict.violations.len(), 1);
        assert!(verdict.violations[0].contains("census-never-marked"));
        assert!(!verdict.violations[0].contains("cap-assembly-violated"));
    }

    /// H1 reads each row; a full row is green and a row past its cap red.
    #[test]
    fn h1_reads_every_row_and_is_red_past_a_cap() {
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
        verdict.judge(1, &census, 1);
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
        };
        let text = over_cap(1, &over).expect("red past the cap");
        assert!(text.contains("cap-high-water-over"), "{text}");
        assert_eq!(over_cap(1, &CapRow { high_water: 2, ..over }), None, "at the cap is legal");
    }
}
