//! `TieredTable` flush and seal: slice sealing, the flush rounds and
//! barrier, claimable confirmation, and the checkpoint walk.

use super::*;

impl TieredTable {
    /// One seal step (ADR-0053 D2/D3): advances the ro-boundary toward
    /// `tail − mutable_target`, landing only on a recorded record-start
    /// mark. The advancement bound is `slice_bytes` plus the mark
    /// granularity (marks are per commit page, so a step may overrun the
    /// slice by under two pages); when the first mark past the boundary
    /// lies beyond the window entirely (a record larger than the slice),
    /// that one mark is taken anyway — minimum one record of progress, or
    /// the pipeline deadlocks behind a single 16 MiB record. Returns the
    /// bytes sealed.
    pub fn seal_slice(&mut self) -> u64 {
        let tail = self.space.tail().to_raw();
        let ro = self.space.ro_boundary().to_raw();
        let target_addr = tail - self.demote.mutable_target_bytes().min(tail - ro);
        if target_addr <= ro {
            return 0;
        }
        let slice_limit = ro.saturating_add(self.demote.slice_bytes).min(target_addr);
        let mut chosen: Option<u64> = None;
        while let Some(&mark) = self.seal_marks.front() {
            let m = mark.to_raw();
            if m <= ro {
                self.seal_marks.pop_front(); // stale: the boundary passed it
                continue;
            }
            if m > target_addr {
                break;
            }
            // Within the slice window always; past it only as the
            // minimum-progress first mark.
            if m <= slice_limit || chosen.is_none() {
                chosen = Some(m);
                self.seal_marks.pop_front();
                if m > slice_limit {
                    break;
                }
                continue;
            }
            break;
        }
        let Some(to) = chosen else { return 0 };
        self.space.advance_ro_boundary(LogicalAddr::from_raw(to).expect("marks are 48-bit"));
        let sealed = to - ro;
        self.space.note_demote_slice(sealed);
        sealed
    }

    /// One release step (ADR-0053 D3): advances the head toward the
    /// release ceiling — the flushed watermark, clamped to the walk
    /// watermark while a hybrid checkpoint walk is pinned (M4-S12,
    /// ADR-0057 D2) — at most `slice_bytes` per call, decommitting
    /// whole pages beneath it (RSS returns to the OS, ADR-0052 D3). The
    /// §3.1 order (`head ≤ flushed`) is structural in `advance_head`.
    /// Returns the bytes released.
    pub fn release_slice(&mut self) -> u64 {
        let head = self.space.head().to_raw();
        let flushed = self.space.release_ceiling();
        let step = (flushed - head).min(self.demote.slice_bytes);
        if step == 0 {
            return 0;
        }
        self.space.advance_head(LogicalAddr::from_raw(head + step).expect("below flushed"));
        step
    }

    /// One flush slice (M4-S11, ADR-0056 D3 — the leg between S07's seal
    /// and release steps): pulls record-aligned chunks from
    /// `[append-cursor, ro_boundary)` up to the pipeline's slice budget,
    /// appends them through `flush` (which owns rotation, early-seal,
    /// and gap seals), fdatasyncs once, and advances `flushed` to the
    /// largest appended chunk end the barrier makes claimable (full,
    /// final frames only until a file seals — the ADR-0056 D5 rewrite
    /// rule). Ring-top gaps confirm immediately after the preceding
    /// file's seal barrier (ADR-0052 D2). Drivers loop on
    /// [`FlushSliceOutcome::appended_bytes`], never on `demote_due`.
    ///
    /// # Errors
    /// [`TierFlushError`] — on `Fsync` the watermark is frozen exactly
    /// where the last good barrier left it (§8.4: the caller stops).
    /// A `StorageFull`-class `Io` failure additionally latches the
    /// disk-admission device leg (M4-S21, ADR-0063 D4): foreground
    /// refuses `DISKFULL` while MAINTAIN retries the unflushed backlog
    /// — the next successful barrier clears the latch, so recovery
    /// after space frees is automatic.
    pub fn flush_slice<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<FlushSliceOutcome, TierFlushError> {
        let res = self.flush_slice_inner(flush);
        if let Err(e) = &res
            && e.is_storage_full()
        {
            self.disk_admit.device_full = true;
        }
        res
    }

