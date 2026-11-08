//! Tiered namespace lifetime grants (ADR-0148 D2–D4): the node's directory
//! of grant positions, its generation, and every cell's terminal receipts.
//!
//! A tiered namespace holds one grant for its whole lifetime, which is one
//! position on every cell. The catalog writer alone writes the directory,
//! one `AtomicU64` a position: the namespace id in bits 0–31, the state in
//! bits 32–33 (1 Reserved, 2 Declared, 3 Retiring), the whole word zero
//! when Vacant. A cell acts on Declared (it may install the namespace's
//! owner) and on Retiring (it reports a receipt once nothing of that id is
//! left on it), never on Reserved or Vacant, so only those two stores need
//! an order: [`DirectoryWriter::publish`] makes them, then bumps the
//! generation with `Release`, and returns the [`DirectoryPublished`] the
//! catalog epoch's one store takes, so no epoch a cell can wait for is
//! published before the directory that goes with it. A cell's pass,
//! [`GrantDirectory::scan`], loads the generation with `Acquire` before it
//! reads a slot and runs only when the generation moved since its last pass:
//! a pass that sees a generation sees every store made before it.
//!
//! Each cell alone writes its own receipts, one `AtomicU32` a position (0 =
//! none): the id of the grant whose every runtime owner on that cell has
//! ended, stored with `Release` after that end. The writer returns a
//! Retiring position to Vacant only when every cell's receipt for it names
//! that exact id ([`ReceiptBoard::all_name`], `Acquire`). Namespace ids never
//! repeat, so a receipt left from an earlier grant of the position never
//! matches a later one, and a duplicate receipt changes nothing.
//!
//! The Loom models at the end of this file check both orderings on these
//! same types (`RUSTFLAGS="--cfg loom"`).

/// Atomics and `Arc`, swapped for Loom's model-checked versions under
/// `--cfg loom` (the pattern `issue` uses).
#[cfg(loom)]
mod sync {
    pub(super) use loom::sync::Arc;
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
}

#[cfg(not(loom))]
mod sync {
    pub(super) use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    pub(super) use std::sync::Arc;
}

