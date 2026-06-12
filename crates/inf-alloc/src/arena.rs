//! Record arena (M0-S13): size-class slab allocator over mmap'd chunks with
//! byte-exact accounting (L5) — the anti-64-byte-entry (RC-3).
//!
//! ## Shape
//! - **Chunks**: anonymous-mmap regions (default 2 MiB). Slab chunks belong
//!   to one size class and are carved by **bump allocation** (freed slots go
//!   to an intrusive per-class free list, so untouched tail pages stay
//!   uncommitted — lazy RSS). Allocations larger than `chunk_size / 4` get a
//!   dedicated page-rounded mapping ("huge") and unmap on free.
//! - **Addresses**: [`ArenaAddr`] packs `{chunk:27, offset:21}` into 48 bits
//!   — exactly the address width the index slot format reserves (§7.3).
//!   Huge chunks use offset 0, so the packing never overflows regardless of
//!   allocation size.
//! - **Accounting**: `live_bytes` counts *requested* bytes; `resident_bytes`
//!   counts mapped bytes; slack is the difference. No global allocator on
//!   this path, no atomics (cell-local, L1).
//!
//! ## Contract
//! `free`/`bytes`/`grow_in_place` take the allocation's length because the
//! arena stores no per-allocation metadata (that is the point: the record
//! header already knows its size). Passing a different `len` than `alloc`
//! gave is a logic error: it corrupts *accounting* (and `debug_assert`s
//! where cheap) but cannot escape mapped memory — `bytes` bounds-checks
//! against the owning chunk.

use core::fmt;

/// Configuration for [`Arena::new`].
#[derive(Copy, Clone, Debug)]
pub struct ArenaConfig {
    /// Slab chunk size; power of two, `>= 64 KiB`. Default 2 MiB.
    pub chunk_size: usize,
    /// Resident-byte budget: `alloc` returns `None` rather than mapping
    /// beyond it (the backpressure seam — bounded everything).
    pub max_resident: Option<usize>,
}

impl Default for ArenaConfig {
    fn default() -> ArenaConfig {
        ArenaConfig { chunk_size: 2 << 20, max_resident: None }
    }
}

/// Packed 48-bit arena address: `{chunk_idx:27, offset:21}`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ArenaAddr(u64);

const OFFSET_BITS: u32 = 21;
const OFFSET_MASK: u64 = (1 << OFFSET_BITS) - 1;
const MAX_ADDR: u64 = (1 << 48) - 1;

impl ArenaAddr {
    /// Raw 48-bit value — what index slots store (§7.3 `addr:48`).
    #[inline]
    pub fn to_raw(self) -> u64 {
        self.0
    }

    /// Rebuilds from a raw 48-bit value; `None` if out of range.
    #[inline]
    pub fn from_raw(raw: u64) -> Option<ArenaAddr> {
        (raw <= MAX_ADDR).then_some(ArenaAddr(raw))
    }

    #[inline]
    fn chunk(self) -> usize {
        (self.0 >> OFFSET_BITS) as usize
    }

    #[inline]
    fn offset(self) -> usize {
        (self.0 & OFFSET_MASK) as usize
    }

    #[inline]
    fn pack(chunk: usize, offset: usize) -> ArenaAddr {
        debug_assert!(offset < (1 << OFFSET_BITS) as usize);
        debug_assert!(chunk < (1 << 27));
        ArenaAddr(((chunk as u64) << OFFSET_BITS) | offset as u64)
    }
}

/// Byte-exact memory attribution for one arena (feeds `MemoryReport`, §7.1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ArenaReport {
    /// Requested bytes of live allocations.
    pub live_bytes: u64,
    /// `resident_bytes - live_bytes`: size-class rounding, free slots in
    /// partially-used chunks, and unreached bump tails.
    pub slack_bytes: u64,
    /// Bytes currently mmap'd (upper bound on this arena's RSS share; anon
    /// pages commit lazily on first touch).
    pub resident_bytes: u64,
    /// Live allocation count.
    pub live_allocs: u64,
    /// Mapped chunk count (slab + huge).
    pub chunks: u64,
}

// ---- size classes -----------------------------------------------------------

/// Tier A: 16..=256 in 8-byte steps (31 classes — zero slack on the 8-byte
/// aligned small-record corpus). Tier B: four geometric steps per doubling
/// (jemalloc spacing, worst-case slack 20%) up to `chunk_size / 4`.
const TIER_A_MAX: usize = 256;
const TIER_A_CLASSES: usize = 31; // (256 - 16) / 8 + 1

