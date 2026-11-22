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
//! - **The marks' derived rule**: the seal marks' and the hole marks' rows
//!   cross nothing (`crossings = 0`): the stride keeps the seal marks under
//!   half their cap, and the window's arithmetic keeps the hole marks under
//!   theirs. A crossing is a derivation the run disproved (tokens
//!   `seal-marks-crossed`, `hole-marks-crossed`).
//! - **The window's turn rule**: a cell's durable plane counts no turn
//!   that found the extent-seal window full (`turn_refused = 0`, the
//!   release form of `len + w ≤ 16`; token `turn-refused`).
//! - **The MAINTAIN room's rule**: no MAINTAIN pass left a namespace with
//!   work for a later one because its task class had no room
//!   (`tier_maint_task_waits = 0`, ADR-0177 D2; token `maint-paced`).
//!   Either class fills only once some 64 tiered namespaces of one cell hold
//!   its slots (a `compact` chain or four `shadow` reads each), which no
//!   simulated run reaches, so a pace there is a room read short. The
//!   count's liveness is the room's crate test, which reads 1 at a one-slot
//!   room.
//! - **The gates' derived rules** (ADR-0177 D4): no reserve of the fabric
//!   gate found every holder taken (`gate_full = 0`, token `gate-full`), no
//!   completion found no holder awaiting it (`gate_stale = 0`, token
//!   `gate-stale`), and the cold gate refused no read for a full route or
//!   holder table (`cold_gate_full = 0`, token `cold-gate-full`). Each is
//!   derived never: the fabric gate's holders cover every task's units and
//!   the orphans its peers' credits allow, an Orphaned holder waits for its
//!   one late completion, and the cold gate's, while its cap is its product
//!   and not its ceiling, cover every read its pool can hold.
//! - **The wait pool's derived rule** (ADR-0177 D5): no wait's first poll
//!   found the cell's pool empty or its list without its key
//!   (`wait_nodes_full = 0`, token `wait-nodes-full`). Derived never: the
//!   pool holds a node for every task slot, a task holds at most one parked
//!   wait, and every list holds the keys its waits name from its build.
//! - Liveness: a server cell has its timer owners' row, which its plane
//!   builds before its loop exists, and on a node of more than one cell the
//!   rows of its three per-origin deques (ADR-0177 D6: `ns-applies`,
//!   `owner-pubs`, `gated`), which it builds for each peer; a cell holding a
//!   tiered namespace has
//!   the reclaim marks', the seal marks' and the hole marks' rows, which its
//!   tables register on the plane's census; and a cell whose durable plane
//!   is built has
//!   the extent-seal window's row (token `cap-row-missing`); the rows
//!   checked are summed on the report line (`cap_rows_checked`), so a run
//!   whose census read nothing is visible.
//!
//! The rules read the product's own counts: wiring checks. The independent
//! evidence is each site's model test and the DST rows' disk-side oracles.
//! Each violation's token is whitespace-free, so a canary row can expect it.

use crate::driver::Violations;
use inf_foundation::bounded::{CapCensus, CapRow, CensusPhase};

/// The reclaim marks' census row, which every cell with a tiered namespace
/// registers.
const EPOCH_MARKS_ROW: &str = "epoch-marks";
/// The seal marks' and the hole marks' census rows, which every tiered
/// table registers on its cell's census when it is built.
const SEAL_MARKS_ROW: &str = inf_store::limits::SEAL_MARKS.name();
const HOLE_MARKS_ROW: &str = inf_store::limits::HOLE_MARKS.name();
/// The extent-seal window's census row, which every cell's durable plane
/// registers when it is built.
const EXTENT_WINDOW_ROW: &str = "extent-window";
/// The timer owners' census row, which every server plane registers.
const TIMER_OWNERS_ROW: &str = inf_server::limits::SERVER_TIMER_OWNERS.name();
/// The rows of the work a server plane parks for its peers, one deque per
/// peer under each (ADR-0177 D6).
const PARKED_ROWS: [&str; 3] = [
    inf_server::limits::NS_APPLIES.name(),
    inf_server::limits::OWNER_PUBS.name(),
    inf_server::limits::GATED_VERDICTS.name(),
];

