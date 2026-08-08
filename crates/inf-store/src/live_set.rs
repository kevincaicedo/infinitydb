//! Per-tier-file live-set counters (M4-S14, ADR-0058) — what compaction
//! needs to know about what is live where, kept **exact within a boot
//! life with zero scans**: dead bytes are attributed at the moment an
//! index slot repoints or deletes (the S06 hook — deaths always carry
//! their length, the caller's verified view), and chunks are charged to
//! their file as the flush files them (a chunk lands in exactly one
//! file — `TierFlush` seals *before* an overflowing range, never inside
//! one). All state is cell-local (L1); losing it loses no data — the
//! counters are a projection of index events (L2).
//!
//! Three attribution destinations, decided by two compares (ADR-0058
//! D2): a pre-life address lands in a **recovered** file (count + byte
//! lower bound), an address at or above the filed coverage buffers in
//! the **pending-span map** until its range files, and everything else
//! lands in a **this-life** file (byte-exact). Ring-top holes belong to
//! no file and never arrive here — gap seals close a file before the
//! hole, so files are hole-free and `live + dead = data_len` is a real
//! identity, not an approximation.
//!
//! Recovery (ADR-0058 D4 — revising the plan's lazy-walk sketch): counts
//! reconstruct exactly *during* recovery — `apply_ref` increments on
//! actual insert, `apply_displace` decrements on actual removal — and
//! serialized byte counters restore under the D5 clamp rules, which only
//! ever under-count dead. The deletion predicate [`FileLiveSet::is_dead`]
//! is therefore sound at first serve: it errs toward retention, never
//! toward the §8 premature unlink.

use std::collections::BTreeMap;

use inf_log::LiveSetFileEntry;
use inf_log::flush::TierFileMeta;

/// One tier file's live-set counters (ADR-0058 D1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLiveSet {
    /// Tier file id (`tier-NNNNNN.itier`).
    pub id: u32,
    /// First logical address of the file's range.
    pub base: u64,
    /// Bytes filed so far (equals `TierFileMeta::data_len` once sealed;
    /// the manifested `durable_len` for a recovered file).
    pub data_len: u64,
    /// Bytes of records in `[base, base + data_len)` attributed dead at
    /// their repoint/delete moment. Exact when `byte_exact`; otherwise a
    /// lower bound (never an over-count — ADR-0058 D4).
    pub dead_bytes: u64,
    /// Index slots naming an address in this file's range. Maintained
    /// only for recovered files (this-life files answer by bytes);
    /// reconstructed exactly by recovery's ref-apply/displace-replay.
    pub live_count: u64,
    /// Seeded from the boot catalog (true) vs created by this life's
    /// flush (false).
    pub recovered: bool,
    /// Whether `data_len − dead_bytes` is exact live bytes. True for
    /// this-life files; for recovered files only via the D5 restore rule
    /// (a fully-dead file cannot regain live records — its duplicate
    /// refs die by their own tail displacement markers).
    pub byte_exact: bool,
    /// The most recently **begun** checkpoint id at the last event that
    /// removed or repointed an index slot naming this file's range
    /// (M4-S15, ADR-0059 D3). A checkpoint whose id exceeds this stamp
    /// began after the last such removal, so its walk enumerated no slot
    /// naming this file and its `.ick` provably holds no reference into
    /// it — the "manifested-after-empty" half of the §3.1 deletion rule.
    pub unref_stamp: u64,
    /// Provisionally excluded from the manifest under construction
    /// (ADR-0059 D3): set by `retire_scan`, cleared by
    /// `abort_retirement` when the swap fails (the old unit still names
    /// the file), consumed by `commit_retirement` when it lands.
    pub retiring: bool,
}

impl FileLiveSet {
    /// Exact live bytes, when the byte counters are exact.
    #[must_use]
    pub fn live_bytes(&self) -> Option<u64> {
        self.byte_exact.then(|| self.data_len - self.dead_bytes)
    }

