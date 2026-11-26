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
//! The node's retirement board (ADR-0156 D3, ADR-0157 D4) sits beside
//! them: the one bell every cell rings after a turn's publish of
//! retirement jobs, which wakes the control thread's worker, and each
//! cell's `closing` word, which the cell stores at its stop before it
//! releases its endpoints and the worker reads before it releases its
//! lane. [`Bell::worker_turn`] is the worker's wait decision, written once
//! here for the control thread, the simulator and the Loom model.
//!
//! Every payload is reserved fallibly, by checked arithmetic, before any
//! cell exists (ADR-0148 D4): each `*Payload::try_new` returns the term it
//! could not reserve, and the `Arc` headers that share the payloads are
//! made afterwards, the reservation's only infallible allocations.
//!
//! The Loom models at the end of this file check the orderings on these
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

/// A payload of the node's grants or its retirement board that the boot
/// could not reserve (ADR-0148 D4): which one, and the bytes it asked;
/// `None` when the size does not fit a `usize`. Nothing of it was made.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PayloadRefused {
    pub payload: GrantsPayload,
    pub bytes: Option<usize>,
}

/// The payloads [`PayloadRefused`] names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrantsPayload {
    /// The directory's slots, [`POSITIONS`] words a node.
    Directory,
    /// The receipts, [`POSITIONS`] a cell.
    Receipts,
    /// The receipt writers' one-shot flags, one a cell.
    ReceiptsTaken,
    /// The cells' `closing` words, one a cell.
    Closing,
}

/// `len` values made by `make`, in one allocation reserved exactly and
/// fallibly first: the vector is filled within its reservation and never
/// grows, so a refusal makes nothing and a success allocates once.
fn reserved<T>(
    len: usize,
    payload: GrantsPayload,
    make: impl FnMut() -> T,
) -> Result<Vec<T>, PayloadRefused> {
    let refused = PayloadRefused { payload, bytes: len.checked_mul(size_of::<T>()) };
    if refused.bytes.is_none() {
        return Err(refused);
    }
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|_| refused)?;
    values.extend(core::iter::repeat_with(make).take(len));
    debug_assert_eq!(values.len(), values.capacity(), "filled within the reservation");
    Ok(values)
}

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
    /// [`POSITIONS`] words, reserved at boot, never grown.
    slots: Vec<AtomicU64>,
    generation: AtomicU64,
}

/// The directory's slots, reserved before any cell: every position Vacant.
#[derive(Debug)]
pub struct DirectoryPayload {
    slots: Vec<AtomicU64>,
}

