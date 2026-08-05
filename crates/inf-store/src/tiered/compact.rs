//! Copy-forward compaction (M4-S15, ADR-0059): MAINTAIN-slice
//! log-structured GC over the cold tier. Live records from the oldest
//! eligible file re-append at the tail (verbatim — they become hot again
//! only in address terms and re-age naturally), the index repoints,
//! deaths self-attribute through the S06/S14 hooks, and a fully-scanned
//! file finalizes its byte counters (the ADR-0058 obligation) on its way
//! into the D3 retirement pipeline — excluded from the next manifest,
//! detached after the swap lands, unlinked when the read pins drain.
//!
//! The store side never does I/O (§3.3): the plane asks
//! [`TieredTable::compaction_work`] for the next read, issues it through
//! the S08 path pre-classed `ReadClass::Maintain` (ADR-0055 D3 — the 3:1
//! deficit exists for this consumer), and feeds the verified bytes to
//! [`TieredTable::compaction_apply`]. Admission refusal ends the slice —
//! compaction runs inside the MAINTAIN round whose flush/release legs
//! resolve the tail-allocation wait, so it must participate only as an
//! allocator, never as a waiter (ADR-0059 D6; the ADR-0053 D4 wait graph
//! stays acyclic).

use inf_foundation::LogicalAddr;
use inf_log::flush::TierFlush;
use inf_log::fs::SegmentFs;

use super::TieredTable;

/// Copy-forward configuration (ADR-0059 D1/D6; S19 exposes the reserved
/// `COMPACTION-DEAD-RATIO` / `COMPACTION-SLICE` keys later).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Dead-ratio trigger threshold, percent of `data_len` (default 50:
    /// below one-half dead, copy-forward moves more bytes than it
    /// reclaims).
    pub dead_ratio_pct: u8,
    /// Per-round budget across scan reads and relocations (default the
    /// ADR-0052 D4 MAINTAIN quantum). A single record larger than the
    /// budget still makes progress ([`CompactionApplied::need`] — the
    /// seal-slice minimum-progress rule).
    pub slice_bytes: u64,
}

impl Default for CompactionConfig {
    fn default() -> CompactionConfig {
        CompactionConfig { dead_ratio_pct: 50, slice_bytes: 1 << 20 }
    }
}

/// The in-flight scan cursor — one candidate at a time (bounded state).
#[derive(Copy, Clone, Debug)]
pub struct CompactCursor {
    /// The candidate file.
    file_id: u32,
    /// Next unscanned address (always a record boundary).
    next: u64,
    /// Exclusive end of the candidate's range (`base + data_len`).
    end: u64,
    /// Live records skipped at the origin cap so far (ADR-0059 D9): a
    /// nonzero count blocks finalization at scan end — the file still
    /// holds live records by construction.
    deferred: u32,
}

/// What the plane should do next for compaction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompactionWork {
    /// Cold-read `len` bytes of `file_id` starting at `addr`
    /// (`ReadClass::Maintain`), then call
    /// [`TieredTable::compaction_apply`] with the verified bytes.
    Read { file_id: u32, addr: LogicalAddr, len: u64 },
    /// No candidate under the current trigger arms. Under disk pressure
    /// this is the `nothing_compactable` verdict S21 consumes — a
    /// zero-dead tier cannot be compacted into free space (L10: the
    /// blind spot is visible, not papered over).
    Idle,
}

/// What one [`TieredTable::compaction_apply`] call did (observability;
/// the storm/DST oracles assert against these).
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct CompactionApplied {
    /// Bytes of the chunk consumed (whole records; the scan cursor
    /// advanced exactly this far).
    pub consumed: u64,
    /// Records relocated to the tail (they were live).
    pub relocated: u32,
    /// Bytes relocated (== the `compaction_bytes` charged this call).
    pub relocated_bytes: u64,
    /// Bytes of records verified dead and skipped (never re-attributed —
    /// their deaths were counted at their repoint/delete moment).
    pub dead_skipped_bytes: u64,
    /// Live records skipped at the relocation-origin cap (ADR-0059 D9):
    /// they relocate after the next covering swap drains their entry.
    pub deferred: u32,
    /// The tail window refused an allocation: the slice ends here and
    /// the scan resumes at `consumed` next round, after flush/release
    /// progress (ADR-0059 D6).
    pub stalled: bool,
    /// Nonzero when the next record is larger than the supplied chunk:
    /// the exact bytes a re-read must span (minimum-one-record
    /// progress — one oversized record may exceed the slice budget,
    /// bounded at one record).
    pub need: u64,
    /// The candidate's scan completed: its byte counters finalized
    /// (ADR-0059 D4) and it is ready for the retirement pipeline.
    pub file_scanned: bool,
}