/// What a cell's planes hold at the end of a life, beside its census. No
/// `Default`: a site that builds the facts names every one it read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CellFacts {
    /// The cell is a server plane, which builds its timer set and its
    /// owners before its loop exists.
    pub(crate) server_plane: bool,
    /// The node's other cells, for each of which a server plane parks work
    /// in its three per-origin deques.
    pub(crate) peers: u16,
    /// Tiered namespaces the cell's tier plane serves (none before the
    /// cell's recovery has built that plane).
    pub(crate) tiered_tables: usize,
    /// The cell's durable plane, once built: its turns that found the
    /// extent-seal window full.
    pub(crate) turn_refused: Option<u64>,
    /// Namespaces the cell's MAINTAIN passes left for a later pass because
    /// their task class had no room (`tier_maint_task_waits`).
    pub(crate) maint_paced: u64,
    /// The fabric gate's reserves that found every holder taken.
    pub(crate) gate_full: u64,
    /// The fabric gate's completions for no holder that awaits one.
    pub(crate) gate_stale: u64,
    /// The cold gate's reads refused for no free route or holder (0 on a
    /// cell with no cold engine).
    pub(crate) cold_gate_full: u64,
    /// First polls that found the cell's wait pool empty or their list
    /// without their key.
    pub(crate) wait_nodes_full: u64,
}

/// The census verdict of one run: every cell of every life it judged.
#[derive(Clone, Debug, Default)]
pub(crate) struct CensusVerdict {
    /// Rows the cap bound read, summed over the cells and lives (node sum).
    pub(crate) rows_checked: u64,
    /// Rows the crossing rule judged, those with `crossings > 0`, summed over
    /// the cells and lives (node sum): the rule's engagement.
    pub(crate) rows_crossed: u64,
    pub(crate) violations: Violations,
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
        if facts.server_plane && census.row(TIMER_OWNERS_ROW).is_none() {
            self.violations.push(format!(
                "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — a server plane and no \
                 `{TIMER_OWNERS_ROW}` row"
            ));
        }
        for row in PARKED_ROWS {
            if facts.server_plane && facts.peers > 0 && census.row(row).is_none() {
                self.violations.push(format!(
                    "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — a server plane with {} \
                     peer(s) and no `{row}` row",
                    facts.peers
                ));
            }
        }
        let tiered_tables = facts.tiered_tables;
        for row in [EPOCH_MARKS_ROW, SEAL_MARKS_ROW, HOLE_MARKS_ROW] {
            if tiered_tables > 0 && census.row(row).is_none() {
                self.violations.push(format!(
                    "CAP CENSUS VIOLATION cell {cell}: cap-row-missing — {tiered_tables} tiered \
                     namespace(s) and no `{row}` row"
                ));
            }
        }
        marks_crossed(cell, census, SEAL_MARKS_ROW, "seal-marks-crossed", &mut self.violations);
        marks_crossed(cell, census, HOLE_MARKS_ROW, "hole-marks-crossed", &mut self.violations);
        maint_paced(cell, facts.maint_paced, &mut self.violations);
        gates_derived(cell, &facts, &mut self.violations);
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
        over_cap(cell, row, &mut self.violations);
        if row.crossings > 0 {
            self.rows_crossed += 1;
            crossed_short(cell, row, &mut self.violations);
        }
    }

    /// Folds another verdict into this one.
    pub(crate) fn absorb(&mut self, other: CensusVerdict) {
        self.rows_checked += other.rows_checked;
        self.rows_crossed += other.rows_crossed;
        self.violations.extend(other.violations);
    }
}

/// The marks' derived rule on one row: a crossing is red, under `token`,
/// recorded under its caller's check, so each row's token prints.
#[track_caller]
fn marks_crossed(
    cell: usize,
    census: &CapCensus,
    row: &str,
    token: &str,
    violations: &mut Violations,
) {
    let Some(row) = census.row(row) else { return };
    if row.crossings > 0 {
        violations.push(format!(
            "CAP CENSUS VIOLATION cell {cell}: {token} — row `{}` crossed {} time(s), derived \
             never to occur",
            row.name, row.crossings
        ));
    }
}

/// The MAINTAIN room's rule: a namespace paced for want of room is red.
fn maint_paced(cell: usize, paced: u64, violations: &mut Violations) {
    if paced > 0 {
        violations.push(format!(
            "MAINT ROOM VIOLATION cell {cell}: maint-paced — {paced} namespace(s) left for a \
             later MAINTAIN pass at a task class no simulated run fills"
        ));
    }
}