impl DirectoryPayload {
    /// Reserves the [`POSITIONS`] slots, one allocation.
    ///
    /// # Errors
    /// [`PayloadRefused`] naming [`GrantsPayload::Directory`].
    pub fn try_new() -> Result<DirectoryPayload, PayloadRefused> {
        let slots = reserved(POSITIONS, GrantsPayload::Directory, || AtomicU64::new(0))?;
        Ok(DirectoryPayload { slots })
    }
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
    /// The directory over its reserved `payload`, every position Vacant,
    /// generation 0, and its one writer, made together so no second writer
    /// exists. The `Arc` header is its one allocation, made after the
    /// payload's (class F at boot: a refusal is the allocator's abort).
    #[must_use]
    pub fn new(payload: DirectoryPayload) -> (Arc<GrantDirectory>, DirectoryWriter) {
        let DirectoryPayload { slots } = payload;
        let directory = Arc::new(GrantDirectory { slots, generation: AtomicU64::new(0) });
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
    /// `cells × POSITIONS` receipts, reserved at boot, never grown.
    receipts: Vec<AtomicU32>,
    /// One flag a cell: its writer was taken.
    taken: Vec<AtomicBool>,
}

/// The receipt board's two payloads, reserved before any cell: every
/// receipt 0, no writer taken.
#[derive(Debug)]
pub struct ReceiptPayload {
    receipts: Vec<AtomicU32>,
    taken: Vec<AtomicBool>,
}

impl ReceiptPayload {
    /// Reserves `cells × POSITIONS` receipts, then `cells` flags: two
    /// allocations, the product checked.
    ///
    /// # Errors
    /// [`PayloadRefused`] naming the payload refused; nothing is kept.
    pub fn try_new(cells: u16) -> Result<ReceiptPayload, PayloadRefused> {
        let cells = usize::from(cells);
        let Some(len) = cells.checked_mul(POSITIONS) else {
            return Err(PayloadRefused { payload: GrantsPayload::Receipts, bytes: None });
        };
        let receipts = reserved(len, GrantsPayload::Receipts, || AtomicU32::new(0))?;
        let taken = reserved(cells, GrantsPayload::ReceiptsTaken, || AtomicBool::new(false))?;
        Ok(ReceiptPayload { receipts, taken })
    }
}

/// One cell's writer half of the board, taken once a cell (not `Clone`).
#[derive(Debug)]
pub struct ReceiptWriter {
    board: Arc<ReceiptBoard>,
    cell: usize,
}

impl ReceiptBoard {
    /// The board over its reserved `payload`. The `Arc` header is its one
    /// allocation, made after the payloads' (class F at boot).
    #[must_use]
    pub fn new(payload: ReceiptPayload) -> Arc<ReceiptBoard> {
        let ReceiptPayload { receipts, taken } = payload;
        Arc::new(ReceiptBoard { receipts, taken })
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

/// The node's retirement board (ADR-0156 D3): the one bell and each cell's
/// `closing` word. Shared by every cell and the control thread.
#[derive(Debug)]
pub struct RetireBoard {
    bell: Bell,
    /// One word a cell, reserved at boot, never grown: set once, at the
    /// cell's stop, before it releases its two endpoints.
    closing: Vec<AtomicBool>,
}

/// The cells' `closing` words, reserved before any cell: none set.
#[derive(Debug)]
pub struct ClosingPayload {
    closing: Vec<AtomicBool>,
}

impl ClosingPayload {
    /// Reserves one word a cell, one allocation.
    ///
    /// # Errors
    /// [`PayloadRefused`] naming [`GrantsPayload::Closing`].
    pub fn try_new(cells: u16) -> Result<ClosingPayload, PayloadRefused> {
        let closing =
            reserved(usize::from(cells), GrantsPayload::Closing, || AtomicBool::new(false))?;
        Ok(ClosingPayload { closing })
    }
}

impl RetireBoard {
    /// The board over its reserved `payload`, the bell clear. The `Arc`
    /// header is its one allocation, made after the payload's (class F at
    /// boot).
    #[must_use]
    pub fn new(payload: ClosingPayload) -> Arc<RetireBoard> {
        let ClosingPayload { closing } = payload;
        Arc::new(RetireBoard { bell: Bell { rung: AtomicBool::new(false) }, closing })
    }

    /// The node's one bell.
    #[must_use]
    pub fn bell(&self) -> &Bell {
        &self.bell
    }

    /// The cells the board holds.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.closing.len()
    }

    /// Cell `cell` is closing (`Release`): stored after every one of its
    /// job positions is Free and before it releases its two endpoints, so
    /// a worker that reads it reads every consumption before it.
    pub fn close(&self, cell: usize) {
        self.closing[cell].store(true, Ordering::Release);
    }

    /// Whether cell `cell` is closing (`Acquire`): the worker then
    /// releases that cell's lane.
    #[must_use]
    pub fn closing(&self, cell: usize) -> bool {
        self.closing[cell].load(Ordering::Acquire)
    }
}

/// The wake the bell's ring and a control post send: the control thread's
/// one-slot wake channel in production, whose full slot means a wake is
/// already pending. It never blocks and never fails the caller.
pub trait BellWake {
    /// Sends the one wake unit if the slot is empty; else nothing.
    fn wake(&self);
}

/// The node's one bell (ADR-0156 D3): every cell rings it after a turn that
/// published retirement jobs; the worker clears it once an iteration, so
/// one swap there reads every cell's publish since its last clear, and at
/// most one wake is pending whatever the cell count.
#[derive(Debug)]
pub struct Bell {
    rung: AtomicBool,
}

/// What [`Bell::worker_turn`] took: the message, if any, and whether the
/// bell was set at the clear (a publish since the last turn's).
#[derive(Debug, PartialEq, Eq)]
pub struct WorkerTurn<M> {
    pub message: Option<M>,
    pub rung: bool,
    /// The turn waited on the wake.
    pub waited: bool,
}

impl Bell {
    /// After a cell's turn published at least one job (`Release`, the
    /// ring's own publish): sets the bell, and on its clear-to-set edge
    /// sends `wake`. A ring that finds the bell set sends nothing: the
    /// worker has not cleared it since an earlier ring, which sent the
    /// wake or found one pending.
    pub fn ring(&self, wake: &impl BellWake) {
        // Canary `inf_canary_retire_bell_silent`: a turn's publish rings no
        // bell, so a worker that found nothing waits past it.
        if cfg!(inf_canary_retire_bell_silent) {
            return;
        }
        if !self.rung.swap(true, Ordering::AcqRel) {
            wake.wake();
        }
    }

