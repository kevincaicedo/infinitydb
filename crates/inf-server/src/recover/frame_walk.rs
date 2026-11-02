//! The frame walk of boot replay (ADR-0174 A1): a replay step that has
//! charged its budget stops at the next record, inside a log frame, and the
//! next step resumes there through a typed cursor over the frame the segment
//! reader still holds.
//!
//! A frame's entry — its read and its one CRC pass — is one unit and stays
//! whole. A **walk** is the records from the cursor to the frame's end: it
//! is *whole* when the frame's bytes not yet charged are below what the
//! budget has left (one ask at its start, as one compare per frame did), and
//! *metered* otherwise, with an ask after every record. A record that
//! answers `NeedsRoom` has changed nothing: the walk charges the bytes it
//! passed, runs the namespace's demote step under the budget left, and
//! presents the same record again once the step made the room, or yields;
//! a `DEL` that read its cold twins (a priced answer) is charged and asked
//! after. A frame's bytes are charged once, as the cursor passes them: a
//! whole walk at the frame's end, a metered walk record by record and its
//! header, trailer and padding at the end.
//!
//! Every move of the cursor lives in [`cursor`]: the cursor, its walk over
//! the frame, the one record a walk presents and the two movers, each of
//! which spends the presented record — on a consumed answer's token, or on
//! the cursor's own test of the begin LSN — and runs the record iterator's
//! end check when it leaves no record, so no walk ends a frame on a count
//! alone. Outside it nothing can move the cursor, build a presentation or
//! mint the token (the store does that), so a move on a `NeedsRoom` answer
//! does not compile. A walk holds one record iterator from its start to
//! its stop and checks the cursor against the frame once, at its start, so
//! a record costs what one iterator step costs.

use std::io;

use inf_log::fs::SegmentFs;
use inf_log::{FrameRecordError, Lsn, NsId, ReadError, ReadStep, SegmentReader};
use inf_store::{DemoteProgress, Keyspace, ReplaySpill, Replayed};

use super::{
    BootFile, Phase, Recovery, StampVerdict, StepBudget, io_invalid, io_msg, replay_apply_failed,
};
use cursor::{CursorWalk, FrameCursor, Moved};

/// The cursor of a replay step inside one frame, and the only code that
/// moves it (ADR-0174 A1). Its fields are private here: outside this module
/// a walk reads them through accessors and writes only `parked_markers`.
pub(super) mod cursor {
    use inf_log::{FrameRecordError, FrameRef, Lsn, RecordCursor, RecordIter, RecordView};
    use inf_store::ConsumedRecord;

    /// Where a replay step stands inside one frame: the frame (its base and
    /// first LSN), the next record (the record iterator's own cursor), the
    /// begin LSN copied at the frame's entry and the displacement markers
    /// the walk has parked. Plain data held across a yield, never a borrow
    /// of the reader's buffer. Neither `Clone` nor `Copy`: a copy saved
    /// before a move and written back after it would apply a record twice.
    #[derive(Debug)]
    pub(in crate::recover) struct FrameCursor {
        frame_base: u32,
        records: RecordCursor,
        begin: Option<Lsn>,
        /// The markers the register holds from this frame's walk so far,
        /// re-validated against the register when a walk resumes.
        pub(super) parked_markers: u8,
    }

    /// What the cursor's movers answer: a record follows, the walk handed
    /// back to present it, or the frame ended (its end check passed).
    pub(super) enum Moved<'c, 'f> {
        Next(CursorWalk<'c, 'f>),
        FrameEnd,
    }

    /// Why a walk or a presentation failed: the record at the cursor did
    /// not decode (corrupt input, as at a frame's first walk), or the
    /// frame given is not the cursor's.
    #[derive(Debug)]
    pub(super) enum PresentError {
        Record(FrameRecordError),
        Lost,
    }

    /// One walk of a frame from the cursor: the cursor, held for the walk,
    /// and the frame's record iterator at the cursor's position, made only
    /// by [`FrameCursor::walk`], whose check that the frame is the cursor's
    /// is the walk's one, so a presented record pays none of its own.
    /// Presenting a record consumes the walk and a move hands it back: a
    /// dropped presentation drops the walk, the cursor still at the record
    /// it presented, and only a new walk from the cursor presents it again.
    /// A value, not a borrow, so the iterator stays in registers across the
    /// records' applies (the in-place form, a borrowed walk, measured
    /// slower on the replay bench).
    pub(super) struct CursorWalk<'c, 'f> {
        cursor: &'c mut FrameCursor,
        records: RecordIter<'f>,
    }