/// The gates' derived rules: each count above 0 is red, under its token.
fn gates_derived(cell: usize, facts: &CellFacts, violations: &mut Violations) {
    // Derived never: the fabric gate's holders cover every task's units and
    // the orphans its peers' credits allow (ADR-0177 D4).
    if facts.gate_full > 0 {
        violations.push(format!(
            "FABRIC GATE VIOLATION cell {cell}: gate-full — {} reserve(s) found every holder \
             taken",
            facts.gate_full
        ));
    }
    // Derived never: an Orphaned holder waits for its request's one
    // completion, so no completion finds a later occupant (ADR-0177 D4).
    if facts.gate_stale > 0 {
        violations.push(format!(
            "FABRIC GATE VIOLATION cell {cell}: gate-stale — {} completion(s) for no holder \
             that awaits one",
            facts.gate_stale
        ));
    }
    // Derived never while the cap is its product, `overflow_cap × (2 + P)`
    // unclamped: every queued read holds one holder and every other is a
    // member's of one of the pool's `P` reads (ADR-0177 D4). A clamped cap
    // can refuse, and that red is the clamp's, not a lost holder.
    if facts.cold_gate_full > 0 {
        violations.push(format!(
            "COLD GATE VIOLATION cell {cell}: cold-gate-full — {} read(s) refused for no free \
             route or holder",
            facts.cold_gate_full
        ));
    }
    // Derived never: the pool holds a node for every task slot, a task holds
    // at most one parked wait, and every list holds its waits' keys from its
    // build (ADR-0177 D5).
    if facts.wait_nodes_full > 0 {
        violations.push(format!(
            "WAIT POOL VIOLATION cell {cell}: wait-nodes-full — {} first poll(s) found no node \
             or no key",
            facts.wait_nodes_full
        ));
    }
}

/// The cap bound on one row: its high water above its cap is red.
fn over_cap(cell: usize, row: &CapRow, violations: &mut Violations) {
    if row.high_water > row.entries_max {
        violations.push(format!(
            "CAP CENSUS VIOLATION cell {cell}: cap-high-water-over — row `{}` held {} entries, \
             above its cap of {}",
            row.name, row.high_water, row.entries_max
        ));
    }
}

/// The crossing rule on a row that crossed: a `Full` or a merge is
/// answered only at a live count a publish raised the high water to, and a
/// retirement lowers `capacity_now` before any `Full` at the new count, so
/// `capacity_now` above the high water is a crossing short of the cap.
/// `high_water <= entries_max` is the cap bound's, so no row reports twice.
fn crossed_short(cell: usize, row: &CapRow, violations: &mut Violations) {
    if row.capacity_now > row.high_water {
        violations.push(format!(
            "CAP CENSUS VIOLATION cell {cell}: cap-crossed-short — row `{}` crossed {} time(s) \
             at a high water of {}, below its capacity of {} now (cap {})",
            row.name, row.crossings, row.high_water, row.capacity_now, row.entries_max
        ));
    }
}

#[cfg(test)]
mod tests {
    use inf_foundation::CellId;
    use inf_foundation::bounded::{
        Admission, Built, Cap, CapFill, CappedDeque, CappedMap, CappedSlab, Coalesce, LookupMut,
        Lossy, MapBuilt, Merge, Reserve, Reserved, Retire, SlabReserved, Taken,
    };

    use super::*;

    /// A cell that holds nothing beside its census: no server plane, no
    /// tiered namespace, no durable plane and every count 0.
    fn bare() -> CellFacts {
        CellFacts {
            server_plane: false,
            peers: 0,
            tiered_tables: 0,
            turn_refused: None,
            maint_paced: 0,
            gate_full: 0,
            gate_stale: 0,
            cold_gate_full: 0,
            wait_nodes_full: 0,
        }
    }

    /// The liveness red: a cell with a tiered namespace and an empty census
    /// (no reclaim marks' row) fails, though neither rule has a row to read.
    #[test]
    fn an_empty_census_beside_a_tiered_namespace_is_red() {
        let (census, serve_mark) = CapCensus::new(CellId(0));
        serve_mark.take();
        let mut verdict = CensusVerdict::default();
        verdict.judge(0, &census, CellFacts { tiered_tables: 1, turn_refused: None, ..bare() });
        assert_eq!(verdict.rows_checked, 0);
        assert_eq!(verdict.violations.len(), 3, "{:?}", verdict.violations);
        for (violation, row) in
            verdict.violations.iter().zip([EPOCH_MARKS_ROW, SEAL_MARKS_ROW, HOLE_MARKS_ROW])
        {
            assert!(violation.contains("cap-row-missing"), "{violation}");
            assert!(violation.contains(row), "{violation}");
        }
        let mut durable = CensusVerdict::default();
        durable.judge(0, &census, CellFacts { tiered_tables: 0, turn_refused: Some(0), ..bare() });
        assert_eq!(durable.violations.len(), 1, "{:?}", durable.violations);
        assert!(durable.violations[0].contains("extent-window"), "{:?}", durable.violations);
        let mut control = CensusVerdict::default();
        control.judge(0, &census, bare());
        assert!(control.violations.is_empty(), "no tiered namespace, no row owed");
    }