    /// The worker's turn (ADR-0156 D3): at most one message by
    /// `try_recv`; only when `circuit_empty` (a whole circuit of empty
    /// visits since the last job and the last set bell) and that found
    /// none, it waits on the wake, then tries once more; then it clears
    /// the bell. A request published before that clear is visited by the
    /// pass that follows: its publish precedes its ring, which the clear
    /// reads. One published after it finds the bell clear and sends the
    /// wake, or finds it set by a ring since the clear, which sent it or
    /// found one pending; a message sent after the empty `try_recv` sends
    /// the wake after its send ([`wake_posted`]); and only this wait
    /// consumes the wake, so the wait returns.
    pub fn worker_turn<M>(
        &self,
        circuit_empty: bool,
        mut try_recv: impl FnMut() -> Option<M>,
        wait: impl FnOnce(),
    ) -> WorkerTurn<M> {
        let mut message = try_recv();
        let mut waited = false;
        if message.is_none() && circuit_empty {
            wait();
            waited = true;
            message = try_recv();
        }
        let rung = self.rung.swap(false, Ordering::AcqRel);
        WorkerTurn { message, rung, waited }
    }
}

/// After a control message's send: sends the worker's wake, so a worker
/// that found the channel empty and waits returns to take it.
pub fn wake_posted(wake: &impl BellWake) {
    // Canary `inf_canary_post_rings_no_wake`: a post sends without the
    // wake, so a waiting worker sleeps with the message queued.
    if cfg!(inf_canary_post_rings_no_wake) {
        return;
    }
    wake.wake();
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
        let (directory, mut writer) =
            GrantDirectory::new(DirectoryPayload::try_new().expect("the directory"));
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
        let board = ReceiptBoard::new(ReceiptPayload::try_new(2).expect("the receipts"));
        let first = ReceiptBoard::writer(&board, 0).expect("cell 0's writer");
        let second = ReceiptBoard::writer(&board, 1).expect("cell 1's writer");
        assert!(ReceiptBoard::writer(&board, 0).is_none(), "one writer a cell");
        assert!(ReceiptBoard::writer(&board, 2).is_none(), "no cell 2");
        let (_directory, writer) =
            GrantDirectory::new(DirectoryPayload::try_new().expect("the directory"));
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

    /// A payload is one exact allocation filled within it, and a size that
    /// does not fit a `usize` is refused with no allocation.
    #[test]
    fn a_payload_is_reserved_exactly_or_refused_whole() {
        let slots = reserved(POSITIONS, GrantsPayload::Directory, || AtomicU64::new(0));
        let slots = slots.expect("64 words");
        assert_eq!((slots.len(), slots.capacity()), (POSITIONS, POSITIONS));
        let refused = reserved(usize::MAX, GrantsPayload::Receipts, || AtomicU32::new(0));
        let refused = refused.expect_err("a size past usize");
        assert_eq!(refused, PayloadRefused { payload: GrantsPayload::Receipts, bytes: None });
        let receipts = ReceiptPayload::try_new(3).expect("three cells");
        assert_eq!((receipts.receipts.len(), receipts.taken.len()), (3 * POSITIONS, 3));
        let board = ReceiptBoard::new(receipts);
        assert_eq!(board.cells(), 3);
    }

    /// A wake that counts its sends.
    #[derive(Default)]
    struct Wakes(core::cell::Cell<u32>);

    impl BellWake for Wakes {
        fn wake(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// The bell sends a wake on its clear-to-set edge only, and a post
    /// sends one every time; each cell's `closing` is its own.
    #[test]
    fn the_bell_wakes_on_its_edge_and_a_post_always() {
        let board = RetireBoard::new(ClosingPayload::try_new(2).expect("two cells"));
        let wakes = Wakes::default();
        board.bell().ring(&wakes);
        board.bell().ring(&wakes);
        assert_eq!(wakes.0.get(), 1, "a ring on a set bell sends nothing");
        let turn = board.bell().worker_turn(false, || None::<u8>, || unreachable!("no wait"));
        assert_eq!(turn, WorkerTurn { message: None, rung: true, waited: false });
        board.bell().ring(&wakes);
        assert_eq!(wakes.0.get(), 2, "cleared, the next ring sends again");
        wake_posted(&wakes);
        wake_posted(&wakes);
        assert_eq!(wakes.0.get(), 4, "a post sends a wake every time");
        assert!(!board.closing(1));
        board.close(1);
        assert!(board.closing(1) && !board.closing(0), "one cell's word");
        assert_eq!(board.cells(), 2);
    }

    /// The turn waits only when its circuit was empty and no message was
    /// queued, then tries once more; a message found first never waits.
    #[test]
    fn a_turn_waits_only_on_an_empty_circuit_with_nothing_queued() {
        let board = RetireBoard::new(ClosingPayload::try_new(1).expect("one cell"));
        let bell = board.bell();
        let mut queue = vec![7u8];
        let turn = bell.worker_turn(true, || queue.pop(), || unreachable!("a message queued"));
        assert_eq!(turn, WorkerTurn { message: Some(7), rung: false, waited: false });
        let turn = bell.worker_turn(false, || None::<u8>, || unreachable!("a job may wait"));
        assert!(!turn.waited, "a circuit with a job never waits");
        let queue = core::cell::RefCell::new(Vec::new());
        let waits = core::cell::Cell::new(0);
        let turn = bell.worker_turn(
            true,
            || queue.borrow_mut().pop(),
            || {
                waits.set(waits.get() + 1);
                queue.borrow_mut().push(9);
            },
        );
        assert_eq!(waits.get(), 1);
        assert_eq!(turn, WorkerTurn { message: Some(9), rung: false, waited: true });
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
            let (directory, mut writer) =
                GrantDirectory::new(DirectoryPayload::try_new().expect("the directory"));
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
            let board = ReceiptBoard::new(ReceiptPayload::try_new(2).expect("the receipts"));
            let (_directory, writer) =
                GrantDirectory::new(DirectoryPayload::try_new().expect("the directory"));
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
            let board = ReceiptBoard::new(ReceiptPayload::try_new(1).expect("the receipts"));
            let receipts = ReceiptBoard::writer(&board, 0).expect("cell 0");
            let (directory, mut writer) =
                GrantDirectory::new(DirectoryPayload::try_new().expect("the directory"));
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

    /// The control thread's one-slot wake channel as the model sees it: a
    /// slot flag. A send to a full slot changes nothing (`try_send`'s
    /// `Full`); only the wait empties it. Every access to a word several
    /// threads write is an RMW: Loom 0.7 lets a later RMW miss a plain store
    /// to the same word that happens before it.
    struct ModelWake {
        slot: AtomicBool,
    }

    impl BellWake for ModelWake {
        fn wake(&self) {
            self.slot.swap(true, Ordering::AcqRel);
        }
    }

    /// The model's cells: two, each one lane of the worker's circuit.
    const CELLS: usize = 2;

    /// The threads beside the worker: the cells and the poster.
    const SENDERS: usize = CELLS + 1;

    /// Executions in which the worker waited on the wake.
    static WORKER_WAITED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// The node's one bell (F18): two cells each publish a job (`Release`,
    /// the ring's own publish) and ring; a poster sends one message on a
    /// one-message channel and wakes; the worker runs its turns with
    /// `idle`, its passes visiting the lanes from a cursor, until it has
    /// taken both jobs and the message. Its wait returns when the wake's
    /// slot is set; one that finds every sender finished and the slot
    /// empty would sleep forever with a job or the message unconsumed (it
    /// waits only while it has not taken all of them), which the model
    /// asserts against: the planted silent ring and the planted post
    /// without its wake each reach one.
    #[test]
    fn loom_the_worker_never_waits_with_a_job_published_or_a_message_queued() {
        loom::model(|| {
            let board = RetireBoard::new(ClosingPayload::try_new(2).expect("two cells"));
            let wake = Arc::new(ModelWake { slot: AtomicBool::new(false) });
            let published = Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
            let channel = Arc::new(AtomicU32::new(0));
            let finished: Arc<[AtomicBool; SENDERS]> =
                Arc::new(core::array::from_fn(|_| AtomicBool::new(false)));
            let cells: Vec<_> = (0..CELLS)
                .map(|cell| {
                    let (board, wake) = (Arc::clone(&board), Arc::clone(&wake));
                    let (published, finished) = (Arc::clone(&published), Arc::clone(&finished));
                    thread::spawn(move || {
                        published[cell].store(true, Ordering::Release);
                        board.bell().ring(&*wake);
                        finished[cell].store(true, Ordering::Release);
                    })
                })
                .collect();
            let poster = {
                let (channel, wake) = (Arc::clone(&channel), Arc::clone(&wake));
                let finished = Arc::clone(&finished);
                thread::spawn(move || {
                    channel.swap(1, Ordering::AcqRel);
                    wake_posted(&*wake);
                    finished[CELLS].store(true, Ordering::Release);
                })
            };
            // The wait: returns once the slot is full, emptying it. With every
            // sender finished, no wake can come: one last look at the slot,
            // and an empty one is a worker asleep with work unconsumed.
            let wait = || {
                loop {
                    if wake.slot.swap(false, Ordering::AcqRel) {
                        return;
                    }
                    if finished.iter().all(|done| done.load(Ordering::Acquire)) {
                        assert!(
                            wake.slot.swap(false, Ordering::AcqRel),
                            "LOST_WAKE: the worker waits with every sender done and its slot \
                             empty"
                        );
                        return;
                    }
                    thread::yield_now();
                }
            };
            let (mut taken, mut handled, mut waited) = ([false; CELLS], false, false);
            let (mut idle, mut cursor) = (0usize, 0usize);
            for _ in 0..16 {
                let turn = board.bell().worker_turn(
                    idle >= CELLS,
                    || (channel.swap(0, Ordering::AcqRel) != 0).then_some(()),
                    wait,
                );
                waited |= turn.waited;
                handled |= turn.message.is_some();
                if turn.rung {
                    idle = 0;
                }
                while idle < CELLS {
                    let lane = cursor;
                    cursor = (cursor + 1) % CELLS;
                    if !taken[lane] && published[lane].load(Ordering::Acquire) {
                        taken[lane] = true;
                        idle = 0;
                    } else {
                        idle += 1;
                    }
                }
                if taken == [true; CELLS] && handled {
                    break;
                }
            }
            assert!(handled, "the message was handled");
            assert_eq!(taken, [true; CELLS], "every published job was taken");
            if waited {
                WORKER_WAITED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            for cell in cells {
                cell.join().expect("cell");
            }
            poster.join().expect("poster");
        });
        let waits = WORKER_WAITED.load(std::sync::atomic::Ordering::Relaxed);
        assert!(waits > 0, "VACUOUS: no execution's worker waited on the wake");
    }
}
