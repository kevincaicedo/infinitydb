//! The tier round's plan (ADR-0155 D1–D3, ADR-0167 A1): one selected span
//! of whole records, at most one capacity rotation before it and at most
//! one trailing gap after it, or a gap alone. Every write position of the
//! round is parsed into a [`FileOffset`] here, the windows the round takes
//! are reserved here, and the next file is created here — the parse and
//! the creation are the round plan's two fallible steps, in that order, both
//! before any writer effect, so a refused plan leaves the pipeline as it
//! was. Staging a plan ([`TierFlush::stage_round`]) cannot fail: it copies
//! and checksums the span into the reserved windows and freezes the ops.

use std::num::NonZeroU64;
use std::path::PathBuf;

use inf_foundation::bounded::Reserved;
use inf_foundation::{FileOffset, LogicalAddr};

use super::{PendingSeal, TierDrive, TierFlush, TierFlushError};
use crate::fs::{SegmentFile, SegmentFs};
use crate::tier::{
    PlannedWrites, Reservation, RoundEffect, SealReason, TIER_BATCH_FRAMES, TIER_FOOTER_BYTES,
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TIER_HEADER_BYTES, TierWriter, WindowNeeds, tier_file_name,
    tier_frame_offset,
};

/// What the store selected at its flush cursor (ADR-0155 D1): whole
/// records, then the ring-top gap that begins where they end if one does;
/// or the gap that begins at the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The records `[addr, addr + len)`; `gap_end` is the end of the
    /// sealed-dead gap starting at `addr + len`, when the span ends at one.
    Span { addr: LogicalAddr, len: NonZeroU64, gap_end: Option<u64> },
    /// A sealed-dead gap at the cursor, ending at `end`.
    Gap { end: u64 },
}

/// The shape of a round (ADR-0155 D1): a nonempty span with at most one
/// rotation before it and one gap after it, or a gap alone. An empty
/// plan, or a rotation with no span, is not representable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PlanShape {
    Span { rotate: bool, addr: LogicalAddr, len: NonZeroU64, gap_end: Option<u64> },
    Gap { end: u64 },
}

/// A file the plan created for its span: the id it consumed, the file and
/// the two directory handles its dirent barriers target.
struct CreatedFile<F: SegmentFs> {
    id: u32,
    path: PathBuf,
    file: F::File,
    holds: [F::File; 2],
}

/// A round's admitted plan: its shape, the file it created, and every
/// write position it parsed, in the order the stage pushes the writes.
/// Made by [`TierFlush::plan_round`] alone, consumed by
/// [`TierFlush::stage_round`].
#[must_use = "a plan holds a reservation and may hold a created file: stage it"]
pub struct RoundPlan<F: SegmentFs> {
    shape: PlanShape,
    created: Option<CreatedFile<F>>,
    writes: PlannedWrites,
}

/// The round plan's answer (ADR-0155 D1/D2): a round to stage, nothing at the
/// cursor, or a refusal that changed nothing.
#[must_use = "a refused plan carries the reason; a round must be staged"]
#[allow(
    clippy::large_enum_variant,
    reason = "a plan moves by value once per round; boxing it would allocate per round, which \
              the round's no-allocation rule forbids"
)]
pub enum Planned<F: SegmentFs> {
    Round(RoundPlan<F>),
    Nothing,
    Refused(TierFlushError),
}

impl<F: SegmentFs> RoundPlan<F> {
    /// The file and base the span lands in — the round plan's created file, else
    /// the active one — for the store's one filing of the span (`None` for a
    /// gap-only plan).
    #[must_use]
    pub fn span_file(&self, flush: &TierFlush<F>) -> Option<(u32, LogicalAddr)> {
        let PlanShape::Span { addr, .. } = self.shape else { return None };
        match &self.created {
            Some(created) => Some((created.id, addr)),
            None => flush.writer.as_ref().map(|w| (flush.active_id, w.base())),
        }
    }

    /// The span's record bytes (0 for a gap-only plan).
    #[must_use]
    pub fn span_bytes(&self) -> u64 {
        match self.shape {
            PlanShape::Span { len, .. } => len.get(),
            PlanShape::Gap { .. } => 0,
        }
    }
}

/// The round a plan stages, counted from the writer's state and the
/// selection alone: its write positions in stage order, the windows they
/// take and the ops and effects around them.
struct Layout {
    positions: [u64; crate::limits::TIER_ROUND_WRITES_MAX as usize],
    writes: usize,
    needs: WindowNeeds,
}