    /// The one record a walk presents, made only by
    /// [`CursorWalk::present`]: it holds the walk — the cursor, so one
    /// presentation is live at a time, and the iterator past the record —
    /// and the record's LSN. Each mover spends it; dropping it moves
    /// nothing.
    #[must_use = "a presented record moves the cursor only by advance or skip_below_begin"]
    pub(super) struct PresentedRecord<'c, 'f> {
        walk: CursorWalk<'c, 'f>,
        lsn: Lsn,
    }

    /// What [`PresentedRecord::skip_below_begin`] did.
    pub(super) enum Skipped<'c, 'f> {
        /// The record lay below the begin LSN: the cursor passed it.
        Moved(Result<Moved<'c, 'f>, FrameRecordError>),
        /// The record is at or above it, handed back to be applied.
        Kept(PresentedRecord<'c, 'f>),
    }

    impl FrameCursor {
        /// The cursor at `frame`'s first record, with the begin LSN of the
        /// recovery unit (`None` without a checkpoint).
        pub(super) fn enter(
            frame: &FrameRef<'_>,
            frame_base: u32,
            begin: Option<Lsn>,
        ) -> FrameCursor {
            FrameCursor { frame_base, records: frame.records().cursor(), begin, parked_markers: 0 }
        }

        #[inline]
        pub(super) fn frame_base(&self) -> u32 {
            self.frame_base
        }

        #[inline]
        pub(super) fn first_lsn(&self) -> Lsn {
            self.records.first_lsn()
        }

        /// The frame's body bytes the walk has passed.
        #[inline]
        pub(super) fn body_offset(&self) -> u32 {
            self.records.body_offset()
        }

        /// The planted canary of `twice_on_resume`: a resumed walk starts
        /// one record back, so the last record consumed applies again.
        #[cfg(inf_canary_recover_twice_on_resume)]
        pub(super) fn rewind_planted(&mut self, frame: &FrameRef<'_>) {
            let consumed = frame.record_count() - self.records.remaining();
            let mut walk = frame.records();
            for _ in 1..consumed {
                let _ = walk.next();
            }
            if consumed > 0 {
                self.records = walk.cursor();
            }
        }

        /// The walk from the cursor over `frame`, the frame the reader holds
        /// for it: `Lost` when the cursor is another frame's, or past this
        /// one's body or count.
        pub(super) fn walk<'c, 'f>(
            &'c mut self,
            frame: &FrameRef<'f>,
        ) -> Result<CursorWalk<'c, 'f>, PresentError> {
            let records = frame.records_from(&self.records).map_err(|_| PresentError::Lost)?;
            Ok(CursorWalk { cursor: self, records })
        }
    }

    impl<'c, 'f> CursorWalk<'c, 'f> {
        /// The cursor the walk holds, to read.
        #[inline]
        pub(super) fn cursor(&self) -> &FrameCursor {
            self.cursor
        }

        /// Decodes the record at the cursor through the record iterator
        /// (its length from the decoder; a body short of its count answers
        /// `MissingRecords` here) and presents it.
        #[inline]
        pub(super) fn present(
            mut self,
        ) -> Result<(RecordView<'f>, PresentedRecord<'c, 'f>), PresentError> {
            match self.records.next() {
                Some(Ok((lsn, view))) => Ok((view, PresentedRecord { walk: self, lsn })),
                Some(Err(error)) => Err(PresentError::Record(error)),
                // A walk starts at a record of a frame that has one and is
                // handed back only while one follows, so this names a
                // cursor at the frame's end, which no presentation follows.
                None => Err(PresentError::Lost),
            }
        }
    }

    impl<'c, 'f> PresentedRecord<'c, 'f> {
        /// Moves past the record when it lies below the begin LSN copied at
        /// the frame's entry — the checkpoint holds its final state — and
        /// hands it back otherwise.
        #[inline]
        pub(super) fn skip_below_begin(self) -> Skipped<'c, 'f> {
            if self.walk.cursor.begin.is_some_and(|begin| self.lsn < begin) {
                Skipped::Moved(self.pass())
            } else {
                Skipped::Kept(self)
            }
        }

        /// The presented record's LSN.
        #[inline]
        pub(super) fn lsn(&self) -> Lsn {
            self.lsn
        }

