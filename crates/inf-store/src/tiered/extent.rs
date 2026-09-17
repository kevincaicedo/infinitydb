//! `TieredTable` blob extents (M4.5-S39): configuration, insert/update/
//! append, reference counts, reclaim bookkeeping, and checkpoint images.

use super::*;

impl TieredTable {
    // ---- blob extents (M4-S17, ADR-0061) ----

    /// Blob routing bounds — the caller (plane) reads the threshold to
    /// decide the write path; the store refuses misrouted values typed.
    #[inline]
    pub fn blob_config(&self) -> crate::extents::BlobConfig {
        self.blob
    }

    /// Replaces the blob bounds (S19's `INF.NS` keys; tests). The
    /// threshold is clamped to this ring's inline bound (ADR-0102 D3):
    /// whatever the registered spec says, the plane's routing decision
    /// can never admit an inline record above `ring / 2`.
    ///
    /// # Panics
    /// Panics on nonsense bounds ([`BlobConfig::validate`]
    /// (crate::extents::BlobConfig::validate)).
    pub fn set_blob_config(&mut self, blob: crate::extents::BlobConfig) {
        blob.validate();
        self.blob = Self::clamp_blob_config(self.space.ring_bytes(), blob);
    }

    /// The largest record the ring admits inline: half the ring
    /// (ADR-0052 D1's `R ≥ 2 × RECORD_INLINE_MAX`, ADR-0102 D3).
    /// [`insert`](Self::insert)/[`update`](Self::update) refuse a longer
    /// record typed; `AddressSpace::alloc`'s assert is the paired
    /// internal invariant behind it.
    #[inline]
    #[must_use]
    pub fn inline_record_max(&self) -> usize {
        (self.space.ring_bytes() / 2) as usize
    }

    pub(super) fn clamp_blob_config(
        ring_bytes: u64,
        blob: crate::extents::BlobConfig,
    ) -> crate::extents::BlobConfig {
        let cap = crate::ns::TierSpec::blob_threshold_max(ring_bytes).max(1);
        crate::extents::BlobConfig { threshold_bytes: blob.threshold_bytes.min(cap), ..blob }
    }

    /// Allocates the next extent id (allocate-once — a failed extent
    /// write quarantines it; the orphan sweep reclaims the file).
    pub fn allocate_extent_id(&mut self) -> u64 {
        self.extents.allocate_id()
    }

    /// Inserts an extent-referencing record for an absent key (ADR-0061
    /// D2/D3): the value lives out of line, the record carries the
    /// 24-byte reference, and the `SealedExtent` token proves the
    /// extent's fdatasync already ran — an unfsynced extent is
    /// unrepresentable here by construction.
    ///
    /// # Errors
    /// Space refusals and bounds violations, typed.
    pub fn insert_extent(
        &mut self,
        key: &[u8],
        hash: u64,
        sealed: &SealedExtent,
    ) -> Result<LogicalAddr, OpError> {
        debug_assert_eq!(hash, self.hash_key(key));
        debug_assert!(
            self.index
                .find(hash, |addr| {
                    addr.to_raw() >= self.space.head().to_raw() && self.record(addr).key == key
                })
                .is_none(),
            "insert of a RAM-verified present key"
        );
        if self.index.needs_grow() {
            self.index.grow(|_, ext| ext);
        }
        let ext = ExtentRef { extent_id: sealed.extent_id().0, offset: 0, len: sealed.data_len() };
        let addr = self.append_extent(key, ext, 0)?;
        self.index.insert(hash, addr);
        Ok(addr)
    }

    /// Overwrites the record at `old` with an extent reference — the
    /// [`overwrite`](Self::overwrite) shape; the displaced record's
    /// reference (extent or inline) releases through `note_death`.
    ///
    /// # Errors
    /// Space refusals and bounds violations, typed.
    pub fn update_extent(
        &mut self,
        key: &[u8],
        hash: u64,
        sealed: &SealedExtent,
        old: LogicalAddr,
        old_len: usize,
        old_version: u32,
    ) -> Result<LogicalAddr, OpError> {
        let ext = ExtentRef { extent_id: sealed.extent_id().0, offset: 0, len: sealed.data_len() };
        let new_addr = self.append_extent(key, ext, old_version.wrapping_add(1))?;
        self.index.replace(hash, old, new_addr);
        self.shadow_note_moved(hash, old, new_addr);
        self.note_death(old, old_len as u64);
        Ok(new_addr)
    }

