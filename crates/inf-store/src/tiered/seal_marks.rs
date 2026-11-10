//! The seal marks of one tiered table (ADR-0053 D2): the record starts its
//! ro-boundary may seal to. Each placement asks one shift and one compare: the
//! first record placed in a stride bucket of commit pages pushes its start.
//! The stride is fixed when the table is built, so that the ring holds at most
//! half the cap's buckets; a window, at most the ring, then holds at most half
//! the cap and one more marks, and the cap is never reached. A seal step lands
//! on a mark, so it may overrun its slice by at most one stride. The marks are
//! a capped deque on the cell's census; at the cap the back mark takes the new
//! start, which coarsens the seal steps and counts as the row's crossing.

use std::rc::Rc;

use inf_foundation::LogicalAddr;
use inf_foundation::bounded::{CapCensus, CapError, CappedDeque, Coalesce, Lossy, Merge};
use inf_foundation::limits::DEQUE_BACKING_BYTES_MAX;

use crate::limits::{SEAL_MARKS, SEAL_MARKS_MAX};

// The marks' one allocation fits a deque's backing bound, so their
// construction never refuses on its layout.
const _: () = assert!(
    SEAL_MARKS_MAX as usize * core::mem::size_of::<SealMark>() <= DEQUE_BACKING_BYTES_MAX,
    "the seal marks fit one deque allocation"
);

/// The buckets a ring may hold: half the cap, so a window holds at most half
/// the cap and one more marks.
const RING_BUCKETS_MAX: u64 = SEAL_MARKS_MAX as u64 / 2;

/// A record start the ro-boundary may seal to. A table's marks strictly
/// increase and lie above its ro-boundary once the stale ones are popped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SealMark(LogicalAddr);

impl SealMark {
    /// The record start.
    pub(crate) fn addr(self) -> LogicalAddr {
        self.0
    }
}

/// A mark may merge into a later one: both are record starts, so a seal step
/// that lands on the later still ends on a record boundary; it only seals
/// more at once.
impl Lossy for SealMark {}

/// The merge at the cap: the back mark takes the new, later start.
pub(crate) struct LaterStart;

impl Merge<SealMark> for LaterStart {
    fn merge(back: &mut SealMark, new: SealMark) {
        debug_assert!(back.0 < new.0, "a later start replaces the back mark");
        *back = new;
    }
}

/// One tiered table's seal marks and the stride that buckets them.
pub(crate) struct SealMarks {
    marks: CappedDeque<SealMark, Coalesce<LaterStart>>,
    /// `log2` of a bucket's bytes: the commit page's and the stride's, so a
    /// placement's bucket is one shift of its offset from the life origin.
    bucket_shift: u32,
    /// The newest mark's bucket; `NO_MARK_BUCKET` before the first.
    last_bucket: u64,
}

/// No bucket: an offset is below 2⁴⁸, so a bucket never reaches it.
const NO_MARK_BUCKET: u64 = u64::MAX;

impl SealMarks {
    /// The marks of a table whose ring is `ring_bytes` over commit pages of
    /// `page_bytes` (both powers of two), on `census` under `SEAL_MARKS`: one
    /// allocation of `SEAL_MARKS_MAX` marks, never grown.
    ///
    /// # Errors
    /// The deque's construction refusal: the allocator refused the backing,
    /// or the census refused the row.
    pub(crate) fn new(
        ring_bytes: u64,
        page_bytes: u64,
        census: &Rc<CapCensus>,
    ) -> Result<SealMarks, CapError> {
        debug_assert!(ring_bytes.is_power_of_two(), "the ring is a power of two");
        debug_assert!(page_bytes.is_power_of_two(), "a commit page is a power of two");
        let page_shift = page_bytes.trailing_zeros();
        let ring_pages = ring_bytes >> page_shift;
        let stride_shift = Self::stride_shift(ring_pages);
        Ok(SealMarks {
            marks: CappedDeque::new(SEAL_MARKS, census)?,
            bucket_shift: page_shift + stride_shift,
            last_bucket: NO_MARK_BUCKET,
        })
    }

    /// The stride, in `log2` pages: the least that buckets `ring_pages` (a
    /// power of two) into at most `RING_BUCKETS_MAX`.
    fn stride_shift(ring_pages: u64) -> u32 {
        // Canary: the stride is one page whatever the ring, so a ring past
        // the cap's reach fills the marks and the back takes later starts.
        if cfg!(inf_canary_seal_mark_stride_one) {
            return 0;
        }
        ring_pages.trailing_zeros().saturating_sub(RING_BUCKETS_MAX.trailing_zeros())
    }