    fn flush_slice_inner<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<FlushSliceOutcome, TierFlushError> {
        let budget = flush.slice_bytes();
        let flushed0 = self.space.flushed().to_raw();
        let sealed0 = flush.sealed().len();
        let mut outcome = FlushSliceOutcome::default();
        // Resume where the pipeline's append cursor stands — bytes may be
        // staged ahead of `flushed` (partial-frame holdback), and they
        // must never be re-appended.
        let mut cursor = flush.append_cursor().unwrap_or(flushed0);
        assert!(cursor >= flushed0, "flush cursor behind the watermark");
        let mut spent = 0u64;
        let mut wrote = false;
        while spent < budget {
            let at = LogicalAddr::from_raw(cursor).expect("watermarks stay 48-bit");
            let Some(chunk) = self.space.next_flush_chunk(at, budget - spent) else { break };
            match chunk {
                FlushChunk::Gap { at, len } => {
                    debug_assert_eq!(at.to_raw(), cursor, "gap starts at the cursor");
                    flush.seal_for_gap()?;
                    let to = at.to_raw() + len;
                    self.space.advance_flushed(
                        LogicalAddr::from_raw(to).expect("watermarks stay 48-bit"),
                    );
                    while self.flush_ends.front().is_some_and(|&e| e <= to) {
                        self.flush_ends.pop_front();
                    }
                    cursor = to;
                    outcome.gaps_crossed += 1;
                }
                FlushChunk::Records { addr, len } => {
                    let n = usize::try_from(len).expect("chunk fits usize");
                    flush.append_range(addr, self.space.bytes(addr, n))?;
                    // File the chunk (M4-S14): it landed in exactly one
                    // file — `append_range` seals *before* an overflowing
                    // range, so the post-call active file is the one that
                    // took it — and pending dead spans it covers drain
                    // into that file's counters at this moment.
                    let (id, base, _, _, _) =
                        flush.active().expect("append_range leaves a file active");
                    self.live.note_filed(id, base.to_raw(), addr.to_raw(), len);
                    cursor = addr.to_raw() + len;
                    if self.flush_ends.len() == FLUSH_ENDS_CAP {
                        self.flush_ends.pop_front(); // dominated candidate
                    }
                    self.flush_ends.push_back(cursor);
                    spent += len;
                    wrote = true;
                }
            }
        }
        outcome.appended_bytes = spent;
        // The device latch's recovery probe (ADR-0063 D4). `wrote` alone
        // is not enough: a sync-time failure leaves every appended byte
        // *staged* (the writer retains its batch and its cursor — the
        // append-atomicity rewind covers the mid-range case), so the
        // retry round pulls no new chunk while the latch refuses the
        // only source of new appends. The barrier therefore also runs
        // whenever the latch is set and staged bytes await durability —
        // it rewrites the retained frames at their own offsets and
        // recovery cannot starve on its own refusal.
        let pending_retry = self.disk_admit.device_full
            && flush.active().is_some_and(|(_, _, data, durable, _)| data > durable);
        if wrote || pending_retry {
            flush.sync()?;
            // A successful barrier is the probe's answer: the writes and
            // the fdatasync both landed.
            self.disk_admit.device_full = false;
        }
        self.confirm_to_claimable(flush);
        outcome.files_sealed =
            u32::try_from(flush.sealed().len() - sealed0).expect("seals per slice fit u32");
        outcome.confirmed_bytes = self.space.flushed().to_raw() - flushed0;
        if outcome.confirmed_bytes > 0 || outcome.appended_bytes > 0 {
            self.space.note_flush_slice(outcome.confirmed_bytes);
        }
        self.charge_flush_device(flush);
        Ok(outcome)
    }