    /// Places one extent-referencing record at the tail and registers
    /// its reference. Charges split per ADR-0061 D8: the **record leg**
    /// (key + 24-byte reference — what flows through WAL and flush)
    /// into `user_bytes`; the **blob leg** (the value length) into
    /// `blob_user_bytes`. The extent's device bytes arrive via
    /// [`note_blob_bytes`](Self::note_blob_bytes).
    fn append_extent(
        &mut self,
        key: &[u8],
        ext: ExtentRef,
        version: u32,
    ) -> Result<LogicalAddr, OpError> {
        if key.len() > crate::record::MAX_KEY_LEN || ext.len > self.blob.max_bytes {
            return Err(OpError::TooLarge);
        }
        debug_assert!(ext.len > 0, "an extent reference names at least one byte");
        let value = ext.encode();
        let spec = RecordSpec {
            key,
            value: &value,
            version,
            expire_at_ms: None,
            kind: RecordKind::StringExtent,
        };
        let len = spec.encoded_len();
        if len > self.inline_record_max() {
            return Err(OpError::TooLarge); // ADR-0102 D3 — unreachable with a legal key
        }
        // M4-S21 disk admission (ADR-0063 D1/D2): the reference record
        // plus the extent's device bytes — the blob is already on disk
        // (`SealedExtent`), so this is the budget catching up with it;
        // the wiring-time gate consults [`disk_full`](Self::disk_full)
        // *before* `ExtentWriter::create` so a full device is not
        // probed with a doomed file per attempt.
        let cost = len as u64 + inf_log::blob::extent_device_bytes(ext.len);
        self.disk_admit_check(cost)?;
        let addr = self.space.alloc(len).ok_or(OpError::OutOfMemory)?;
        self.shadow_note_alloc();
        self.disk_admit_debit(cost);
        self.note_seal_mark(addr);
        spec.write(self.space.bytes_mut(addr, len));
        self.live_bytes += len as u64;
        self.write.user_bytes += (key.len() + crate::record::EXTENT_REF_LEN) as u64;
        self.write.blob_user_bytes += ext.len;
        self.extents.register(addr.to_raw(), ext.extent_id, ext.len);
        Ok(addr)
    }

    /// Charges extent device bytes (the `blob_bytes` leg, ADR-0061 D8) —
    /// the plane reads `ExtentWriter::device_bytes()` after `finish` and
    /// folds it here, the `note_compaction_bytes` seam shape.
    #[inline]
    pub fn note_blob_bytes(&mut self, bytes: u64) {
        self.write.blob_bytes += bytes;
    }

    /// Counts one blob read-modify-write rewrite (ADR-0061 D7 — the
    /// reserved doc-path cost seam; extents are immutable, so RMW is
    /// read → new extent → new reference → old reference dies).
    #[inline]
    pub fn note_blob_rmw(&mut self) {
        self.extents.note_rmw();
    }

    /// Counts one typed cold-read failure served to a client (review of
    /// 2026-08-30, C2′): the plane's resolve funnel and SCAN's key
    /// fetch report here — the `note_blob_bytes` seam shape.
    #[inline]
    pub fn note_cold_read_error(&mut self) {
        self.space.note_cold_read_error();
    }

    /// Blob-extent observables (`INFO tiering`; the §3.3 zero-assert
    /// lists — memory-mode namespaces have no table, hence all-zero).
    #[must_use]
    pub fn extent_stats(&self) -> crate::extents::ExtentStats {
        self.extents.stats()
    }

    /// Live refcount of one extent (tests + the DST refcount oracle).
    #[must_use]
    pub fn extent_refcount(&self, extent_id: u64) -> u64 {
        self.extents.refcount(extent_id)
    }

    /// The reference at `addr`, if that record stores out of line —
    /// `(extent id, value len)`.
    #[must_use]
    pub fn extent_reference_at(&self, addr: LogicalAddr) -> Option<(u64, u64)> {
        self.extents.reference_at(addr.to_raw())
    }

    /// Every live reference-map entry, ascending by address —
    /// `(record addr, extent id, value len)`. Control-plane observability
    /// (bounded by `disk_budget / threshold` entries — the L5 term); the
    /// DST refcount oracle and tests read it.
    pub fn extent_references(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.extents.entries_below(u64::MAX)
    }