const fn tier_b_classes(chunk_size: usize) -> usize {
    // Doublings from 256 to chunk_size/4, four classes each.
    let mut n = 0;
    let mut top = TIER_A_MAX;
    while top < chunk_size / 4 {
        n += 4;
        top *= 2;
    }
    n
}

#[inline]
fn class_size(idx: usize) -> usize {
    if idx < TIER_A_CLASSES {
        16 + idx * 8
    } else {
        // Tier B group g (0-based), step s in 0..4: 256 * 2^g * (1 + (s+1)/4)
        let b = idx - TIER_A_CLASSES;
        let (group, step) = (b / 4, b % 4);
        (TIER_A_MAX << group) / 4 * (5 + step)
    }
}

#[inline]
fn class_of(len: usize, large_threshold: usize) -> Option<usize> {
    if len > large_threshold {
        return None;
    }
    if len <= TIER_A_MAX {
        let sz = len.max(16).next_multiple_of(8);
        return Some((sz - 16) / 8);
    }
    // Smallest tier-B class >= len: group from the doubling, then the step.
    let group = (len - 1).ilog2() as usize - 8; // len in (256·2^g, 256·2^(g+1)]
    let base = TIER_A_MAX << group; // class sizes: base·5/4, 6/4, 7/4, 8/4
    let step = (len * 4).div_ceil(base) - 5; // 0..=3
    Some(TIER_A_CLASSES + group * 4 + step)
}

// ---- chunks -----------------------------------------------------------------

const PAGE: usize = 4096;
const NONE_U64: u64 = u64::MAX;
const NONE_U32: u32 = u32::MAX;

struct Chunk {
    base: *mut u8,
    /// Mapped length; 0 after a huge chunk is unmapped (entry recycled).
    len: usize,
}

struct ClassState {
    /// Intrusive free list head (packed addr; `NONE_U64` = empty). Each free
    /// slot stores the next head in its first 8 bytes.
    free_head: u64,
    free_slots: u64,
    /// Current bump chunk index (`NONE_U32` = none yet) and its fill mark.
    bump_chunk: u32,
    bump_offset: u32,
}

impl ClassState {
    const EMPTY: ClassState =
        ClassState { free_head: NONE_U64, free_slots: 0, bump_chunk: NONE_U32, bump_offset: 0 };
}

/// Cell-local record arena. `!Send`/`!Sync` by construction (raw chunk
/// pointers) — one owner core (L1).
pub struct Arena {
    cfg: ArenaConfig,
    large_threshold: usize,
    classes: Vec<ClassState>,
    chunks: Vec<Chunk>,
    /// Recycled `chunks` indices from unmapped huge chunks.
    free_chunk_slots: Vec<u32>,
    live_bytes: u64,
    resident_bytes: u64,
    live_allocs: u64,
}

impl Arena {
    /// Builds an arena.
    ///
    /// # Panics
    /// Panics if `chunk_size` is not a power of two `>= 64 KiB` or exceeds
    /// the 2 MiB address-packing bound.
    pub fn new(cfg: ArenaConfig) -> Arena {
        assert!(
            cfg.chunk_size.is_power_of_two()
                && cfg.chunk_size >= (64 << 10)
                && cfg.chunk_size <= (2 << 20),
            "chunk_size must be a power of two in [64 KiB, 2 MiB]"
        );
        let n_classes = TIER_A_CLASSES + tier_b_classes(cfg.chunk_size);
        Arena {
            large_threshold: cfg.chunk_size / 4,
            cfg,
            classes: (0..n_classes).map(|_| ClassState::EMPTY).collect(),
            chunks: Vec::new(),
            free_chunk_slots: Vec::new(),
            live_bytes: 0,
            resident_bytes: 0,
            live_allocs: 0,
        }
    }

    /// Allocates `len` bytes. `None` means the resident budget is exhausted —
    /// the caller must surface backpressure (OOM error / eviction), never
    /// grow elsewhere.
    pub fn alloc(&mut self, len: usize) -> Option<ArenaAddr> {
        let addr = match class_of(len, self.large_threshold) {
            Some(class) => self.alloc_classed(class)?,
            None => self.alloc_huge(len)?,
        };
        self.live_bytes += len as u64;
        self.live_allocs += 1;
        Some(addr)
    }

