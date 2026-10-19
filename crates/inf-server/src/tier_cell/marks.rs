//! The extent-reclaim marks of one tiered namespace (ADR-0061 D5). MAINTAIN
//! turns the durable cell's fsync watermark into the namespace's reclaim
//! epoch through `(seq, epoch)` marks: once the watermark covers a mark's
//! seq, every extent death stamped at or below its epoch was staged at or
//! below that seq and is durable, so its extent may be unlinked.
//!
//! One mark per ack class (ADR-0163 D2). The watermark only ever takes the
//! last seq of an ack-map entry it pops, so two marks one entry releases pop
//! at one completion, and the later stands for both: merging them is exact.
//! The marks are a capped deque; at its cap the back takes the later pair
//! whole, which only delays reclaim to the later class's barrier.

use std::rc::Rc;

use inf_foundation::bounded::{CapCensus, CapError, CappedDeque, Coalesce, Lossy, Merge};
use inf_foundation::limits::DEQUE_BACKING_BYTES_MAX;

use crate::durable::ack_class::{AckClass, AckClasses};
use crate::limits::{EPOCH_MARKS, EPOCH_MARKS_MAX};

// The marks' one allocation fits a deque's backing bound, so their
// construction never refuses on its layout.
const _: () = assert!(
    EPOCH_MARKS_MAX as usize * core::mem::size_of::<EpochMark>() <= DEQUE_BACKING_BYTES_MAX,
    "the reclaim marks fit one deque allocation"
);

/// A reclaim mark: every extent death stamped at or below `epoch` was staged
/// at a durable seq at or below `seq`. A namespace's marks strictly increase
/// in both fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EpochMark {
    seq: u64,
    epoch: u64,
}

/// A mark may merge into a later one: the later pair satisfies the earlier
/// mark's claim too, so a merge delays reclaim and never advances it.
impl Lossy for EpochMark {}

/// The merge at a back of the pair's own class, and at the cap: the back
/// takes the later pair whole, its seq and its epoch, so its deaths are
/// reclaimed once the later pair's barrier makes them durable.
pub(crate) struct LaterMark;

impl Merge<EpochMark> for LaterMark {
    fn merge(back: &mut EpochMark, new: EpochMark) {
        #[cfg(not(inf_canary_epoch_mark_keeps_old_seq))]
        {
            *back = new;
        }
        // Canary: the back takes the later epoch and keeps its own seq, so
        // it pops at the earlier class's barrier with the later deaths.
        #[cfg(inf_canary_epoch_mark_keeps_old_seq)]
        {
            back.epoch = new.epoch;
        }
    }
}

/// One tiered namespace's reclaim marks and the reclaim epoch they yield.
pub(crate) struct EpochMarks {
    marks: CappedDeque<EpochMark, Coalesce<LaterMark>>,
    /// Every extent death stamped at or below it is durable.
    durable_epoch: u64,
}

impl EpochMarks {
    /// The marks of one namespace, a row of `census` under `EPOCH_MARKS`:
    /// one allocation of `EPOCH_MARKS_MAX` marks, never grown.
    ///
    /// # Errors
    /// The deque's construction refusal: the allocator refused the backing,
    /// or the census refused the row.
    pub(crate) fn new(census: &Rc<CapCensus>) -> Result<EpochMarks, CapError> {
        Ok(EpochMarks { marks: CappedDeque::new(EPOCH_MARKS, census)?, durable_epoch: 0 })
    }

    /// The reclaim epoch: every extent death stamped at or below it is
    /// durable.
    pub(crate) fn durable_epoch(&self) -> u64 {
        self.durable_epoch
    }