    /// Drains the flush completely (shutdown, tests, DST quiesce): runs
    /// slices until nothing appends, then seals the active file so the
    /// partial tail frame becomes claimable, and confirms `flushed` up
    /// to the sealed end (= `ro_boundary` when the space had no pending
    /// gap at the very end).
    ///
    /// # Errors
    /// As [`flush_slice`](Self::flush_slice).
    pub fn flush_drain<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<(), TierFlushError> {
        loop {
            let outcome = self.flush_slice(flush)?;
            if outcome.appended_bytes == 0 && outcome.gaps_crossed == 0 {
                break;
            }
        }
        flush.seal_shutdown()?;
        if let Some(limit) = flush.confirmable_end() {
            let before = self.space.flushed().to_raw();
            if limit > before {
                self.space
                    .advance_flushed(LogicalAddr::from_raw(limit).expect("watermarks stay 48-bit"));
                self.space.note_flush_slice(limit - before);
            }
            let now_flushed = self.space.flushed().to_raw();
            while self.flush_ends.front().is_some_and(|&e| e <= now_flushed) {
                self.flush_ends.pop_front();
            }
        }
        self.charge_flush_device(flush);
        Ok(())
    }

    /// Barrier seal under backpressure (M4-S11, ADR-0056 D8): call when
    /// a tail-allocation stall is outstanding and the last
    /// [`flush_slice`](Self::flush_slice) appended nothing — the
    /// partial-frame holdback is all that separates `flushed` from the
    /// stalled writer's wake target, and sealing makes it claimable.
    /// Confirms `flushed` to the sealed end.
    ///
    /// # Errors
    /// As [`flush_slice`](Self::flush_slice).
    pub fn flush_barrier<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<(), TierFlushError> {
        // The stall seal writes the footer + barrier — the same device
        // surface as a slice, so the M4-S21 latch rides it identically.
        // (With no active writer the seal is a no-op: no probe ran, so
        // the latch must not clear on that Ok.)
        let probed = flush.active().is_some();
        match flush.seal_stall() {
            Ok(()) if probed => self.disk_admit.device_full = false,
            Ok(()) => {}
            Err(e) => {
                if e.is_storage_full() {
                    self.disk_admit.device_full = true;
                }
                return Err(e);
            }
        }
        if let Some(limit) = flush.confirmable_end() {
            let before = self.space.flushed().to_raw();
            if limit > before {
                self.space
                    .advance_flushed(LogicalAddr::from_raw(limit).expect("watermarks stay 48-bit"));
                self.space.note_flush_slice(limit - before);
            }
            let now_flushed = self.space.flushed().to_raw();
            while self.flush_ends.front().is_some_and(|&e| e <= now_flushed) {
                self.flush_ends.pop_front();
            }
        }
        self.charge_flush_device(flush);
        Ok(())
    }

    /// Advances `flushed` to the largest staged chunk end the pipeline's
    /// claimable bound covers, pruning confirmed candidates (the shared
    /// confirm of the seam slice and the reactor round — ADR-0056 D5's
    /// claim rule in one place).
    fn confirm_to_claimable<F: SegmentFs>(&mut self, flush: &TierFlush<F>) {
        let Some(limit) = flush.confirmable_end() else { return };
        let confirm = self.flush_ends.iter().copied().filter(|&e| e <= limit).max().unwrap_or(0);
        if confirm > self.space.flushed().to_raw() {
            self.space
                .advance_flushed(LogicalAddr::from_raw(confirm).expect("watermarks stay 48-bit"));
        }
        let now_flushed = self.space.flushed().to_raw();
        while self.flush_ends.front().is_some_and(|&e| e <= now_flushed) {
            self.flush_ends.pop_front();
        }
    }

    // ---- reactor-drive flush rounds (M4.5-S31, ADR-0084) ----

