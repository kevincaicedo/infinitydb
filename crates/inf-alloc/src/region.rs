//! RAM region ring for the tiered address space (M4-S01, ADR-0052).
//!
//! One `Region` is a single contiguous virtual-address reservation of a
//! power-of-two size, committed and decommitted in fixed power-of-two
//! pages. The owner (`inf-store`'s `AddressSpace`) maps monotonically
//! growing logical addresses onto it modulo the reservation size — the
//! ring — and guarantees no live range wraps (seal-at-ring-top,
//! ADR-0052 D2). This crate only owns the memory mechanics:
//!
//! - reserve: `mmap(PROT_NONE, MAP_NORESERVE)` — virtual space, no pages;
//! - commit: `mprotect(PROT_READ | PROT_WRITE)` on whole pages (physical
//!   pages materialize on first touch);
//! - decommit: `madvise(DONTNEED)` **then** `mprotect(PROT_NONE)` — pages
//!   go back to the OS immediately (RSS tells the truth in the same
//!   sample the accounting does, L5). Access faults while a page is
//!   decommitted; after a ring offset is recommitted, Rust borrow/custody
//!   and owner-side re-resolution — not the MMU — prevent stale access.
//!
//! Under Miri the reservation is mapped `READ | WRITE` up front and the
//! protection calls are elided (Miri models anonymous mmap/munmap but not
//! mprotect/madvise — the arena precedent); the commit bitmap and every
//! bounds/state assert run identically, so Miri still checks the API
//! contract and slice provenance.

use core::fmt;

/// Construction parameters. Both values are powers of two;
/// `page_bytes` divides `reserve_bytes`.
#[derive(Copy, Clone, Debug)]
pub struct RegionConfig {
    /// Total reservation (the ring size `R`). Power of two.
    pub reserve_bytes: usize,
    /// Commit/decommit granularity. Power of two, divides `reserve_bytes`.
    /// Default 1 MiB (ADR-0052 D4 — Proposed default, S22 A/B vs 256 KiB).
    pub page_bytes: usize,
}

/// Default commit page size (ADR-0052 D4).
pub const REGION_PAGE_BYTES: usize = 1 << 20;

/// Byte-exact attribution snapshot (L5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RegionReport {
    /// Reserved virtual bytes (constant for the region's lifetime).
    pub reserved_bytes: u64,
    /// Committed bytes — the region's physical footprint upper bound.
    pub committed_bytes: u64,
}

/// A page-granular committed window over one contiguous reservation.
///
/// Not `Send`/`Sync` (raw base pointer): one cell, one owner (L1).
pub struct Region {
    base: *mut u8,
    reserve_bytes: usize,
    page_bytes: usize,
    /// One flag per page; the assert substrate for every commit/decommit/
    /// access. `pages ≤ reserve/page` stays small (1 GiB / 1 MiB = 1024).
    committed: Box<[bool]>,
    committed_pages: usize,
}

impl Region {
    /// Reserves the ring. `None` when the OS refuses the reservation
    /// (operating error — namespace creation surfaces it, never panics).
    ///
    /// # Panics
    /// Panics on config violations (non-power-of-two sizes, page not
    /// dividing the reservation) — programmer errors, not inputs.
    pub fn new(config: RegionConfig) -> Option<Region> {
        assert!(config.reserve_bytes.is_power_of_two(), "reserve_bytes must be a power of two");
        assert!(config.page_bytes.is_power_of_two(), "page_bytes must be a power of two");
        assert!(config.page_bytes <= config.reserve_bytes, "page larger than reservation");
        let pages = config.reserve_bytes / config.page_bytes;
        let base = map_reservation(config.reserve_bytes)?;
        Some(Region {
            base,
            reserve_bytes: config.reserve_bytes,
            page_bytes: config.page_bytes,
            committed: vec![false; pages].into_boxed_slice(),
            committed_pages: 0,
        })
    }

    /// Commit granularity in bytes.
    #[inline]
    pub fn page_bytes(&self) -> usize {
        self.page_bytes
    }

    /// Total pages in the reservation.
    #[inline]
    pub fn pages(&self) -> usize {
        self.committed.len()
    }

