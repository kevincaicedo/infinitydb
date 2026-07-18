//! The logical address space (M4-S01, master plan §9, M4 plan §3.1,
//! ADR-0051/ADR-0052) — one per cell per **durable-tiered** namespace.
//!
//! Records live at monotonically-growing 48-bit logical addresses. Four
//! watermarks partition the space (§3.1 watermark order, normative):
//!
//! ```text
//!   head ≤ flushed ≤ ro_boundary ≤ tail
//!   [.. cold (tier files) ..)[.. read-only RAM ..)[.. mutable RAM ..)
//!    ▲ head                   ▲ ro_boundary        ▲ tail
//!         ▲ flushed — inside the read-only range: only sealed read-only
//!           bytes ever flush; RAM pages release only below `flushed`.
//! ```
//!
//! The lifecycle of an address range: allocated at the tail (mutable,
//! in-place updates — M4-S05) → sealed as `ro_boundary` advances over it
//! (immutable; update = copy-to-tail — M4-S06) → flushed to a tier file
//! (fdatasync'd — M4-S11) → RAM pages released as `head` advances
//! (disk-only; reads suspend — M4-S08). Two §3.1 corollaries this module
//! enforces at the API layer, not by convention: [`bytes_mut`]
//! (AddressSpace::bytes_mut) refuses addresses below `ro_boundary` (an
//! in-place update there would silently invalidate a flushed disk copy),
//! and [`advance_head`](AddressSpace::advance_head) refuses to pass
//! `flushed` (dropping unflushed bytes is data loss).
//!
//! RAM residency is a fixed ring (`inf_alloc::Region`, ADR-0052): resolve
//! is `base + ((addr − life_origin) & (R − 1))`, allocation seals to the
//! ring top rather than letting a record wrap (the hole is counted dead
//! and tripwired), and the committed page window slides with the
//! watermarks. Addresses are never reused within a boot life; recovery
//! re-appends into a **new life** at a fresh origin (§3.1 "addresses are
//! per-life") — content, never addresses, is what oracles compare.
//!
//! Memory-mode namespaces have **no** `AddressSpace` (ADR-0051): the
//! degenerate case is the absence of this object, not a branch inside it.

use inf_alloc::{Region, RegionConfig};
use inf_foundation::{LocalCounter, LogicalAddr};

/// Region classification for one address — the resolver's answer
/// (M4 plan §3.1). Two compares, no loads beyond the watermark line.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AddrClass {
    /// `addr ≥ ro_boundary`: RAM tail, in-place updates allowed (M4-S05).
    Mutable,
    /// `head ≤ addr < ro_boundary`: RAM, immutable — update copies to the
    /// tail (M4-S06).
    ReadOnly,
    /// `addr < head`: disk-only — the caller maps `tier file base + delta`
    /// and reads through `IoToken` suspension (M4-S04/S08). Never
    /// synchronously readable from here.
    Cold,
}

/// Construction parameters. The ring size is derived from the namespace
/// memory budget by the caller (S07/S19 own budget policy).
#[derive(Copy, Clone, Debug)]
pub struct AddressSpaceConfig {
    /// Ring reservation `R` in bytes. Power of two (ADR-0052 D1).
    pub reserve_bytes: usize,
    /// Commit/decommit page size (ADR-0052 D4; default
    /// [`inf_alloc::REGION_PAGE_BYTES`]).
    pub page_bytes: usize,
    /// Where this boot life's RAM window begins (0 for a fresh namespace;
    /// recovery re-appends into a new life — §3.1).
    pub life_origin: LogicalAddr,
}

/// Always-on tiering code-path counters (M4-S03): cheap, cell-local, and
/// asserted **identically zero** in memory-mode/cache-profile runs — the
/// §3.3 "provably unexecuted" rule made mechanical. Aggregated per
/// keyspace and reported through `INFO tiering`.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct TieringCounters {
    /// Tail allocations served (every record entering the space).
    pub tail_allocs: u64,
    /// Ring-top seals (ADR-0052 D2) — expect ≈ allocated_bytes / R.
    pub seal_holes: u64,
    /// Bytes skipped by ring-top seals (dead on arrival, tripwired).
    pub seal_hole_bytes: u64,
    /// Region pages committed (tail fill).
    pub region_commit_pages: u64,
    /// Region pages decommitted (release below `flushed`).
    pub region_decommit_pages: u64,
    /// Resolver answers of [`AddrClass::Cold`] — cold-read candidates.
    pub cold_resolves: u64,
}

