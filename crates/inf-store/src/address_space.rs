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

#[allow(clippy::disallowed_types, reason = "container: R")]
use std::collections::VecDeque;

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

/// One unit of flush work in `[cursor, ro_boundary)` — the M4-S11
/// pipeline's pull vocabulary (ADR-0056 D3). `Records` ranges end on
/// record boundaries (a recorded seal cut, a hole start, or the
/// ro-boundary itself), so `advance_flushed` to a chunk end never leaves
/// a record half-durable (§3.1). `Gap` is an ADR-0052 D2 ring-top
/// sealed-dead interval: zero content — the flush seals the preceding
/// file, then advances `flushed` across it without writing bytes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FlushChunk {
    /// Contiguous sealed record bytes at `addr`, RAM-resident and
    /// immutable (below `ro_boundary`), readable via
    /// [`AddressSpace::bytes`].
    Records {
        /// First address of the range.
        addr: LogicalAddr,
        /// Range length in bytes (ends on a record boundary).
        len: u64,
    },
    /// A ring-top sealed-dead hole (no record, no content, no index slot).
    Gap {
        /// First dead address.
        at: LogicalAddr,
        /// Dead bytes to skip.
        len: u64,
    },
}

/// What one tail allocation would do to the space — the one computation
/// of the ring-top hole, the start and the committed top an allocation
/// of `len` bytes needs (ADR-0174 D2 rule 1). [`AddressSpace::alloc`],
/// [`AddressSpace::stall_target`] and [`AddressSpace::room`] consume it;
/// none carries a copy of the arithmetic (two copies are how the stall
/// target came to answer a need above the tail).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Prospect {
    /// The record's length.
    len: u64,
    /// The ring-top hole the allocation makes first (ADR-0052 D2); 0
    /// when the record fits below the ring top.
    hole: u64,
    /// The record's start, relative to the life origin: the tail past
    /// the hole.
    start_rel: u64,
    /// The committed top the allocation needs — after the hole and after
    /// the record — relative to the origin, page-aligned, never below
    /// the committed top of today.
    top_rel: u64,
}

/// The room question's answer (ADR-0174 D2 rule 1): what must happen
/// before an allocation of `len` bytes can be placed. The need it
/// carries is never above the tail — a need that would lie above it is
/// answered as a [`Pad`](Room::Pad) instead (D2 rule 6), and a record
/// no window state can hold is [`End`](Room::End) — so a demote step
/// always has a reachable target.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Room {
    /// The allocation fits now: [`AddressSpace::alloc`] places it.
    Fits,
    /// The head must reach this address — a page multiple above the
    /// head, at or below the tail — before the allocation fits: the
    /// demote step's target.
    Demote(LogicalAddr),
    /// The need lies above the tail: pad the tail to this address first
    /// ([`AddressSpace::pad_tail`] — the ring top when the record needs
    /// a ring-top hole, else the next commit-page boundary), then ask
    /// again. The next answer is `Demote` or `Fits`.
    Pad(LogicalAddr),
    /// No watermark progress places it: the 48-bit end of the space, or
    /// a record longer than the window. The second cause exists only on
    /// the live path, where a budget shrink keeps the ring and the
    /// records placed under the old budget (`TieredTable::set_demotion`)
    /// and relocation re-appends one; at boot the ring is the spec's,
    /// so every record the length refusals admit is at most the window.
    End,
}

/// A [`AddressSpace::pad_tail`] refusal: the pages the pad would commit
/// exceed the window. Nothing changed. [`AddressSpace::room`] answers
/// `Pad` only where the pad fits, so this is the pair check of that
/// arithmetic, never an operating condition.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WindowFull;

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
    /// Cold reads that failed typed at the command surface (queue
    /// saturation, device error, frame CRC, extent open/read, replan
    /// exhaustion — review of 2026-08-30, C2′): every one answered a
    /// client `-ERR`/`BUSY`, never an absence, and this counter makes
    /// the rate scrapeable (L10). Zero in memory mode and in any
    /// healthy run.
    pub cold_read_errors: u64,
    /// Writes that suspended on flushed-watermark progress (M4-S07,
    /// ADR-0053 D4) — the always-on backpressure tripwire.
    pub tail_alloc_stalls: u64,
    /// Demotion slices that advanced the ro-boundary (seal steps, M4-S07).
    pub demote_slices: u64,
    /// Bytes sealed read-only by demotion slices (M4-S07).
    pub demote_sealed_bytes: u64,
    /// Flush slices that appended or confirmed (M4-S11).
    pub flush_slices: u64,
    /// Bytes confirmed durable by flush barriers (`flushed` advancement,
    /// M4-S11 — gap bytes included: they are confirmed, zero-content).
    pub flush_confirmed_bytes: u64,
    /// Copy-forward compaction slices that scanned or relocated (M4-S15,
    /// ADR-0059). Always-on; joins the S03 zero-assert set — a
    /// memory-mode run must never count one.
    pub compact_slices: u64,
    /// Writes that re-resolved because the key's slot moved while the
    /// write was suspended on an extent read (review of 2026-08-30,
    /// F-L06-03) — a legal interleaving, counted so the race is
    /// observable (a green race test must have seen ≥ 1).
    pub write_replans: u64,
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
#[allow(clippy::disallowed_types, reason = "container: R")]
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
    /// Admission bound on the committed window (M4-S07, ADR-0053 D1):
    /// page-aligned, ≤ the ring. Defaults to the ring (the structural VA
    /// ceiling); [`set_window_limit`](Self::set_window_limit) tightens it
    /// to the namespace memory budget + slice slack so backpressure
    /// engages at the budget, not at the (up to 2×) ring wall.
    window_limit: u64,
    dead_bytes: u64,
    /// Sealed-dead intervals not yet passed by `flushed` — `(start, len)`,
    /// in address order (ADR-0056 D3: ADR-0052 D2's "recorded sealed-dead
    /// interval" made literal so the flush can skip them): ring-top holes
    /// and tail pads (ADR-0174 D2 rule 6). Structurally ≤ 3 pending:
    /// `tail − head ≤ R` means the RAM window crosses at most one
    /// interior ring multiple, plus one in transition, plus one page pad,
    /// whose mark `flushed` passes before its record applies (a page pad
    /// needs a record longer than the window less a page, so the demote
    /// step that follows it seals past the pad).
    hole_marks: VecDeque<(u64, u64)>,
    /// Record-boundary flush cut points in `(flushed, ro_boundary]` —
    /// every `advance_ro_boundary` target is one (seal steps land on
    /// record starts, M4-S07). Bounded by [`FLUSH_CUT_CAP`]; when full,
    /// new cuts are dropped and the flush advances in coarser
    /// (hole/ro-bounded) steps — degraded granularity, never wrong
    /// boundaries.
    flush_cuts: VecDeque<u64>,
    /// Active checkpoint-walk pin (M4-S12, ADR-0057 D2): while a hybrid
    /// walk is in flight, the head must not pass the walk watermark — a
    /// release past it would strand an image-class record (`addr ≥ W`)
    /// on disk mid-walk, forcing the walker to choose between a cold
    /// read and a lie. One walk per cell, ever (the ADR-0016 D7
    /// posture); flush and seal stay unpinned, so backpressure targets
    /// keep waking.
    walk_pin: Option<u64>,
    /// Record-release pin (M4.5-S37, ADR-0093 D3): while a shadow ticket
    /// is open, the head must not pass its winner — the RAM-resident
    /// record whose key-verified slot outranks the unverified cold
    /// twin in lookup order. Set to the oldest unresolved winner by the
    /// table (`sync_shadow_pin`); `None` when no ticket is open. Like
    /// the walk pin: release clamps, flush and seal stay unpinned.
    record_pin: Option<u64>,
    counters: TieringCounters,
    /// Interior-mutable: the resolver is `&self` on the hottest path.
    cold_resolves: LocalCounter,
}