    /// The §3.1 deletion precondition (`live(F) == 0`), sound in both
    /// arms (ADR-0058 D1): this-life files answer by exact bytes;
    /// recovered files answer by the exact slot count. Necessary, never
    /// sufficient — S15 additionally requires the manifested-after-empty
    /// checkpoint stamp and a drained read-pin count.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        if self.byte_exact { self.dead_bytes == self.data_len } else { self.live_count == 0 }
    }
}

/// One tiered namespace's live-set bookkeeping on one cell (L1).
pub struct LiveSet {
    /// Address-ordered (equivalently: id-ordered — both monotone in
    /// flush order). Small (one entry per ~1 GiB tier file); deaths
    /// binary-search it.
    files: Vec<FileLiveSet>,
    /// Dead spans at or above `coverage_end` — records that died before
    /// their range filed. Keyed by span start; non-overlapping (a record
    /// dies once); adjacent-merged on insert. L5 term: bounded by the
    /// un-filed region's record count, merged in practice because deaths
    /// cluster in allocation order (disclosed, ADR-0058 D2).
    pending: BTreeMap<u64, u64>,
    /// Exclusive end of filed coverage: this life's origin at boot,
    /// advanced by every filed chunk. Addresses at or above it cannot be
    /// in any file yet.
    coverage_end: u64,
    /// The most recently begun checkpoint's id (M4-S15, ADR-0059 D3):
    /// the value slot-removal events stamp their file with. Seeded with
    /// the manifested id at recovery; advanced by `note_ckpt_begun` at
    /// every walk begin. Zero before the first checkpoint of a fresh
    /// namespace — every id compares above it, which is correct (no walk
    /// exists whose refs could name anything).
    ckpt_begun: u64,
}

impl LiveSet {
    /// A fresh namespace's (empty) live set.
    #[must_use]
    pub fn new(life_origin: u64) -> LiveSet {
        LiveSet {
            files: Vec::new(),
            pending: BTreeMap::new(),
            coverage_end: life_origin,
            ckpt_begun: 0,
        }
    }

    /// Seeds the recovered files from the manifested catalog (ADR-0058
    /// D4): counts start at zero and reconstruct during recovery; byte
    /// counters start at the sound floor and restore under the D5 rules
    /// when a live-set checkpoint section arrives. `boot_ckpt_id` is the
    /// manifested checkpoint's id — every recovered file's unref stamp
    /// starts there (ADR-0059 D3), so the first post-boot checkpoint
    /// (whose id is strictly higher) can already cover a file the replay
    /// left empty.
    ///
    /// # Panics
    /// Panics when the catalog is not ascending/non-overlapping or
    /// reaches past the new life's origin — manifest-decode invariants
    /// fed back, so a violation is a caller bug.
    pub fn seed_recovered(
        &mut self,
        catalog: &[TierFileMeta],
        life_origin: u64,
        boot_ckpt_id: u64,
    ) {
        assert!(self.files.is_empty() && self.pending.is_empty(), "seed on a used live set");
        for meta in catalog {
            let base = meta.base.to_raw();
            if let Some(last) = self.files.last() {
                assert!(base >= last.base + last.data_len, "catalog ranges must not overlap");
            }
            assert!(base + meta.data_len <= life_origin, "a manifested file inside the new life");
            self.files.push(FileLiveSet {
                id: meta.id,
                base,
                data_len: meta.data_len,
                dead_bytes: 0,
                live_count: 0,
                recovered: true,
                byte_exact: false,
                unref_stamp: boot_ckpt_id,
                retiring: false,
            });
        }
        self.coverage_end = life_origin;
        self.ckpt_begun = boot_ckpt_id;
    }