    /// The staging epoch of the last successful [`stage_wal`]
    /// (Self::stage_wal) — the durability coordinate the plane maps its
    /// commit watermark onto for [`extent_reclaim_work`]
    /// (Self::extent_reclaim_work).
    #[must_use]
    pub fn wal_epoch(&self) -> u64 {
        self.wal_epoch
    }

    /// Disposal candidates whose killing record is durable (`stamp ≤
    /// durable_epoch`), at most `max` (one MAINTAIN slice's budget —
    /// ADR-0061 D5), each typed with its [`ReclaimOrigin`]
    /// (crate::extents::ReclaimOrigin) (ADR-0096 D1). The plane composes
    /// the in-flight read pin check, dispatches the disposal on the
    /// origin (death → unlink; boot orphan → probe + quarantine rename;
    /// second verdict → unlink the twin), and answers each candidate
    /// with [`extent_reclaim_done`](Self::extent_reclaim_done),
    /// [`extent_reclaim_quarantined`](Self::extent_reclaim_quarantined),
    /// or [`extent_reclaim_deferred`](Self::extent_reclaim_deferred).
    pub fn extent_reclaim_work(
        &mut self,
        durable_epoch: u64,
        max: usize,
    ) -> Vec<crate::extents::ReclaimCandidate> {
        self.extents.reclaim_work(durable_epoch, max)
    }

    /// Confirms one unlink completed (`statfs` sees the space).
    pub fn extent_reclaim_done(&mut self, extent_id: u64) {
        self.extents.reclaim_done(extent_id);
    }

    /// Confirms one boot-orphan quarantine (ADR-0096 D2 — renamed, not
    /// unlinked; the bytes wait for a later boot's second verdict).
    pub fn extent_reclaim_quarantined(&mut self, extent_id: u64) {
        self.extents.reclaim_quarantined(extent_id);
    }

    /// Returns one candidate after a non-fatal disposal failure
    /// (`blob_unlink_fail` — counted, re-offered, boot-sweep-re-driven).
    pub fn extent_reclaim_deferred(&mut self, extent_id: u64) {
        self.extents.reclaim_deferred(extent_id);
    }

    /// Seeds the boot orphan sweep with the extent directory listing
    /// (names only — ADR-0061 D6; disposal per ADR-0096). Call after
    /// replay completes; parked replay deaths stamp durable here (they
    /// were replayed *from* the log) and every listed-but-unreferenced
    /// extent becomes a typed reclaim candidate drained by ordinary
    /// MAINTAIN slices, never at boot. `quarantined` is the
    /// `.quarantine` listing; the returned ids are quarantined extents
    /// the replayed map references — the caller renames them back
    /// before serving (ADR-0096 D3).
    #[must_use = "revived quarantined extents must be renamed back before serving"]
    pub fn extent_sweep_seed(&mut self, listed: &[u64], quarantined: &[u64]) -> Vec<u64> {
        self.extents.sweep_seed(listed, quarantined)
    }

    /// Applies one checkpoint tag-9 image / tail `StringExtentRef`
    /// record (ADR-0057 D4 rule 2 over the extent kind): blind
    /// key-verified RAM upsert; a cold candidate is deliberately
    /// ignored (no boot-path cold read).
    ///
    /// # Errors
    /// Space refusals from the store (recovery fail-stop at the caller).
    pub fn apply_extent_image(
        &mut self,
        key: &[u8],
        hash: u64,
        ext: ExtentRef,
    ) -> Result<LogicalAddr, OpError> {
        if let TieredLookup::Ram(addr) = self.lookup(key, hash, &[]) {
            let parts = self.record(addr);
            let (len, version) = (parts.encoded_len, parts.version);
            let new_addr = self.append_extent(key, ext, version.wrapping_add(1))?;
            self.index.replace(hash, addr, new_addr);
            self.note_death(addr, len as u64);
            return Ok(new_addr);
        }
        if self.index.needs_grow() {
            self.index.grow(|_, ext| ext);
        }
        let addr = self.append_extent(key, ext, 0)?;
        self.index.insert(hash, addr);
        // ADR-0093 D5: shadow tickets are rebuilt from the finished index
        // by `rebuild_shadow_tickets` at recovery-complete, not tracked
        // through replay (a winner's address moves under the ticket as
        // images overwrite it — the incremental path orphaned slots).
        Ok(addr)
    }

