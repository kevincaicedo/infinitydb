//! Read-driven promotion (M4.5-S30, ADR-0085): a verified foreground
//! cold read may relocate the fetched record image to the tail — the
//! second producer of the ADR-0059 unlogged relocation, differing from
//! copy-forward compaction only in candidate selection (the record a
//! command just fetched and key-verified, instead of a file's scan).
//!
//! The store side stays I/O-free (§3.3): the plane hands
//! [`TieredTable::try_promote`] the bytes its cold read already fetched;
//! nothing here reads a device byte, stages a WAL record, or waits.
//! Promotion is strictly best-effort — every refusal is a counted skip
//! (ADR-0085 D3), never a park, so the ADR-0053 D4 wait graph gains no
//! edge and a skipped promotion is exactly today's behavior.
//!
//! Replay/crash soundness is inherited, not invented: a promotion
//! chains its origin into the ADR-0059 D9 map exactly as a compaction
//! relocation does, the walk-pin veto is D9-1 verbatim, and the origin
//! cap defers rather than drops (D9-2 — "relocated without a recorded
//! origin" stays unrepresentable). The strengthening particular to
//! promotion: source and destination are byte-identical images, so any
//! checkpoint/WAL interleaving serves identical bytes whichever copy it
//! resolves.

use inf_foundation::LogicalAddr;

use super::TieredTable;

/// Admission-filter slots (ADR-0085 D2): 2¹⁴ × 4 B = 64 KiB per table —
/// a fixed L5 term, reported via `INFO tiering`. Direct-mapped; at the
/// finding's cold-read rate (~15k/s) the mean slot lifetime is ~1 s —
/// long against hot-key re-read intervals, short against sweep periods.
const FILTER_SLOTS: usize = 1 << 14;

/// Second-touch promotion filter (ADR-0085 D2): a direct-mapped tag
/// array, no sweeps, no lists, no per-record metadata — one cache line
/// touched per verified cold read. A tag collision evicts a pending
/// first touch (that key re-earns); a 31-bit tag false positive
/// promotes one read early (still key-verified against the live index
/// pair by the caller's sequence). Zero is the empty sentinel; tags
/// force the low bit so a real tag is never zero.
#[derive(Debug)]
pub(super) struct PromoteFilter {
    tags: Box<[u32]>,
}

impl PromoteFilter {
    pub(super) fn new() -> PromoteFilter {
        PromoteFilter { tags: vec![0u32; FILTER_SLOTS].into_boxed_slice() }
    }

    /// Records a verified cold read of `hash`. `true` = second touch —
    /// the caller may promote. The tag deliberately stays on a hit so a
    /// vetoed promotion retries on the key's next cold read; only a
    /// successful promotion clears it ([`clear`](Self::clear)).
    fn touch(&mut self, hash: u64) -> bool {
        let idx = (hash as usize) & (FILTER_SLOTS - 1);
        let tag = (hash >> 32) as u32 | 1;
        if self.tags[idx] == tag {
            true
        } else {
            self.tags[idx] = tag;
            false
        }
    }

    /// Returns the slot to the pool after a successful promotion — the
    /// record is RAM-resident now; the slot serves other keys.
    fn clear(&mut self, hash: u64) {
        let idx = (hash as usize) & (FILTER_SLOTS - 1);
        let tag = (hash >> 32) as u32 | 1;
        if self.tags[idx] == tag {
            self.tags[idx] = 0;
        }
    }

    /// The filter's fixed footprint (the L5 report term).
    pub(super) fn bytes() -> u64 {
        (FILTER_SLOTS * core::mem::size_of::<u32>()) as u64
    }
}

/// Promotion observability (ADR-0085 D6): the A/B and the DST oracles
/// read these; a silent promotion path would be an L10 violation.
/// `promoted_bytes` is a **volume** counter in the `compaction_bytes`
/// mold — it explains `flush_bytes` (the device cost of a promotion
/// arrives through the flush leg, which already counts it) and is
/// never a write-amplification numerator term (ADR-0060 D2 inherited).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct PromotionCounters {
    /// Records relocated to the tail by a verified cold read.
    pub promotions: u64,
    /// Bytes those relocations placed (volume, not a device leg).
    pub promoted_bytes: u64,
    /// Verified cold reads that recorded a first touch (no promotion).
    pub first_touch: u64,
    /// Second touches skipped: tail window refused the allocation
    /// (ADR-0059 D6 — the slice-end analog; never a wait).
    pub skip_window: u64,
    /// Second touches skipped: a checkpoint walk is pinned (D9-1).
    pub skip_pinned: u64,
    /// Second touches skipped: disk admission refused (ADR-0063 D2 —
    /// promotion pays admission like a foreground append).
    pub skip_disk: u64,
    /// Second touches skipped: the index pair went stale between the
    /// fetch and the promotion (the record moved or died).
    pub skip_stale: u64,
    /// Second touches skipped: the record is at the relocation-origin
    /// cap (D9-2 deferral — it re-offers after its covering swap).
    pub skip_cap: u64,
}