    /// Stages one reactor-drive flush round — the queued twin of
    /// [`flush_slice`](Self::flush_slice): the same chunk pull, the same
    /// rotation/early-seal decisions, **no device I/O and no watermark
    /// movement**. Device intents land on the pipeline's round for the
    /// plane to ride (`IoOp::LogWrite`/`Fdatasync`); every durability
    /// fact defers to a round effect that
    /// [`complete_flush_round`](Self::complete_flush_round) applies at
    /// the round's last barrier completion. Rounds end early at a
    /// ring-top gap (effect-ordering simplicity; gaps are once per ring
    /// wrap). Returns the staged record bytes.
    ///
    /// # Errors
    /// File-creation metadata I/O only (the once-per-`TIER-FILE-BYTES`
    /// open — ADR-0084 D2); a `StorageFull`-class refusal latches the
    /// device leg exactly like the seam drive (ADR-0063 D4).
    pub fn stage_flush_round<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<u64, TierFlushError> {
        let res = self.stage_flush_round_inner(flush);
        if let Err(e) = &res {
            if e.is_storage_full() {
                self.disk_admit.device_full = true;
            }
            // A failed stage leaves either no round or a round holding
            // only what it staged before the failure — a sealed file's
            // ops, each on a handle the pipeline owns (the writer's, a
            // pending seal's, a directory hold's): creation touches the
            // round only after every step that can fail (F-L01-02). The
            // plane submits such a round as any other; the seal's own
            // barrier covers it.
            debug_assert!(flush.round_handles_owned(), "a staged op outlived its handle");
        }
        res
    }

    fn stage_flush_round_inner<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> Result<u64, TierFlushError> {
        debug_assert!(!flush.round_active(), "staging over an in-flight round");
        let budget = flush.slice_bytes();
        let flushed0 = self.space.flushed().to_raw();
        let mut cursor = flush.append_cursor().unwrap_or(flushed0);
        assert!(cursor >= flushed0, "flush cursor behind the watermark");
        let mut spent = 0u64;
        let mut wrote = false;
        while spent < budget {
            let at = LogicalAddr::from_raw(cursor).expect("watermarks stay 48-bit");
            let Some(chunk) = self.space.next_flush_chunk(at, budget - spent) else { break };
            match chunk {
                FlushChunk::Gap { at, len } => {
                    debug_assert_eq!(at.to_raw(), cursor, "gap starts at the cursor");
                    flush.seal_for_gap_queued(at.to_raw() + len);
                    break;
                }
                FlushChunk::Records { addr, len } => {
                    let n = usize::try_from(len).expect("chunk fits usize");
                    flush.append_range_queued(addr, self.space.bytes(addr, n))?;
                    // File the chunk (M4-S14) — stage-time, exactly like
                    // the seam drive (durability is not the counters'
                    // input; the recovery appliers reconcile).
                    let (id, base, _, _, _) =
                        flush.active().expect("append_range leaves a file active");
                    self.live.note_filed(id, base.to_raw(), addr.to_raw(), len);
                    cursor = addr.to_raw() + len;
                    if self.flush_ends.len() == FLUSH_ENDS_CAP {
                        self.flush_ends.pop_front(); // dominated candidate
                    }
                    self.flush_ends.push_back(cursor);
                    spent += len;
                    wrote = true;
                }
            }
        }
        if wrote {
            flush.sync_queued();
        }
        Ok(spent)
    }