impl Layout {
    fn new() -> Layout {
        Layout {
            positions: [0; crate::limits::TIER_ROUND_WRITES_MAX as usize],
            writes: 0,
            needs: WindowNeeds::default(),
        }
    }

    fn write(&mut self, position: u64, batch: bool) {
        if let Some(slot) = self.positions.get_mut(self.writes) {
            *slot = position;
        }
        self.writes += 1;
        if batch {
            self.needs.batch += 1;
        } else {
            self.needs.single += 1;
        }
    }

    /// A writer at `data_len` seals: its partial tail frame, then its
    /// footer.
    fn seal(&mut self, data_len: u64) {
        self.tail(data_len);
        let frames = data_len.div_ceil(TIER_FRAME_DATA as u64);
        self.write(TIER_HEADER_BYTES as u64 + frames * TIER_FRAME_BYTES as u64, false);
    }

    /// The partial tail frame of a writer at `data_len`, if it has one.
    fn tail(&mut self, data_len: u64) {
        if !data_len.is_multiple_of(TIER_FRAME_DATA as u64) {
            self.write(tier_frame_offset(data_len / TIER_FRAME_DATA as u64), false);
        }
    }

    /// A span appended from `from` to `to` data bytes: one batch write per
    /// run of up to `TIER_BATCH_FRAMES` frames the span completes.
    fn span(&mut self, from: u64, to: u64) {
        let data = TIER_FRAME_DATA as u64;
        let (first, end) = (from / data, to / data);
        let mut frame = first;
        while frame < end {
            self.write(tier_frame_offset(frame), true);
            frame += TIER_BATCH_FRAMES as u64;
        }
    }
}

impl<F: SegmentFs> TierFlush<F> {
    /// Plans one reactor-drive round for `selection` (ADR-0155 D1–D3): at
    /// most one capacity rotation, the span in one file, at most one
    /// trailing gap. Parses every write position (ADR-0167 A1), reserves
    /// the windows the round takes, then creates the next file if the span
    /// needs one, its id checked first; any refusal releases what the plan
    /// took and changes nothing else.
    ///
    /// # Panics
    /// Panics on the seam drive, with a round staged, or with a plan's
    /// reservation still held: one round per pipeline, planned only when
    /// the last one finished.
    pub fn plan_round(&mut self, selection: Selection) -> Planned<F> {
        self.plan_round_with(selection, |_, _, position| position)
    }

    /// [`plan_round`](Self::plan_round) with each laid-out write position
    /// read through `position_of(index, writes, position)` before its
    /// parse: the production plan passes it through; a test plants an
    /// unaddressable position there, in the round plan, where ADR-0167 A1 parses.
    pub(crate) fn plan_round_with(
        &mut self,
        selection: Selection,
        position_of: impl Fn(usize, usize, u64) -> u64,
    ) -> Planned<F> {
        assert_eq!(self.drive, TierDrive::Reactor, "a tier round planned on the seam drive");
        assert!(!self.round.is_staged(), "a tier round planned over a staged round");
        let shape = self.shape_of(selection);
        let layout = self.layout_of(shape);
        if layout.writes > layout.positions.len() {
            return Planned::Refused(self.span_over_pool(shape));
        }
        let mut writes = PlannedWrites::new();
        for (index, &position) in layout.positions.iter().take(layout.writes).enumerate() {
            match FileOffset::new(position_of(index, layout.writes, position)) {
                Ok(offset) => writes.push(offset),
                Err(refused) => {
                    return Planned::Refused(TierFlushError::Unaddressable {
                        path: self.config.shard_dir.join("cold"),
                        offset_bytes: refused.offset_bytes(),
                    });
                }
            }
        }
        let mut needs = layout.needs;
        // A planted canary: the plan reserves one batch window fewer than
        // its span takes (the round envelope test must go red).
        if cfg!(inf_canary_tier_plan_window_short) {
            needs.batch = needs.batch.saturating_sub(1);
        }
        match self.pool.reserve(needs) {
            Reservation::Reserved => {}
            Reservation::OverPool => return Planned::Refused(self.span_over_pool(shape)),
        }
        let created = match shape {
            PlanShape::Span { rotate, .. } if rotate || self.writer.is_none() => {
                match self.create_planned() {
                    Ok(created) => Some(created),
                    Err(err) => {
                        self.pool.release_reservation();
                        return Planned::Refused(err);
                    }
                }
            }
            PlanShape::Span { .. } | PlanShape::Gap { .. } => None,
        };
        Planned::Round(RoundPlan { shape, created, writes })
    }