    fn alloc_classed(&mut self, class: usize) -> Option<ArenaAddr> {
        let size = class_size(class);
        // 1. Reuse a freed slot.
        let head = self.classes[class].free_head;
        if head != NONE_U64 {
            let addr = ArenaAddr(head);
            let next = self.read_freelink(addr);
            let st = &mut self.classes[class];
            st.free_head = next;
            st.free_slots -= 1;
            return Some(addr);
        }
        // 2. Bump the current chunk.
        let st = &mut self.classes[class];
        if st.bump_chunk != NONE_U32 && (st.bump_offset as usize + size) <= self.cfg.chunk_size {
            let addr = ArenaAddr::pack(st.bump_chunk as usize, st.bump_offset as usize);
            st.bump_offset += size as u32;
            return Some(addr);
        }
        // 3. Map a fresh chunk for this class.
        let idx = self.map_chunk(self.cfg.chunk_size)?;
        let st = &mut self.classes[class];
        st.bump_chunk = idx;
        st.bump_offset = size as u32;
        Some(ArenaAddr::pack(idx as usize, 0))
    }

    fn alloc_huge(&mut self, len: usize) -> Option<ArenaAddr> {
        let mapped = len.next_multiple_of(PAGE);
        let idx = self.map_chunk(mapped)?;
        Some(ArenaAddr::pack(idx as usize, 0))
    }

    /// Returns `len` bytes at `addr` to the arena. `len` must be the length
    /// passed to the `alloc` that produced `addr`.
    ///
    /// # Panics
    /// Panics if `addr` does not refer to a live chunk.
    pub fn free(&mut self, addr: ArenaAddr, len: usize) {
        match class_of(len, self.large_threshold) {
            Some(class) => {
                let head = self.classes[class].free_head;
                self.write_freelink(addr, head);
                let st = &mut self.classes[class];
                st.free_head = addr.to_raw();
                st.free_slots += 1;
            }
            None => {
                let mapped = len.next_multiple_of(PAGE);
                debug_assert_eq!(addr.offset(), 0, "huge allocations are chunk-aligned");
                self.unmap_chunk(addr.chunk(), mapped);
            }
        }
        self.live_bytes -= len as u64;
        self.live_allocs -= 1;
    }

    /// True (and accounting updated) if the allocation can change size from
    /// `old_len` to `new_len` without moving — same size class, or within a
    /// huge mapping's page rounding. On `false` the caller must
    /// alloc-copy-free. Shrinks within a class also succeed (the slot is
    /// already paid for; only `live_bytes` moves).
    pub fn resize_in_place(&mut self, addr: ArenaAddr, old_len: usize, new_len: usize) -> bool {
        let fits = match (
            class_of(old_len, self.large_threshold),
            class_of(new_len, self.large_threshold),
        ) {
            (Some(old_class), Some(new_class)) => old_class == new_class,
            (None, None) => new_len.next_multiple_of(PAGE) == old_len.next_multiple_of(PAGE),
            _ => false,
        };
        let _ = addr;
        if fits {
            self.live_bytes = self.live_bytes - old_len as u64 + new_len as u64;
        }
        fits
    }

    /// Immutable view of `len` bytes at `addr`.
    ///
    /// # Panics
    /// Panics if the range escapes the owning chunk (corrupt addr/len pair)
    /// or the chunk was unmapped (use-after-free of a huge allocation).
    #[inline]
    pub fn bytes(&self, addr: ArenaAddr, len: usize) -> &[u8] {
        let chunk = &self.chunks[addr.chunk()];
        let offset = addr.offset();
        assert!(offset + len <= chunk.len, "arena range escapes chunk (stale addr or bad len)");
        // SAFETY: chunk.base is a live mapping of chunk.len bytes (len == 0
        // entries are rejected by the bounds check above); the arena owns it
        // until unmap/drop, and &self borrows prevent concurrent mutation.
        unsafe { core::slice::from_raw_parts(chunk.base.add(offset), len) }
    }

    /// Mutable view of `len` bytes at `addr` (same contract as [`bytes`](Self::bytes)).
    #[inline]
    pub fn bytes_mut(&mut self, addr: ArenaAddr, len: usize) -> &mut [u8] {
        let chunk = &self.chunks[addr.chunk()];
        let offset = addr.offset();
        assert!(offset + len <= chunk.len, "arena range escapes chunk (stale addr or bad len)");
        // SAFETY: as `bytes`, plus &mut self guarantees exclusive access.
        unsafe { core::slice::from_raw_parts_mut(chunk.base.add(offset), len) }
    }

    /// Byte-exact attribution snapshot.
    pub fn report(&self) -> ArenaReport {
        ArenaReport {
            live_bytes: self.live_bytes,
            slack_bytes: self.resident_bytes - self.live_bytes,
            resident_bytes: self.resident_bytes,
            live_allocs: self.live_allocs,
            chunks: self.chunks.iter().filter(|c| c.len > 0).count() as u64,
        }
    }

    // ---- internals ----