    /// Applies a completed round's deferred effects **in stage order**
    /// (durable-watermark advances, seal catalog commits, gap crossings
    /// — ADR-0084 D2), then runs the shared confirm. The caller (plane)
    /// guarantees every op of the round reached a terminal successful
    /// completion — a failed barrier never gets here (§8.4 fail-stop).
    pub fn complete_flush_round<F: SegmentFs>(
        &mut self,
        flush: &mut TierFlush<F>,
    ) -> FlushSliceOutcome {
        let flushed0 = self.space.flushed().to_raw();
        let sealed0 = flush.sealed().len();
        let probed = flush.round_barrier_count() > 0;
        let mut outcome = FlushSliceOutcome::default();
        for effect in flush.finish_round() {
            match effect {
                inf_log::RoundEffect::DurableTo { data_len } => flush.confirm_durable_to(data_len),
                inf_log::RoundEffect::SealCommit => flush.commit_oldest_seal(),
                inf_log::RoundEffect::GapCross { to } => {
                    self.space.advance_flushed(
                        LogicalAddr::from_raw(to).expect("watermarks stay 48-bit"),
                    );
                    while self.flush_ends.front().is_some_and(|&e| e <= to) {
                        self.flush_ends.pop_front();
                    }
                    outcome.gaps_crossed += 1;
                }
            }
        }
        debug_assert_eq!(flush.pending_seal_count(), 0, "every staged seal committed");
        // A successful barrier set is the ADR-0063 D4 probe's answer.
        if probed {
            self.disk_admit.device_full = false;
        }
        self.confirm_to_claimable(flush);
        outcome.files_sealed =
            u32::try_from(flush.sealed().len() - sealed0).expect("seals per round fit u32");
        outcome.confirmed_bytes = self.space.flushed().to_raw() - flushed0;
        self.space.note_flush_slice(outcome.confirmed_bytes);
        self.charge_flush_device(flush);
        outcome
    }

    /// Latches the ADR-0063 D4 device leg from a reactor-drive write
    /// completion that reported `ENOSPC` (the plane's completion handler
    /// is the only caller; the seam drive latches inside the slice).
    pub fn note_flush_device_full(&mut self) {
        self.disk_admit.device_full = true;
    }

    // ---- hybrid checkpoint walk (M4-S12, ADR-0057 D1/D2) ----

    /// Latches this walk's watermark `W` (= the current flushed
    /// watermark) and pins page release beneath it — every entry below
    /// `W` refs, every entry at or above it images, and the pin makes
    /// the image half structurally RAM-resident for the whole walk.
    /// One walk in flight per cell, ever. `ckpt_id` is the id the
    /// publication this walk feeds will manifest — subsequent
    /// slot-removals stamp their file with it, which is what lets a
    /// later checkpoint prove it emitted no reference into an emptied
    /// file (M4-S15, ADR-0059 D3).
    pub fn begin_ckpt_walk(&mut self, ckpt_id: u64) -> LogicalAddr {
        self.live.note_ckpt_begun(ckpt_id);
        self.walk_ckpt_id = Some(ckpt_id);
        self.space.begin_walk()
    }

    /// The checkpoint id the latest walk began under (`None` before the
    /// first walk of this life). Outlives the walk: a table whose value
    /// trails a published checkpoint that walked it was walked under a
    /// leaked pin — the F-L03-04 witness (ADR-0057 A3).
    #[must_use]
    pub fn walk_ckpt_id(&self) -> Option<u64> {
        self.walk_ckpt_id
    }

    /// Releases the walk pin; the held-back release debt drains in the
    /// next MAINTAIN slices.
    pub fn end_ckpt_walk(&mut self) {
        self.space.end_walk();
    }