        /// Moves past the record on its consumed answer's token.
        #[inline]
        pub(super) fn advance(
            self,
            token: ConsumedRecord,
        ) -> Result<Moved<'c, 'f>, FrameRecordError> {
            drop(token);
            self.pass()
        }

        /// The planted canary of `skip_on_resume`: passes a record with no
        /// token.
        #[cfg(inf_canary_recover_skip_on_resume)]
        pub(super) fn pass_planted(self) -> Result<Moved<'c, 'f>, FrameRecordError> {
            self.pass()
        }

        /// The cursor to the iterator's position; a move that leaves no
        /// record runs the iterator once more, its end check: `None` is the
        /// frame's end, bytes after the counted records its typed error.
        #[inline]
        fn pass(self) -> Result<Moved<'c, 'f>, FrameRecordError> {
            let PresentedRecord { mut walk, .. } = self;
            walk.cursor.records = walk.records.cursor();
            if walk.cursor.records.remaining() > 0 {
                return Ok(Moved::Next(walk));
            }
            if cfg!(inf_canary_recover_frame_end_counted) {
                // The planted canary ends the frame on its count alone.
                return Ok(Moved::FrameEnd);
            }
            match walk.records.next() {
                None => Ok(Moved::FrameEnd),
                Some(Err(error)) => Err(error),
                // The iterator yields no record once its count is spent.
                Some(Ok(_)) => Err(FrameRecordError::TrailingBytes { at: 0 }),
            }
        }
    }
}

/// Where a step's walk of one frame ended.
pub(super) enum WalkEnd {
    /// The frame ended: `ask_yield` when the walk was metered and its
    /// budget is spent, so the step yields at the boundary.
    FrameEnd { ask_yield: bool },
    /// The step yields inside the frame, at `cursor`, with `unpaid` of the
    /// frame's bytes not charged yet.
    Parked { cursor: FrameCursor, kind: YieldKind, unpaid: u64 },
}

/// Which unit a step yielded inside (ADR-0174 A1.2's liveness counters).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum YieldKind {
    Frame,
    DemoteStep,
}

/// A walk's own state: metered or whole, the body offset up to which the
/// frame is charged, the frame's extent, base and first LSN (a record
/// error names the frame by it), and whether an earlier step parked the
/// cursor it started at.
struct Walk {
    metered: bool,
    charged_to: u32,
    padded: u64,
    frame_base: u32,
    at: Lsn,
    resumed: bool,
}

/// One turn's answer: the next record, its walk handed back, or the walk
/// stopped.
enum Turn<'c, 'f> {
    Next(CursorWalk<'c, 'f>),
    Stop(Stop),
}

/// Why a walk stopped: the frame's end (`ask_yield` when the walk was
/// metered and its budget is spent, so the step yields at the boundary);
/// a yield inside the frame; or a record that needs room in `ns`, its
/// presentation dropped, so the cursor is at it.
enum Stop {
    FrameEnd { ask_yield: bool },
    Parked(YieldKind),
    NeedsRoom(NsId, Lsn),
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// Walks the frame `reader` holds for `cursor` from the cursor on,
    /// under `budget`: whole when what the frame has not charged is below
    /// what is left, metered otherwise (one ask). `resumed` is a walk a
    /// step starts at a cursor an earlier step parked.
    ///
    /// # Errors
    /// The frame's record errors, as at its first walk (`replay apply
    /// failed`, naming the frame); a store or demote refusal; `CursorLost`
    /// when the reader no longer holds the cursor's frame or the register
    /// no longer holds the markers the walk parked.
    pub(super) fn walk_frame(
        &mut self,
        ks: &mut Keyspace,
        reader: &SegmentReader<BootFile<F>>,
        mut cursor: FrameCursor,
        budget: &mut StepBudget,
        resumed: bool,
    ) -> io::Result<WalkEnd> {
        let base = cursor.frame_base();
        let frame = reader
            .held_frame(base)
            .filter(|frame| frame.first_lsn() == cursor.first_lsn())
            .ok_or_else(|| cursor_lost(base, "the reader no longer holds the frame"))?;
        if resumed && ks.displace_register_len() != usize::from(cursor.parked_markers) {
            return Err(cursor_lost(base, "the register no longer holds the walk's markers"));
        }
        #[cfg(inf_canary_recover_twice_on_resume)]
        if resumed {
            cursor.rewind_planted(&frame);
        }
        let padded = u64::from(frame.padded_len());
        let unpaid = padded.saturating_sub(u64::from(cursor.body_offset()));
        // The planted canary walks every frame whole, never asking.
        let metered = !cfg!(inf_canary_recover_frame_whole) && unpaid >= budget.left();
        let charged_to = cursor.body_offset();
        let at = frame.first_lsn();
        let mut walk = Walk { metered, charged_to, padded, frame_base: base, at, resumed };
        // Bound: one walk, plus one more per demote step that made room (a
        // record asks for room at most four times).
        loop {
            let live = cursor
                .walk(&frame)
                .map_err(|_| cursor_lost(base, "the cursor is past the frame's body or count"))?;
            let kind = match self.walk_records(ks, live, &mut walk, budget)? {
                Stop::FrameEnd { ask_yield } => return Ok(WalkEnd::FrameEnd { ask_yield }),
                Stop::Parked(kind) => {
                    // The markers the register holds at the yield, which
                    // the resumed walk re-validates.
                    cursor.parked_markers = parked_markers(ks);
                    kind
                }
                Stop::NeedsRoom(ns, lsn) => {
                    match self.make_room(ks, ns, &mut cursor, &mut walk, budget, lsn)? {
                        Some(kind) => kind,
                        // The room is made: a new walk presents the record.
                        None => continue,
                    }
                }
            };
            let unpaid = padded.saturating_sub(u64::from(cursor.body_offset()));
            return Ok(WalkEnd::Parked { cursor, kind, unpaid });
        }
    }