    fn read_freelink(&self, addr: ArenaAddr) -> u64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(self.bytes(addr, 8));
        u64::from_le_bytes(raw)
    }

    fn write_freelink(&mut self, addr: ArenaAddr, next: u64) {
        self.bytes_mut(addr, 8).copy_from_slice(&next.to_le_bytes());
    }

    fn map_chunk(&mut self, len: usize) -> Option<u32> {
        if let Some(budget) = self.cfg.max_resident
            && self.resident_bytes as usize + len > budget
        {
            return None;
        }
        // SAFETY: anonymous private mapping; we request no fixed address and
        // check the result before use.
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
        let chunk = Chunk { base: base.cast(), len };
        let idx = match self.free_chunk_slots.pop() {
            Some(idx) => {
                self.chunks[idx as usize] = chunk;
                idx
            }
            None => {
                let idx = u32::try_from(self.chunks.len()).expect("chunk index fits u32");
                assert!((idx as u64) < (1 << 27), "chunk count exceeds 48-bit address space");
                self.chunks.push(chunk);
                idx
            }
        };
        self.resident_bytes += len as u64;
        Some(idx)
    }

    fn unmap_chunk(&mut self, idx: usize, expected_len: usize) {
        let chunk = &mut self.chunks[idx];
        assert!(chunk.len > 0, "double unmap of huge chunk");
        debug_assert_eq!(chunk.len, expected_len, "huge free with mismatched len");
        // SAFETY: base/len are exactly the live mapping created in map_chunk.
        unsafe { libc::munmap(chunk.base.cast(), chunk.len) };
        self.resident_bytes -= chunk.len as u64;
        chunk.base = core::ptr::null_mut();
        chunk.len = 0;
        self.free_chunk_slots.push(idx as u32);
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        for chunk in &self.chunks {
            if chunk.len > 0 {
                // SAFETY: live mapping owned by this arena.
                unsafe { libc::munmap(chunk.base.cast(), chunk.len) };
            }
        }
    }
}