    /// One bounded slice of the hybrid walk (ADR-0057 D1): resize-stable
    /// home-group enumeration **from the index sidecar** — the cold
    /// majority emits `{hash, addr}` with zero record touches; entries
    /// at or above the walk watermark emit full images from RAM
    /// (structurally: `addr ≥ W ≥ head` while pinned — the walker never
    /// resolves a cold address, so `cold_resolves` is flat across a
    /// walk, asserted by the checkpoint-under-load storm). Inherits the
    /// SCAN guarantee: every entry present for the whole walk is emitted
    /// at least once; mid-walk mutations may re-emit (the WAL tail from
    /// begin re-covers them — replay is exact, D4). Returns the next
    /// cursor (0 = done).
    ///
    /// # Panics
    /// Panics when no walk is pinned ([`begin_ckpt_walk`]
    /// (Self::begin_ckpt_walk) first).
    pub fn ckpt_walk_slice(
        &self,
        cursor: u64,
        count: usize,
        emit_ref: impl FnMut(u64, LogicalAddr),
        mut emit_image: impl FnMut(RecordParts<'_>),
    ) -> u64 {
        let mut at = crate::index::WalkCursor { group: cursor, chain: None };
        let done = self.ckpt_walk_slice_bounded(&mut at, count, emit_ref, |parts| {
            emit_image(parts);
            true
        });
        if done { 0 } else { at.group }
    }

    /// [`ckpt_walk_slice`](Self::ckpt_walk_slice) with an image emission
    /// the caller may refuse (ADR-0117 D2): `emit_image` returns `false`
    /// to stop *before* that entry, and the next call resumes at exactly
    /// it through the cursor's in-chain position. References are never
    /// refused (24 B each — a chunk of them sits far under any bound).
    /// Returns `true` once the index is fully walked.
    pub fn ckpt_walk_slice_bounded(
        &self,
        cursor: &mut crate::index::WalkCursor,
        count: usize,
        mut emit_ref: impl FnMut(u64, LogicalAddr),
        mut emit_image: impl FnMut(RecordParts<'_>) -> bool,
    ) -> bool {
        let w = self.space.walk_watermark().expect("walk not begun").to_raw();
        let mask = self.index.group_count() as u64 - 1;
        let mut group = cursor.group & mask;
        let mut resume = cursor.chain.take();
        let mut emitted = 0usize;
        loop {
            let space = &self.space;
            let stopped = self.index.scan_home_group_ext_from(
                group as usize,
                resume.take(),
                |addr, hash, _pos| {
                    // ADR-0093 A12 (batch 23): a ticket's winner is imaged
                    // even below the flushed watermark — sealing and
                    // flushing pass it (D3), only release is pinned, so it
                    // is RAM-resident here; a ref would restore it as a
                    // second cold slot of its key with no RAM sibling for
                    // the rebuild to pair (a stale read and a phantom key
                    // after recovery).
                    if addr.to_raw() < w && !self.is_shadow_winner(addr) {
                        emit_ref(hash, addr);
                    } else {
                        let head = space.bytes(addr, crate::record::HEADER_LEN);
                        let full_len = crate::record::encoded_len_from_header(head);
                        let parts = RecordParts::of(RecordView::new(space.bytes(addr, full_len)));
                        if !emit_image(parts) {
                            return false;
                        }
                    }
                    emitted += 1;
                    true
                },
            );
            if let Some(pos) = stopped {
                *cursor = crate::index::WalkCursor { group, chain: Some(pos) };
                return false;
            }
            group = crate::store::next_rev_cursor(group, mask);
            if group == 0 {
                *cursor = crate::index::WalkCursor::START;
                return true;
            }
            if emitted >= count {
                *cursor = crate::index::WalkCursor { group, chain: None };
                return false;
            }
        }
    }

    /// One bounded SCAN slice over the index (M4-S26): resize-stable
    /// home-group enumeration emitting `{hash, addr}` — the plane
    /// resolves keys (RAM slots directly; cold slots fetch + decode,
    /// which is what a beyond-RAM enumeration inherently costs).
    /// Inherits the SCAN guarantee: entries present the whole scan emit
    /// at least once; mid-scan mutations may duplicate. Returns the
    /// next cursor (0 = done).
    pub fn scan_slots(
        &self,
        cursor: u64,
        count: usize,
        mut emit: impl FnMut(u64, LogicalAddr),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        loop {
            self.index.scan_home_group_ext(cursor as usize, |addr, hash| {
                // ADR-0093 A3: a ticket's cold slot is emitted like any
                // cold slot — it is either the key's old record (the key
                // is named twice within one scan, legal by the SCAN
                // contract) or a collision key the winner's read has not
                // yet told apart (which must be named). Hiding it hid a
                // key; counted for the campaign.
                if self.is_shadow_cold(addr) {
                    self.note_shadow_scan_twin();
                }
                emit(hash, addr);
                emitted += 1;
            });
            cursor = crate::store::next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }
}