    /// The ring size `R`.
    #[inline]
    pub fn reserve_bytes(&self) -> usize {
        self.reserve_bytes
    }

    /// Byte-exact snapshot (L5).
    pub fn report(&self) -> RegionReport {
        RegionReport {
            reserved_bytes: self.reserve_bytes as u64,
            committed_bytes: (self.committed_pages * self.page_bytes) as u64,
        }
    }

    /// Commits `count` pages starting at `first_page` (no wrap — the
    /// address-space owner splits ring-wrapping runs).
    ///
    /// # Panics
    /// Panics if any page in the range is already committed — the caller
    /// tracks the committed window exactly; drift is a desync, not an
    /// operating condition.
    pub fn commit_pages(&mut self, first_page: usize, count: usize) {
        assert!(count > 0, "empty commit");
        assert!(first_page + count <= self.pages(), "commit past reservation");
        for page in first_page..first_page + count {
            assert!(!self.committed[page], "double commit of page {page}");
            self.committed[page] = true;
        }
        self.committed_pages += count;
        protect_read_write(self.base, first_page * self.page_bytes, count * self.page_bytes);
    }

    /// Decommits `count` pages starting at `first_page` (no wrap). The
    /// pages return to the OS now; the range faults on any later access
    /// until recommitted.
    ///
    /// # Panics
    /// Panics if any page in the range is not committed (desync).
    pub fn decommit_pages(&mut self, first_page: usize, count: usize) {
        assert!(count > 0, "empty decommit");
        assert!(first_page + count <= self.pages(), "decommit past reservation");
        for page in first_page..first_page + count {
            assert!(self.committed[page], "decommit of uncommitted page {page}");
            self.committed[page] = false;
        }
        self.committed_pages -= count;
        release_and_protect_none(self.base, first_page * self.page_bytes, count * self.page_bytes);
    }

    /// Borrows `len` bytes at ring offset `offset`. The range must sit
    /// inside the reservation and every page it touches must be committed
    /// (debug-asserted per page; the bounds check is always on).
    #[inline]
    pub fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        self.check_range(offset, len);
        // SAFETY: offset+len bounds-checked against the reservation; the
        // touched pages are committed (debug-asserted above, guaranteed by
        // the owner's watermark discipline); the mapping outlives `self`;
        // `&self` provides aliasing discipline for the returned borrow.
        unsafe { core::slice::from_raw_parts(self.base.add(offset), len) }
    }

    /// Mutable variant of [`bytes`](Self::bytes).
    #[inline]
    pub fn bytes_mut(&mut self, offset: usize, len: usize) -> &mut [u8] {
        self.check_range(offset, len);
        // SAFETY: as `bytes`, and `&mut self` makes the borrow exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(offset), len) }
    }

    #[inline]
    fn check_range(&self, offset: usize, len: usize) {
        assert!(len > 0, "empty region access");
        assert!(
            offset.checked_add(len).is_some_and(|end| end <= self.reserve_bytes),
            "region access out of bounds"
        );
        #[cfg(debug_assertions)]
        for page in offset / self.page_bytes..(offset + len - 1) / self.page_bytes + 1 {
            debug_assert!(self.committed[page], "access to uncommitted page {page}");
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: base/reserve_bytes are exactly the live mapping created
        // in `map_reservation`; the region owns it for its whole lifetime.
        unsafe { libc::munmap(self.base.cast(), self.reserve_bytes) };
    }
}

impl fmt::Debug for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Region {{ {:?}, page_bytes: {} }}", self.report(), self.page_bytes)
    }
}

#[cfg(not(miri))]
fn map_reservation(len: usize) -> Option<*mut u8> {
    // SAFETY: anonymous private PROT_NONE reservation; no fixed address
    // requested; result checked against MAP_FAILED before use.
    let base = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    Some(base.cast())
}

#[cfg(miri)]
fn map_reservation(len: usize) -> Option<*mut u8> {
    // Miri models anonymous mmap but not mprotect: map READ|WRITE up
    // front; commit/decommit stay pure bookkeeping under Miri.
    // SAFETY: anonymous private mapping, result checked before use.
    let base = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    Some(base.cast())
}