    /// Restores one serialized byte-counter entry (ADR-0058 D5). The
    /// entry applies only when its `data_len` equals the manifested
    /// length — a mismatch means part of the serialized aggregate covers
    /// bytes recovery re-appended, and keeping any of it could over-count
    /// dead inside the durable range (the one forbidden direction). An
    /// entry naming no catalog file is legal (a filed-but-unconfirmed
    /// file the manifest did not name) and restores nothing.
    pub fn restore_entry(&mut self, entry: &LiveSetFileEntry) {
        let Some(file) = self.files.iter_mut().find(|f| f.id == entry.file_id) else {
            return;
        };
        debug_assert!(file.recovered, "restore targets catalog files only");
        if entry.data_len != file.data_len {
            return;
        }
        debug_assert!(entry.dead_bytes <= entry.data_len, "decoder audits dead ≤ len");
        file.dead_bytes = entry.dead_bytes;
        file.byte_exact = entry.byte_exact && entry.dead_bytes == file.data_len;
    }

    /// Charges one filed chunk to its file (the `flush_slice` Records
    /// arm): extends the file's length and drains pending dead spans the
    /// chunk now covers. A span straddling the chunk end splits — legal,
    /// because a merged span is a union of whole records and a chunk end
    /// is a record boundary between two of them.
    ///
    /// # Panics
    /// Panics when the chunk is not contiguous with the file (mirrors
    /// `TierFlush::append_range`'s own contiguity contract).
    pub fn note_filed(&mut self, id: u32, base: u64, chunk_addr: u64, chunk_len: u64) {
        assert!(chunk_len > 0, "empty filed chunk");
        assert!(chunk_addr >= self.coverage_end, "chunk below filed coverage");
        match self.files.last_mut() {
            Some(last) if last.id == id => {
                assert_eq!(last.base + last.data_len, chunk_addr, "filed chunks are contiguous");
                last.data_len += chunk_len;
            }
            _ => {
                debug_assert!(
                    self.files.last().is_none_or(|last| id > last.id),
                    "file ids ascend in flush order"
                );
                assert_eq!(base, chunk_addr, "a new file starts at its first chunk");
                self.files.push(FileLiveSet {
                    id,
                    base,
                    data_len: chunk_len,
                    dead_bytes: 0,
                    live_count: 0,
                    recovered: false,
                    byte_exact: true,
                    unref_stamp: self.ckpt_begun,
                    retiring: false,
                });
            }
        }
        self.coverage_end = chunk_addr + chunk_len;
        let drained = self.drain_pending_below(self.coverage_end);
        if drained > 0 {
            let file = self.files.last_mut().expect("filed above");
            file.dead_bytes += drained;
            debug_assert!(file.dead_bytes <= file.data_len, "dead exceeds file bytes");
        }
    }

    /// Attributes one record death at the S06 hook moment (ADR-0058 D2).
    /// The routing: not-yet-filed addresses buffer as pending spans;
    /// filed addresses charge their containing file — and a recovered
    /// file's slot count decrements alongside (the death removed exactly
    /// one index slot naming this range).
    pub fn note_dead(&mut self, addr: u64, len: u64) {
        assert!(len > 0, "empty death");
        if addr >= self.coverage_end {
            self.insert_pending(addr, len);
            return;
        }
        let ckpt_begun = self.ckpt_begun;
        let Some(file) = self.file_containing_mut(addr, len) else {
            // A death inside a ring-top hole or below the first
            // manifested file has no record to die — a caller bug, not
            // an operating condition.
            debug_assert!(false, "death at {addr} outside every file range");
            return;
        };
        file.dead_bytes += len;
        debug_assert!(file.dead_bytes <= file.data_len, "dead exceeds file bytes");
        if file.recovered {
            debug_assert!(file.live_count > 0, "death in a file with no counted slots");
            file.live_count = file.live_count.saturating_sub(1);
        }
        // The death removed/repointed an index slot naming this file —
        // stamp it (ADR-0059 D3). Drained pending spans deliberately do
        // not stamp: no walk can ever have emitted a ref into a range
        // that filed after its record died (the slot was image-class for
        // every walk that saw it).
        file.unref_stamp = ckpt_begun;
    }