/// Byte-exact attribution snapshot (L5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AddressSpaceReport {
    /// Reserved virtual bytes (the ring `R`).
    pub reserved_bytes: u64,
    /// Committed bytes — the RAM-window physical footprint bound.
    pub committed_bytes: u64,
    /// Total bytes allocated this life (`tail − life_origin`).
    pub allocated_bytes: u64,
    /// Sealed-dead bytes this life (ring-top holes; S06 adds dead records
    /// at its repoint hook — the S14 live-set input attaches there).
    pub dead_bytes: u64,
}

/// One namespace's logical address space on one cell (L1: single owner,
/// no shared state — `Region` holds a raw pointer, so this is `!Send` by
/// construction).
pub struct AddressSpace {
    region: Region,
    /// `R − 1` — the resolve mask (ADR-0052 D1).
    ring_mask: u64,
    page_bytes: u64,
    life_origin: u64,
    head: u64,
    flushed: u64,
    ro_boundary: u64,
    tail: u64,
    /// Committed window bounds, page-aligned, **relative to the origin**
    /// (monotone; the ring mapping happens at the `Region` boundary).
    commit_floor_rel: u64,
    commit_top_rel: u64,
    dead_bytes: u64,
    counters: TieringCounters,
    /// Interior-mutable: the resolver is `&self` on the hottest path.
    cold_resolves: LocalCounter,
}

impl AddressSpace {
    /// Reserves the ring. `None` when the OS refuses the reservation —
    /// namespace creation surfaces that as a typed error, never a panic.
    ///
    /// # Panics
    /// Panics on config violations (non-power-of-two ring, ring smaller
    /// than four pages) — programmer errors.
    pub fn new(config: AddressSpaceConfig) -> Option<AddressSpace> {
        assert!(config.reserve_bytes.is_power_of_two(), "ring must be a power of two");
        assert!(config.reserve_bytes >= 4 * config.page_bytes, "ring smaller than four pages");
        let region = Region::new(RegionConfig {
            reserve_bytes: config.reserve_bytes,
            page_bytes: config.page_bytes,
        })?;
        let origin = config.life_origin.to_raw();
        Some(AddressSpace {
            region,
            ring_mask: config.reserve_bytes as u64 - 1,
            page_bytes: config.page_bytes as u64,
            life_origin: origin,
            head: origin,
            flushed: origin,
            ro_boundary: origin,
            tail: origin,
            commit_floor_rel: 0,
            commit_top_rel: 0,
            dead_bytes: 0,
            counters: TieringCounters::default(),
            cold_resolves: LocalCounter::new(),
        })
    }

    // ---- the resolver (M4-S01 budget: ≤ 2 ns added on RAM addresses) ----

    /// Classifies an address: two watermark compares, nothing else. The
    /// caller already holds the slot's address; RAM access goes through
    /// [`bytes`](Self::bytes)/[`bytes_mut`](Self::bytes_mut), cold reads
    /// through the tier-file mapping (S04+).
    #[inline]
    pub fn resolve(&self, addr: LogicalAddr) -> AddrClass {
        let a = addr.to_raw();
        debug_assert!(a < self.tail, "resolve past tail");
        if a >= self.ro_boundary {
            AddrClass::Mutable
        } else if a >= self.head {
            AddrClass::ReadOnly
        } else {
            self.cold_resolves.incr();
            AddrClass::Cold
        }
    }