    /// The records of one walk from `live` on, until the walk stops.
    fn walk_records<'c, 'f>(
        &mut self,
        ks: &mut Keyspace,
        mut live: CursorWalk<'c, 'f>,
        walk: &mut Walk,
        budget: &mut StepBudget,
    ) -> io::Result<Stop> {
        // Bound: one turn per record the frame has left.
        loop {
            live = match self.walk_record(ks, live, walk, budget)? {
                Turn::Next(next) => next,
                Turn::Stop(stop) => return Ok(stop),
            };
        }
    }

    /// One turn of a walk: the record at the cursor presented, then skipped
    /// below the begin LSN, applied and moved past, or waiting for room.
    fn walk_record<'c, 'f>(
        &mut self,
        ks: &mut Keyspace,
        live: CursorWalk<'c, 'f>,
        walk: &mut Walk,
        budget: &mut StepBudget,
    ) -> io::Result<Turn<'c, 'f>> {
        let at = walk.at;
        let (view, presented) = match live.present() {
            Ok(presented) => presented,
            Err(cursor::PresentError::Record(error)) => return Err(record_failed(at, error)),
            Err(cursor::PresentError::Lost) => {
                return Err(cursor_lost(walk.frame_base, "the cursor names no record"));
            }
        };
        // The planted canary runs the begin test only on a walk that starts
        // at a frame's entry.
        let begin_test = !(cfg!(inf_canary_recover_begin_skip_entry_only) && walk.resumed);
        let presented = if begin_test {
            match presented.skip_below_begin() {
                cursor::Skipped::Moved(moved) => {
                    self.stats.records_pre_begin += 1;
                    let moved = moved.map_err(|error| record_failed(at, error))?;
                    return Ok(self.after_move(walk, budget, moved));
                }
                cursor::Skipped::Kept(presented) => presented,
            }
        } else {
            presented
        };
        let lsn = presented.lsn();
        let answer = ks
            .apply_record(&view, self.now, self.anchor, &mut self.recovering_tiers)
            .map_err(io_invalid)
            .map_err(|error| replay_apply_failed(at, &error))?;
        let (token, priced) = match answer {
            Replayed::Consumed(token) => (token, false),
            Replayed::ConsumedPriced(token) => (token, true),
            Replayed::NeedsRoom(ns) => {
                // The planted canary passes, on a resumed walk, a record
                // that needs room.
                #[cfg(inf_canary_recover_skip_on_resume)]
                if walk.resumed {
                    let moved = presented.pass_planted().map_err(|e| record_failed(at, e))?;
                    return Ok(self.after_move(walk, budget, moved));
                }
                drop(presented);
                return Ok(Turn::Stop(Stop::NeedsRoom(ns, lsn)));
            }
        };
        self.count_outcome(ks, token.outcome());
        let moved = presented.advance(token).map_err(|error| record_failed(at, error))?;
        if priced {
            // Its reads are charged with the bytes passed, and the rest of
            // the walk is metered; the frame's end charges what is left.
            self.drain_frame(budget);
            if let Moved::Next(next) = &moved {
                self.charge_passed(next.cursor(), walk, budget);
            }
            walk.metered = true;
        }
        if cfg!(inf_canary_recover_ask_per_record) && !walk.metered {
            // The planted canary asks after every record of a whole walk.
            let _ = budget.spent();
        }
        Ok(self.after_move(walk, budget, moved))
    }