use sync::{Arc, AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Grant positions: the node's lifetime limit. A Loom model runs on four,
/// since the orderings do not depend on the count and every slot is a
/// scheduling point.
#[cfg(not(loom))]
pub const POSITIONS: usize = crate::limits::TIERED_NAMESPACE_LIFETIMES_PER_NODE_MAX;
#[cfg(loom)]
pub const POSITIONS: usize = 4;

/// The generation's bump, the directory's one ordering: `Release`, after
/// every slot store of the publish.
#[cfg(not(inf_canary_grants_generation_relaxed))]
const GENERATION_BUMP: Ordering = Ordering::Release;
/// Canary: the bump without its ordering. The Loom scan model must fail.
#[cfg(inf_canary_grants_generation_relaxed)]
const GENERATION_BUMP: Ordering = Ordering::Relaxed;

const STATE_SHIFT: u32 = 32;
const STATE_RESERVED: u64 = 1;
const STATE_DECLARED: u64 = 2;
const STATE_RETIRING: u64 = 3;

/// A grant position, `index < POSITIONS`. Made only by this module: by the
/// writer's [`DirectoryWriter::positions`] and by a directory read, so a
/// cell holds a position only for a grant the directory named.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantPos(u8);

impl GrantPos {
    fn at(index: usize) -> GrantPos {
        debug_assert!(index < POSITIONS, "a grant position below the limit");
        // `POSITIONS <= 256` (the limit's const assertion), so it fits.
        GrantPos(u8::try_from(index).unwrap_or(u8::MAX))
    }

    /// The position's index, below [`POSITIONS`].
    #[must_use]
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// What the directory holds at one position.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrantSlot {
    /// No grant.
    Vacant,
    /// Granted to a `CREATE` whose catalog write has not returned: no cell
    /// acts on it.
    Reserved(u32),
    /// The catalog names namespace `id`: a cell may install its owner here.
    Declared(u32),
    /// The catalog no longer names `id` (a drop or a rollback, durable): a
    /// cell reports its receipt once nothing of `id` is left on it.
    Retiring(u32),
}

impl GrantSlot {
    fn encode(self) -> u64 {
        let (state, id) = match self {
            GrantSlot::Vacant => return 0,
            GrantSlot::Reserved(id) => (STATE_RESERVED, id),
            GrantSlot::Declared(id) => (STATE_DECLARED, id),
            GrantSlot::Retiring(id) => (STATE_RETIRING, id),
        };
        state << STATE_SHIFT | u64::from(id)
    }

    /// Total over every word: only [`GrantSlot::encode`] writes one.
    fn decode(word: u64) -> GrantSlot {
        let id = (word & u64::from(u32::MAX)) as u32;
        match word >> STATE_SHIFT {
            STATE_RESERVED => GrantSlot::Reserved(id),
            STATE_DECLARED => GrantSlot::Declared(id),
            STATE_RETIRING => GrantSlot::Retiring(id),
            _ => GrantSlot::Vacant,
        }
    }
}

/// The node's directory: one slot a position and the generation the
/// writer's publish bumps. Shared by the writer and every cell; written by
/// the one [`DirectoryWriter`].
#[derive(Debug)]
pub struct GrantDirectory {
    slots: [AtomicU64; POSITIONS],
    generation: AtomicU64,
}

/// Proof that the directory's stores of one catalog answer are published:
/// made only by [`DirectoryWriter::publish`], consumed by the catalog
/// epoch's store, so a cell that reads the epoch reads the directory too.
#[derive(Debug)]
#[must_use = "the catalog epoch's store takes it"]
pub struct DirectoryPublished(());

/// A cell's place in the directory's generations: the one its last pass
/// read.
#[derive(Debug, Default)]
pub struct DirectoryCursor {
    seen: u64,
}

impl GrantDirectory {
    /// A directory with every position Vacant, generation 0, and its one
    /// writer, made together so no second writer exists.
    #[must_use]
    pub fn new() -> (Arc<GrantDirectory>, DirectoryWriter) {
        let directory = Arc::new(GrantDirectory {
            slots: core::array::from_fn(|_| AtomicU64::new(0)),
            generation: AtomicU64::new(0),
        });
        let staged = [None; POSITIONS];
        let writer = DirectoryWriter { directory: Arc::clone(&directory), staged, staged_count: 0 };
        (directory, writer)
    }

    /// The slot at `pos` now (`Acquire`).
    #[must_use]
    pub fn slot(&self, pos: GrantPos) -> GrantSlot {
        GrantSlot::decode(self.slots[pos.index()].load(Ordering::Acquire))
    }

    /// The position Declared for namespace `id`, if any: at most
    /// [`POSITIONS`] loads, at DDL rate. A cell reads it in the step that
    /// installs `id`'s owner, after the catalog epoch that published it.
    #[must_use]
    pub fn find_declared(&self, id: u32) -> Option<GrantPos> {
        (0..POSITIONS).map(GrantPos::at).find(|&pos| self.slot(pos) == GrantSlot::Declared(id))
    }

    /// A cell's pass: when the generation moved since `cursor`'s last pass,
    /// records it and hands every position's slot to `visit`, at most
    /// [`POSITIONS`] loads; else one load and nothing. Returns whether the
    /// pass ran. The generation is loaded with `Acquire` before any slot.
    pub fn scan(
        &self,
        cursor: &mut DirectoryCursor,
        mut visit: impl FnMut(GrantPos, GrantSlot),
    ) -> bool {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == cursor.seen {
            return false;
        }
        cursor.seen = generation;
        for index in 0..POSITIONS {
            let word = self.slots[index].load(Ordering::Relaxed);
            visit(GrantPos::at(index), GrantSlot::decode(word));
        }
        true
    }
}

/// The catalog writer's half of the directory, one a node (not `Clone`).
/// Reserved and Vacant stores land at once, since no cell acts on either;
/// Declared and Retiring stores are staged and made by [`publish`], before
/// the generation's bump.
///
/// [`publish`]: DirectoryWriter::publish
#[derive(Debug)]
pub struct DirectoryWriter {
    directory: Arc<GrantDirectory>,
    staged: [Option<GrantSlot>; POSITIONS],
    staged_count: usize,
}

impl DirectoryWriter {
    /// The directory this writer writes.
    #[must_use]
    pub fn directory(&self) -> &Arc<GrantDirectory> {
        &self.directory
    }

    /// Every position, lowest first: the writer's book walks them. The
    /// iterator borrows nothing, so the writer may store while it walks.
    pub fn positions(&self) -> impl Iterator<Item = GrantPos> + use<> {
        (0..POSITIONS).map(GrantPos::at)
    }

    /// Stores `Reserved(id)` at `pos` now.
    pub fn reserve(&mut self, pos: GrantPos, id: u32) {
        self.store_now(pos, GrantSlot::Reserved(id));
    }

    /// Stores Vacant at `pos` now (a reclaimed Retiring grant).
    pub fn vacate(&mut self, pos: GrantPos) {
        self.store_now(pos, GrantSlot::Vacant);
    }

    /// Stages `Declared(id)` at `pos` for the next [`publish`](Self::publish).
    pub fn stage_declared(&mut self, pos: GrantPos, id: u32) {
        self.stage(pos, GrantSlot::Declared(id));
    }

    /// Stages `Retiring(id)` at `pos` for the next [`publish`](Self::publish).
    pub fn stage_retiring(&mut self, pos: GrantPos, id: u32) {
        self.stage(pos, GrantSlot::Retiring(id));
    }

    fn store_now(&mut self, pos: GrantPos, slot: GrantSlot) {
        debug_assert!(self.staged[pos.index()].is_none(), "no staged store is overwritten");
        self.directory.slots[pos.index()].store(slot.encode(), Ordering::Release);
    }

    fn stage(&mut self, pos: GrantPos, slot: GrantSlot) {
        let at = &mut self.staged[pos.index()];
        self.staged_count += usize::from(at.is_none());
        *at = Some(slot);
    }

    /// Makes every staged store, then, when there was one, bumps the
    /// generation with `Release`: a cell's pass that sees the new generation
    /// sees each store. At most [`POSITIONS`] stores and one bump.
    pub fn publish(&mut self) -> DirectoryPublished {
        if self.staged_count == 0 {
            return DirectoryPublished(());
        }
        // Canary `inf_canary_grants_generation_first`: the bump before the
        // stores, so a pass can record the new generation and read an old
        // slot.
        if cfg!(inf_canary_grants_generation_first) {
            self.directory.generation.fetch_add(1, GENERATION_BUMP);
        }
        for index in 0..POSITIONS {
            if let Some(slot) = self.staged[index].take() {
                self.directory.slots[index].store(slot.encode(), Ordering::Release);
            }
        }
        self.staged_count = 0;
        if !cfg!(inf_canary_grants_generation_first) {
            self.directory.generation.fetch_add(1, GENERATION_BUMP);
        }
        DirectoryPublished(())
    }
}

/// Every cell's terminal receipts: [`POSITIONS`] a cell.
#[derive(Debug)]
pub struct ReceiptBoard {
    receipts: Box<[AtomicU32]>,
    taken: Box<[AtomicBool]>,
}

/// One cell's writer half of the board, taken once a cell (not `Clone`).
#[derive(Debug)]
pub struct ReceiptWriter {
    board: Arc<ReceiptBoard>,
    cell: usize,
}

impl ReceiptBoard {
    /// A board of `cells` cells, every receipt 0.
    #[must_use]
    pub fn new(cells: u16) -> Arc<ReceiptBoard> {
        let cells = usize::from(cells);
        Arc::new(ReceiptBoard {
            receipts: (0..cells.saturating_mul(POSITIONS)).map(|_| AtomicU32::new(0)).collect(),
            taken: (0..cells).map(|_| AtomicBool::new(false)).collect(),
        })
    }

    /// The cells the board holds.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.taken.len()
    }

    /// Cell `cell`'s writer, the first time it is asked for; `None` after,
    /// or for a cell the board does not hold.
    #[must_use]
    pub fn writer(this: &Arc<Self>, cell: u16) -> Option<ReceiptWriter> {
        let cell = usize::from(cell);
        if this.taken.get(cell)?.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(ReceiptWriter { board: Arc::clone(this), cell })
    }

    fn at(&self, cell: usize, pos: GrantPos) -> &AtomicU32 {
        &self.receipts[cell * POSITIONS + pos.index()]
    }

    /// Whether every cell's receipt at `pos` names exactly `id` (`Acquire`
    /// each): each cell's every owner of `id` has ended. At most one load a
    /// cell.
    #[must_use]
    pub fn all_name(&self, pos: GrantPos, id: u32) -> bool {
        (0..self.cells()).all(|cell| {
            let receipt = self.at(cell, pos).load(Ordering::Acquire);
            // Canary `inf_canary_receipt_id_blind`: any receipt reclaims, so
            // one left from an earlier grant of the position returns a
            // grant whose owners have not ended.
            if cfg!(inf_canary_receipt_id_blind) { receipt != 0 } else { receipt == id }
        })
    }
}