impl fmt::Debug for Arena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Arena {{ {:?} }}", self.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_table_is_monotonic_and_covers() {
        let cfg = ArenaConfig::default();
        let threshold = cfg.chunk_size / 4;
        let n = TIER_A_CLASSES + tier_b_classes(cfg.chunk_size);
        let mut prev = 0;
        for idx in 0..n {
            let sz = class_size(idx);
            assert!(sz > prev, "class {idx} not monotonic");
            prev = sz;
        }
        assert_eq!(class_size(n - 1), threshold, "largest class is the threshold");
        // Every len maps to the smallest class >= len.
        for len in 1..=threshold {
            let idx = class_of(len, threshold).expect("classed");
            assert!(class_size(idx) >= len, "len {len} got class below it");
            assert!(idx == 0 || class_size(idx - 1) < len, "len {len} not minimal");
        }
        assert_eq!(class_of(threshold + 1, threshold), None);
    }

    #[test]
    fn alloc_free_roundtrip_preserves_bytes() {
        let mut arena = Arena::new(ArenaConfig::default());
        let addr = arena.alloc(100).expect("alloc");
        arena.bytes_mut(addr, 100).copy_from_slice(&[0xAB; 100]);
        assert_eq!(arena.bytes(addr, 100), &[0xAB; 100]);
        let r = arena.report();
        assert_eq!((r.live_bytes, r.live_allocs), (100, 1));
        arena.free(addr, 100);
        let r = arena.report();
        assert_eq!((r.live_bytes, r.live_allocs), (0, 0));
        assert_eq!(r.slack_bytes, r.resident_bytes);
    }

    #[test]
    fn freed_slot_is_reused_before_bump() {
        let mut arena = Arena::new(ArenaConfig::default());
        let a = arena.alloc(64).expect("a");
        let b = arena.alloc(64).expect("b");
        assert_ne!(a, b);
        arena.free(a, 64);
        let c = arena.alloc(60).expect("c"); // same class (64)
        assert_eq!(a, c, "free list reused before bump");
    }

    #[test]
    fn huge_allocations_map_and_unmap() {
        let mut arena = Arena::new(ArenaConfig::default());
        let len = (1 << 20) + 13; // > 512 KiB threshold
        let addr = arena.alloc(len).expect("huge");
        arena.bytes_mut(addr, len)[len - 1] = 0x7E;
        assert_eq!(arena.bytes(addr, len)[len - 1], 0x7E);
        let resident = arena.report().resident_bytes;
        assert_eq!(resident, (len as u64).next_multiple_of(PAGE as u64));
        arena.free(addr, len);
        assert_eq!(arena.report().resident_bytes, 0);
    }

    #[test]
    #[should_panic(expected = "escapes chunk")]
    fn stale_huge_addr_panics_not_ub() {
        let mut arena = Arena::new(ArenaConfig::default());
        let len = 1 << 20;
        let addr = arena.alloc(len).expect("huge");
        arena.free(addr, len);
        let _ = arena.bytes(addr, len);
    }

    #[test]
    fn budget_exhaustion_is_none_not_growth() {
        let cfg = ArenaConfig { chunk_size: 64 << 10, max_resident: Some(128 << 10) };
        let mut arena = Arena::new(cfg);
        let a = arena.alloc(60 << 10).expect("first chunk-ish"); // huge for 64K chunks? threshold = 16K -> huge path, 60K mapped
        assert!(arena.report().resident_bytes <= 128 << 10);
        // Next huge allocation would exceed the budget.
        assert_eq!(arena.alloc(80 << 10), None);
        arena.free(a, 60 << 10);
        assert!(arena.alloc(80 << 10).is_some(), "budget freed up");
    }

    #[test]
    fn resize_in_place_within_class_only() {
        let mut arena = Arena::new(ArenaConfig::default());
        let addr = arena.alloc(60).expect("alloc"); // class 64
        assert!(arena.resize_in_place(addr, 60, 64));
        assert_eq!(arena.report().live_bytes, 64);
        assert!(!arena.resize_in_place(addr, 64, 65)); // class 72
        assert_eq!(arena.report().live_bytes, 64);
        assert!(arena.resize_in_place(addr, 64, 58), "shrink within class");
        assert_eq!(arena.report().live_bytes, 58);
        assert!(!arena.resize_in_place(addr, 58, 40), "class 48 is a move");
    }

    /// M0-S13 AC: alloc/free storm — accounting reconciles to zero drift,
    /// byte-exact, after 10⁶ random ops (deterministic xorshift; Miri runs a
    /// scaled-down storm). Data integrity is verified per allocation with a
    /// fill pattern; live ranges are implicitly disjoint or the patterns
    /// would tear.
    #[test]
    fn storm_reconciles_byte_exact() {
        let ops: usize = if cfg!(miri) { 4_000 } else { 1_000_000 };
        let mut arena = Arena::new(ArenaConfig::default());
        let mut live: Vec<(ArenaAddr, usize, u8)> = Vec::new();
        let mut expected_live: u64 = 0;
        let mut x: u64 = 0x243F_6A88_85A3_08D3;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for op in 0..ops {
            if rand() & 1 == 0 || live.is_empty() {
                // Size mix: mostly small records, some tier-B, rare huge.
                let len = match rand() % 100 {
                    0..=79 => 16 + (rand() % 300) as usize,
                    80..=97 => 512 + (rand() % 8192) as usize,
                    _ => (520 << 10) + (rand() % 4096) as usize,
                };
                let Some(addr) = arena.alloc(len) else { panic!("unbudgeted alloc failed") };
                let fill = (rand() & 0xFF) as u8;
                arena.bytes_mut(addr, len).fill(fill);
                live.push((addr, len, fill));
                expected_live += len as u64;
            } else {
                let idx = (rand() as usize) % live.len();
                let (addr, len, fill) = live.swap_remove(idx);
                let slice = arena.bytes(addr, len);
                assert!(
                    slice.iter().all(|&b| b == fill),
                    "op {op}: allocation torn (overlap or stale reuse)"
                );
                arena.free(addr, len);
                expected_live -= len as u64;
            }
            debug_assert_eq!(arena.report().live_bytes, expected_live);
        }
        assert_eq!(arena.report().live_bytes, expected_live);
        for (addr, len, fill) in live.drain(..) {
            assert!(arena.bytes(addr, len).iter().all(|&b| b == fill));
            arena.free(addr, len);
        }
        let r = arena.report();
        assert_eq!((r.live_bytes, r.live_allocs), (0, 0), "zero drift");
        assert_eq!(r.slack_bytes, r.resident_bytes);
    }

    /// The (16 B, 64 B) gate corpus: 8 B header + 16 + 64 = 88 B records land
    /// exactly in the 88 B class — zero slack; with the 8 B index slot at
    /// load factor 0.85 the per-key budget is ~18.6 B (artifact lands with
    /// the store-level measurement, M0-S15).
    #[test]
    fn gate_corpus_class_has_zero_slack() {
        let threshold = ArenaConfig::default().chunk_size / 4;
        let record = 8 + 16 + 64;
        let class = class_of(record, threshold).expect("classed");
        assert_eq!(class_size(class), 88);
    }
}