    /// A frame's entry, one unit: the reader's next frame read whole and
    /// audited in one CRC pass, its stamp's continuity classified once and
    /// the frame counted once (ADR-0031 D3: before any record of it
    /// applies) — `Some` cursor at its first record. A segment's end ends
    /// this segment's replay here, `None`, with the phase set: a frame-level
    /// failure marks the data end at the failed frame's base and replay
    /// continues with the next segment; whether those bytes were a torn
    /// tail, inert residue, or interior corruption is decided by the slack
    /// scans (Audit phase) — on evidence, not on which code path noticed
    /// them (M2-S14, ADR-0018). Read I/O errors fail-stop as-is.
    pub(super) fn enter_frame(
        &mut self,
        idx: usize,
        reader: &mut SegmentReader<BootFile<F>>,
        start_offset: u64,
    ) -> io::Result<Option<FrameCursor>> {
        let end = match reader.next_step() {
            Ok(ReadStep::Frame(frame)) => {
                let header_len = u32::try_from(frame.header_len()).expect("40");
                let frame_base = frame.first_lsn().offset - header_len;
                let verdict = self.classify_stamp(
                    frame.stamp(),
                    self.segments[idx],
                    frame_base,
                    frame.first_lsn(),
                )?;
                match verdict {
                    StampVerdict::Replay => {
                        self.stats.frames += 1;
                        return Ok(Some(FrameCursor::enter(&frame, frame_base, self.begin_lsn)));
                    }
                    StampVerdict::ResidueStop => {
                        // The frame validates but belongs to a discarded
                        // life: end this segment's data here and stop
                        // replay — the audit scans the rest.
                        self.residue_stop = true;
                        self.stats.epoch_residue_stops += 1;
                        (frame_base, None)
                    }
                }
            }
            Ok(ReadStep::End(end)) => {
                // Continuity never spans a segment boundary (ADR-0031 D3):
                // a torn rotation-tail frame leaves the same clean-end
                // shape as ordinary prealloc slack, so a seq step across
                // segments is legal physics, not malformation. Epoch
                // monotonicity and the cross-segment attestation check
                // still span.
                self.prev_stamp = None;
                (end.at(), None)
            }
            Err(err @ ReadError::Io { .. }) => return Err(io_invalid(err)),
            Err(ReadError::ForeignSegment { offset, .. }) => {
                // ADR-0090 D2 as amended: a decoded frame at its own
                // offset stamped for another segment id is recycled-life
                // residue — this segment's data ends here, as cleanly as at
                // a zero tail. Replay goes on in the next segment (a
                // foreign frame says nothing about the segments after it);
                // the audit proves the slack.
                self.prev_stamp = None;
                self.stats.segment_residue_stops += 1;
                (offset, None)
            }
            Err(err) => {
                let offset = match &err {
                    ReadError::Frame { offset, .. } | ReadError::LsnMismatch { offset, .. } => {
                        *offset
                    }
                    ReadError::Io { .. } | ReadError::ForeignSegment { .. } => {
                        unreachable!("Io and foreign-segment reads are handled above")
                    }
                };
                // The contiguity run breaks here (ADR-0031 D3): frames in
                // later segments are a fresh baseline — the audit owns
                // everything at and beyond this break. Epoch monotonicity
                // still spans breaks (`max_prefix_epoch`).
                self.prev_stamp = None;
                (offset, Some(err))
            }
        };
        let (data_end, failure) = end;
        self.stats.segments += 1;
        self.ends.push((data_end, failure));
        // The consumed bytes (to the data end, never past the reader) plus
        // the segment's slack (progress only — slack is skipped, not read).
        let consumed = u64::from(reader.offset().min(data_end)).saturating_sub(start_offset);
        self.bytes_done += consumed;
        self.bytes_consumed += consumed;
        self.bytes_done += self.seg_sizes[idx].saturating_sub(u64::from(data_end));
        self.phase = Phase::Audit { idx };
        Ok(None)
    }