    /// The per-origin deques' liveness (ADR-0177 D6): a server plane with
    /// peers and without the three rows is red, one violation a row; with
    /// them, or with no peer, green.
    #[test]
    fn a_server_cell_with_peers_without_its_parked_rows_is_red() {
        const TIMERS: Cap = Cap::entries::<1>("timer-owners", CapFill::Serving);
        let facts = CellFacts { server_plane: true, peers: 2, ..bare() };
        let (bare, serve_mark) = CapCensus::new(CellId(1));
        let _owners = CappedSlab::<u8, u64, Retire>::builder(TIMERS, &bare).expect("built");
        serve_mark.take();
        let mut red = CensusVerdict::default();
        red.judge(1, &bare, facts);
        assert_eq!(red.violations.len(), 3, "{:?}", red.violations);
        for (violation, row) in red.violations.iter().zip(PARKED_ROWS) {
            assert!(
                violation.contains("cap-row-missing") && violation.contains(row),
                "{violation}"
            );
        }
        let mut alone = CensusVerdict::default();
        alone.judge(1, &bare, CellFacts { peers: 0, ..facts });
        assert!(alone.violations.is_empty(), "{:?}", alone.violations);
        let (census, serve_mark) = CapCensus::new(CellId(1));
        let _owners = CappedSlab::<u8, u64, Retire>::builder(TIMERS, &census).expect("built");
        let deques: Vec<_> = PARKED_ROWS
            .iter()
            .map(|&name| {
                let cap = match name {
                    "ns-applies" => inf_server::limits::NS_APPLIES,
                    "owner-pubs" => inf_server::limits::OWNER_PUBS,
                    _ => inf_server::limits::GATED_VERDICTS,
                };
                CappedDeque::<u8, Reserve>::new(cap, &census).expect("built")
            })
            .collect();
        serve_mark.take();
        let mut green = CensusVerdict::default();
        green.judge(1, &census, facts);
        assert!(green.violations.is_empty(), "{:?}", green.violations);
        drop(deques);
    }

    /// The timer owners' liveness: a server plane's census without the
    /// `timer-owners` row is red; with the row its assembly builds, green.
    #[test]
    fn a_server_cell_without_its_timer_owners_row_is_red() {
        let facts = CellFacts { server_plane: true, ..bare() };
        let (bare, serve_mark) = CapCensus::new(CellId(1));
        serve_mark.take();
        let mut red = CensusVerdict::default();
        red.judge(1, &bare, facts);
        assert_eq!(red.violations.len(), 1, "{:?}", red.violations);
        assert!(red.violations[0].contains("cap-row-missing"), "{:?}", red.violations);
        assert!(red.violations[0].contains(TIMER_OWNERS_ROW), "{:?}", red.violations);
        const OWNERS: Cap = Cap::entries::<1>("timer-owners", CapFill::Serving);
        let (census, serve_mark) = CapCensus::new(CellId(1));
        serve_mark.take();
        let _owners = CappedSlab::<u8, u64, Retire>::builder(OWNERS, &census).expect("built");
        let mut green = CensusVerdict::default();
        green.judge(1, &census, facts);
        assert!(green.violations.is_empty(), "{:?}", green.violations);
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
        red.judge(3, &census, CellFacts { tiered_tables: 0, turn_refused: Some(1), ..bare() });
        assert_eq!(red.violations.len(), 1, "{:?}", red.violations);
        assert!(red.violations[0].contains("turn-refused"), "{:?}", red.violations);
        let mut green = CensusVerdict::default();
        green.judge(3, &census, CellFacts { tiered_tables: 0, turn_refused: Some(0), ..bare() });
        assert!(green.violations.is_empty(), "{:?}", green.violations);
    }