impl ReceiptWriter {
    /// The cell this writer writes for.
    #[must_use]
    pub fn cell(&self) -> usize {
        self.cell
    }

    /// Reports that every owner of grant `id` at `pos` on this cell has
    /// ended (`Release`, after that end).
    pub fn publish(&self, pos: GrantPos, id: u32) {
        self.board.at(self.cell, pos).store(id, Ordering::Release);
    }

    /// This cell's receipt at `pos` (its own store).
    #[must_use]
    pub fn receipt(&self, pos: GrantPos) -> u32 {
        self.board.at(self.cell, pos).load(Ordering::Relaxed)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Every slot survives its word, and the zero word is Vacant.
    #[test]
    fn every_slot_round_trips_its_word() {
        for slot in [
            GrantSlot::Vacant,
            GrantSlot::Reserved(16),
            GrantSlot::Declared(u32::MAX - 1),
            GrantSlot::Retiring(17),
        ] {
            assert_eq!(GrantSlot::decode(slot.encode()), slot);
        }
        assert_eq!(GrantSlot::Vacant.encode(), 0);
    }

    /// Reserved and Vacant land at once with no generation; Declared and
    /// Retiring land only at the publish, which bumps the generation once,
    /// and a pass runs only on a generation it has not seen.
    #[test]
    fn a_publish_bumps_the_generation_once_after_its_stores() {
        let (directory, mut writer) = GrantDirectory::new();
        let pos = writer.positions().next().expect("position 0");
        writer.reserve(pos, 16);
        assert_eq!(directory.slot(pos), GrantSlot::Reserved(16));
        let mut cursor = DirectoryCursor::default();
        assert!(!directory.scan(&mut cursor, |_, _| {}), "a reservation bumps nothing");
        writer.stage_declared(pos, 16);
        assert_eq!(directory.slot(pos), GrantSlot::Reserved(16), "staged, not stored");
        let _published = writer.publish();
        let mut seen = Vec::new();
        assert!(directory.scan(&mut cursor, |p, slot| seen.push((p, slot))));
        assert_eq!(seen.len(), POSITIONS, "a pass reads every position");
        assert_eq!(seen[0], (pos, GrantSlot::Declared(16)));
        assert!(!directory.scan(&mut cursor, |_, _| {}), "the same generation: no pass");
        assert_eq!(directory.find_declared(16), Some(pos));
        assert_eq!(directory.find_declared(17), None);
        let _nothing = writer.publish();
        assert!(!directory.scan(&mut cursor, |_, _| {}), "an empty publish bumps nothing");
        writer.stage_retiring(pos, 16);
        let _published = writer.publish();
        assert!(directory.scan(&mut cursor, |_, _| {}));
        assert_eq!(directory.slot(pos), GrantSlot::Retiring(16));
        assert_eq!(directory.find_declared(16), None, "a Retiring grant installs nothing");
        writer.vacate(pos);
        assert_eq!(directory.slot(pos), GrantSlot::Vacant);
    }

    /// The reclaim reads exact ids: a receipt left from an earlier grant of
    /// the position never returns a later one, and every cell must name it.
    #[test]
    fn a_reclaim_needs_every_cell_to_name_the_exact_id() {
        let board = ReceiptBoard::new(2);
        let first = ReceiptBoard::writer(&board, 0).expect("cell 0's writer");
        let second = ReceiptBoard::writer(&board, 1).expect("cell 1's writer");
        assert!(ReceiptBoard::writer(&board, 0).is_none(), "one writer a cell");
        assert!(ReceiptBoard::writer(&board, 2).is_none(), "no cell 2");
        let (_directory, writer) = GrantDirectory::new();
        let pos = writer.positions().nth(3).expect("position 3");
        assert!(!board.all_name(pos, 16), "no receipt yet");
        first.publish(pos, 16);
        assert!(!board.all_name(pos, 16), "one cell of two");
        second.publish(pos, 16);
        assert!(board.all_name(pos, 16), "both cells name 16");
        first.publish(pos, 16);
        assert!(board.all_name(pos, 16), "a duplicate receipt changes nothing");
        assert!(!board.all_name(pos, 17), "an old receipt never returns a later grant");
        assert_eq!(first.receipt(pos), 16);
        assert_eq!(first.cell(), 0);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::thread;

    use super::*;

    /// Executions in which the cell's pass read the writer's generation.
    static PASSES_SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// The publish against a cell's pass (F9): a grant Declared and seen,
    /// then the writer stages it Retiring and publishes while the cell
    /// passes. A pass that records the new generation reads Retiring; one
    /// that misses it would record the generation and never pass again, so
    /// the cell's receipt would never be written.
    #[test]
    fn loom_a_pass_that_sees_the_generation_sees_the_retiring() {
        loom::model(|| {
            let (directory, mut writer) = GrantDirectory::new();
            let pos = writer.positions().nth(1).expect("position 1");
            writer.reserve(pos, 16);
            writer.stage_declared(pos, 16);
            let _declared = writer.publish();
            let mut cursor = DirectoryCursor::default();
            assert!(directory.scan(&mut cursor, |_, _| {}), "the Declared pass");
            let retire = thread::spawn(move || {
                writer.stage_retiring(pos, 16);
                let _published = writer.publish();
            });
            let mut seen = None;
            let passed = directory.scan(&mut cursor, |p, slot| {
                if p == pos {
                    seen = Some(slot);
                }
            });
            if passed {
                PASSES_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(
                    seen,
                    Some(GrantSlot::Retiring(16)),
                    "F9: a pass recorded the new generation and missed the Retiring"
                );
            }
            retire.join().expect("writer");
        });
        let passes = PASSES_SEEN.load(std::sync::atomic::Ordering::Relaxed);
        assert!(passes > 0, "VACUOUS: no pass read the writer's generation");
    }

    /// Executions in which the writer's reclaim saw every receipt.
    static RECLAIMS_SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A receipt against the reclaim: each cell ends its owner (a `Relaxed`
    /// effect, which carries no order of its own) before its receipt; a
    /// writer whose reclaim reads every receipt reads every end.
    #[test]
    fn loom_a_reclaim_that_reads_every_receipt_reads_every_end() {
        loom::model(|| {
            let board = ReceiptBoard::new(2);
            let (_directory, writer) = GrantDirectory::new();
            let pos = writer.positions().nth(2).expect("position 2");
            let ended = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
            let cells: Vec<_> = (0..2u16)
                .map(|cell| {
                    let receipts = ReceiptBoard::writer(&board, cell).expect("a cell's writer");
                    let ended = Arc::clone(&ended);
                    thread::spawn(move || {
                        ended[usize::from(cell)].store(1, Ordering::Relaxed);
                        receipts.publish(pos, 16);
                    })
                })
                .collect();
            if board.all_name(pos, 16) {
                RECLAIMS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for end in ended.iter() {
                    assert_eq!(end.load(Ordering::Relaxed), 1, "a reclaim before a cell's end");
                }
            }
            for cell in cells {
                cell.join().expect("cell");
            }
        });
        let reclaims = RECLAIMS_SEEN.load(std::sync::atomic::Ordering::Relaxed);
        assert!(reclaims > 0, "VACUOUS: no reclaim read both receipts");
    }

    /// Executions in which the cell's pass read the reused grant Retiring
    /// and wrote its receipt, and those in which its receipt stayed the old
    /// one while the writer's reclaim of the reused grant ran.
    static REUSE_RECEIPTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static REUSE_OLD_KEPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A stale observation across reuse: a cell that last saw the position
    /// Retiring(16) and published 16 meets the position reclaimed and
    /// granted to 17 while it passes; whatever its pass reads, the writer's
    /// reclaim of 17 never accepts the 16. Both outcomes of the pass are
    /// reached: one that read Retiring(17) and wrote its receipt, one that
    /// kept the old receipt under the reclaim.
    #[test]
    fn loom_an_old_receipt_never_reclaims_a_reused_position() {
        loom::model(|| {
            let board = ReceiptBoard::new(1);
            let receipts = ReceiptBoard::writer(&board, 0).expect("cell 0");
            let (directory, mut writer) = GrantDirectory::new();
            let pos = writer.positions().next().expect("position 0");
            receipts.publish(pos, 16);
            let reuse = thread::spawn(move || {
                writer.vacate(pos);
                writer.reserve(pos, 17);
                writer.stage_declared(pos, 17);
                let _published = writer.publish();
                writer.stage_retiring(pos, 17);
                let _published = writer.publish();
                writer
            });
            let mut cursor = DirectoryCursor::default();
            directory.scan(&mut cursor, |p, slot| {
                if p == pos
                    && let GrantSlot::Retiring(id) = slot
                {
                    receipts.publish(pos, id);
                }
            });
            let _writer = reuse.join().expect("writer");
            let reclaimed = board.all_name(pos, 17);
            let named = receipts.receipt(pos);
            let reached = if named == 17 { &REUSE_RECEIPTED } else { &REUSE_OLD_KEPT };
            reached.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(!reclaimed || named == 17, "the reclaim of 17 accepted receipt {named}");
        });
        let receipted = REUSE_RECEIPTED.load(std::sync::atomic::Ordering::Relaxed);
        let kept = REUSE_OLD_KEPT.load(std::sync::atomic::Ordering::Relaxed);
        assert!(receipted > 0, "VACUOUS: no pass read the reused grant Retiring");
        assert!(kept > 0, "VACUOUS: no reclaim of the reused grant met the old receipt");
    }
}