    /// After a move: the frame's end, charged; a yield inside it, for a
    /// metered walk whose budget is spent; else the next record.
    fn after_move<'c, 'f>(
        &mut self,
        walk: &mut Walk,
        budget: &mut StepBudget,
        moved: Moved<'c, 'f>,
    ) -> Turn<'c, 'f> {
        match moved {
            Moved::FrameEnd => {
                // The frame's bytes not charged yet: all of them for a
                // whole walk, the header, trailer and padding for a
                // metered one (and the last record's bytes after a priced
                // one); then the boot I/O its records did.
                budget.charge(walk.padded.saturating_sub(u64::from(walk.charged_to)));
                self.drain_frame(budget);
                Turn::Stop(Stop::FrameEnd { ask_yield: walk.metered && budget.spent() })
            }
            Moved::Next(next) if walk.metered => {
                self.charge_passed(next.cursor(), walk, budget);
                if budget.spent() {
                    Turn::Stop(Stop::Parked(YieldKind::Frame))
                } else {
                    Turn::Next(next)
                }
            }
            Moved::Next(next) => Turn::Next(next),
        }
    }

    /// A record that answered `NeedsRoom` in `ns`: the bytes passed are
    /// charged, the demote step runs under what is left and its boot I/O
    /// is charged. `Some` is the step's yield: inside the demote step when
    /// it answered `More`, inside the frame when it made the room and the
    /// budget is spent; `None` presents the same record again. The rest of
    /// the walk is metered.
    fn make_room(
        &mut self,
        ks: &mut Keyspace,
        ns: NsId,
        cursor: &mut FrameCursor,
        walk: &mut Walk,
        budget: &mut StepBudget,
        lsn: Lsn,
    ) -> io::Result<Option<YieldKind>> {
        walk.metered = true;
        self.charge_passed(cursor, walk, budget);
        cursor.parked_markers = parked_markers(ks);
        let left = budget.left();
        let (Some(machine), Some(table)) =
            (self.recovering_tiers.replay_mut(ns), ks.tiered_store_mut(ns))
        else {
            return Err(io_msg(format!(
                "ns {}: a record at {lsn} needs room with no machine",
                ns.0
            )));
        };
        // The planted canary runs the demote step whole inside the step, as
        // it ran inside one record's apply.
        let progress = if cfg!(inf_canary_recover_frame_whole) {
            // Bound: each call walks a record at least, or completes.
            while machine.demote_step(table, u64::MAX).map_err(|r| refused(ns, &r))?
                == DemoteProgress::More
            {}
            DemoteProgress::Room
        } else {
            machine.demote_step(table, left).map_err(|refusal| refused(ns, &refusal))?
        };
        self.drain_frame(budget);
        Ok(match progress {
            DemoteProgress::More => Some(YieldKind::DemoteStep),
            DemoteProgress::Room if !cfg!(inf_canary_recover_frame_whole) && budget.spent() => {
                Some(YieldKind::Frame)
            }
            DemoteProgress::Room => None,
        })
    }

    /// Drains the machines' boot I/O into the step's charge and its budget.
    fn drain_frame(&mut self, budget: &mut StepBudget) {
        let before = self.step_charge;
        self.drain_boot_io();
        // The planted canary leaves the boot I/O out of the yield test.
        if !cfg!(inf_canary_replay_yield_uncharged) {
            budget.charge(self.step_charge.saturating_sub(before));
        }
    }

    /// Charges the frame's body bytes the cursor passed since the walk's
    /// last charge.
    fn charge_passed(&self, cursor: &FrameCursor, walk: &mut Walk, budget: &mut StepBudget) {
        let to = cursor.body_offset();
        budget.charge(u64::from(to.saturating_sub(walk.charged_to)));
        walk.charged_to = to;
    }
}

/// The register's markers, at most the displacement register's cap.
fn parked_markers(ks: &Keyspace) -> u8 {
    u8::try_from(ks.displace_register_len()).unwrap_or(u8::MAX)
}

/// A record error inside a CRC-valid frame: the boot's refusal, as the
/// frame's first walk reports it.
fn record_failed(at: Lsn, error: FrameRecordError) -> io::Error {
    replay_apply_failed(at, &io_invalid(error))
}

/// A walk whose cursor's frame or markers are gone: a typed boot refusal
/// naming the phase and the cursor, a defect class no input reaches.
fn cursor_lost(frame_base: u32, why: &str) -> io::Error {
    io_msg(format!("replay: CursorLost at the frame based at {frame_base:#x}: {why}"))
}

fn refused(ns: NsId, refusal: &inf_store::ReplayRefusal) -> io::Error {
    super::replay_refused(ns, refusal)
}