    /// The refusal of a span the pool cannot hold.
    fn span_over_pool(&self, shape: PlanShape) -> TierFlushError {
        TierFlushError::SpanOverPool {
            path: self.config.shard_dir.join("cold"),
            span_bytes: shape_span_bytes(shape),
            span_bytes_max: self.pool.span_bytes_max(),
        }
    }

    /// The round plan's shape: a span rotates the active file when it holds data
    /// and the span would take it past the file target (ADR-0056 D2's
    /// early seal, one check per span: the span stays in one file).
    /// Whether it rotates or not, a span continues the active file's
    /// logical stream: it starts at that writer's cursor, which a rotation
    /// makes the new file's base.
    fn shape_of(&self, selection: Selection) -> PlanShape {
        match selection {
            Selection::Span { addr, len, gap_end } => {
                let capacity = self.config.file_capacity;
                let rotate = self.writer.as_ref().is_some_and(|w| {
                    w.data_len() > 0 && w.data_len().saturating_add(len.get()) > capacity
                });
                if let Some(w) = self.writer.as_ref() {
                    let cursor = w.base().to_raw() + w.data_len();
                    assert_eq!(addr.to_raw(), cursor, "a tier span starts at the write cursor");
                }
                PlanShape::Span { rotate, addr, len, gap_end }
            }
            Selection::Gap { end } => PlanShape::Gap { end },
        }
    }

    /// The writes the stage will push, in its order: the rotated or
    /// gap-sealed file's tail and footer, the new file's header, the
    /// span's batches, then its tail (a sync) or its tail and footer (a
    /// trailing gap's seal).
    fn layout_of(&self, shape: PlanShape) -> Layout {
        let mut layout = Layout::new();
        let held = self.writer.as_ref().map(TierWriter::data_len);
        match shape {
            PlanShape::Gap { .. } => {
                if let Some(data_len) = held {
                    layout.seal(data_len);
                }
            }
            PlanShape::Span { rotate, len, gap_end, .. } => {
                let from = match held {
                    Some(data_len) if rotate => {
                        layout.seal(data_len);
                        layout.write(0, false);
                        0
                    }
                    Some(data_len) => data_len,
                    None => {
                        layout.write(0, false);
                        0
                    }
                };
                let to = from + len.get();
                layout.span(from, to);
                if gap_end.is_some() {
                    layout.seal(to);
                } else {
                    layout.tail(to);
                }
            }
        }
        layout
    }

    /// Creates the next tier file for a plan: the id first (no id is ever
    /// reused within a namespace lifetime, so the last one is refused before
    /// any effect), then the cold directory, its two directory handles and
    /// the file. Consumes the id only when the file exists.
    fn create_planned(&mut self) -> Result<CreatedFile<F>, TierFlushError> {
        let cold = self.config.shard_dir.join("cold");
        let id = self.next_id;
        let Some(next) = id.checked_add(1) else {
            return Err(TierFlushError::FileIdsExhausted { path: cold });
        };
        self.fs
            .create_dir_all(&cold)
            .map_err(|source| TierFlushError::Io { path: cold.clone(), source })?;
        let open = |dir: PathBuf| {
            if inf_foundation::fault::fire(crate::fault::TIER_DIR_OPEN_FAIL) {
                return Err(TierFlushError::Io {
                    path: dir,
                    source: crate::fault::injected(crate::fault::TIER_DIR_OPEN_FAIL),
                });
            }
            self.fs.open_dir(&dir).map_err(|source| TierFlushError::Io { path: dir, source })
        };
        let holds = [open(self.config.shard_dir.clone())?, open(cold.clone())?];
        let path = cold.join(tier_file_name(id));
        let file = self
            .fs
            .create_tier(&path, self.config.mode)
            .map_err(|source| TierFlushError::Io { path: cold, source })?;
        self.next_id = next;
        Ok(CreatedFile { id, path, file, holds })
    }