#[cfg(not(miri))]
fn protect_read_write(base: *mut u8, offset: usize, len: usize) {
    // SAFETY: [offset, offset+len) is page-aligned and inside the live
    // reservation (asserted by the caller against the page bitmap).
    let rc =
        unsafe { libc::mprotect(base.add(offset).cast(), len, libc::PROT_READ | libc::PROT_WRITE) };
    assert!(rc == 0, "mprotect(commit) failed: {}", std::io::Error::last_os_error());
}

#[cfg(miri)]
fn protect_read_write(_base: *mut u8, _offset: usize, _len: usize) {}

#[cfg(not(miri))]
fn release_and_protect_none(base: *mut u8, offset: usize, len: usize) {
    // Linux MADV_DONTNEED drops the pages now (next touch would fault via
    // PROT_NONE anyway); non-Linux dev tiers use MADV_FREE (correctness
    // tier only — RSS honesty is gated on Linux, ADR-0052 D3).
    #[cfg(target_os = "linux")]
    const ADVICE: libc::c_int = libc::MADV_DONTNEED;
    #[cfg(not(target_os = "linux"))]
    const ADVICE: libc::c_int = libc::MADV_FREE;
    // SAFETY: page-aligned range inside the live reservation (caller-
    // asserted); DONTNEED/FREE on private anonymous memory discards
    // committed pages, which is exactly the contract of decommit.
    let rc = unsafe { libc::madvise(base.add(offset).cast(), len, ADVICE) };
    assert!(rc == 0, "madvise(decommit) failed: {}", std::io::Error::last_os_error());
    // SAFETY: same range; PROT_NONE makes access fault until recommit.
    let rc = unsafe { libc::mprotect(base.add(offset).cast(), len, libc::PROT_NONE) };
    assert!(rc == 0, "mprotect(decommit) failed: {}", std::io::Error::last_os_error());
}

#[cfg(miri)]
fn release_and_protect_none(_base: *mut u8, _offset: usize, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Region {
        // 16 pages of 4 KiB — tiny ring for tests.
        Region::new(RegionConfig { reserve_bytes: 1 << 16, page_bytes: 1 << 12 })
            .expect("reservation")
    }

    #[test]
    fn commit_write_read_round_trip() {
        let mut region = small();
        region.commit_pages(0, 2);
        region.bytes_mut(0, 8192).fill(0xAB);
        assert!(region.bytes(0, 8192).iter().all(|&b| b == 0xAB));
        assert_eq!(region.report().committed_bytes, 8192);
    }

    #[test]
    fn writes_span_page_boundaries() {
        let mut region = small();
        region.commit_pages(0, 3);
        // A "record" crossing two page boundaries stays one contiguous slice.
        let span = region.bytes_mut(4096 - 100, 4096 + 200);
        span.fill(0x5A);
        assert_eq!(region.bytes(4096 - 100, 4096 + 200).len(), 4096 + 200);
        assert!(region.bytes(4096, 100).iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn decommit_then_recommit_reuses_pages() {
        let mut region = small();
        region.commit_pages(0, 4);
        region.bytes_mut(0, 4 * 4096).fill(0xEE);
        region.decommit_pages(0, 2);
        assert_eq!(region.report().committed_bytes, 2 * 4096);
        region.commit_pages(0, 2);
        assert_eq!(region.report().committed_bytes, 4 * 4096);
        // Fresh pages read as zero on Linux after DONTNEED; under Miri the
        // pages were never dropped. Either way the API allows access again.
        let _ = region.bytes(0, 2 * 4096);
    }

    #[test]
    #[should_panic(expected = "double commit")]
    fn double_commit_panics() {
        let mut region = small();
        region.commit_pages(1, 2);
        region.commit_pages(2, 1);
    }

    #[test]
    #[should_panic(expected = "decommit of uncommitted")]
    fn decommit_uncommitted_panics() {
        let mut region = small();
        region.decommit_pages(0, 1);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn out_of_bounds_access_panics() {
        let mut region = small();
        region.commit_pages(0, 16);
        let _ = region.bytes_mut(1 << 16, 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "uncommitted page")]
    fn access_to_uncommitted_page_panics_in_debug() {
        let region = small();
        let _ = region.bytes(0, 1);
    }
}