    /// Counts one applied checkpoint reference into its containing
    /// recovered file (recovery only — ADR-0058 D4; the caller's
    /// `(hash, addr)` idempotency guard already collapsed duplicates).
    pub fn note_ref(&mut self, addr: u64) {
        let Some(file) = self.file_containing_mut(addr, 1) else {
            // A ref naming a hole/gap address: the checkpoint and the
            // manifest disagree — the cold read of that slot will fail
            // loudly downstream; the count stays honest by not counting.
            debug_assert!(false, "ref at {addr} outside every manifested file");
            return;
        };
        debug_assert!(file.recovered, "refs name pre-life addresses only");
        file.live_count += 1;
    }

    /// Uncounts one displaced pre-life slot (recovery replay —
    /// ADR-0058 D4; the caller verified the removal actually happened).
    pub fn note_displaced(&mut self, addr: u64) {
        let ckpt_begun = self.ckpt_begun;
        let Some(file) = self.file_containing_mut(addr, 1) else {
            debug_assert!(false, "displacement at {addr} outside every manifested file");
            return;
        };
        debug_assert!(file.live_count > 0, "displacement in a file with no counted slots");
        file.live_count = file.live_count.saturating_sub(1);
        file.unref_stamp = ckpt_begun;
    }

    /// The per-file counters, address-ordered (S15's trigger input; the
    /// walk driver serializes these — `.ick` tag 0x04).
    #[must_use]
    pub fn files(&self) -> &[FileLiveSet] {
        &self.files
    }

    /// Pending (not-yet-filed) dead spans — observability for the L5
    /// disclosure; drains to zero whenever the flush catches up.
    #[must_use]
    pub fn pending_spans(&self) -> usize {
        self.pending.len()
    }

    /// Total pending dead bytes (the un-filed share of the space's
    /// per-life dead-byte aggregate).
    #[must_use]
    pub fn pending_dead_bytes(&self) -> u64 {
        self.pending.values().sum()
    }

    // ---- copy-forward retirement (M4-S15, ADR-0059) ----

    /// Records that checkpoint `ckpt_id` has begun its walk — the value
    /// subsequent slot-removals stamp their file with (D3). Ids never
    /// decrease (a retried publication may reuse the same id — its walk
    /// began later, but the equal stamp still blocks retirement at that
    /// id, which errs toward retention).
    pub fn note_ckpt_begun(&mut self, ckpt_id: u64) {
        debug_assert!(ckpt_id >= self.ckpt_begun, "checkpoint ids are monotone");
        self.ckpt_begun = ckpt_id;
    }

    /// Finalizes a file copy-forward has fully scanned (the ADR-0058
    /// obligation, discharged per ADR-0059 D4): every record was
    /// verified dead or relocated out, so `dead == len` is a theorem —
    /// assigned for recovered files (healing the disclosed lower bound),
    /// asserted for byte-exact ones.
    ///
    /// # Panics
    /// Panics when the file is unknown or provably not empty — the scan
    /// completing with a live slot still naming the file is a programmer
    /// error, not an operating condition.
    pub fn finalize_scanned(&mut self, id: u32) {
        let file = self.files.iter_mut().find(|f| f.id == id).expect("finalize of unknown file");
        assert!(!file.retiring, "finalize of a retiring file");
        if file.byte_exact {
            assert_eq!(file.dead_bytes, file.data_len, "scan complete but bytes still live");
        } else {
            assert_eq!(file.live_count, 0, "scan complete but slots still name the file");
            file.dead_bytes = file.data_len;
            file.byte_exact = true;
        }
    }

    /// Marks retirable files as retiring for the manifest under
    /// construction (D3 phase 1): `is_dead` ∧ stamped before checkpoint
    /// `ckpt_id` began ∧ accepted by `eligible` (the caller's sealed-file
    /// check — the live set cannot see the flush catalog). Returns how
    /// many files newly entered the retiring state.
    pub fn retire_scan(&mut self, ckpt_id: u64, eligible: impl Fn(u32) -> bool) -> u32 {
        debug_assert!(ckpt_id >= self.ckpt_begun, "retire against a stale checkpoint id");
        let mut marked = 0u32;
        for file in &mut self.files {
            if !file.retiring && file.is_dead() && file.unref_stamp < ckpt_id && eligible(file.id) {
                file.retiring = true;
                marked += 1;
            }
        }
        marked
    }