    /// Borrows `len` RAM-resident bytes at `addr` (mutable or read-only
    /// region — the §3.1 read path; cold bytes are *not here*).
    ///
    /// # Panics
    /// Panics when `addr < head` — asking RAM for cold bytes is a
    /// resolver-bypass bug, not an operating condition.
    #[inline]
    pub fn bytes(&self, addr: LogicalAddr, len: usize) -> &[u8] {
        let a = addr.to_raw();
        assert!(a >= self.head, "RAM read below head");
        debug_assert!(a + len as u64 <= self.tail, "read past tail");
        let offset = (a - self.life_origin) & self.ring_mask;
        debug_assert!(
            offset + len as u64 <= self.ring_mask + 1,
            "record wraps the ring — seal invariant broken"
        );
        self.region.bytes(offset as usize, len)
    }

    /// Mutably borrows `len` bytes at `addr` — **mutable region only**.
    ///
    /// # Panics
    /// Panics when `addr < ro_boundary`: an in-place write below the
    /// boundary would silently invalidate a (present or future) disk copy
    /// — the §3.1 corollary this API makes unrepresentable.
    #[inline]
    pub fn bytes_mut(&mut self, addr: LogicalAddr, len: usize) -> &mut [u8] {
        let a = addr.to_raw();
        assert!(a >= self.ro_boundary, "in-place write below ro_boundary");
        debug_assert!(a + len as u64 <= self.tail, "write past tail");
        let offset = (a - self.life_origin) & self.ring_mask;
        self.region.bytes_mut(offset as usize, len)
    }

    // ---- tail allocation ----

    /// Allocates `len` bytes at the tail, committing ring pages as
    /// needed. `None` when the RAM window (in whole pages) would exceed
    /// the ring — the budget/backpressure signal S07 turns into
    /// suspend-on-flushed-progress — or at the 48-bit end of the space.
    ///
    /// May advance the tail past a ring-top hole first (ADR-0052 D2);
    /// hole bytes are dead on arrival, counted, and tripwired. No state
    /// changes on refusal.
    pub fn alloc(&mut self, len: usize) -> Option<LogicalAddr> {
        assert!(len > 0, "empty allocation");
        let ring = self.ring_mask + 1;
        assert!((len as u64) <= ring / 2, "allocation exceeds half the ring");
        // Prospective values first — refusal must mutate nothing.
        let rel_tail = self.tail - self.life_origin;
        let ring_offset = rel_tail & self.ring_mask;
        let hole = if ring_offset + len as u64 > ring { ring - ring_offset } else { 0 };
        let alloc_rel = rel_tail + hole;
        let new_rel_tail = alloc_rel + len as u64;
        let new_top_rel = self.page_ceil(new_rel_tail).max(self.commit_top_rel);
        if new_top_rel - self.commit_floor_rel > ring {
            return None; // RAM window (whole pages) would exceed the ring.
        }
        // Monotonic space: refuse at the 48-bit end, never wrap.
        let addr = LogicalAddr::from_raw(self.life_origin.checked_add(alloc_rel)?)?;
        addr.advanced(len as u64)?;
        if new_top_rel > self.commit_top_rel {
            self.commit_rel_pages(self.commit_top_rel, new_top_rel);
            self.commit_top_rel = new_top_rel;
        }
        if hole > 0 {
            self.counters.seal_holes += 1;
            self.counters.seal_hole_bytes += hole;
            self.dead_bytes += hole;
        }
        self.counters.tail_allocs += 1;
        self.tail = self.life_origin + new_rel_tail;
        self.assert_watermark_order();
        Some(addr)
    }

    // ---- watermark advancement (each is monotone; §3.1 order asserted) ----

    /// Seals `[ro_boundary, to)`: those bytes are immutable from now on
    /// (updates copy to the tail — S06) and are what flush may cover.
    ///
    /// # Panics
    /// Panics unless `ro_boundary ≤ to ≤ tail`. The caller advances at
    /// record boundaries — this module has no record knowledge (§3.3:
    /// `inf-store` owns addresses; records are the caller's vocabulary).
    pub fn advance_ro_boundary(&mut self, to: LogicalAddr) {
        let t = to.to_raw();
        assert!(t >= self.ro_boundary, "ro_boundary retreat");
        assert!(t <= self.tail, "ro_boundary past tail");
        self.ro_boundary = t;
        self.assert_watermark_order();
    }