    /// Marks held, at most `EPOCH_MARKS_MAX`.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.marks.len()
    }

    /// One MAINTAIN tick at the namespace's WAL epoch `epoch`. The covered
    /// front marks pop first. Then, with the staged seq and the epoch both
    /// past the back mark (a record of this namespace was staged since it),
    /// adjacent marks of one class merge, and the pair `(staged seq, epoch)`
    /// replaces a back of its own class, or is pushed, or at the cap merges
    /// into the back. The covered front marks pop again. At most
    /// `EPOCH_MARKS_MAX + 1` class searches.
    pub(crate) fn tick(&mut self, epoch: u64, durable: &impl AckClasses) {
        let durable_seq = durable.durable_seq();
        self.pop_covered(durable_seq);
        let staged_seq = durable.staged_seq();
        let moved =
            self.marks.back().is_none_or(|back| back.seq < staged_seq && back.epoch < epoch);
        if moved {
            let pair = EpochMark { seq: staged_seq, epoch };
            let back_class = self.compact(durable);
            let pair_class = durable.ack_class(staged_seq);
            match self.marks.back_mut() {
                Some(back) if back_class == Some(pair_class) => LaterMark::merge(back, pair),
                Some(_) | None => self.marks.push(pair),
            }
            debug_assert!(self.strictly_increasing(), "reclaim marks increase in both fields");
        }
        self.pop_covered(durable_seq);
    }

    /// Pops every front mark at or below the watermark into the reclaim
    /// epoch: at most `EPOCH_MARKS_MAX` pops.
    fn pop_covered(&mut self, durable_seq: u64) {
        while self.marks.front().is_some_and(|front| front.seq <= durable_seq) {
            if let Some(front) = self.marks.pop_front() {
                self.durable_epoch = self.durable_epoch.max(front.epoch);
            }
        }
    }

    /// Merges each pair of adjacent marks of one class into the later, and
    /// returns the back mark's class. Each mark's class is searched once: the
    /// pass offers each mark as the later of a pair and then as the earlier
    /// of the next, and the class it found for the later is reused.
    fn compact(&mut self, durable: &impl AckClasses) -> Option<AckClass> {
        let mut last: Option<(EpochMark, AckClass)> = None;
        self.marks.merge_adjacent(|earlier, later| {
            let earlier_class = match last {
                Some((mark, class)) if mark == *earlier => class,
                Some(_) | None => durable.ack_class(earlier.seq),
            };
            let later_class = durable.ack_class(later.seq);
            last = Some((*later, later_class));
            earlier_class == later_class
        });
        let back = *self.marks.back()?;
        Some(match last {
            Some((mark, class)) if mark == back => class,
            Some(_) | None => durable.ack_class(back.seq),
        })
    }

    /// The marks strictly increase in both fields.
    fn strictly_increasing(&self) -> bool {
        self.marks
            .iter()
            .zip(self.marks.iter().skip(1))
            .all(|(a, b)| a.seq < b.seq && a.epoch < b.epoch)
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "test-only: the unbounded models the marks are checked against"
)]
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use inf_foundation::CellId;
    use inf_foundation::bounded::CapCensus;
    use inf_foundation::rng::{Entropy, SplitMix64};
    use inf_log::{Lsn, SegmentId};

    use super::*;
    use crate::durable::ack_class::ack_class_of;

    /// The rule the capped marks replace, kept as the oracle: a mark per
    /// tick whose staged seq and epoch both moved past the last mark, every
    /// mark kept until the watermark covers it. It reads no class.
    #[derive(Default)]
    struct List {
        marks: Vec<(u64, u64)>,
        durable_epoch: u64,
    }

    impl List {
        fn tick(&mut self, staged: u64, epoch: u64, durable_seq: u64) {
            if self.marks.last().is_none_or(|&(seq, e)| seq < staged && e < epoch) {
                self.marks.push((staged, epoch));
            }
            let covered = self.marks.iter().take_while(|&&(seq, _)| seq <= durable_seq).count();
            for (_, e) in self.marks.drain(..covered) {
                self.durable_epoch = self.durable_epoch.max(e);
            }
        }
    }

    /// One fsync-ledger barrier: the frame end it covers, whether it is
    /// done, and whether it waits for its frame's write (a linked or
    /// write-through barrier) or covers only what was written (a
    /// standalone).
    struct Barrier {
        covers: u32,
        done: bool,
        on_frame: bool,
    }

    /// The durable plane as the marks see it, written from the ledger's
    /// rules alone: staged seqs, frames sealed whole from the builder, a
    /// written prefix, an ack map coalesced at each write where no
    /// unfolded barrier covers a point between two written entries, a FIFO
    /// ledger folded by its done prefix, and the watermark at the last seq
    /// of each ack entry the fold pops.
    #[derive(Default)]
    struct Durable {
        staged: u64,
        epoch: u64,
        sealed: u64,
        next_end: u32,
        map: VecDeque<(Lsn, u64)>,
        unwritten: VecDeque<u32>,
        written_end: u32,
        ledger: VecDeque<Barrier>,
        durable_end: u32,
        watermark: u64,
    }

    fn lsn(end: u32) -> Lsn {
        Lsn::new(SegmentId(1), end)
    }

    impl AckClasses for Durable {
        fn staged_seq(&self) -> u64 {
            self.staged
        }

        fn durable_seq(&self) -> u64 {
            self.watermark
        }

        fn ack_class(&self, seq: u64) -> AckClass {
            let (front, back) = self.map.as_slices();
            ack_class_of(front, back, self.watermark, seq)
        }
    }

    impl Durable {
        /// Stages one record; `own` records are the namespace's, which move
        /// its WAL epoch.
        fn stage(&mut self, own: bool) {
            self.staged += 1;
            if own {
                self.epoch += 1;
            }
        }

        /// LOG: the builder seals whole into one frame; `barrier` gives the
        /// frame a linked or write-through barrier of its own.
        fn seal(&mut self, barrier: bool) -> bool {
            if self.staged == self.sealed {
                return false;
            }
            self.next_end += 64;
            self.map.push_back((lsn(self.next_end), self.staged));
            self.unwritten.push_back(self.next_end);
            if barrier {
                self.ledger.push_back(Barrier {
                    covers: self.next_end,
                    done: false,
                    on_frame: true,
                });
            }
            self.sealed = self.staged;
            true
        }

        /// The oldest unwritten frame's `LogWritten`: the written prefix
        /// grows and the ack map coalesces over it.
        fn write(&mut self) -> bool {
            let Some(end) = self.unwritten.pop_front() else { return false };
            self.written_end = end;
            let mut i = 0;
            while i + 1 < self.map.len() {
                let (a, b) = (self.map[i].0, self.map[i + 1].0);
                if b > lsn(self.written_end) {
                    break;
                }
                let point_between =
                    self.ledger.iter().any(|p| a <= lsn(p.covers) && lsn(p.covers) < b);
                if point_between {
                    i += 1;
                } else {
                    self.map.remove(i);
                }
            }
            true
        }

        /// A standalone barrier over the written prefix, when it is dirty
        /// and no unfolded barrier covers it already.
        fn standalone(&mut self) -> bool {
            let covered = self.ledger.back().map_or(self.durable_end, |p| p.covers);
            if self.written_end <= covered {
                return false;
            }
            self.ledger.push_back(Barrier {
                covers: self.written_end,
                done: false,
                on_frame: false,
            });
            true
        }

        /// Barrier `at` completes; the done prefix folds and the watermark
        /// takes the last seq of each ack entry it covers.
        fn complete(&mut self, at: usize) -> bool {
            let ready = self
                .ledger
                .get(at)
                .is_some_and(|p| !p.done && (!p.on_frame || p.covers <= self.written_end));
            if !ready {
                return false;
            }
            self.ledger[at].done = true;
            while self.ledger.front().is_some_and(|p| p.done) {
                if let Some(p) = self.ledger.pop_front() {
                    self.durable_end = p.covers;
                }
            }
            while self.map.front().is_some_and(|&(end, _)| end <= lsn(self.durable_end)) {
                if let Some((_, seq)) = self.map.pop_front() {
                    self.watermark = seq;
                }
            }
            true
        }
    }

    /// The marks, their census and the oracle list, ticked together.
    struct Rig {
        census: Rc<CapCensus>,
        marks: EpochMarks,
        list: List,
        durable: Durable,
    }

    impl Rig {
        fn new() -> Rig {
            let (census, _serve_mark) = CapCensus::new(CellId(1));
            let marks = EpochMarks::new(&census).expect("built");
            Rig { census, marks, list: List::default(), durable: Durable::default() }
        }

        fn crossings(&self) -> u64 {
            self.census.row("epoch-marks").map_or(0, |row| row.crossings)
        }

        /// One MAINTAIN tick on both, then the property: never above the
        /// list's reclaim epoch, and equal to it while no cap merge happened.
        fn tick(&mut self) {
            let d = &self.durable;
            self.marks.tick(d.epoch, d);
            self.list.tick(d.staged, d.epoch, d.watermark);
            let (marks, list) = (self.marks.durable_epoch(), self.list.durable_epoch);
            assert!(marks <= list, "durable_epoch {marks} above the list's {list}: reclaim early");
            if self.crossings() == 0 {
                assert_eq!(marks, list, "durable_epoch {marks} below the list's {list}");
            }
            assert!(self.marks.len() <= EPOCH_MARKS_MAX as usize, "marks past their cap");
        }
    }

    /// The everysec regime: 1,000 ticks, each with one write of the
    /// namespace, sealed into a plain frame that is written, inside one
    /// barrier period. Written frames with no barrier between them are one
    /// ack class, so the marks hold that class and the builder's: two, where
    /// the uncapped rule held 1,000. The tick's standalone then reclaims
    /// every death exactly as the list does, with no crossing.
    #[test]
    fn a_thousand_ticks_inside_one_barrier_period_hold_two_marks() {
        let mut rig = Rig::new();
        for _ in 0..1_000 {
            rig.durable.stage(true);
            rig.tick();
            assert!(rig.marks.len() <= 2, "{} marks inside one barrier period", rig.marks.len());
            assert!(rig.durable.seal(false));
            assert!(rig.durable.write());
        }
        assert_eq!(rig.list.marks.len(), 1_000, "the uncapped rule's marks");
        assert!(rig.durable.standalone());
        assert!(rig.durable.complete(0));
        rig.tick();
        assert_eq!(rig.marks.durable_epoch(), 1_000);
        assert_eq!(rig.marks.len(), 0);
        assert_eq!(rig.crossings(), 0, "every merge was exact");
    }

    /// `count` ticks, each sealed into a frame with a barrier of its own
    /// that is written but not folded, the watermark held, then one tick
    /// more: `count` ack classes.
    fn sealed_ticks_with_the_watermark_held(count: u32) -> Rig {
        let mut rig = Rig::new();
        for _ in 0..count {
            rig.durable.stage(true);
            rig.tick();
            assert!(rig.durable.seal(true));
            assert!(rig.durable.write());
        }
        rig.tick();
        rig
    }

    /// The cap's edge from below: 15 and 16 ack classes with the watermark
    /// held keep one mark each and cross nothing, the row's high water the
    /// class count; the reclaim epoch equals the list's after every tick
    /// (the rig checks it while nothing crossed), through every barrier's
    /// fold.
    #[test]
    fn fifteen_and_sixteen_sealed_ticks_hold_one_mark_each_without_a_crossing() {
        for count in [15, 16] {
            let mut rig = sealed_ticks_with_the_watermark_held(count);
            assert_eq!(rig.list.marks.len(), count as usize, "the uncapped rule's marks");
            assert_eq!(rig.marks.len(), count as usize);
            let row = rig.census.row("epoch-marks").expect("the marks' row");
            assert_eq!((row.high_water, row.crossings), (count, 0));
            for _ in 0..count {
                assert!(rig.durable.complete(0), "the front barrier completes and folds");
                rig.tick();
            }
            assert_eq!(rig.marks.durable_epoch(), u64::from(count));
            assert_eq!(rig.crossings(), 0);
        }
    }

    /// The wedge: 17 ticks, each sealed into a frame with a barrier of its
    /// own that is written but not folded, the watermark held: 17 ack
    /// classes. The marks hold their cap of 16 and count one crossing (the
    /// back took the later pair), never reclaiming early; once the barriers
    /// fold the reclaim epoch is the list's.
    #[test]
    fn seventeen_sealed_ticks_with_the_watermark_held_hold_sixteen_marks() {
        let mut rig = sealed_ticks_with_the_watermark_held(17);
        assert_eq!(rig.list.marks.len(), 17, "the uncapped rule's marks");
        assert_eq!(rig.marks.len(), 16);
        assert_eq!(rig.crossings(), 1);
        assert_eq!(rig.marks.durable_epoch(), 0);
        for _ in 0..17 {
            assert!(rig.durable.complete(0), "the front barrier completes and folds");
            rig.tick();
        }
        assert_eq!(rig.marks.durable_epoch(), 17);
        assert_eq!(rig.marks.durable_epoch(), rig.list.durable_epoch);
    }

    /// A standalone covers the written frame only: the tick finds the
    /// first frame written and the second in flight, so their marks are two
    /// classes; the standalone over the written frame reclaims its death as
    /// the list does. Taking the two marks for one class (the back replaced
    /// by the later pair) would leave the death waiting for the later frame.
    /// A tick with nothing staged since the last mark takes no mark and asks
    /// no class, so the history stages a second record before the second
    /// tick: the first mark's record is then in a sealed frame and the
    /// second's in the builder, two classes, and the standalone covers the
    /// first frame alone.
    #[test]
    fn a_standalone_over_the_written_frame_reclaims_its_death() {
        let mut rig = Rig::new();
        rig.durable.stage(true);
        rig.tick();
        assert!(rig.durable.seal(false));
        rig.durable.stage(true);
        rig.tick();
        assert!(rig.durable.seal(false));
        assert!(rig.durable.write());
        assert!(rig.durable.standalone());
        assert!(rig.durable.complete(0));
        rig.tick();
        assert_eq!(rig.marks.durable_epoch(), 1, "the standalone's death is reclaimed");
    }

    /// The model test: seeded histories of ticks, seals with and without a
    /// barrier, write completions, standalones and barrier completions in
    /// any order, the reclaim epoch checked against the list after every
    /// tick. Engaged: merges shrink the marks below the list on every seed,
    /// and some seeds hold past 16 classes and cross.
    #[test]
    fn a_seeded_history_never_reclaims_before_the_list_and_matches_it_uncrossed() {
        let (mut merged_seeds, mut crossed_seeds) = (0, 0);
        for seed in 0..64 {
            let (merged, crossed) = model_run(seed, 3_000);
            merged_seeds += u32::from(merged);
            crossed_seeds += u32::from(crossed);
        }
        assert!(merged_seeds >= 60, "VACUOUS: only {merged_seeds} seeds merged a mark");
        assert!(crossed_seeds >= 1, "VACUOUS: no seed held more classes than the cap");
        assert!(
            crossed_seeds <= 32,
            "{crossed_seeds} seeds crossed: equality is checked too rarely"
        );
    }

    /// One seeded history; returns whether marks merged and whether the cap
    /// was crossed.
    fn model_run(seed: u64, events: u32) -> (bool, bool) {
        let mut rig = Rig::new();
        let mut rng = SplitMix64::new(seed);
        // A fifth of the seeds hold barriers back: more classes than the cap.
        let hold = seed.is_multiple_of(5);
        let mut merged = false;
        for _ in 0..events {
            match rng.next_below(20) {
                0..=5 => {
                    for _ in 0..rng.next_below(4) {
                        let own = rng.next_below(3) != 0;
                        rig.durable.stage(own);
                    }
                    rig.tick();
                    merged |= rig.marks.len() < rig.list.marks.len();
                }
                6..=9 => {
                    // At most 8 frames in flight, the staging ring's bound.
                    if rig.durable.unwritten.len() < 8 {
                        let barrier = rng.next_below(2) == 0;
                        rig.durable.seal(barrier);
                    }
                }
                10..=14 => {
                    rig.durable.write();
                }
                15 => {
                    rig.durable.standalone();
                }
                _ => {
                    // Barriers complete in any order; a held seed completes
                    // few, so classes pile up past the cap.
                    let pending = rig.durable.ledger.len() as u64;
                    if pending > 0 && (!hold || rng.next_below(8) == 0) {
                        let at = if hold { rng.next_below(pending) } else { 0 };
                        if !rig.durable.complete(at as usize) {
                            rig.durable.complete(rng.next_below(pending) as usize);
                        }
                    }
                }
            }
        }
        (merged, rig.crossings() > 0)
    }
}