    /// Whether a file is provisionally excluded from the manifest under
    /// construction (the `tier_manifest` skip predicate).
    #[must_use]
    pub fn is_retiring(&self, id: u32) -> bool {
        self.files.iter().any(|f| f.id == id && f.retiring)
    }

    /// The swap landed (D3 phase 3): retiring files leave the table —
    /// they are no longer trigger input, and the returned ids drive the
    /// catalog detach + the pin-gated unlink at the plane layer.
    pub fn commit_retirement(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self.files.iter().filter(|f| f.retiring).map(|f| f.id).collect();
        self.files.retain(|f| !f.retiring);
        ids
    }

    /// The swap failed as a counted abort (D3 phase 4): the old unit —
    /// which still names every retiring file — stays authoritative, so
    /// the marks clear and the files are re-offered at the next
    /// publication. Nothing was unlinked, nothing else mutated.
    pub fn abort_retirement(&mut self) {
        for file in &mut self.files {
            file.retiring = false;
        }
    }

    /// The cold floor (ADR-0059 D5): the lowest surviving file's base —
    /// no live address exists below it. Derived, never stored: the file
    /// table is the single source of truth, and the §3.2 watermark
    /// vocabulary stays four entries. Equals the filed-coverage end when
    /// no files survive (an empty tier's floor is wherever filing would
    /// resume).
    #[must_use]
    pub fn cold_floor(&self) -> u64 {
        self.files.first().map_or(self.coverage_end, |f| f.base)
    }

    /// The latest begun checkpoint id (the D3 stamp source; the D9
    /// origin map's drop threshold at a landed swap).
    #[must_use]
    pub(crate) fn ckpt_begun(&self) -> u64 {
        self.ckpt_begun
    }

    // ---- internals ----

    fn file_containing_mut(&mut self, addr: u64, len: u64) -> Option<&mut FileLiveSet> {
        let idx = self.files.partition_point(|f| f.base + f.data_len <= addr);
        let file = self.files.get_mut(idx)?;
        // Whole-record containment: records never span files (§3.2 tier
        // format v1), so a straddling range is a caller bug.
        (addr >= file.base && addr + len <= file.base + file.data_len).then_some(file)
    }

    /// Inserts a dead span, merging with an adjacent predecessor and/or
    /// successor (spans never overlap — a record dies exactly once).
    fn insert_pending(&mut self, addr: u64, len: u64) {
        debug_assert!(
            self.pending.range(..=addr).next_back().is_none_or(|(&p, &pl)| p + pl <= addr),
            "overlapping dead spans (double death)"
        );
        debug_assert!(
            self.pending.range(addr..addr + len).next().is_none(),
            "overlapping dead spans (double death)"
        );
        let mut start = addr;
        let mut end = addr + len;
        if let Some((&p, &pl)) = self.pending.range(..addr).next_back()
            && p + pl == addr
        {
            self.pending.remove(&p);
            start = p;
        }
        if let Some(&sl) = self.pending.get(&end) {
            self.pending.remove(&end);
            end += sl;
        }
        self.pending.insert(start, end - start);
    }