    /// The MAINTAIN room's rule: one namespace paced is red by its token, none
    /// is green.
    #[test]
    fn a_paced_namespace_is_red() {
        let (census, serve_mark) = CapCensus::new(CellId(2));
        serve_mark.take();
        let mut red = CensusVerdict::default();
        red.judge(2, &census, CellFacts { maint_paced: 1, ..bare() });
        assert_eq!(red.violations.len(), 1, "{:?}", red.violations);
        assert!(red.violations[0].contains("maint-paced"), "{:?}", red.violations);
        let mut green = CensusVerdict::default();
        green.judge(2, &census, CellFacts { maint_paced: 0, ..bare() });
        assert!(green.violations.is_empty(), "{:?}", green.violations);
    }

    /// The gates' derived rules: each count planted at 1 is red, alone, by its own token; every
    /// count at 0 is green. The cold gate's token ends in the fabric gate's, so each red is read
    /// whole between the cell and the dash.
    #[test]
    fn a_gate_count_above_zero_is_red_by_its_token() {
        let (census, serve_mark) = CapCensus::new(CellId(2));
        serve_mark.take();
        let planted = [
            (CellFacts { gate_full: 1, ..bare() }, "gate-full"),
            (CellFacts { gate_stale: 1, ..bare() }, "gate-stale"),
            (CellFacts { cold_gate_full: 1, ..bare() }, "cold-gate-full"),
            (CellFacts { wait_nodes_full: 1, ..bare() }, "wait-nodes-full"),
        ];
        for (facts, token) in planted {
            let mut red = CensusVerdict::default();
            red.judge(2, &census, facts);
            assert_eq!(red.violations.len(), 1, "{token}: {:?}", red.violations);
            let named = format!("cell 2: {token} —");
            assert!(red.violations[0].contains(&named), "{token}: {:?}", red.violations);
        }
        let mut green = CensusVerdict::default();
        green.judge(2, &census, bare());
        assert!(green.violations.is_empty(), "{:?}", green.violations);
    }

    /// The marks' derived rule: a seal-marks row that merged at its cap and
    /// a hole-marks row that refused at its cap are each red by their own
    /// token; rows that never crossed are green.
    #[test]
    fn a_marks_row_that_crossed_is_red_by_its_token() {
        struct Mark;
        impl Lossy for Mark {}
        struct Later;
        impl Merge<Mark> for Later {
            fn merge(back: &mut Mark, new: Mark) {
                *back = new;
            }
        }
        let facts = CellFacts { tiered_tables: 1, ..bare() };
        let (census, serve_mark) = CapCensus::new(CellId(5));
        serve_mark.take();
        let epoch = Cap::entries::<16>("epoch-marks", CapFill::Serving);
        let _epoch = CappedDeque::<u64, Reserve>::new(epoch, &census).expect("built");
        let seal = inf_store::limits::SEAL_MARKS;
        let mut marks = CappedDeque::<Mark, Coalesce<Later>>::new(seal, &census).expect("built");
        let mut holes = CappedDeque::<u64, Reserve>::new(inf_store::limits::HOLE_MARKS, &census)
            .expect("built");
        let mut green = CensusVerdict::default();
        green.judge(5, &census, facts);
        assert!(green.violations.is_empty(), "{:?}", green.violations);
        for _ in 0..=seal.entries_max() {
            marks.push(Mark);
        }
        for _ in 0..=inf_store::limits::HOLE_MARKS_MAX {
            if let Reserved::Slot(slot) = holes.reserve() {
                slot.publish(1);
            }
        }
        let mut red = CensusVerdict::default();
        red.judge(5, &census, facts);
        assert_eq!(red.violations.len(), 2, "{:?}", red.violations);
        assert!(red.violations[0].contains("seal-marks-crossed"), "{:?}", red.violations);
        assert!(red.violations[1].contains("hole-marks-crossed"), "{:?}", red.violations);
    }

