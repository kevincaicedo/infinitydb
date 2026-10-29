//! The ack class of a staged durable seq: which barrier makes it durable.
//! The extent-reclaim marks (ADR-0061 D5) hold one mark per class, so this
//! module is their whole read of the durable cell: the class, its search
//! over the ack map, and the trait a MAINTAIN tick reads through.

use inf_log::Lsn;
use inf_log::fs::SegmentFs;

use super::DurableCell;

/// Which barrier makes a staged durable seq durable: the class of a reclaim
/// mark taken at that seq (ADR-0061 D5's marks hold one mark per class).
/// Two seqs of one class are made durable by one completion, since the
/// watermark only ever takes the last seq of an ack-map entry it pops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AckClass {
    /// At or below the watermark: durable already.
    Covered,
    /// Released by the ack map's entry whose last seq is this one: the
    /// first entry whose last seq is at or above the seq asked about.
    Frame(u64),
    /// Staged and not yet sealed: the next sealed frame carries it, the
    /// staging builder sealing whole into one frame.
    Builder,
}

/// What one MAINTAIN tick reads of the cell's durable plane: one handle,
/// read in the same step, never across a suspension.
pub(crate) trait AckClasses {
    /// The last durable seq assigned to a staged record.
    fn staged_seq(&self) -> u64;
    /// The fsync watermark: every seq at or below it is durable.
    fn durable_seq(&self) -> u64;
    /// Which barrier makes `seq` durable.
    fn ack_class(&self, seq: u64) -> AckClass;
}

/// The reclaim marks' read of this cell: the last staged seq, the
/// watermark, and the ack class of a seq, in one MAINTAIN step.
impl<F: SegmentFs> AckClasses for DurableCell<F> {
    fn staged_seq(&self) -> u64 {
        self.last_seq
    }

    fn durable_seq(&self) -> u64 {
        self.ack_gate.watermark()
    }

    /// `Covered` at or below the watermark, else the ack-map entry that
    /// will release `seq`, else the staging builder: one binary search of
    /// the map's two slices, no allocation.
    fn ack_class(&self, seq: u64) -> AckClass {
        let (front, back) = self.frame_seqs.as_slices();
        ack_class_of(front, back, self.ack_gate.watermark(), seq)
    }
}

/// The class of `seq` over the ack map `(front, back)` (its two slices, in
/// order) and the watermark: one binary search of the map, whose entries'
/// seqs strictly increase.
pub(crate) fn ack_class_of(
    front: &[(Lsn, u64)],
    back: &[(Lsn, u64)],
    watermark: u64,
    seq: u64,
) -> AckClass {
    // Canary: every seq reads as the builder's, so a tick takes any two
    // marks for one class and the back is replaced by every later pair.
    #[cfg(inf_canary_epoch_mark_class_blind)]
    {
        let _ = (front, back, watermark, seq);
        AckClass::Builder
    }
    // Canary: every seq above the watermark reads as a class of its own, so
    // no two marks merge and each tick that staged a write takes a mark:
    // inside one barrier period the marks fill to their cap and cross it.
    #[cfg(inf_canary_epoch_mark_class_per_seq)]
    {
        let _ = (front, back);
        if seq <= watermark { AckClass::Covered } else { AckClass::Frame(seq) }
    }
    #[cfg(not(any(inf_canary_epoch_mark_class_blind, inf_canary_epoch_mark_class_per_seq)))]
    {
        if seq <= watermark {
            return AckClass::Covered;
        }
        let first_at_or_above = |entries: &[(Lsn, u64)]| {
            entries.get(entries.partition_point(|&(_, last)| last < seq)).map(|&(_, last)| last)
        };
        match first_at_or_above(front).or_else(|| first_at_or_above(back)) {
            Some(last) => AckClass::Frame(last),
            None => AckClass::Builder,
        }
    }
}