impl TieredTable {
    /// This table's copy-forward configuration.
    #[inline]
    #[must_use]
    pub fn compaction_config(&self) -> CompactionConfig {
        self.compact_cfg
    }

    /// Replaces the copy-forward configuration (tests/S19 wiring).
    ///
    /// # Panics
    /// Panics on a nonsense config — a ratio above 100 or an empty
    /// slice budget cannot express a policy.
    pub fn set_compaction_config(&mut self, cfg: CompactionConfig) {
        assert!(cfg.dead_ratio_pct <= 100, "dead ratio is a percentage");
        assert!(cfg.slice_bytes > 0, "empty compaction slice budget");
        self.compact_cfg = cfg;
    }

    /// The next unit of compaction work, bounded by `max_bytes`
    /// (ADR-0059 D1/D2). Picks a candidate when none is in flight:
    /// sealed, fully cold (`end ≤ head`), not retiring, not already
    /// dead — lowest base first among files at or above the dead-ratio
    /// threshold; under `pressure`, the highest dead ratio with any
    /// dead bytes at all. Recovered files observed empty by count
    /// finalize here without a scan (ADR-0059 D4 — zero I/O).
    pub fn compaction_work<F: SegmentFs>(
        &mut self,
        flush: &TierFlush<F>,
        pressure: bool,
        max_bytes: u64,
    ) -> CompactionWork {
        assert!(max_bytes > 0, "empty compaction read budget");
        // Compaction pauses while a walk is pinned (ADR-0059 D9-1): a
        // mid-walk relocation lets one walk emit a ref and an image for
        // the same key — a duplicate born inside the checkpoint that no
        // tail marker can heal. Deferral is bounded by the walk (index
        // + RAM images, ADR-0057 D2), the same debt release accepts.
        if self.space.walk_watermark().is_some() {
            return CompactionWork::Idle;
        }
        if self.compact.is_none() {
            self.compact = self.pick_candidate(flush, pressure);
            // ADR-0063 D5: pressure asked for space and no eligible file
            // has a dead byte — the counted `nothing_compactable` alarm
            // (walk-pin deferral above deliberately does not count: that
            // is a bounded pause, not a blind spot).
            if pressure && self.compact.is_none() {
                self.compact_idle_pressure += 1;
            }
        }
        let Some(cur) = self.compact else { return CompactionWork::Idle };
        debug_assert!(cur.next < cur.end, "a finished cursor must not persist");
        let len = (cur.end - cur.next).min(max_bytes);
        CompactionWork::Read {
            file_id: cur.file_id,
            addr: LogicalAddr::from_raw(cur.next).expect("scan cursor is 48-bit"),
            len,
        }
    }