/// Cap on retained flush cuts (~32 KiB worst case). One cut per seal
/// step; the committed-window admission bound caps sealed-pending bytes,
/// so at the default one-commit-page slice this depth covers a 4 GiB
/// window before coarsening kicks in.
const FLUSH_CUT_CAP: usize = 4096;

impl AddressSpace {
    /// Reserves the ring. `None` when the OS refuses the reservation —
    /// namespace creation surfaces that as a typed error, never a panic.
    ///
    /// # Panics
    /// Panics on config violations (non-power-of-two ring, ring smaller
    /// than four pages) — programmer errors.
    #[allow(clippy::disallowed_types, reason = "container: R")]
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
            window_limit: config.reserve_bytes as u64,
            dead_bytes: 0,
            hole_marks: VecDeque::new(),
            flush_cuts: VecDeque::new(),
            walk_pin: None,
            record_pin: None,
            counters: TieringCounters::default(),
            cold_resolves: LocalCounter::new(),
        })
    }

    // ---- checkpoint-walk pin (M4-S12, ADR-0057 D2) ----

    /// Latches the walk watermark `W` = the current flushed watermark
    /// and pins page release beneath it for the walk's duration. Every
    /// entry below `W` refs, every entry at or above it images — and the
    /// pin makes the image half structurally RAM-resident (`addr ≥ W ≥
    /// head` holds until [`end_walk`](Self::end_walk)).
    ///
    /// # Panics
    /// Panics when a walk is already pinned (one checkpoint in flight
    /// per cell, ever — ADR-0016 D7).
    pub fn begin_walk(&mut self) -> LogicalAddr {
        assert!(self.walk_pin.is_none(), "one checkpoint walk in flight per cell");
        self.walk_pin = Some(self.flushed);
        LogicalAddr::from_raw(self.flushed).expect("watermarks stay 48-bit")
    }

    /// Releases the walk pin (walk complete or aborted — either way the
    /// held-back release debt drains in the next MAINTAIN slices).
    ///
    /// # Panics
    /// Panics when no walk is pinned.
    pub fn end_walk(&mut self) {
        assert!(self.walk_pin.take().is_some(), "end_walk without begin_walk");
    }

    /// The active walk's watermark, if one is pinned.
    #[must_use]
    pub fn walk_watermark(&self) -> Option<LogicalAddr> {
        self.walk_pin.map(|w| LogicalAddr::from_raw(w).expect("watermarks stay 48-bit"))
    }

    /// How far the head may advance right now: `flushed`, clamped to the
    /// walk pin while a hybrid walk is in flight (ADR-0057 D2) and to
    /// the record pin while a shadow ticket is open (ADR-0093 D3).
    /// Release drivers step toward this, never toward `flushed` directly.
    #[must_use]
    pub fn release_ceiling(&self) -> u64 {
        let ceiling = self.walk_pin.map_or(self.flushed, |w| w.min(self.flushed));
        self.record_pin.map_or(ceiling, |p| p.min(ceiling))
    }

    /// Pins page release at `at` (a RAM-resident record's address — the
    /// oldest unresolved shadow winner, ADR-0093 D3) or lifts the pin.
    /// The pin never names an address below the head: a winner is
    /// registered while RAM-resident and the ceiling then keeps it so.
    ///
    /// # Panics
    /// Debug-panics on a pin below the head (a winner that already went
    /// cold — the invariant the pin exists to keep).
    pub fn set_record_pin(&mut self, at: Option<LogicalAddr>) {
        let pin = at.map(LogicalAddr::to_raw);
        debug_assert!(pin.is_none_or(|p| p >= self.head), "record pin below the head");
        debug_assert!(pin.is_none_or(|p| p < self.tail), "record pin past the tail");
        self.record_pin = pin;
    }

    /// The record pin, if one is set (tests and `INFO`).
    #[must_use]
    pub fn record_pin(&self) -> Option<LogicalAddr> {
        self.record_pin.map(|p| LogicalAddr::from_raw(p).expect("watermarks stay 48-bit"))
    }

    /// Tightens the committed-window admission bound to `bytes` (rounded
    /// down to whole pages, clamped to the ring) — the ADR-0053 D1
    /// budget bound: `MEM-BUDGET + slice` for tiered namespaces. Alloc
    /// refusals and [`stall_target`](Self::stall_target) then key off
    /// this limit, so backpressure engages at the budget while the ring
    /// stays the structural VA ceiling.
    ///
    /// # Panics
    /// Panics when the rounded limit is smaller than four pages — a
    /// nonsense budget the namespace-creation path rejects typed before
    /// reaching here.
    pub fn set_window_limit(&mut self, bytes: u64) {
        let limit = self.page_floor(bytes).min(self.ring_mask + 1);
        assert!(limit >= 4 * self.page_bytes, "window limit smaller than four pages");
        self.window_limit = limit;
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

    /// The one computation of what allocating `len` bytes at the tail
    /// would need (ADR-0174 D2 rule 1): the ring-top hole, the start and
    /// the committed top. Pure.
    ///
    /// # Panics
    /// Panics on an empty allocation or one above half the ring —
    /// programmer errors, bounded by the record and blob limits.
    fn prospect(&self, len: usize) -> Prospect {
        assert!(len > 0, "empty allocation");
        let ring = self.ring_mask + 1;
        assert!((len as u64) <= ring / 2, "allocation exceeds half the ring");
        let len = len as u64;
        let rel_tail = self.tail - self.life_origin;
        let ring_offset = rel_tail & self.ring_mask;
        let hole = if ring_offset + len > ring { ring - ring_offset } else { 0 };
        let start_rel = rel_tail + hole;
        let top_rel = self.page_ceil(start_rel + len).max(self.commit_top_rel);
        Prospect { len, hole, start_rel, top_rel }
    }

    /// [`Room::Fits`]'s predicate: the committed window the prospect
    /// needs, in whole pages, stays inside the admission bound
    /// (ADR-0053 D1).
    #[inline]
    fn fits(&self, prospect: Prospect) -> bool {
        prospect.top_rel - self.commit_floor_rel <= self.window_limit
    }

    /// The 48-bit end check, in one place for [`alloc`](Self::alloc) and
    /// [`room`](Self::room): the prospect's start address when the
    /// record's end lies inside the space, else `None` — the space is
    /// monotonic for a namespace's whole existence and never wraps.
    #[inline]
    fn start_inside(&self, prospect: Prospect) -> Option<LogicalAddr> {
        let start = LogicalAddr::from_raw(self.life_origin.checked_add(prospect.start_rel)?)?;
        start.advanced(prospect.len)?;
        Some(start)
    }

    /// An address of this life from its relative offset. Every offset
    /// classified by [`room`](Self::room) lies at or below a record end
    /// [`start_inside`](Self::start_inside) admitted.
    #[inline]
    fn addr_at(&self, rel: u64) -> LogicalAddr {
        LogicalAddr::from_raw(self.life_origin + rel).expect("watermarks stay 48-bit")
    }

    /// The room question (ADR-0174 D2 rule 1): what must happen before
    /// `len` bytes can be placed at the tail. Pure; mutates nothing.
    /// Boot replay asks it at most four times per record (`Demote`,
    /// `Pad`, `Demote`, `Fits` — D2 rule 6's proof, whose premise is a
    /// window of at least half the ring: the ring is derived from the
    /// spec at every boot), and the live path consumes the same
    /// arithmetic through [`alloc`](Self::alloc) and
    /// [`stall_target`](Self::stall_target).
    ///
    /// # Panics
    /// As [`alloc`](Self::alloc).
    pub fn room(&self, len: usize) -> Room {
        self.classify(self.prospect(len))
    }

    /// Classifies a prospect against the window and the tail (D2 rules 1
    /// and 6). A record longer than the window is `End` before any need
    /// is computed, so every need below is one of a record the window
    /// holds. A need below or at the tail is a `Demote`; one above it is
    /// unreachable by any release — the tail pads first: in case (a), a
    /// ring-top hole, to the ring top the hole would make, which may need
    /// room of its own, always at or below the tail (the hole is shorter
    /// than the record, which is at most the window); in case (b) to the
    /// next commit-page boundary, which commits nothing.
    fn classify(&self, prospect: Prospect) -> Room {
        if self.start_inside(prospect).is_none() || prospect.len > self.window_limit {
            return Room::End;
        }
        if self.fits(prospect) {
            return Room::Fits;
        }
        let rel_tail = self.tail - self.life_origin;
        let need_rel = prospect.top_rel - self.window_limit;
        debug_assert!(need_rel > self.head - self.life_origin, "a need the head already passed");
        if need_rel <= rel_tail {
            return Room::Demote(self.addr_at(need_rel));
        }
        if cfg!(inf_canary_replay_no_pad) {
            return Room::Demote(self.addr_at(rel_tail));
        }
        if prospect.hole > 0 {
            let pad = self.prospect(prospect.hole as usize);
            debug_assert_eq!(pad.start_rel, rel_tail, "a ring-top pad starts at the tail");
            debug_assert_eq!(pad.top_rel, prospect.start_rel, "and ends at the ring top");
            if self.fits(pad) {
                return Room::Pad(self.addr_at(prospect.start_rel));
            }
            let pad_need_rel = pad.top_rel - self.window_limit;
            debug_assert!(pad_need_rel <= rel_tail, "a ring-top pad's need is reachable");
            return Room::Demote(self.addr_at(pad_need_rel));
        }
        Room::Pad(self.addr_at(self.page_ceil(rel_tail)))
    }

    /// Allocates `len` bytes at the tail, committing ring pages as
    /// needed, exactly where [`room`](Self::room) answers [`Room::Fits`]:
    /// `None` when the RAM window (in whole pages) would exceed the
    /// admission bound (the budget window — ADR-0053 D1; the ring by
    /// default) — the backpressure signal S07 turns into
    /// suspend-on-flushed-progress — or at the 48-bit end of the space.
    /// Both decide on one [`Prospect`] by the same two predicates,
    /// [`fits`](Self::fits) and [`start_inside`](Self::start_inside);
    /// `alloc` never classifies a refusal, whose reason is
    /// [`stall_target`](Self::stall_target)'s or `room`'s to tell, so
    /// the placing path pays the arithmetic once and makes no call.
    ///
    /// May advance the tail past a ring-top hole first (ADR-0052 D2);
    /// hole bytes are dead on arrival, counted, and tripwired. No state
    /// changes on refusal.
    pub fn alloc(&mut self, len: usize) -> Option<LogicalAddr> {
        let prospect = self.prospect(len);
        if !self.fits(prospect) {
            return None;
        }
        let addr = self.start_inside(prospect)?;
        if prospect.top_rel > self.commit_top_rel {
            self.commit_rel_pages(self.commit_top_rel, prospect.top_rel);
            self.commit_top_rel = prospect.top_rel;
        }
        if prospect.hole > 0 {
            self.counters.seal_holes += 1;
            self.counters.seal_hole_bytes += prospect.hole;
            self.dead_bytes += prospect.hole;
            self.hole_marks.push_back((self.tail, prospect.hole));
            debug_assert!(self.hole_marks.len() <= 3, "two ring tops and one pad at most");
        }
        self.counters.tail_allocs += 1;
        self.tail = self.life_origin + prospect.start_rel + prospect.len;
        self.assert_watermark_order();
        Some(addr)
    }

    /// Pads the tail to `to`, a [`Room::Pad`] answer (ADR-0174 D2 rule
    /// 6): the ring top when the next record needs a ring-top hole, else
    /// the next commit-page boundary. The skipped span is a sealed-dead
    /// interval (ADR-0052 D2): counted dead, a hole mark, no record; the
    /// flush crosses it as a gap after the preceding file's seal. A
    /// ring-top pad is the ring-top seal the record would have made and
    /// counts as one; a page pad commits nothing.
    ///
    /// # Errors
    /// [`WindowFull`] when the pages the pad commits would exceed the
    /// window — nothing changed. `room` answers `Pad` only where they do
    /// not; this is that arithmetic's pair check.
    ///
    /// # Panics
    /// Panics when `to` is not a pad target of the current tail (at or
    /// below it, or neither the ring top nor the next page boundary) —
    /// a stale or invented answer, a programmer error.
    pub fn pad_tail(&mut self, to: LogicalAddr) -> Result<(), WindowFull> {
        let t = to.to_raw();
        assert!(t > self.tail, "pad at or below the tail");
        let rel_tail = self.tail - self.life_origin;
        let ring = self.ring_mask + 1;
        let ring_top_rel = rel_tail + (ring - (rel_tail & self.ring_mask));
        let to_rel = t - self.life_origin;
        assert!(
            to_rel == ring_top_rel || to_rel == self.page_ceil(rel_tail),
            "pad target is neither the ring top nor the next page"
        );
        let top_rel = self.page_ceil(to_rel).max(self.commit_top_rel);
        if top_rel - self.commit_floor_rel > self.window_limit {
            return Err(WindowFull);
        }
        if top_rel > self.commit_top_rel {
            self.commit_rel_pages(self.commit_top_rel, top_rel);
            self.commit_top_rel = top_rel;
        }
        let pad = t - self.tail;
        if to_rel == ring_top_rel {
            self.counters.seal_holes += 1;
            self.counters.seal_hole_bytes += pad;
        }
        self.dead_bytes += pad;
        self.hole_marks.push_back((self.tail, pad));
        debug_assert!(self.hole_marks.len() <= 3, "two ring tops and one pad at most");
        self.tail = t;
        self.assert_watermark_order();
        Ok(())
    }

    /// The flushed-watermark value that would let a refused `alloc(len)`
    /// fit (M4-S07 backpressure, ADR-0053 D4). `None` means either the
    /// allocation fits right now or no watermark progress can help (the
    /// 48-bit end of the space) — callers only consult this **after** a
    /// refused alloc, so `None` there reads as hard out-of-space, never
    /// ambiguity. Pure query: counting a stall is the caller's explicit
    /// act ([`note_tail_alloc_stall`](Self::note_tail_alloc_stall)).
    ///
    /// Reads the same [`Prospect`] as [`alloc`](Self::alloc): the window
    /// binds at `top_rel − commit_floor_rel ≤ window_limit`,
    /// `commit_floor_rel` follows `head`, and `head` may only chase
    /// `flushed` — so the wait key is `life_origin + (top_rel −
    /// window_limit)`. Both terms are page multiples, so once `flushed`
    /// reaches the target and the release slice runs, the retried
    /// allocation fits by construction (no spurious wakes). The target
    /// is reported as it stands, above the tail included — the live
    /// path's answer to that case is its own finding, FCR-STTIER-N4;
    /// boot replay asks [`room`](Self::room) instead.
    pub fn stall_target(&self, len: usize) -> Option<LogicalAddr> {
        let prospect = self.prospect(len);
        if self.fits(prospect) {
            return None; // fits now — the caller's alloc will succeed.
        }
        LogicalAddr::from_raw(self.life_origin.checked_add(prospect.top_rel - self.window_limit)?)
    }

    /// Counts one tail-allocation stall (the caller is about to park on
    /// flushed-watermark progress — M4-S07). Always-on tripwire; joins
    /// the S03 zero-assert set.
    pub fn note_tail_alloc_stall(&mut self) {
        self.counters.tail_alloc_stalls += 1;
    }

    /// Counts one demotion slice and the bytes it sealed (M4-S07). Called
    /// by the table's seal/release steps — the address space itself has
    /// no slice vocabulary.
    pub fn note_demote_slice(&mut self, sealed_bytes: u64) {
        self.counters.demote_slices += 1;
        self.counters.demote_sealed_bytes += sealed_bytes;
    }

    /// Counts one flush slice and the bytes it confirmed durable
    /// (M4-S11). Always-on; joins the S03 zero-assert set.
    pub fn note_flush_slice(&mut self, confirmed_bytes: u64) {
        self.counters.flush_slices += 1;
        self.counters.flush_confirmed_bytes += confirmed_bytes;
    }

    /// Counts one copy-forward compaction slice (M4-S15). Called by the
    /// table's applier — the address space itself has no slice
    /// vocabulary. Always-on; joins the S03 zero-assert set.
    pub fn note_compact_slice(&mut self) {
        self.counters.compact_slices += 1;
    }

    // ---- flush-work query (M4-S11, ADR-0056 D3) ----

    /// Whether a flush stage whose cursor stands at the raw address
    /// `cursor` has a chunk to take: the cursor is below the ro-boundary.
    /// [`next_flush_chunk`](Self::next_flush_chunk) answers `None` exactly
    /// when this is false, and the tier round asks it before offering its
    /// slice to the device budget, so no credit is asked for work that
    /// does not exist (ADR-0170 D3). The stage's cursor never passes the
    /// ro-boundary (`next_flush_chunk`'s chunks end at or below it).
    #[must_use]
    pub fn flush_work_at(&self, cursor: u64) -> bool {
        debug_assert!(cursor <= self.ro_boundary, "flush cursor above ro_boundary");
        cursor < self.ro_boundary
    }

    /// The next unit of flush work at `cursor`, bounded by `max_bytes` —
    /// a pure query; the pipeline appends/seals, fdatasyncs, then
    /// confirms via [`advance_flushed`](Self::advance_flushed). `cursor`
    /// starts at `flushed` and walks chunk ends within one slice (the
    /// barrier lands once, at the slice end or at a gap seal).
    ///
    /// `Records` ends at the last recorded seal cut inside the budget —
    /// or, when the first cut past `cursor` already exceeds it, at that
    /// cut alone (minimum one boundary of progress, the seal-slice
    /// oversized-record rule); a hole start, the ro-boundary, or the RAM
    /// ring top bound it (the last one because the chunk is read as one
    /// contiguous slice — see the body). Returns `None` when `cursor`
    /// reached `ro_boundary`.
    ///
    /// # Panics
    /// Panics unless `flushed ≤ cursor ≤ ro_boundary` (programmer error:
    /// the cursor is this module's own vocabulary fed back).
    pub fn next_flush_chunk(&self, cursor: LogicalAddr, max_bytes: u64) -> Option<FlushChunk> {
        assert!(max_bytes > 0, "empty flush budget");
        let c = cursor.to_raw();
        assert!(c >= self.flushed, "flush cursor below flushed");
        assert!(c <= self.ro_boundary, "flush cursor above ro_boundary");
        if !self.flush_work_at(c) {
            return None;
        }
        // A hole beginning at the cursor is the gap chunk (its whole
        // extent is sealed: ro never lands inside a hole — holes contain
        // no record start).
        let next_hole = self.hole_marks.iter().find(|&&(start, _)| start >= c);
        if let Some(&(start, len)) = next_hole
            && start == c
        {
            debug_assert!(start + len <= self.ro_boundary, "ro inside a hole");
            return Some(FlushChunk::Gap {
                at: LogicalAddr::from_raw(start).expect("watermarks stay 48-bit"),
                len,
            });
        }
        // Records: bounded by the first hole start past the cursor (a
        // record end) or the ro-boundary (a record start) — both legal
        // `advance_flushed` targets — **and by the ring top**, because the
        // caller reads the chunk as one contiguous slice
        // ([`bytes`](Self::bytes)) and the ring wraps there.
        //
        // The ring top usually needs no bound of its own: a record that
        // would straddle it is pushed past it by a seal hole (see
        // [`alloc`](Self::alloc)), and that hole's start is already the
        // `hard` bound above. The exception is a record ending *exactly*
        // on the top — legal, no hole created — after which nothing in the
        // hole/cut vocabulary marks the wrap. M4-S16 found a flush chunk
        // spanning it that way (the top is a record boundary, so the chunk
        // looked legal): in a ring-sized region the read panics, and in a
        // region with slack it would hand the tier file bytes from the
        // wrong end of the ring under a valid CRC. Both are worse than a
        // short chunk, and a short chunk is free — the next call resumes at
        // the top.
        let ring = self.ring_mask + 1;
        let ring_top = c + (ring - ((c - self.life_origin) & self.ring_mask));
        let hard = next_hole
            .map_or(self.ro_boundary, |&(start, _)| start.min(self.ro_boundary))
            .min(ring_top);
        let budget_end = c.saturating_add(max_bytes).min(hard);
        let mut end = 0u64;
        for &cut in &self.flush_cuts {
            if cut <= c {
                continue;
            }
            if cut > hard {
                break;
            }
            if cut <= budget_end {
                end = cut;
            } else {
                if end == 0 {
                    end = cut; // minimum one boundary of progress
                }
                break;
            }
        }
        if end == 0 {
            // No cut recorded in the span (coarse advancement after a
            // dropped cut, or a legacy caller advancing ro directly):
            // the hard bound is itself a record boundary.
            end = hard;
        }
        debug_assert!(end > c, "flush chunk advances");
        Some(FlushChunk::Records { addr: cursor, len: end - c })
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
        if t > self.ro_boundary && self.flush_cuts.len() < FLUSH_CUT_CAP {
            // Every advance target is a record boundary (the caller's
            // contract above) — recorded so the flush can advance
            // `flushed` at seal-step granularity (ADR-0056 D3). A full
            // deque drops the cut: coarser flush steps, never a wrong
            // boundary.
            self.flush_cuts.push_back(t);
        }
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
        // Consume passed marks; `flushed` must never land strictly inside
        // a hole (zero-content ranges advance whole — ADR-0056 D3).
        while let Some(&(start, len)) = self.hole_marks.front() {
            if start + len <= t {
                self.hole_marks.pop_front();
                continue;
            }
            debug_assert!(t <= start, "flushed lands inside a sealed-dead hole");
            break;
        }
        while self.flush_cuts.front().is_some_and(|&c| c <= t) {
            self.flush_cuts.pop_front();
        }
        self.flushed = t;
        self.assert_watermark_order();
    }

    /// Releases RAM below `to`: whole pages now strictly below the head
    /// decommit (RSS returns to the OS — ADR-0052 D3).
    ///
    /// # Panics
    /// Panics unless `head ≤ to ≤ flushed`: pages never release above
    /// `flushed` — dropping unflushed bytes is data loss (§3.1). With a
    /// checkpoint walk pinned, additionally `to ≤` the walk watermark
    /// (ADR-0057 D2 — releasing past it would strand an image-class
    /// record on disk mid-walk).
    pub fn advance_head(&mut self, to: LogicalAddr) {
        let t = to.to_raw();
        assert!(t >= self.head, "head retreat");
        assert!(t <= self.flushed, "page release above flushed");
        assert!(t <= self.release_ceiling(), "page release past a pinned watermark or record");
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

    /// Commit/decommit page size (ADR-0052 D4) — the seal-mark and slice
    /// granularity S07's demotion machinery keys on.
    #[inline]
    pub fn page_bytes(&self) -> u64 {
        self.page_bytes
    }

    /// Always-on counters snapshot (S03 scrapes and asserts these).
    pub fn counters(&self) -> TieringCounters {
        TieringCounters { cold_resolves: self.cold_resolves.get(), ..self.counters }
    }

    /// Counts one typed cold-read failure served to a client (C2′ —
    /// the plane's resolve funnel and SCAN's key fetch report here).
    pub fn note_cold_read_error(&mut self) {
        self.counters.cold_read_errors += 1;
    }

    /// Counts one write replan (F-L06-03 — the plane's write funnel
    /// found a stale address after an extent read and re-resolved).
    pub fn note_write_replan(&mut self) {
        self.counters.write_replans += 1;
    }

    /// The ring reservation in bytes (`R`, a power of two).
    #[inline]
    pub fn ring_bytes(&self) -> u64 {
        self.ring_mask + 1
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

    /// Flush chunks (M4-S11): cut-bounded within the budget, minimum one
    /// boundary of progress past it, the ro-boundary as the final bound.
    #[test]
    fn flush_chunks_respect_cuts_and_budget() {
        let mut space = space(1 << 16, 1 << 12);
        let a = space.alloc(600).expect("fits");
        let b = space.alloc(600).expect("fits");
        let c = space.alloc(600).expect("fits");
        let end = space.alloc(1).expect("fits");
        // Three seal steps → three cuts (each a record start).
        space.advance_ro_boundary(b);
        space.advance_ro_boundary(c);
        space.advance_ro_boundary(end);
        // Budget covers the first two ranges: the chunk ends at the
        // largest cut inside it.
        let chunk = space.next_flush_chunk(a, 1300).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: a, len: 1200 });
        // A budget smaller than the first cut still advances one
        // boundary (the oversized-progress rule).
        let chunk = space.next_flush_chunk(a, 100).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: a, len: 600 });
        // From the last cut, the ro-boundary bounds the final chunk.
        let chunk = space.next_flush_chunk(c, 1 << 20).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: c, len: 600 });
        assert_eq!(space.next_flush_chunk(end, 64), None, "cursor at ro");
        // Confirming pops consumed cuts and the arithmetic re-runs from
        // the new floor.
        space.advance_flushed(b);
        let chunk = space.next_flush_chunk(b, 1 << 20).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: b, len: 1200 });
    }

    /// A ring-top hole surfaces as a `Gap` chunk exactly at its start;
    /// records before it end at the hole start, and `advance_flushed`
    /// consumes the mark when it crosses.
    #[test]
    fn flush_chunks_split_at_ring_top_holes() {
        let mut space = space(1 << 16, 1 << 12);
        let ring = 1u64 << 16;
        // Fill the front half, demote it fully (the honest lifecycle:
        // the window admission bound needs released pages before the
        // tail may approach the ring top — ADR-0053 D1).
        let a = space.alloc((ring / 2 - 100) as usize).expect("fits");
        let a_end = LogicalAddr::from_raw(ring / 2 - 100).expect("fits");
        space.advance_ro_boundary(a_end);
        space.advance_flushed(a_end);
        space.advance_head(a_end);
        let _ = a;
        // Fill to 200 short of the ring top, then allocate 300: hole.
        let b = space.alloc((ring / 2 - 100) as usize).expect("fits");
        let hole_start = b.to_raw() + (ring / 2 - 100);
        let c = space.alloc(300).expect("fits");
        assert_eq!(c.to_raw(), ring, "sealed to the ring top");
        let tail = space.tail();
        space.advance_ro_boundary(tail);
        // Records bound at the hole start even with a huge budget.
        let chunk = space.next_flush_chunk(b, u64::MAX).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: b, len: hole_start - b.to_raw() });
        let at_hole = LogicalAddr::from_raw(hole_start).expect("fits");
        let chunk = space.next_flush_chunk(at_hole, 64).expect("work exists");
        assert_eq!(chunk, FlushChunk::Gap { at: at_hole, len: 200 });
        space.advance_flushed(at_hole);
        space.advance_flushed(c);
        let chunk = space.next_flush_chunk(c, u64::MAX).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: c, len: 300 });
    }

    /// M4-S16 regression: a record that ends **exactly** on the ring top
    /// creates no seal hole, so nothing in the hole vocabulary marks the
    /// wrap — and a flush chunk that spans it would be read as one
    /// contiguous slice out of a wrapping ring (panic in a ring-sized
    /// region; wrong bytes under a valid CRC in one with slack). Every
    /// chunk must therefore stop at the top and resume from it.
    ///
    /// Pre-fix this test failed on the first assertion: the chunk ran from
    /// below the top to the ro-boundary above it, because the top is a
    /// legal record boundary and the cut/hole bounds said nothing about
    /// the ring.
    #[test]
    fn flush_chunks_stop_at_an_exactly_filled_ring_top() {
        let ring = 1u64 << 16;
        let quarter = (ring / 4) as usize;
        let mut sp = space(ring as usize, 1 << 12);
        // Walk the tail to one quarter short of the ring top, releasing as
        // we go (the window bound admits only budget-worth of RAM at a
        // time). No allocation straddles anything yet.
        for _ in 0..3 {
            sp.alloc(quarter).expect("fits");
            sp.advance_ro_boundary(sp.tail());
            sp.advance_flushed(sp.tail());
            sp.advance_head(sp.tail());
        }
        // This record ends *exactly* on the top: no hole is created, so
        // nothing marks the wrap. The next one starts at the top.
        let last = sp.alloc(quarter).expect("fits");
        assert_eq!(last.to_raw() + quarter as u64, ring, "exact fit against the top");
        let across = sp.alloc(quarter).expect("fits past the top");
        assert_eq!(across.to_raw(), ring, "no hole: the previous record fit exactly");
        assert_eq!(sp.counters().seal_holes, 0, "an exact fit needs no hole");
        sp.advance_ro_boundary(sp.tail());

        let chunk = sp.next_flush_chunk(last, u64::MAX).expect("work exists");
        assert_eq!(
            chunk,
            FlushChunk::Records { addr: last, len: quarter as u64 },
            "the chunk stops at the ring top instead of spanning the wrap"
        );
        // It resumes from the top for the record on the far side, and the
        // consumer's own guard agrees both chunks are single slices (the
        // pair assertion this bug slipped between).
        sp.advance_flushed(across);
        let chunk = sp.next_flush_chunk(across, u64::MAX).expect("work exists");
        assert_eq!(chunk, FlushChunk::Records { addr: across, len: quarter as u64 });
        assert_eq!(sp.bytes(across, quarter).len(), quarter);
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

    fn page_floor_of(rel: u64, page: u64) -> u64 {
        rel / page * page
    }

    fn page_ceil_of(rel: u64, page: u64) -> u64 {
        rel.div_ceil(page) * page
    }

    /// Builds a space at `window` (bytes, a page multiple) over its ring
    /// with the tail at `rel_tail` and the committed floor at `floor`:
    /// `floor` bytes placed and released, then `rel_tail − floor` placed,
    /// sealed and flushed but resident. No chunk crosses the ring top.
    fn space_at(ring: usize, page: usize, window: u64, rel_tail: u64, floor: u64) -> AddressSpace {
        let mut sp = space(ring, page);
        sp.set_window_limit(window);
        let half = ring as u64 / 2;
        let mut placed = 0u64;
        let mut place = |sp: &mut AddressSpace, up_to: u64, release: bool| {
            while placed < up_to {
                let chunk = (up_to - placed).min(half);
                sp.alloc(chunk as usize).expect("placement chunk fits");
                placed += chunk;
                sp.advance_ro_boundary(sp.tail());
                sp.advance_flushed(sp.tail());
                if release {
                    sp.advance_head(sp.tail());
                }
            }
        };
        place(&mut sp, floor, true);
        place(&mut sp, rel_tail, false);
        assert_eq!(sp.tail().to_raw(), rel_tail);
        assert_eq!(sp.head().to_raw(), floor);
        sp
    }

    /// The four-ask protocol (ADR-0174 D2 rule 1) over one state and one
    /// record, judged by the window inequality written out here — never
    /// by `alloc`, which consumes the same prospect: after the asks the
    /// record is placed with `page_ceil(end) − page_floor(head) ≤ W`, the
    /// region's own committed count is at most `W`, every `Demote(H)` has
    /// `head < H ≤ tail`, at most one pad, and `Fits` within four asks.
    /// Returns the pads placed: (ring-top pads, page pads).
    fn room_protocol(sp: &mut AddressSpace, len: usize, window: u64, page: u64) -> (u32, u32) {
        let ring = sp.ring_bytes();
        let mut asks = 0u32;
        let mut pads = (0u32, 0u32);
        loop {
            asks += 1;
            assert!(
                asks <= 4,
                "a fifth ask: len {len} window {window} tail {}",
                sp.tail().to_raw()
            );
            match sp.room(len) {
                Room::Fits => {
                    let addr = sp.alloc(len).expect("Fits places");
                    let end = addr.to_raw() + len as u64;
                    let head = sp.head().to_raw();
                    assert!(
                        page_ceil_of(end, page) - page_floor_of(head, page) <= window,
                        "the window inequality: end {end} head {head} window {window}"
                    );
                    assert!(sp.report().committed_bytes <= window, "the region's own count");
                    return pads;
                }
                Room::Demote(h) => {
                    let (h, tail, head) = (h.to_raw(), sp.tail().to_raw(), sp.head().to_raw());
                    assert!(h <= tail, "a need above the tail: {h} > {tail} (len {len})");
                    assert!(h > head, "a need the head passed: {h} <= {head}");
                    assert_eq!(h % page, 0, "a demote target is a page multiple");
                    // The demote step's model: seal, flush, release to H.
                    sp.advance_ro_boundary(sp.tail());
                    sp.advance_flushed(sp.tail());
                    sp.advance_head(LogicalAddr::from_raw(h).expect("48-bit"));
                }
                Room::Pad(to) => {
                    if to.to_raw() % ring == 0 {
                        pads.0 += 1;
                    } else {
                        pads.1 += 1;
                    }
                    assert!(pads.0 + pads.1 <= 1, "a second pad for one record (len {len})");
                    assert!(to.to_raw() > sp.tail().to_raw(), "a pad above the tail");
                    sp.pad_tail(to).expect("room answered Pad: the pad fits");
                }
                Room::End => unreachable!("far below the 48-bit end"),
            }
        }
    }

    /// Every legal state at `page`: windows of 4–9 pages over their
    /// rings — `next_pow2(W)`, and `2W` where `W` is a power of two
    /// (`MEM-BUDGET + MAINTAIN-SLICE` a little above it: the window is
    /// half its ring, ADR-0174 D2 rule 6's case (b)) — the tail at each
    /// offset of `offsets(ring)`, every legal committed floor, and each
    /// length of `lengths(ring)`. Returns (cases, ring-top pads, page
    /// pads); both pad kinds must occur, else the arm is VACUOUS.
    fn room_property(
        page: usize,
        offsets: impl Fn(u64) -> Vec<u64>,
        lengths: impl Fn(u64) -> Vec<u64>,
    ) -> (u64, u64, u64) {
        let page_bytes = page as u64;
        let mut cases = 0u64;
        let mut pads = (0u64, 0u64);
        let mut states: Vec<(u64, u64)> = Vec::new();
        for window_pages in 4..=9u64 {
            let window = window_pages * page_bytes;
            states.push((window, window.next_power_of_two()));
            if window_pages.is_power_of_two() {
                states.push((window, 2 * window));
            }
        }
        for (window, ring) in states {
            for rel_tail in offsets(ring) {
                let top = page_ceil_of(rel_tail, page_bytes);
                let lowest = top.saturating_sub(window);
                let mut floor = lowest;
                while floor <= page_floor_of(rel_tail, page_bytes) {
                    for len in lengths(ring) {
                        let mut sp = space_at(ring as usize, page, window, rel_tail, floor);
                        let placed = room_protocol(&mut sp, len as usize, window, page_bytes);
                        pads.0 += u64::from(placed.0);
                        pads.1 += u64::from(placed.1);
                        cases += 1;
                    }
                    floor += page_bytes;
                }
            }
        }
        assert!(pads.0 > 0 && pads.1 > 0, "VACUOUS: pads (ring top, page) {pads:?}");
        (cases, pads.0, pads.1)
    }

    /// The room property, exhaustive at a 4-unit page (DRR FCR-STTIER-01
    /// §6, I15; ADR-0174 D2 rule 6): every tail offset of the ring,
    /// every length up to half the ring.
    #[test]
    fn room_reaches_fits_in_four_asks_exhaustively_at_a_four_unit_page() {
        let (cases, ring_top_pads, page_pads) =
            room_property(4, |ring| (0..ring).collect(), |ring| (1..=ring / 2).collect());
        assert!(cases > 20_000, "the exhaustive arm ran {cases} cases");
        eprintln!(
            "room property: {cases} cases, {ring_top_pads} ring-top pads, {page_pads} page pads"
        );
    }

    /// The same property on the real 4 KiB page: every page boundary ∓ 1
    /// as the tail offset and as the length, the one-byte and half-ring
    /// records included.
    #[test]
    fn room_reaches_fits_in_four_asks_at_page_boundaries() {
        let page = 4u64 << 10;
        let around = move |limit: u64, from: u64| -> Vec<u64> {
            let mut out = vec![from, limit];
            let mut k = page;
            while k <= limit {
                out.extend([k - 1, k, k + 1]);
                k += page;
            }
            out.retain(|&v| v >= from && v <= limit);
            out.sort_unstable();
            out.dedup();
            out
        };
        let (cases, _, _) = room_property(
            page as usize,
            move |ring| around(ring - 1, 0),
            move |ring| around(ring / 2, 1),
        );
        assert!(cases > 5_000, "the boundary arm ran {cases} cases");
    }

    /// The hostile specs of DRR FCR-STTIER-01 §6 at the real 1 MiB
    /// commit page: a window below its ring with records at the inline
    /// maximum — `MEM-BUDGET 4mb BLOB-THRESHOLD 3mb` (a 5 MiB window, an
    /// 8 MiB ring, 2.9 MiB records), `4mb + 64kb` (a 4 MiB window, half
    /// its ring, records of 3.9 MiB and of exactly half the ring), and
    /// the default slice at 8, 16 and 32 MiB budgets with the longest
    /// inline record — at every 64 KiB tail offset of the ring. Each
    /// spec must place a pad on at least one offset, else VACUOUS.
    #[test]
    fn room_pads_the_hostile_specs_at_the_commit_page() {
        const MIB: u64 = 1 << 20;
        let page = MIB;
        let specs: [(u64, u64, &[u64]); 5] = [
            (5 * MIB, 8 * MIB, &[2_900 * MIB / 1000]),
            (4 * MIB, 8 * MIB, &[3_900 * MIB / 1000, 4 * MIB, 4 * MIB - 1]),
            (9 * MIB, 16 * MIB, &[8 * MIB, 8 * MIB - 1]),
            (17 * MIB, 32 * MIB, &[16 * MIB, 16 * MIB - 1]),
            (33 * MIB, 64 * MIB, &[32 * MIB, 32 * MIB - 1]),
        ];
        for (window, ring, lens) in specs {
            let mut pads_placed = 0u64;
            let mut rel_tail = 0u64;
            while rel_tail < ring {
                for &len in lens {
                    let floor = page_floor_of(rel_tail, page);
                    let mut sp = space_at(ring as usize, page as usize, window, rel_tail, floor);
                    let placed = room_protocol(&mut sp, len as usize, window, page);
                    pads_placed += u64::from(placed.0 + placed.1);
                }
                rel_tail += 64 << 10;
            }
            assert!(pads_placed > 0, "VACUOUS: no pad at window {window} ring {ring}");
        }
    }

    /// A record longer than the window has no placing answer (ADR-0174
    /// D2 rule 1's `End`: no watermark progress places it). The state is
    /// the live path's after a budget shrink: the ring stays, the window
    /// falls to four pages, and a record placed under the old budget —
    /// up to half the ring — comes back through relocation. With the
    /// tail within `hole > window` of the ring top, `room` answers `End`,
    /// `alloc` answers `None` and nothing changes; the stall target still
    /// reports the need as it stands. A record of the window itself keeps
    /// a reachable `Demote`.
    #[test]
    fn a_record_longer_than_the_window_answers_end() {
        let page = 1u64 << 12;
        let ring = 16 * page;
        // Placed under a window of half the ring: two resident pages,
        // the tail six pages below the ring top.
        let mut sp = space_at(ring as usize, page as usize, 8 * page, 10 * page, 8 * page);
        sp.set_window_limit(4 * page);
        let before = (sp.tail(), sp.counters(), sp.report());
        // Seven pages: a ring-top hole of six, both above the window.
        let len = (7 * page) as usize;
        assert_eq!(sp.room(len), Room::End);
        assert_eq!(sp.alloc(len), None);
        assert_eq!((sp.tail(), sp.counters(), sp.report()), before, "refusal mutates nothing");
        let target = sp.stall_target(len).expect("the need as it stands");
        assert_eq!(target.to_raw(), 19 * page, "the live target, above the tail");
        match sp.room((4 * page) as usize) {
            Room::Demote(h) => assert_eq!(h, sp.tail(), "a record of the window demotes"),
            other => panic!("a record of the window demotes: {other:?}"),
        }
    }

    /// The 48-bit end (ADR-0174 D2 rule 1's `End`): a life whose origin
    /// lies within one ring of the end of the space answers `End` from
    /// `room` and `None` from `alloc` for a record that would cross it,
    /// changing nothing, and the stall target names no watermark; a
    /// record ending exactly at the last address places.
    #[test]
    fn the_forty_eight_bit_end_answers_end() {
        let page = 1u64 << 12;
        let origin = LogicalAddr::from_raw(LogicalAddr::MAX_RAW + 1 - 2 * page).expect("48-bit");
        let mut sp = AddressSpace::new(AddressSpaceConfig {
            reserve_bytes: 1 << 16,
            page_bytes: page as usize,
            life_origin: origin,
        })
        .expect("reservation");
        assert_eq!(sp.alloc(page as usize), Some(origin), "one page below the end fits");
        let before = (sp.tail(), sp.counters(), sp.report());
        assert_eq!(sp.room(page as usize), Room::End, "the next page would cross the end");
        assert_eq!(sp.alloc(page as usize), None);
        assert_eq!((sp.tail(), sp.counters(), sp.report()), before, "refusal mutates nothing");
        assert_eq!(sp.stall_target(page as usize), None, "no watermark progress helps");
        let last = sp.alloc((page - 1) as usize).expect("ends exactly at the last address");
        assert_eq!(last.to_raw() + page - 1, LogicalAddr::MAX_RAW);
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
