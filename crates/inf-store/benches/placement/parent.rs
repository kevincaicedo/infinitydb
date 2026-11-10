//! The parent's tiered placement, copied whole for the placement A/B: the
//! address space's tail allocation (its one prospect, the window's fit and
//! the 48-bit end, the ring-top hole's mark on an unbounded list) and the
//! table's seal-mark step (one mark per commit page by a division, on an
//! unbounded list). Only what one placement and the leg's release run is
//! here; it is deleted with this module when the verdict is recorded.
use std::collections::VecDeque;

use inf_alloc::{Region, RegionConfig};
use inf_store::LogicalAddr;

/// The parent's cap on retained flush cuts.
const FLUSH_CUT_CAP: usize = 4096;

#[derive(Copy, Clone)]
struct Prospect {
    len: u64,
    hole: u64,
    start_rel: u64,
    top_rel: u64,
}

/// The parent's address space, the fields one placement and the release
/// read.
pub struct Space {
    region: Region,
    ring_mask: u64,
    page_bytes: u64,
    life_origin: u64,
    head: u64,
    flushed: u64,
    ro_boundary: u64,
    tail: u64,
    commit_floor_rel: u64,
    commit_top_rel: u64,
    window_limit: u64,
    dead_bytes: u64,
    hole_marks: VecDeque<(u64, u64)>,
    flush_cuts: VecDeque<u64>,
    seal_holes: u64,
    seal_hole_bytes: u64,
    tail_allocs: u64,
    region_commit_pages: u64,
    region_decommit_pages: u64,
}

impl Space {
    pub fn new(ring_bytes: usize, page_bytes: usize, window: u64) -> Space {
        let region = Region::new(RegionConfig { reserve_bytes: ring_bytes, page_bytes })
            .expect("the ring reserves");
        Space {
            region,
            ring_mask: ring_bytes as u64 - 1,
            page_bytes: page_bytes as u64,
            life_origin: 0,
            head: 0,
            flushed: 0,
            ro_boundary: 0,
            tail: 0,
            commit_floor_rel: 0,
            commit_top_rel: 0,
            window_limit: window & !(page_bytes as u64 - 1),
            dead_bytes: 0,
            hole_marks: VecDeque::new(),
            flush_cuts: VecDeque::new(),
            seal_holes: 0,
            seal_hole_bytes: 0,
            tail_allocs: 0,
            region_commit_pages: 0,
            region_decommit_pages: 0,
        }
    }

    fn prospect(&self, len: u64) -> Prospect {
        assert!(len > 0, "empty allocation");
        let ring = self.ring_mask + 1;
        assert!(len <= ring / 2, "allocation exceeds half the ring");
        let rel_tail = self.tail - self.life_origin;
        let ring_offset = rel_tail & self.ring_mask;
        let hole = if ring_offset + len > ring { ring - ring_offset } else { 0 };
        let start_rel = rel_tail + hole;
        let top_rel = self.page_ceil(start_rel + len).max(self.commit_top_rel);
        Prospect { len, hole, start_rel, top_rel }
    }

    #[inline]
    fn fits(&self, prospect: Prospect) -> bool {
        prospect.top_rel - self.commit_floor_rel <= self.window_limit
    }

    #[inline]
    fn start_inside(&self, prospect: Prospect) -> Option<LogicalAddr> {
        let start = LogicalAddr::from_raw(self.life_origin.checked_add(prospect.start_rel)?)?;
        start.advanced(prospect.len)?;
        Some(start)
    }

    /// Out of line, as the table's call of the product's `alloc` is.
    #[inline(never)]
    pub fn alloc(&mut self, len: usize) -> Option<LogicalAddr> {
        let prospect = self.prospect(len as u64);
        if !self.fits(prospect) {
            return None;
        }
        let addr = self.start_inside(prospect)?;
        if prospect.top_rel > self.commit_top_rel {
            self.commit_rel_pages(self.commit_top_rel, prospect.top_rel);
            self.commit_top_rel = prospect.top_rel;
        }
        if prospect.hole > 0 {
            self.seal_holes += 1;
            self.seal_hole_bytes += prospect.hole;
            self.dead_bytes += prospect.hole;
            self.hole_marks.push_back((self.tail, prospect.hole));
            debug_assert!(self.hole_marks.len() <= 3, "two ring tops and one pad at most");
        }
        self.tail_allocs += 1;
        self.tail = self.life_origin + prospect.start_rel + prospect.len;
        Some(addr)
    }