    /// Applies one cold-read chunk of the current candidate (ADR-0059
    /// D2): walks whole records, verifies liveness by the exact
    /// `(hash, addr)` pair, relocates live records verbatim, and skips
    /// dead ones. The chunk must start exactly at the scan cursor.
    ///
    /// # Panics
    /// Panics when no scan is in flight or the chunk does not start at
    /// the cursor (plane bug — the cursor is this module's own
    /// vocabulary fed back), and when record framing overruns the
    /// file's range (CRC-verified bytes cannot legally do this).
    pub fn compaction_apply(
        &mut self,
        file_id: u32,
        chunk_addr: LogicalAddr,
        bytes: &[u8],
    ) -> CompactionApplied {
        let cur = self.compact.expect("compaction_apply without a scan in flight");
        assert_eq!(cur.file_id, file_id, "chunk for a different candidate");
        assert_eq!(cur.next, chunk_addr.to_raw(), "chunk must start at the scan cursor");
        let remaining = usize::try_from(cur.end - cur.next).expect("file range fits usize");
        let window = bytes.len().min(remaining);
        let mut applied = CompactionApplied::default();
        let mut off = 0usize;
        while off + TieredTable::RECORD_HEADER_LEN <= window {
            let len = crate::record::encoded_len_from_header(&bytes[off..]);
            assert!(
                cur.next + (off + len) as u64 <= cur.end,
                "record framing overruns the file range"
            );
            if off + len > window {
                // The record does not fit this chunk: ask for exactly it
                // (minimum-one-record progress; bounded at one record).
                applied.need = len as u64;
                break;
            }
            let image = &bytes[off..off + len];
            let parts = TieredTable::decode_record(image);
            debug_assert_eq!(parts.encoded_len, len, "image framing agrees with its header");
            let hash = TieredTable::hash_key(parts.key);
            let old = LogicalAddr::from_raw(cur.next + off as u64).expect("in-range address");
            // Exact-pair liveness (ADR-0058 D6 discipline): within one
            // life this cannot alias — addresses are monotonic and
            // `life_origin` bounds every recovered file's end, so a
            // matching slot names exactly this record.
            if self.index.contains_pair(hash, old) {
                // A record at the origin cap defers (ADR-0059 D9-2):
                // its entry drains at the next covering swap, and a
                // deferred record blocks finalization, never soundness.
                let chained = self.reloc_origins.get(&(hash, old.to_raw()));
                if chained.is_some_and(|origins| origins.len() >= super::RELOC_ORIGIN_CAP) {
                    applied.deferred += 1;
                    off += len;
                    continue;
                }
                let Some(new_addr) = self.relocate(image) else {
                    applied.stalled = true;
                    break;
                };
                // Chain the origin forward (D9-2): the address every
                // un-superseded checkpoint may ref joins the new
                // placement's entry, stamped with the latest begun
                // walk id for the covering-swap drop rule.
                let mut origins =
                    self.reloc_origins.remove(&(hash, old.to_raw())).unwrap_or_default();
                origins.push((old.to_raw(), self.live.ckpt_begun()));
                self.reloc_origins.insert((hash, new_addr.to_raw()), origins);
                self.index.replace(hash, old, new_addr);
                // References move with the verbatim copy (M4-S17,
                // ADR-0061 D4): the map entry relocates *before*
                // `note_death`, which then finds nothing at `old` — the
                // refcount never dips or spikes across a relocation, and
                // blob bytes never flow through compaction.
                if image[0] >> 4 == crate::record::TypeTag::StringExtent as u8 {
                    let moved = self.extents.relocate(old.to_raw(), new_addr.to_raw());
                    debug_assert!(moved, "an extent record's address is always mapped");
                }
                self.note_death(old, len as u64);
                self.note_compaction_bytes(len as u64);
                applied.relocated += 1;
                applied.relocated_bytes += len as u64;
            } else {
                // Dead — skip, never attribute (the death was counted at
                // its repoint/delete moment; re-attributing here is the
                // double-count ADR-0058 D4 forbids).
                applied.dead_skipped_bytes += len as u64;
            }
            off += len;
        }
        applied.consumed = off as u64;
        let next = cur.next + applied.consumed;
        let deferred = cur.deferred + applied.deferred;
        if next == cur.end && !applied.stalled {
            // Finalize only a provably-empty file (ADR-0059 D4): a
            // deferred record is still live, so the scan just ends —
            // the file re-offers after its covering swap.
            if deferred == 0 {
                self.live.finalize_scanned(cur.file_id);
                applied.file_scanned = true;
            }
            self.compact = None;
        } else {
            self.compact =
                Some(CompactCursor { file_id: cur.file_id, next, end: cur.end, deferred });
        }
        if applied.consumed > 0 || applied.stalled {
            self.space.note_compact_slice();
        }
        applied
    }