    /// A record placed at `addr`, an offset `offset` from the life origin:
    /// the first record in its bucket pushes its start, after the marks the
    /// ro-boundary (`ro`, read only then) passed pop.
    #[inline]
    pub(crate) fn note(
        &mut self,
        addr: LogicalAddr,
        offset: u64,
        ro: impl FnOnce() -> LogicalAddr,
    ) {
        let bucket = offset >> self.bucket_shift;
        if bucket != self.last_bucket {
            self.push(addr, ro());
            self.last_bucket = bucket;
        }
    }

    /// The bucket's first record: stale marks pop, then its start is pushed,
    /// or at the cap merged into the back.
    fn push(&mut self, addr: LogicalAddr, ro: LogicalAddr) {
        while self.marks.front().is_some_and(|mark| mark.0 <= ro) {
            self.marks.pop_front();
        }
        debug_assert!(
            self.marks.back().is_none_or(|back| back.0 < addr),
            "seal marks strictly increase"
        );
        self.marks.push(SealMark(addr));
    }

    /// The oldest mark.
    pub(crate) fn front(&self) -> Option<SealMark> {
        self.marks.front().copied()
    }

    /// Pops the oldest mark: a seal step passed or took it.
    pub(crate) fn pop_front(&mut self) {
        self.marks.pop_front();
    }

    /// Marks held, at most `SEAL_MARKS_MAX`.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.marks.len()
    }

    /// The stride's bytes: the commit page times the stride's pages.
    pub(crate) fn stride_bytes(&self) -> u64 {
        1 << self.bucket_shift
    }

    /// The marks, oldest first.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = SealMark> + '_ {
        self.marks.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use inf_foundation::CellId;

    use super::*;

    const PAGE: u64 = 4 << 10;

    /// Marks at a stride of one 4 KiB page, whatever the ring: the cap's
    /// edge, which the stride keeps every ring from reaching.
    fn marks_at_stride_one(census: &Rc<CapCensus>) -> SealMarks {
        SealMarks {
            marks: CappedDeque::new(SEAL_MARKS, census).expect("built"),
            bucket_shift: PAGE.trailing_zeros(),
            last_bucket: NO_MARK_BUCKET,
        }
    }

    /// One record at the start of each of `pages` pages above page 0, the
    /// ro-boundary at the origin: every page's record pushes a mark.
    fn note_pages(marks: &mut SealMarks, pages: u64) {
        for page in 1..=pages {
            let offset = page * PAGE;
            let addr = LogicalAddr::from_raw(offset).expect("48-bit");
            marks.note(addr, offset, || LogicalAddr::ZERO);
        }
    }

    /// The cap's edge at a stride of one page: `SEAL_MARKS_MAX − 1` and
    /// `SEAL_MARKS_MAX` buckets keep one mark each and cross nothing; one
    /// more holds the cap, counts one crossing, and the back mark is the
    /// newest start (`LaterStart`), so every mark is still a record start.
    #[test]
    fn the_seal_marks_cap_admits_its_last_mark_and_merges_the_next() {
        for count in [SEAL_MARKS_MAX - 1, SEAL_MARKS_MAX] {
            let census = CapCensus::new(CellId(0)).0;
            let mut marks = marks_at_stride_one(&census);
            note_pages(&mut marks, u64::from(count));
            let held = marks.len();
            assert_eq!(
                held, count as usize,
                "seal-marks-edge-short — {held} seal marks after {count} buckets"
            );
            let row = census.row(SEAL_MARKS.name()).expect("the seal marks' row");
            assert_eq!((row.high_water, row.crossings), (count, 0));
        }
        let census = CapCensus::new(CellId(0)).0;
        let mut marks = marks_at_stride_one(&census);
        let past = u64::from(SEAL_MARKS_MAX) + 1;
        note_pages(&mut marks, past);
        assert_eq!(marks.len(), SEAL_MARKS_MAX as usize, "the cap holds");
        let row = census.row(SEAL_MARKS.name()).expect("the seal marks' row");
        assert_eq!(row.crossings, 1, "the merge at the cap counts");
        let newest = marks.iter().last().expect("a back mark");
        assert_eq!(newest.addr().to_raw(), past * PAGE, "the back took the newest start");
    }
}