    /// Records that a tier file durably covers `[flushed, to)`
    /// (fdatasync'd — S11 advances this only after the barrier).
    ///
    /// # Panics
    /// Panics unless `flushed ≤ to ≤ ro_boundary`: bytes above the
    /// ro-boundary must never flush (§3.1 — their in-place updates would
    /// make the disk copy stale).
    pub fn advance_flushed(&mut self, to: LogicalAddr) {
        let t = to.to_raw();
        assert!(t >= self.flushed, "flushed retreat");
        assert!(t <= self.ro_boundary, "flush above ro_boundary");
        self.flushed = t;
        self.assert_watermark_order();
    }

    /// Releases RAM below `to`: whole pages now strictly below the head
    /// decommit (RSS returns to the OS — ADR-0052 D3).
    ///
    /// # Panics
    /// Panics unless `head ≤ to ≤ flushed`: pages never release above
    /// `flushed` — dropping unflushed bytes is data loss (§3.1).
    pub fn advance_head(&mut self, to: LogicalAddr) {
        let t = to.to_raw();
        assert!(t >= self.head, "head retreat");
        assert!(t <= self.flushed, "page release above flushed");
        self.head = t;
        let new_floor_rel = self.page_floor(t - self.life_origin);
        if new_floor_rel > self.commit_floor_rel {
            self.decommit_rel_pages(self.commit_floor_rel, new_floor_rel);
            self.commit_floor_rel = new_floor_rel;
        }
        self.assert_watermark_order();
    }

    // ---- observation ----