    /// Marks retirable files for the manifest under construction
    /// (ADR-0059 D3 phase 1): `is_dead` ∧ unref-stamped before
    /// checkpoint `ckpt_id` began ∧ present in the sealed catalog (the
    /// active file is never excluded — the flush is still appending to
    /// it). Call between the walk's end and `tier_manifest`. Returns how
    /// many files newly entered the retiring state.
    pub fn retire_scan<F: SegmentFs>(&mut self, ckpt_id: u64, flush: &TierFlush<F>) -> u32 {
        let marked = self.live.retire_scan(ckpt_id, |id| flush.sealed().iter().any(|m| m.id == id));
        // A mid-scan candidate that emptied under foreground deaths can
        // retire out from under its own scan — abandon the cursor (the
        // file needs no further copying by definition of `is_dead`).
        if let Some(cur) = self.compact
            && self.live.is_retiring(cur.file_id)
        {
            self.compact = None;
        }
        marked
    }

    /// The swap landed (ADR-0059 D3 phase 3): retiring files leave the
    /// live set, and relocation origins the landed checkpoint provably
    /// does not reference drain from the D9 map (same stamp rule). Call
    /// on **every** landed swap, retirements or not. The returned ids
    /// drive `TierFlush::detach_sealed` and the pin-gated unlink at the
    /// plane layer (`ColdReads::inflight_on` is runtime state this
    /// crate never sees).
    pub fn commit_retirement(&mut self) -> Vec<u32> {
        let landed = self.live.ckpt_begun();
        self.reloc_origins.retain(|_, origins| {
            origins.retain(|&(_, stamp)| stamp >= landed);
            !origins.is_empty()
        });
        let ids = self.live.commit_retirement();
        debug_assert!(
            self.compact.is_none_or(|cur| !ids.contains(&cur.file_id)),
            "retire_scan already abandoned a retiring candidate's cursor"
        );
        ids
    }

    /// The swap failed as a counted abort (ADR-0059 D3 phase 4): the
    /// old unit still names every retiring file — the marks clear and
    /// the files are re-offered at the next publication.
    pub fn abort_retirement(&mut self) {
        self.live.abort_retirement();
    }

    /// The cold floor (ADR-0059 D5): the lowest surviving tier file's
    /// base — no live address exists below it. The endurance AC's "head
    /// advances" is this coordinate strictly increasing.
    #[inline]
    #[must_use]
    pub fn cold_floor(&self) -> u64 {
        self.live.cold_floor()
    }

    /// Candidate selection (ADR-0059 D1). Also finalizes recovered
    /// files observed empty by count — they need no scan, and their
    /// byte counters heal to exact on sight.
    fn pick_candidate<F: SegmentFs>(
        &mut self,
        flush: &TierFlush<F>,
        pressure: bool,
    ) -> Option<CompactCursor> {
        let head = self.space.head().to_raw();
        let pct = u64::from(self.compact_cfg.dead_ratio_pct);
        // Finalize-on-sight for count-dead recovered files (bounded by
        // the file count; control-plane work protecting an O(1) verdict).
        let heal: Vec<u32> = self
            .live
            .files()
            .iter()
            .filter(|f| f.recovered && !f.byte_exact && !f.retiring && f.live_count == 0)
            .map(|f| f.id)
            .collect();
        for id in heal {
            self.live.finalize_scanned(id);
        }
        let eligible = |f: &crate::live_set::FileLiveSet| {
            !f.retiring
                && !f.is_dead()
                && f.base + f.data_len <= head
                && flush.sealed().iter().any(|m| m.id == f.id)
        };
        // Dead-ratio arm: lowest base first (files are address-ordered).
        let threshold = self
            .live
            .files()
            .iter()
            .find(|f| eligible(f) && f.dead_bytes * 100 >= f.data_len * pct);
        // Pressure arm: widest eligible with any dead bytes, by ratio.
        let candidate = threshold.or_else(|| {
            if !pressure {
                return None;
            }
            self.live
                .files()
                .iter()
                .filter(|f| eligible(f) && f.dead_bytes > 0)
                .max_by(|a, b| (a.dead_bytes * b.data_len).cmp(&(b.dead_bytes * a.data_len)))
        });
        candidate.map(|f| CompactCursor {
            file_id: f.id,
            next: f.base,
            end: f.base + f.data_len,
            deferred: 0,
        })
    }
}