    /// Stages an admitted plan: the rotation's seal, the created file's
    /// header and dirent barriers, the span copied and checksummed into the
    /// reserved windows, then its sync or its trailing gap's seal — every
    /// position the round plan's, every window its reservation's. `bytes` are the
    /// span's records (empty for a gap-only plan). Cannot fail.
    ///
    /// # Panics
    /// Panics when `bytes` is not the span's length (the store reads the
    /// span the plan admitted).
    pub fn stage_round(&mut self, plan: RoundPlan<F>, bytes: &[u8]) {
        let RoundPlan { shape, created, mut writes } = plan;
        match shape {
            PlanShape::Gap { end } => {
                if let Some(writer) = self.writer.take() {
                    self.seal_writer_queued(writer, SealReason::RingTopGap, &mut writes);
                }
                self.round.push_effect(RoundEffect::GapCross { to: end });
            }
            PlanShape::Span { rotate, addr, len, gap_end } => {
                assert_eq!(bytes.len() as u64, len.get(), "the stage copies the planned span");
                if rotate && let Some(writer) = self.writer.take() {
                    self.seal_writer_queued(writer, SealReason::Capacity, &mut writes);
                }
                if let Some(created) = created {
                    self.open_planned(created, addr, &mut writes);
                }
                let w = self.writer.as_mut().expect("a span stages into an active file");
                w.append_queued(addr, bytes, &mut self.round, &mut self.pool, &mut writes);
                match gap_end {
                    Some(end) => {
                        let writer = self.writer.take().expect("staged above");
                        self.seal_writer_queued(writer, SealReason::RingTopGap, &mut writes);
                        self.round.push_effect(RoundEffect::GapCross { to: end });
                    }
                    None => w_sync(self, &mut writes),
                }
            }
        }
        // Once per round, before any op of it is issued: with the pairing
        // at every write, a stage that pushed fewer writes than the plan
        // listed stops here rather than leave a planned position unwritten.
        assert!(writes.is_spent(), "the stage pushed every planned write");
        debug_assert!(self.round_handles_owned(), "every staged op names a held handle");
    }

    /// The created file becomes the active writer: its header write and
    /// both dirent barriers join the round (the segment-create rule,
    /// completion-gated: no manifest names the file before its name is
    /// durable), and the directory handles stay held until the round ends.
    fn open_planned(
        &mut self,
        created: CreatedFile<F>,
        base: LogicalAddr,
        writes: &mut PlannedWrites,
    ) {
        let CreatedFile { id, path, file, holds } = created;
        let writer = TierWriter::open_queued(
            file,
            path,
            base,
            self.config.mode,
            (self.config.cell, self.config.ns, self.config.file_capacity),
            &mut self.round,
            &mut self.pool,
            writes,
        );
        for handle in holds {
            let fd = handle.raw_fd().expect("reactor drive requires fd-backed dirs (ADR-0084)");
            self.round.push_barrier(fd);
            match self.round_dir_holds.reserve() {
                Reserved::Slot(slot) => slot.publish(handle),
                Reserved::Full => {
                    panic!("a tier round's directory holds past the round plan's two")
                }
            }
        }
        self.active_id = id;
        self.writer = Some(writer);
    }

    /// Stages a writer's seal (its tail and footer, its barrier) and the
    /// `SealCommit` its completion applies; the file waits in the pending
    /// seals until then.
    pub(super) fn seal_writer_queued(
        &mut self,
        writer: TierWriter<F>,
        reason: SealReason,
        writes: &mut PlannedWrites,
    ) {
        let sealed = writer.seal_queued(reason, &mut self.round, &mut self.pool, writes);
        self.round.push_effect(RoundEffect::SealCommit);
        let seal = PendingSeal {
            id: self.active_id,
            base: sealed.base,
            data_len: sealed.outcome.data_len,
            confirmed_len: sealed.confirmed_len,
            reason,
            path: sealed.outcome.path,
            device_bytes: sealed.outcome.device_bytes,
            file: sealed.file,
        };
        match self.pending_seals.reserve() {
            Reserved::Slot(slot) => slot.publish(seal),
            Reserved::Full => panic!("a tier round's staged seals past the round plan's two"),
        }
    }
}

/// The active writer's sync: its remaining batch, its partial tail frame,
/// its barrier and the `DurableTo` its completion applies.
fn w_sync<F: SegmentFs>(flush: &mut TierFlush<F>, writes: &mut PlannedWrites) {
    let w = flush.writer.as_mut().expect("a span stages into an active file");
    w.sync_queued(&mut flush.round, &mut flush.pool, writes);
}

fn shape_span_bytes(shape: PlanShape) -> u64 {
    match shape {
        PlanShape::Span { len, .. } => len.get(),
        PlanShape::Gap { .. } => 0,
    }
}

// The footer block is one frame: the layout counts it as one single
// window, as the stage takes it.
const _: () = assert!(TIER_FOOTER_BYTES == TIER_FRAME_BYTES, "a footer is one block");