    /// Oldest RAM-resident address (below it: disk only).
    #[inline]
    pub fn head(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.head).expect("watermarks stay 48-bit")
    }

    /// Durably-flushed boundary (§3.1: `head ≤ flushed ≤ ro_boundary`).
    #[inline]
    pub fn flushed(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.flushed).expect("watermarks stay 48-bit")
    }

    /// Mutable/read-only boundary.
    #[inline]
    pub fn ro_boundary(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.ro_boundary).expect("watermarks stay 48-bit")
    }

    /// Next address to be allocated.
    #[inline]
    pub fn tail(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.tail).expect("watermarks stay 48-bit")
    }

    /// This boot life's origin (§3.1 "addresses are per-life").
    #[inline]
    pub fn life_origin(&self) -> LogicalAddr {
        LogicalAddr::from_raw(self.life_origin).expect("origin is 48-bit")
    }

    /// Always-on counters snapshot (S03 scrapes and asserts these).
    pub fn counters(&self) -> TieringCounters {
        TieringCounters { cold_resolves: self.cold_resolves.get(), ..self.counters }
    }

    /// Byte-exact attribution snapshot (L5).
    pub fn report(&self) -> AddressSpaceReport {
        let region = self.region.report();
        AddressSpaceReport {
            reserved_bytes: region.reserved_bytes,
            committed_bytes: region.committed_bytes,
            allocated_bytes: self.tail - self.life_origin,
            dead_bytes: self.dead_bytes,
        }
    }

    /// Dead-byte attribution hook (M4-S06): the copy-to-tail repoint and
    /// the delete path charge the displaced record's bytes here at the
    /// moment the index stops pointing at them — never later (attributing
    /// at compaction-read time would make S14's counters lazy-wrong
    /// forever, per the plan's pitfall note). The address names the
    /// containing range: S14 keys these bytes per tier file from exactly
    /// this coordinate, and S17's blob refcounts ride the same site.
    pub fn note_dead_bytes(&mut self, addr: LogicalAddr, len: u64) {
        let a = addr.to_raw();
        debug_assert!(a >= self.life_origin, "dead range below this life");
        debug_assert!(a + len <= self.tail, "dead range past tail");
        self.dead_bytes += len;
        debug_assert!(self.dead_bytes <= self.tail - self.life_origin, "dead exceeds allocated");
    }

    // ---- internals ----

    #[inline]
    fn page_floor(&self, rel: u64) -> u64 {
        rel & !(self.page_bytes - 1)
    }

    #[inline]
    fn page_ceil(&self, rel: u64) -> u64 {
        (rel + self.page_bytes - 1) & !(self.page_bytes - 1)
    }

    /// Commits the page-aligned relative range `[from, to)`, splitting at
    /// the ring wrap (the `Region` never sees a wrapping run).
    fn commit_rel_pages(&mut self, from_rel: u64, to_rel: u64) {
        self.for_each_ring_run(from_rel, to_rel, |region, first_page, count| {
            region.commit_pages(first_page, count);
        });
        self.counters.region_commit_pages += (to_rel - from_rel) / self.page_bytes;
    }

    /// Decommits the page-aligned relative range `[from, to)`.
    fn decommit_rel_pages(&mut self, from_rel: u64, to_rel: u64) {
        self.for_each_ring_run(from_rel, to_rel, |region, first_page, count| {
            region.decommit_pages(first_page, count);
        });
        self.counters.region_decommit_pages += (to_rel - from_rel) / self.page_bytes;
    }

    fn for_each_ring_run(
        &mut self,
        from_rel: u64,
        to_rel: u64,
        mut apply: impl FnMut(&mut Region, usize, usize),
    ) {
        debug_assert_eq!(from_rel, self.page_floor(from_rel), "unaligned range start");
        debug_assert_eq!(to_rel, self.page_floor(to_rel), "unaligned range end");
        debug_assert!(to_rel - from_rel <= self.ring_mask + 1, "range exceeds the ring");
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

    /// The §3.1 watermark order plus the ring-window bound — asserted
    /// after every mutation (debug builds; compiled out in release).
    #[inline]
    fn assert_watermark_order(&self) {
        debug_assert!(self.life_origin <= self.head, "head below origin");
        debug_assert!(self.head <= self.flushed, "head above flushed");
        debug_assert!(self.flushed <= self.ro_boundary, "flushed above ro_boundary");
        debug_assert!(self.ro_boundary <= self.tail, "ro_boundary above tail");
        debug_assert!(
            self.commit_top_rel - self.commit_floor_rel <= self.ring_mask + 1,
            "committed window exceeds the ring"
        );
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("life_origin", &self.life_origin)
            .field("head", &self.head)
            .field("flushed", &self.flushed)
            .field("ro_boundary", &self.ro_boundary)
            .field("tail", &self.tail)
            .field("report", &self.report())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(ring: usize, page: usize) -> AddressSpace {
        AddressSpace::new(AddressSpaceConfig {
            reserve_bytes: ring,
            page_bytes: page,
            life_origin: LogicalAddr::ZERO,
        })
        .expect("reservation")
    }

    #[test]
    fn alloc_write_read_round_trip() {
        let mut space = space(1 << 16, 1 << 12);
        let addr = space.alloc(100).expect("fits");
        space.bytes_mut(addr, 100).fill(0xAB);
        assert_eq!(space.resolve(addr), AddrClass::Mutable);
        assert!(space.bytes(addr, 100).iter().all(|&b| b == 0xAB));
        assert_eq!(space.counters().tail_allocs, 1);
    }

    #[test]
    fn lifecycle_classifies_through_all_regions() {
        let mut space = space(1 << 16, 1 << 12);
        let a = space.alloc(64).expect("fits");
        let b = space.alloc(64).expect("fits");
        space.advance_ro_boundary(b);
        assert_eq!(space.resolve(a), AddrClass::ReadOnly);
        assert_eq!(space.resolve(b), AddrClass::Mutable);
        space.advance_flushed(b);
        space.advance_head(b);
        assert_eq!(space.resolve(a), AddrClass::Cold);
        assert_eq!(space.counters().cold_resolves, 1);
    }

    #[test]
    fn ring_top_seal_keeps_records_contiguous() {
        let mut space = space(1 << 16, 1 << 12);
        // Fill to 100 bytes short of the ring top (two half-ring-bounded
        // allocations), then allocate 300: the seal skips the 100-byte
        // hole and the record lands ring-aligned.
        let first = space.alloc(1 << 15).expect("fits");
        let _ = space.alloc((1 << 15) - 100).expect("fits");
        space.advance_ro_boundary(space.tail());
        space.advance_flushed(space.tail());
        space.advance_head(space.tail());
        let sealed = space.alloc(300).expect("fits after release");
        assert_eq!(sealed.to_raw(), 1 << 16, "record starts at the ring boundary");
        assert_eq!(space.counters().seal_holes, 1);
        assert_eq!(space.counters().seal_hole_bytes, 100);
        assert_eq!(space.report().dead_bytes, 100);
        space.bytes_mut(sealed, 300).fill(0xCD);
        assert!(space.bytes(sealed, 300).iter().all(|&b| b == 0xCD));
        assert_eq!(space.resolve(first), AddrClass::Cold);
    }

    #[test]
    fn window_exhaustion_refuses_without_mutation() {
        let mut space = space(1 << 16, 1 << 12);
        let a = space.alloc((1 << 15) as usize).expect("half ring");
        let _ = space.alloc((1 << 14) as usize).expect("three quarters");
        let before_tail = space.tail();
        let before = space.counters();
        assert!(space.alloc((1 << 15) as usize).is_none(), "window full");
        assert_eq!(space.tail(), before_tail, "refusal mutates nothing");
        assert_eq!(space.counters(), before);
        // Release the first allocation; the same request then fits.
        space.advance_ro_boundary(space.tail());
        space.advance_flushed(space.tail());
        space.advance_head(a.advanced(1 << 15).expect("fits"));
        assert!(space.alloc((1 << 15) as usize).is_some());
    }

    #[test]
    fn head_advance_decommits_and_recommit_wraps() {
        let mut space = space(1 << 16, 1 << 12);
        for _ in 0..4 {
            space.alloc(1 << 12).expect("page");
        }
        assert_eq!(space.report().committed_bytes, 4 << 12);
        space.advance_ro_boundary(space.tail());
        space.advance_flushed(space.tail());
        space.advance_head(space.tail());
        assert_eq!(space.report().committed_bytes, 0, "release returned the pages");
        // Keep allocating around the ring — recommit crosses the wrap.
        for _ in 0..20 {
            let addr = space.alloc(1 << 12).expect("page");
            space.bytes_mut(addr, 1 << 12).fill(0x11);
            space.advance_ro_boundary(space.tail());
            space.advance_flushed(space.tail());
            space.advance_head(space.tail());
        }
        assert_eq!(space.counters().region_decommit_pages, 24);
    }

    #[test]
    #[should_panic(expected = "in-place write below ro_boundary")]
    fn write_below_ro_boundary_panics() {
        let mut space = space(1 << 16, 1 << 12);
        let a = space.alloc(64).expect("fits");
        space.advance_ro_boundary(space.tail());
        let _ = space.bytes_mut(a, 64);
    }

    #[test]
    #[should_panic(expected = "flush above ro_boundary")]
    fn flush_above_ro_boundary_panics() {
        let mut space = space(1 << 16, 1 << 12);
        let _ = space.alloc(64).expect("fits");
        space.advance_flushed(space.tail());
    }

    #[test]
    #[should_panic(expected = "page release above flushed")]
    fn release_above_flushed_panics() {
        let mut space = space(1 << 16, 1 << 12);
        let _ = space.alloc(64).expect("fits");
        space.advance_ro_boundary(space.tail());
        space.advance_head(space.tail());
    }

    #[test]
    #[should_panic(expected = "RAM read below head")]
    fn ram_read_of_cold_address_panics() {
        let mut space = space(1 << 16, 1 << 12);
        let a = space.alloc(64).expect("fits");
        space.advance_ro_boundary(space.tail());
        space.advance_flushed(space.tail());
        space.advance_head(space.tail());
        let _ = space.bytes(a, 64);
    }

    #[test]
    fn new_life_origin_offsets_the_ring() {
        let origin = LogicalAddr::from_raw(0x10_0000).expect("fits");
        let mut space = AddressSpace::new(AddressSpaceConfig {
            reserve_bytes: 1 << 16,
            page_bytes: 1 << 12,
            life_origin: origin,
        })
        .expect("reservation");
        let a = space.alloc(128).expect("fits");
        assert_eq!(a, origin, "first allocation starts the new life");
        space.bytes_mut(a, 128).fill(0x77);
        assert!(space.bytes(a, 128).iter().all(|&b| b == 0x77));
    }
}