impl PromotionCounters {
    /// Field-wise fold for the cell aggregate (`INFO tiering`).
    /// Saturating: a scrape must never panic a serving cell.
    pub fn add(&mut self, ns: PromotionCounters) {
        self.promotions = self.promotions.saturating_add(ns.promotions);
        self.promoted_bytes = self.promoted_bytes.saturating_add(ns.promoted_bytes);
        self.first_touch = self.first_touch.saturating_add(ns.first_touch);
        self.skip_window = self.skip_window.saturating_add(ns.skip_window);
        self.skip_pinned = self.skip_pinned.saturating_add(ns.skip_pinned);
        self.skip_disk = self.skip_disk.saturating_add(ns.skip_disk);
        self.skip_stale = self.skip_stale.saturating_add(ns.skip_stale);
        self.skip_cap = self.skip_cap.saturating_add(ns.skip_cap);
    }
}

impl TieredTable {
    /// Whether verified cold reads may promote (ADR-0085 D6: the
    /// `tiered-promote-on-read` CONFIG key, pushed per cell). Disabled
    /// is fully inert — no filter traffic, no counters: the off arm of
    /// an A/B is exactly the pre-S30 read path.
    #[inline]
    #[must_use]
    pub fn promote_enabled(&self) -> bool {
        self.promote_enabled
    }

    /// Flips promotion admission (hot — the `push_pressure` fan).
    pub fn set_promote_enabled(&mut self, on: bool) {
        self.promote_enabled = on;
    }

    /// This table's promotion counters (`INFO tiering`).
    #[inline]
    #[must_use]
    pub fn promotion_counters(&self) -> PromotionCounters {
        self.promote_stats
    }

    /// The admission filter's fixed footprint (L5 report term).
    #[must_use]
    pub fn promote_filter_bytes() -> u64 {
        PromoteFilter::bytes()
    }

    /// Offers one verified cold read for promotion (ADR-0085 D4): the
    /// caller fetched `image` from `addr`, key-verified it, and holds
    /// no borrow. Executes compaction's live arm — exact-pair liveness,
    /// origin-cap deferral, verbatim relocation, origin chaining, index
    /// repoint, extent-map relocation, death attribution — or skips
    /// with a counted reason (never waits, never errors). `true` = the
    /// record now lives at the tail.
    ///
    /// No WAL record stages here: promotions are unlogged relocations;
    /// the next displacing mutation's `take_displacement_origins` +
    /// marker staging (the existing command-wiring path) carries the
    /// ADR-0059 D9 repair.
    pub fn try_promote(&mut self, hash: u64, addr: LogicalAddr, image: &[u8]) -> bool {
        if !self.promote_enabled {
            return false;
        }
        debug_assert_eq!(
            crate::record::encoded_len_from_header(image),
            image.len(),
            "promotion image is not exactly one record"
        );
        debug_assert_eq!(
            hash,
            TieredTable::hash_key(TieredTable::decode_record(image).key),
            "promotion hash does not match the fetched image's key"
        );
        if !self.promote_filter.touch(hash) {
            self.promote_stats.first_touch += 1;
            return false;
        }
        // Vetoes (D3) — the filter tag stays, so the next cold read of
        // this key retries the promotion. Deliberately no `demote_due`
        // veto: page-granular head advancement leaves that signal true
        // at quiescence (flushed sits mid-page), so a soft veto keyed on
        // it starves promotion outright — the window bound below is the
        // honest subordination (ADR-0085 D3, revised during the test
        // leg).
        if self.space.walk_watermark().is_some() {
            self.promote_stats.skip_pinned += 1;
            return false;
        }
        // Exact-pair liveness (ADR-0058 D6 discipline, compaction's
        // bar): the record may have moved or died since the fetch.
        if !self.index.contains_pair(hash, addr) {
            self.promote_stats.skip_stale += 1;
            return false;
        }
        // Origin-cap deferral (D9-2): promoting would drop an origin,
        // which is unrepresentable; the record re-offers after its
        // covering swap drains the entry.
        let chained = self.reloc_origins.get(&(hash, addr.to_raw()));
        if chained.is_some_and(|origins| origins.len() >= super::RELOC_ORIGIN_CAP) {
            self.promote_stats.skip_cap += 1;
            return false;
        }
        // Disk admission (ADR-0063 D2, `append`'s refuse-before/
        // debit-after discipline): unlike compaction, a promotion
        // reclaims nothing at placement time, so it pays admission
        // like a foreground append. Refusal is a skip, never typed.
        let len = image.len() as u64;
        if self.disk_admit_check(len).is_err() {
            self.promote_stats.skip_disk += 1;
            return false;
        }
        let Some(new_addr) = self.relocate(image) else {
            self.promote_stats.skip_window += 1;
            return false;
        };
        self.disk_admit_debit(len);
        // Chain the origin forward (D9-2, compaction_apply's lines):
        // the address every un-superseded checkpoint may ref joins the
        // new placement's entry, stamped for the covering-swap drop.
        let mut origins = self.reloc_origins.remove(&(hash, addr.to_raw())).unwrap_or_default();
        origins.push((addr.to_raw(), self.live.ckpt_begun()));
        self.reloc_origins.insert((hash, new_addr.to_raw()), origins);
        self.index.replace(hash, addr, new_addr);
        // References move with the verbatim copy (ADR-0061 D4): the
        // map entry relocates *before* `note_death`, so the refcount
        // never dips across a promotion.
        if image[0] >> 4 == crate::record::TypeTag::StringExtent as u8 {
            let moved = self.extents.relocate(addr.to_raw(), new_addr.to_raw());
            debug_assert!(moved, "an extent record's address is always mapped");
        }
        self.note_death(addr, len);
        self.promote_filter.clear(hash);
        self.promote_stats.promotions += 1;
        self.promote_stats.promoted_bytes += len;
        true
    }
}