    /// Restores one checkpoint 0x05 blob-reference entry (ADR-0061 D6):
    /// the cold record at `addr` references `extent_id`. Pairs with the
    /// 0x03 ref that restored the slot; the map entry is what lets the
    /// replayed tail's displacements decrement the right extent.
    ///
    /// # Panics
    /// Debug-panics when `addr` is not below this life's origin — 0x05
    /// entries name cold (address-preserved) records only.
    pub fn restore_extent_entry(&mut self, addr: u64, extent_id: u64, len: u64) {
        debug_assert!(
            LogicalAddr::from_raw(addr).is_some_and(|a| a < self.space.life_origin()),
            "0x05 entries name pre-life addresses"
        );
        self.extents.register(addr, extent_id, len);
    }

    /// The checkpoint 0x05 emission set: reference-map entries strictly
    /// below the pinned walk watermark, ascending — cold records only
    /// (RAM-resident extent records ride tag-9 images; both would be a
    /// double count at restore).
    ///
    /// # Panics
    /// Panics when no walk is pinned ([`begin_ckpt_walk`]
    /// (Self::begin_ckpt_walk) first).
    pub fn extent_ckpt_entries(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        let w = self.space.walk_watermark().expect("walk not begun").to_raw();
        self.extents.entries_below(w)
    }

    /// [`extent_ckpt_entries`](Self::extent_ckpt_entries) resumed at the
    /// address cursor `resume` — the pass-3 slice form (review of
    /// 2026-08-30, C4): stable under mid-walk removals below the cursor,
    /// which the ordinal `.skip` resume it replaces was not.
    ///
    /// # Panics
    /// Panics when no walk is pinned ([`begin_ckpt_walk`]
    /// (Self::begin_ckpt_walk) first).
    pub fn extent_ckpt_entries_from(
        &self,
        resume: u64,
    ) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        let w = self.space.walk_watermark().expect("walk not begun").to_raw();
        self.extents.entries_from(resume, w)
    }

    /// Takes the relocation origins of the record at `addr` (M4-S15,
    /// ADR-0059 D9): the `(address, stamp)` pairs un-superseded
    /// checkpoints may still reference this record by. A displacing
    /// mutation or delete stages one `ColdDisplace` per returned
    /// address **before** its ordinary marker, so whichever recovery
    /// unit survives, replay kills exactly the ref that unit holds.
    /// Empty — allocation-free — in the common (never-relocated) case.
    /// Take only when the markers' staging is committed (the entry is
    /// consumed; a taken-but-unstaged origin would reopen the D9
    /// hazard — command wiring's recorded obligation).
    pub fn take_displacement_origins(&mut self, hash: u64, addr: LogicalAddr) -> Vec<(u64, u64)> {
        if self.reloc_origins.is_empty() {
            return Vec::new();
        }
        self.reloc_origins.remove(&(hash, addr.to_raw())).unwrap_or_default()
    }

    /// How many origins [`take_displacement_origins`](Self::take_displacement_origins)
    /// would return for `(hash, addr)` — the marker count a displacing
    /// command budgets before it stages (ADR-0093 A11); ≤ [`RELOC_ORIGIN_CAP`].
    #[must_use]
    pub fn displacement_origins_len(&self, hash: u64, addr: LogicalAddr) -> usize {
        if self.reloc_origins.is_empty() {
            return 0;
        }
        self.reloc_origins.get(&(hash, addr.to_raw())).map_or(0, Vec::len)
    }

    /// Dead-byte attribution at the repoint/delete moment, keyed by the
    /// dead record's own address (M4-S06 hook; M4-S14 routing, ADR-0058
    /// D2). Pre-life addresses charge their recovered tier file only —
    /// the space's `live + dead = allocated` identity and the table's
    /// `live_bytes` are per-life (ADR-0057 D6: `live_bytes` boots
    /// covering images + tail only), so a post-recovery cold overwrite
    /// or delete must not touch either. S17's blob refcounts ride this
    /// same site.
    pub(super) fn note_death(&mut self, addr: LogicalAddr, len: u64) {
        if addr >= self.space.life_origin() {
            self.space.note_dead_bytes(addr, len);
            self.live_bytes -= len;
        }
        self.live.note_dead(addr.to_raw(), len);
        // The blob refcount decrement (M4-S17, ADR-0061 D4) sits on the
        // unconditional side: a post-recovery cold death skips the
        // per-life aggregates above but must still release its extent
        // reference — the map, not the record bytes, is the identity.
        self.extents.note_death(addr.to_raw());
    }
}