    /// Removes and sums pending span bytes strictly below `end`,
    /// splitting a span that crosses it.
    fn drain_pending_below(&mut self, end: u64) -> u64 {
        let mut drained = 0u64;
        while let Some((&start, &len)) = self.pending.first_key_value() {
            if start >= end {
                break;
            }
            self.pending.remove(&start);
            if start + len <= end {
                drained += len;
            } else {
                drained += end - start;
                self.pending.insert(end, start + len - end);
                break;
            }
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use inf_foundation::LogicalAddr;
    use inf_log::tier::SealReason;

    use super::*;

    fn meta(id: u32, base: u64, len: u64) -> TierFileMeta {
        TierFileMeta {
            id,
            base: LogicalAddr::from_raw(base).expect("48-bit"),
            data_len: len,
            reason: SealReason::Capacity,
            path: PathBuf::from(format!("tier-{id:06}.itier")),
        }
    }

    /// Filing charges chunks to files, drains pending spans (splitting
    /// at chunk ends), and keeps `dead ≤ len` per file.
    #[test]
    fn filing_drains_pending_spans_with_splits() {
        let mut live = LiveSet::new(0);
        // Deaths before anything filed: three spans, two adjacent (merge).
        live.note_dead(100, 50);
        live.note_dead(150, 30); // merges with [100,150)
        live.note_dead(400, 100);
        assert_eq!(live.pending_spans(), 2);
        assert_eq!(live.pending_dead_bytes(), 180);
        // File 0 covers [0, 450): drains the merged span whole and
        // splits [400, 500) at the chunk end.
        live.note_filed(0, 0, 0, 450);
        assert_eq!(live.files()[0].dead_bytes, 80 + 50);
        assert_eq!(live.pending_dead_bytes(), 50, "the split remainder stays pending");
        // The next chunk (same file) drains the remainder.
        live.note_filed(0, 0, 450, 150);
        assert_eq!(live.files()[0].dead_bytes, 180);
        assert_eq!(live.pending_spans(), 0);
        assert_eq!(live.files()[0].data_len, 600);
        assert!(live.files()[0].byte_exact);
        // A death at a filed address charges the file directly.
        live.note_dead(200, 100);
        assert_eq!(live.files()[0].dead_bytes, 280);
        // A second file after a gap (hole): base = its first chunk.
        live.note_filed(1, 1000, 1000, 200);
        assert_eq!(live.files().len(), 2);
        assert_eq!(live.files()[1].base, 1000);
        // Fully-dead this-life file: the deletion predicate flips.
        live.note_dead(1000, 200);
        assert!(live.files()[1].is_dead());
        assert!(!live.files()[0].is_dead());
        assert_eq!(live.files()[0].live_bytes(), Some(600 - 280));
    }

    /// Recovered files count slots: refs in, displacements/deaths out;
    /// bytes restore only under the D5 length-match rule.
    #[test]
    fn recovered_files_count_slots_and_clamp_restores() {
        let mut live = LiveSet::new(10_000);
        live.seed_recovered(&[meta(0, 0, 4096), meta(1, 4096, 2048)], 10_000, 7);
        // Restore: file 0 matches its manifested length; file 1 was
        // serialized longer than manifested (filed-ahead) → floor.
        live.restore_entry(&LiveSetFileEntry {
            file_id: 0,
            data_len: 4096,
            dead_bytes: 1000,
            byte_exact: true,
        });
        live.restore_entry(&LiveSetFileEntry {
            file_id: 1,
            data_len: 3000,
            dead_bytes: 2900,
            byte_exact: true,
        });
        // An entry for a file the manifest never named: ignored.
        live.restore_entry(&LiveSetFileEntry {
            file_id: 9,
            data_len: 64,
            dead_bytes: 0,
            byte_exact: true,
        });
        assert_eq!(live.files()[0].dead_bytes, 1000);
        assert!(!live.files()[0].byte_exact, "partially-dead restores inexact");
        assert_eq!(live.files()[1].dead_bytes, 0, "length mismatch restores the floor");
        // Counts: three refs into file 0, one into file 1.
        live.note_ref(0);
        live.note_ref(512);
        live.note_ref(2048);
        live.note_ref(4096);
        assert_eq!(live.files()[0].live_count, 3);
        assert_eq!(live.files()[1].live_count, 1);
        // Replay displaces one; a post-recovery cold death takes another.
        live.note_displaced(512);
        live.note_dead(0, 96);
        assert_eq!(live.files()[0].live_count, 1);
        assert_eq!(live.files()[0].dead_bytes, 1096);
        assert!(!live.files()[0].is_dead(), "one counted slot keeps it alive");
        live.note_dead(2048, 128);
        assert!(live.files()[0].is_dead(), "count-dead, even though bytes are a lower bound");
        assert!(!live.files()[1].is_dead());
    }

    /// The ADR-0059 D3 retirement lifecycle: stamps gate on the covering
    /// checkpoint, aborts roll back, commits remove — and the cold floor
    /// advances only when the lowest file actually leaves.
    #[test]
    fn retirement_gates_on_the_covering_checkpoint_stamp() {
        let mut live = LiveSet::new(0);
        live.note_filed(0, 0, 0, 400);
        live.note_filed(1, 400, 400, 300);
        assert_eq!(live.cold_floor(), 0);
        // Checkpoint 1 begins; file 0 empties during its walk — the
        // stamp equals the in-flight id, so checkpoint 1 cannot cover it.
        live.note_ckpt_begun(1);
        live.note_dead(0, 400);
        assert!(live.files()[0].is_dead());
        assert_eq!(live.retire_scan(1, |_| true), 0, "emptied mid-walk: not coverable by 1");
        // Checkpoint 2 began after the last removal: retirable now.
        live.note_ckpt_begun(2);
        assert_eq!(live.retire_scan(2, |_| true), 1);
        assert!(live.is_retiring(0));
        // The swap fails: the mark rolls back, nothing else changed.
        live.abort_retirement();
        assert!(!live.is_retiring(0));
        assert_eq!(live.files().len(), 2);
        // Re-offered and landed at the next publication.
        assert_eq!(live.retire_scan(2, |_| true), 1);
        assert_eq!(live.commit_retirement(), vec![0]);
        assert_eq!(live.files().len(), 1);
        assert_eq!(live.cold_floor(), 400, "the floor advanced over the retired file");
        // The sealed-eligibility hook is honored (an unsealed file —
        // e.g. the active one — never retires even when dead).
        live.note_dead(400, 300);
        live.note_ckpt_begun(3);
        assert_eq!(live.retire_scan(3, |_| false), 0);
    }

    /// The ADR-0059 D4 finalization: a fully-scanned recovered file's
    /// lower bound heals to exact; a byte-exact file asserts instead.
    #[test]
    fn finalize_scanned_heals_the_recovered_lower_bound() {
        let mut live = LiveSet::new(10_000);
        live.seed_recovered(&[meta(0, 0, 4096)], 10_000, 3);
        live.note_ref(0);
        live.note_ref(1024);
        // Copy-forward relocates both live records (deaths route here),
        // leaving the byte counters a lower bound (0 restored + 2 exact).
        live.note_dead(0, 512);
        live.note_dead(1024, 512);
        assert!(live.files()[0].is_dead());
        assert!(!live.files()[0].byte_exact);
        assert_eq!(live.files()[0].dead_bytes, 1024, "a lower bound before finalization");
        live.finalize_scanned(0);
        let file = &live.files()[0];
        assert!(file.byte_exact);
        assert_eq!(file.dead_bytes, file.data_len);
        assert_eq!(file.live_bytes(), Some(0));
    }

    /// The D5 fully-dead rule: `byte_exact` survives recovery only when
    /// the serialized counters prove the file had nothing live to lose.
    #[test]
    fn fully_dead_files_restore_byte_exact() {
        let mut live = LiveSet::new(1 << 20);
        live.seed_recovered(&[meta(3, 0, 512)], 1 << 20, 7);
        live.restore_entry(&LiveSetFileEntry {
            file_id: 3,
            data_len: 512,
            dead_bytes: 512,
            byte_exact: true,
        });
        let file = &live.files()[0];
        assert!(file.byte_exact && file.is_dead());
        assert_eq!(file.live_bytes(), Some(0));
    }
}