    /// The release the leg runs between windows, the parent's three
    /// watermark steps to the tail: seal, flush and release.
    pub fn release_to_tail(&mut self) {
        let t = self.tail;
        assert!(t >= self.ro_boundary, "ro_boundary retreat");
        if t > self.ro_boundary && self.flush_cuts.len() < FLUSH_CUT_CAP {
            self.flush_cuts.push_back(t);
        }
        self.ro_boundary = t;
        assert!(t >= self.flushed, "flushed retreat");
        while let Some(&(start, len)) = self.hole_marks.front() {
            if start + len <= t {
                self.hole_marks.pop_front();
                continue;
            }
            break;
        }
        while self.flush_cuts.front().is_some_and(|&c| c <= t) {
            self.flush_cuts.pop_front();
        }
        self.flushed = t;
        assert!(t >= self.head, "head retreat");
        assert!(t <= self.flushed, "page release above flushed");
        self.head = t;
        let new_floor_rel = self.page_floor(t - self.life_origin);
        if new_floor_rel > self.commit_floor_rel {
            self.decommit_rel_pages(self.commit_floor_rel, new_floor_rel);
            self.commit_floor_rel = new_floor_rel;
        }
    }

    pub fn life_origin(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.life_origin).expect("origin is 48-bit")
    }

    pub fn ro_boundary(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.ro_boundary).expect("watermarks stay 48-bit")
    }

    pub fn page_bytes(&self) -> u64 {
        self.page_bytes
    }

    /// Engagement: allocations served and pages committed.
    pub fn counts(&self) -> (u64, u64) {
        (self.tail_allocs, self.region_commit_pages)
    }

    #[inline]
    fn page_floor(&self, rel: u64) -> u64 {
        rel & !(self.page_bytes - 1)
    }

    #[inline]
    fn page_ceil(&self, rel: u64) -> u64 {
        (rel + self.page_bytes - 1) & !(self.page_bytes - 1)
    }

    fn commit_rel_pages(&mut self, from_rel: u64, to_rel: u64) {
        self.for_each_ring_run(from_rel, to_rel, |region, first_page, count| {
            region.commit_pages(first_page, count);
        });
        self.region_commit_pages += (to_rel - from_rel) / self.page_bytes;
    }

    fn decommit_rel_pages(&mut self, from_rel: u64, to_rel: u64) {
        self.for_each_ring_run(from_rel, to_rel, |region, first_page, count| {
            region.decommit_pages(first_page, count);
        });
        self.region_decommit_pages += (to_rel - from_rel) / self.page_bytes;
    }

    fn for_each_ring_run(
        &mut self,
        from_rel: u64,
        to_rel: u64,
        mut apply: impl FnMut(&mut Region, usize, usize),
    ) {
        let ring_pages = ((self.ring_mask + 1) / self.page_bytes) as usize;
        let mut page = (from_rel / self.page_bytes) as usize;
        let end = (to_rel / self.page_bytes) as usize;
        while page < end {
            let ring_page = page & (ring_pages - 1);
            let run = (end - page).min(ring_pages - ring_page);
            apply(&mut self.region, ring_page, run);
            page += run;
        }
    }
}

/// The parent's seal marks: one per commit page, on an unbounded list.
pub struct Marks {
    seal_marks: VecDeque<LogicalAddr>,
    last_mark_page: u64,
}

impl Marks {
    pub fn new() -> Marks {
        Marks { seal_marks: VecDeque::new(), last_mark_page: u64::MAX }
    }

    /// The parent's `note_seal_mark`, out of line, as the table's is.
    #[inline(never)]
    pub fn note(&mut self, space: &Space, addr: LogicalAddr) {
        let page = addr.offset_from(space.life_origin()) / space.page_bytes();
        if page != self.last_mark_page {
            let ro = space.ro_boundary();
            while self.seal_marks.front().is_some_and(|&mark| mark <= ro) {
                self.seal_marks.pop_front();
            }
            self.seal_marks.push_back(addr);
            self.last_mark_page = page;
        }
    }

    /// Engagement: the marks held.
    pub fn len(&self) -> usize {
        self.seal_marks.len()
    }
}