    /// The assembly rule's liveness: a census its cell's loop never marked
    /// is red, by the token only that violation carries.
    #[test]
    fn a_census_never_marked_is_red() {
        let (census, _serve_mark) = CapCensus::new(CellId(2));
        let mut verdict = CensusVerdict::default();
        verdict.judge(2, &census, bare());
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
        let _seal = CappedDeque::<u64, Reserve>::new(inf_store::limits::SEAL_MARKS, &census);
        let _hole = CappedDeque::<u64, Reserve>::new(inf_store::limits::HOLE_MARKS, &census);
        for _ in 0..20 {
            marks.push(Mark);
        }
        for _ in 0..3 {
            if let Reserved::Slot(slot) = pairs.reserve() {
                slot.publish(1);
            }
        }
        let mut verdict = CensusVerdict::default();
        verdict.judge(1, &census, CellFacts { tiered_tables: 1, turn_refused: None, ..bare() });
        assert_eq!(verdict.rows_checked, 4);
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
        let mut red = Violations::new();
        over_cap(1, &over, &mut red);
        assert_eq!(red.len(), 1, "red past the cap: {red:?}");
        assert!(red[0].contains("cap-high-water-over"), "{red:?}");
        let mut green = Violations::new();
        over_cap(1, &CapRow { high_water: 2, ..over }, &mut green);
        assert!(green.is_empty(), "at the cap is legal: {green:?}");
    }

    /// The crossing rule on a real `Retire` slab of 4: two slots retire one
    /// at a time — one entry published, placed at its last generation and
    /// taken, twice — so the high water is 1 and the capacity now 2 before
    /// the fill, which publishes up to the first `Full` and asserts nothing
    /// on the way. The verdict is the first assertion: a correct slab fills
    /// 2 (`(4, 2, 2, 1)`, green); one that answers `Full` a slot short fills
    /// 1 and crosses at a high water of 1 (`(4, 2, 1, 1)`), which only this
    /// rule reads. The row's values are asserted after it, the control.
    #[test]
    fn a_retire_slab_crossing_after_two_retirements_reads_its_capacity_now() {
        const SLAB: Cap = Cap::entries::<4>("retire-slab", CapFill::Serving);
        let (census, serve_mark) = CapCensus::new(CellId(4));
        let mut builder = CappedSlab::<u8, u32, Retire>::builder(SLAB, &census).expect("row");
        let mut slab = loop {
            match builder.step() {
                Built::More(next) => builder = next,
                Built::Live(slab) => break slab,
                Built::Refused(_, refused) => panic!("{refused}"),
            }
        };
        serve_mark.take();
        for value in 0..2 {
            let SlabReserved::Slot(slot) = slab.reserve() else { panic!("a free slot") };
            let mut id = slot.publish(value);
            assert!(matches!(slab.place_generation_last(&mut id), LookupMut::Live(_)));
            assert_eq!(slab.take(id), Taken::Value(value), "the slot retires");
        }
        while let SlabReserved::Slot(slot) = slab.reserve() {
            slot.publish(9);
        }
        let mut verdict = CensusVerdict::default();
        verdict.judge(4, &census, bare());
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        let row = census.row("retire-slab").expect("row");
        let shape = (row.entries_max, row.capacity_now, row.high_water, row.crossings);
        assert_eq!(shape, (4, 2, 2, 1));
        assert_eq!(verdict.rows_crossed, 1);
        census.mark_stopping();
    }

    /// The crossing rule on a real `CappedMap` of 4 filled while serving:
    /// fresh keys admitted and published up to the first `Full`, nothing
    /// asserted on the way. The verdict is the first assertion: a correct
    /// map fills 4 and crosses at its cap (`(4, 4, 4, 1)`, green); one that
    /// answers `Full` an entry short fills 3 and crosses at a high water of
    /// 3 (`(4, 4, 3, 1)`), which only this rule reads. A map retires no
    /// slot, so its capacity now is its cap. The row's values are asserted
    /// after the verdict, the control.
    #[test]
    fn a_map_crossing_while_serving_reads_its_full_cap() {
        const MAP: Cap = Cap::entries::<4>("crossing-map", CapFill::Serving);
        let (census, serve_mark) = CapCensus::new(CellId(6));
        let mut builder = CappedMap::<u64, u8>::builder(MAP, &census).expect("row");
        let mut map = loop {
            match builder.step() {
                MapBuilt::More(next) => builder = next,
                MapBuilt::Live(map) => break map,
                MapBuilt::Refused(_, refused) => panic!("{refused}"),
            }
        };
        serve_mark.take();
        let mut key = 0u64;
        while let Admission::Vacant(slot) = map.admit(key) {
            slot.publish(1);
            key += 1 << 40;
        }
        let mut verdict = CensusVerdict::default();
        verdict.judge(6, &census, bare());
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        let row = census.row("crossing-map").expect("row");
        let shape = (row.entries_max, row.capacity_now, row.high_water, row.crossings);
        assert_eq!(shape, (4, 4, 4, 1));
        assert_eq!(verdict.rows_crossed, 1);
        census.mark_stopping();
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
