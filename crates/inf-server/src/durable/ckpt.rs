//! The durable cell's checkpoint side: the `.ick` slice state machine
//! (ADR-0072/ADR-0079 section classes, the tier and sidecar walks), the
//! manifest swap, and their completion hooks.

use super::*;

// ---- tiered checkpoint walk (M4-S26; ADR-0057 D1/D3, ADR-0059 D3) ----

/// One tiered walk step's verdict.
enum TierStep {
    /// Staged entries (or budget/section bounds hit) — continue.
    Progress,
    /// A pending section of another class must seal before this pass
    /// stages — the caller queues the block now (class purity).
    SealFirst,
    /// The next image would breach the section bound (ADR-0117 D1): the
    /// pending section seals now and the walk resumes at that image.
    SealForBound,
    /// This namespace's walk is complete (retirement scan included).
    NsDone,
}

/// Entries pulled per `ckpt_walk_slice` call (home-group granular).
const TIER_WALK_CHUNK: usize = 256;

/// One bounded step of a tiered namespace's hybrid walk. Passes: 0 =
/// address refs (cold majority, zero record touches), 1 = RAM images,
/// 2 = per-file live-set entries, 3 = cold blob references, 4 = end
/// walk + retirement scan (ADR-0059 D3 phase 1 — between walk end and
/// the manifest). Section classes never mix inside a pass, so seals
/// happen only at pass boundaries.
#[allow(clippy::too_many_arguments)] // the fill loop's split fields
fn tier_walk_step<F: SegmentFs>(
    stream: &mut inf_log::IckStream,
    table: &mut inf_store::TieredTable,
    tier_ns: &mut crate::tier_cell::TierNs<F>,
    ns: NsId,
    ckpt_id: u64,
    cursor: &mut u64,
    resume: &mut Option<inf_store::ChainPos>,
    tier_pass: &mut u8,
    slice_cap: u32,
    emitted: &mut u32,
) -> TierStep {
    // Pass boundaries: seal any pending section before a class change.
    if *cursor == 0 && resume.is_none() && stream.can_seal() {
        return TierStep::SealFirst;
    }
    match *tier_pass {
        0 => {
            if *cursor == 0 {
                // The pin belongs to this checkpoint id (ADR-0057 A3,
                // F-L03-04): a pin still held here leaked from an aborted
                // walk the reconciliation never saw (the `EINVAL`
                // downgrade retries without a backoff), so it is released
                // — with its stamped-but-uncovered retirement marks — and
                // the walk re-latches `W` and the stamp under its own id.
                if table.space().walk_watermark().is_some() {
                    table.end_ckpt_walk();
                    table.abort_retirement();
                }
                table.begin_ckpt_walk(ckpt_id);
            }
            let w = table.space().walk_watermark().expect("begun at cursor 0").to_raw();
            let next = table.ckpt_walk_slice(
                *cursor,
                TIER_WALK_CHUNK,
                |hash, addr| {
                    stream.stage_addr_ref(ns.0, w, hash, addr.to_raw());
                    *emitted = emitted.saturating_add(24);
                },
                |_image| {},
            );
            advance_pass(cursor, tier_pass, next);
            TierStep::Progress
        }
        1 => {
            let mut at = inf_store::WalkCursor { group: *cursor, chain: resume.take() };
            let done = table.ckpt_walk_slice_bounded(
                &mut at,
                TIER_WALK_CHUNK,
                |_hash, _addr| {},
                |parts| {
                    let rec = match parts.type_tag {
                        inf_store::TypeTag::String => {
                            RecordView::StringPostImage { ns, key: parts.key, value: parts.value }
                        }
                        inf_store::TypeTag::StringExtent => {
                            let ext = inf_store::ExtentRef::decode(parts.value);
                            RecordView::StringExtentRef {
                                ns,
                                key: parts.key,
                                extent_id: ext.extent_id,
                                offset: ext.offset,
                                len: ext.len,
                            }
                        }
                        // Documents are not command-reachable on tiered
                        // namespaces in M4 — a doc image here is a bug.
                        other => {
                            debug_assert!(false, "tiered walk met a {other:?} record");
                            return true;
                        }
                    };
                    // ADR-0117 D1/D2: seal before an image the section
                    // cannot take; the walk resumes at this entry.
                    if !stream.fits(rec.encoded_len()) {
                        return false;
                    }
                    *emitted = emitted.saturating_add(rec.encoded_len() as u32);
                    stream.stage_record(&rec);
                    true
                },
            );
            if done {
                *cursor = 0;
                *tier_pass = 2;
                return TierStep::Progress;
            }
            *cursor = at.group;
            *resume = at.chain;
            if at.chain.is_some() { TierStep::SealForBound } else { TierStep::Progress }
        }
        2 => {
            // Resume by file id, not ordinal (the C4 rule applied
            // defensively): `files()` ascends by id and today only
            // shrinks at `commit_retirement` — which the swap sequences
            // strictly after the walk — but that ordering is a coupling,
            // not an invariant, and an id-keyed resume costs nothing.
            let files: Vec<_> = table
                .live_set()
                .files()
                .iter()
                .filter(|f| u64::from(f.id) >= *cursor)
                .take(TIER_WALK_CHUNK)
                .map(|f| (f.id, f.data_len, f.dead_bytes, f.byte_exact))
                .collect();
            if files.is_empty() {
                *cursor = 0;
                *tier_pass = 3;
                return TierStep::Progress;
            }
            for (file_id, data_len, dead_bytes, byte_exact) in &files {
                stream.stage_live_set(ns.0, *file_id, *data_len, *dead_bytes, *byte_exact);
                *emitted = emitted.saturating_add(24);
                *cursor = u64::from(*file_id) + 1;
                if stream.section_full() || *emitted >= slice_cap {
                    break;
                }
            }
            TierStep::Progress
        }
        3 => {
            // The 0x05 resume is an *address* (review of 2026-08-30, C4 /
            // F-L03-01, F-L14-02): the reference map mutates between
            // slices (foreground DEL/overwrite, compaction), and the
            // ordinal `.skip` this replaces stepped over one live entry
            // per below-cursor removal — the checkpoint then published
            // silently short and the next boot's sweep unlinked a live
            // extent. `range(cursor..W)` is stable under removals on
            // either side of the cursor.
            let entries: Vec<(u64, u64, u64)> =
                table.extent_ckpt_entries_from(*cursor).take(TIER_WALK_CHUNK).collect();
            if entries.is_empty() {
                *cursor = 0;
                *tier_pass = 4;
                return TierStep::Progress;
            }
            for (addr, extent_id, len) in &entries {
                stream.stage_blob_ref(ns.0, *addr, *extent_id, *len);
                *emitted = emitted.saturating_add(24);
                *cursor = *addr + 1;
                if stream.section_full() || *emitted >= slice_cap {
                    break;
                }
            }
            TierStep::Progress
        }
        _ => {
            // End the walk (release debt drains next MAINTAIN), then the
            // retirement scan stamps fully-dead candidates against this
            // checkpoint (ADR-0059 D3 phase 1).
            table.end_ckpt_walk();
            let _ = table.retire_scan(ckpt_id, &tier_ns.flush);
            TierStep::NsDone
        }
    }
}

/// Walk-slice cursor bookkeeping: 0 = this pass finished.
fn advance_pass(cursor: &mut u64, tier_pass: &mut u8, next: u64) {
    if next == 0 {
        *cursor = 0;
        *tier_pass += 1;
    } else {
        *cursor = next;
    }
}

// ---- index-sidecar walk (M4.5-S06; ADR-0078 D1/D2) ----

/// Pairs pulled per sidecar emission call — bounds the section-target
/// overshoot to one chunk (the staging buffer absorbs it).
const SIDECAR_CHUNK_ENTRIES: u32 = 256;

/// One bounded step of the sidecar phase. The plan captures once at
/// entry (converged, non-degraded indexes on the walk's namespaces —
/// ADR-0078 D1); each index streams through its re-seek cursor and
/// closes with a FINAL marker. Eligibility is re-checked every step:
/// a drop, rebuild, or degrade mid-emission abandons the stream — no
/// FINAL, and the loader discards it as incomplete. Returns `true`
/// when the pending section must seal now (index boundary, FINAL, or
/// a foreign class left from the record walk).
#[allow(clippy::too_many_arguments)] // the fill loop's split fields
fn sidecar_walk_step(
    stream: &mut inf_log::IckStream,
    ks: &Keyspace,
    ns_ids: &[u32],
    plan: &mut Option<Vec<IdxSidecarMeta>>,
    at: &mut usize,
    cursor: &mut inf_store::OrderedCursor,
    emitted_pairs: &mut u64,
    done: &mut bool,
    slice_cap: u32,
    emitted: &mut u32,
) -> bool {
    let plan = plan.get_or_insert_with(|| {
        let mut entries = Vec::new();
        for &ns_raw in ns_ids {
            for (id, generation, fixed8, _entries) in ks.idx_sidecar_candidates(NsId(ns_raw)) {
                entries.push(IdxSidecarMeta {
                    ns: ns_raw,
                    index_id: id.0,
                    generation,
                    key_encoding_version: inf_store::INDEX_KEY_ENCODING_VERSION,
                    fixed8,
                });
            }
        }
        entries
    });
    loop {
        let Some(meta) = plan.get(*at).copied() else {
            *done = true;
            // A pending tail section seals through the caller's
            // walk-complete condition.
            return false;
        };
        // Class purity: a pending foreign section (the record walk's
        // tail, or a previous index's) seals before this index stages.
        let key = (meta.ns, meta.index_id, meta.generation);
        if stream.has_pending_section() && stream.pending_idx_stream() != Some(key) {
            return true;
        }
        if !ks.idx_sidecar_eligible(NsId(meta.ns), IndexId(meta.index_id), meta.generation) {
            *at += 1;
            *cursor = inf_store::OrderedCursor::from_start();
            *emitted_pairs = 0;
            if stream.has_pending_section() {
                return true; // flush the abandoned partial section
            }
            continue;
        }
        let pulled = ks.idx_sidecar_emit(
            NsId(meta.ns),
            IndexId(meta.index_id),
            cursor,
            SIDECAR_CHUNK_ENTRIES,
            |key_bytes, entry_ref| {
                stream.stage_idx_entry(&meta, *emitted_pairs, key_bytes, entry_ref);
                *emitted_pairs += 1;
                let entry_bytes = if meta.fixed8 { 16 } else { 2 + key_bytes.len() + 8 };
                *emitted = emitted.saturating_add(entry_bytes as u32);
            },
        );
        if pulled < SIDECAR_CHUNK_ENTRIES {
            // Exhausted at this instant (fuzzy — tail catch-up owns any
            // later drift): close the stream and seal the FINAL section.
            stream.stage_idx_final(&meta, *emitted_pairs);
            *at += 1;
            *cursor = inf_store::OrderedCursor::from_start();
            *emitted_pairs = 0;
            return true;
        }
        if stream.section_full() || *emitted >= slice_cap {
            return false; // a full section seals below; budget ends the slice
        }
    }
}

impl<F: SegmentFs> DurableCell<F> {
    /// One fuzzy-checkpoint slice (M2-S10, ADR-0016 D5): runs under the
    /// `GroupClass::Checkpoint` deficit — `budget_units` convert at
    /// 1 unit ≈ 1 KiB streamed, hard-capped by `CkptConfig::slice_bytes`.
    /// Returns the units to charge. All data writes ride the driver; the
    /// only blocking ops are file create/rename/dir-fsync metadata (the
    /// rotor-prealloc class).
    pub fn ckpt_slice(
        &mut self,
        ks: &mut Keyspace,
        tier: Option<&mut crate::tier_cell::TierCell<F>>,
        cx: &mut LoopCx<'_>,
        budget_units: u32,
        anchor: WallAnchor,
    ) -> u32 {
        let mut tier = tier;
        if self.failed {
            return 0;
        }
        // Idle: trigger check → stage the begin marker (one record; its
        // frame seals at this iteration's LOG step). A pending MANIFEST
        // blocks the next trigger: one recovery-unit transition in flight,
        // ever (M2-S11 — the pending swap resolves within one everysec
        // window, so this never starves the trigger).
        if matches!(self.ckpt.phase, CkptPhase::Idle) {
            // ADR-0088 D4: the accumulator is on-disk frame bytes (header,
            // trailer, v3 padding — what the device saw); the record cap
            // rides beside it; before the first checkpoint the interval is
            // the floor alone (the published file's size is the only
            // measurement — a content estimate chased a growing dataset
            // and never fired, the node_e2e threshold test's finding).
            let total = self.commit.stats().frame_bytes_queued;
            if self.ckpt.tick_backoff() {
                return 0;
            }
            if self.manifest.idle() && self.ckpt.should_begin(total, self.records_appended) {
                let id = self.ckpt.pending_id();
                let effect = MutationEffect::CkptBegin { ckpt_id: id };
                if self.staging.would_fit(effect.encoded_len()) {
                    let at = self.staging.stage(&effect).expect("admission pre-checked");
                    self.commit.note_staged(FsyncClass::Everysec);
                    self.records_appended += 1;
                    self.last_seq += 1;
                    self.ckpt.consume_request();
                    // The epoch this checkpoint satisfies (M2-S20) — one
                    // transition in flight, so a single value suffices.
                    self.ckpt.epoch_in_flight = self.ckpt.req_epoch;
                    // Trigger re-base is begin-anchored (`bytes_at_last`
                    // docs): everything staged after this instant is tail
                    // the new checkpoint does not cover.
                    self.ckpt.bytes_at_begin = total;
                    self.ckpt.records_at_begin = self.records_appended;
                    self.ckpt.phase = CkptPhase::AwaitBeginLsn { id, at };
                }
            }
            return 0;
        }
        // Begin LSN resolves at LOG (`on_frame_sealed`); nothing to do yet.
        if matches!(self.ckpt.phase, CkptPhase::AwaitBeginLsn { .. }) {
            return 0;
        }
        // Begun: create the .ick.new file + queue the header write.
        if let CkptPhase::Begun { id, begin_lsn } = self.ckpt.phase {
            let ns_ids = ks.durable_ns_ids();
            // v2 iff tiered namespaces or index declarations on durable
            // namespaces exist (ADR-0073 D2 / ADR-0078 D7) — cells with
            // neither keep writing v1 byte-identically.
            let v2 =
                ns_ids.iter().any(|&raw| ks.is_tiered(NsId(raw))) || ks.idx_declared_on_durable();
            if let Err(err) = self.ckpt.open_stream(id, begin_lsn, ns_ids, v2, cx.now) {
                self.ckpt.abort("create", &err.to_string());
                return 1;
            }
            let CkptPhase::Stream(st) = &mut self.ckpt.phase else { unreachable!("just opened") };
            let lease = st.in_flight.as_ref().expect("header staged by open_stream");
            // The header is one block, charged unconditionally (the file
            // is already created); the class deficit absorbs it and the
            // first section offer pays for it (ADR-0088 D2).
            self.budget.charge(IoClass::Checkpoint, u64::from(lease.len()), 1);
            st.write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: st.fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(&st.stream, lease),
                token: ckpt_token(TokenClass::CkptWrite, st.write_seq),
                barrier: WriteBarrier::None,
            });
            return 1;
        }
        let cfg = self.ckpt.cfg;
        // Publish once the completion fdatasync landed (rename+dir-fsync).
        // A published `.ick` hands off to the MANIFEST driver: the swap
        // waits for the watermark to cover begin (M2-S11 publication
        // guard), then runs in `manifest_slice`.
        if matches!(&self.ckpt.phase, CkptPhase::Stream(st) if st.sync_done) {
            let CkptPhase::Stream(st) = std::mem::replace(&mut self.ckpt.phase, CkptPhase::Idle)
            else {
                unreachable!("matched above")
            };
            let (id, begin_lsn) = (st.id, st.begin_lsn);
            let unix_now = anchor.unix_from_internal(cx.now);
            match self.ckpt.publish(st, unix_now) {
                Ok(()) => {
                    // F-L03-04 witness: every tiered table this walk
                    // covered began under `id` (pass 0 owns the pin).
                    let behind = ks
                        .tiered_namespaces()
                        .filter(|(_, table)| table.walk_ckpt_id().is_some_and(|w| w < id))
                        .count() as u64;
                    self.ckpt.note_walks_behind(behind);
                    self.manifest.note_published_ick(id, begin_lsn, self.ckpt.epoch_in_flight);
                }
                Err(err) => self.ckpt.abort("publish", &err.to_string()),
            }
            return 1;
        }
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else { return 0 };
        // One deref, split fields (borrow splitting doesn't cross `Box`).
        let crate::ckpt::Streaming {
            id,
            stream,
            fd,
            ns_ids,
            ns_idx,
            cursor,
            resume,
            bound_splits: walk_bound_splits,
            tier_pass,
            walk_done,
            v2,
            sidecar_plan,
            sidecar_at,
            sidecar_cursor,
            sidecar_emitted,
            sidecar_done,
            footer_staged,
            sync_issued,
            in_flight,
            write_seq,
            opened_at,
            streamed_bytes,
            ..
        } = &mut **st;
        // One section in flight max: wait for its completion.
        if in_flight.is_some() {
            return 0;
        }
        // Pacing (ADR-0017): the walk streams at most `stream_bytes_per_sec
        // × elapsed` — an unpaced walk dirties pages at memcpy speed and
        // the kernel's writeback throttling then stalls the log write's
        // CQE path (the S12-measured foreground cliff). Injected time, so
        // DST compresses it (L7). Sidecar emission is walk output like
        // any other (M4.5-S06) and rides the same meter.
        // ADR-0088 D5: with a device model present the budget governs the
        // rate (the pace's reason — writeback throttling — is gone with
        // the direct `.ick`); the pace is the unprobed fallback only.
        if self.budget.model_absent()
            && !(*walk_done && *sidecar_done)
            && cfg.stream_bytes_per_sec > 0
        {
            let elapsed_ms = cx.now.saturating_sub(*opened_at).as_millis();
            let allowed = (u64::from(cfg.stream_bytes_per_sec) * elapsed_ms.max(1)) / 1000;
            if *streamed_bytes >= allowed {
                return 0;
            }
        }
        // Footer written+released → the completion barrier.
        if *footer_staged {
            if !*sync_issued {
                *sync_issued = true;
                *write_seq += 1;
                self.budget.charge(IoClass::Checkpoint, 0, 1);
                cx.push(IoOp::Fdatasync {
                    fd: *fd,
                    token: ckpt_token(TokenClass::CkptSync, *write_seq),
                });
            }
            return 0;
        }
        // Fill: pull post-images under the byte budget (the walker — the
        // resize-stable SCAN cursor, ADR-0016 D2).
        let mut emitted: u32 = 0;
        let mut force_seal = false;
        let mut bound_splits = 0u64;
        let slice_cap = cfg.slice_bytes.min(budget_units.saturating_mul(1024)).max(1);
        if !*walk_done {
            while emitted < slice_cap && !*walk_done && !stream.section_full() && !force_seal {
                let Some(&ns_raw) = ns_ids.get(*ns_idx) else {
                    *walk_done = true;
                    break;
                };
                let ns = NsId(ns_raw);
                // Tiered namespaces walk the ADR-0057 hybrid (M4-S26):
                // refs, images, live-set + blob sections, retirement scan.
                if ks.is_tiered(ns) {
                    let step = match (ks.tiered_store_mut(ns), tier.as_deref_mut()) {
                        (Some(table), Some(tc)) => match tc.ns_mut(ns) {
                            Some(t) => tier_walk_step(
                                stream,
                                table,
                                t,
                                ns,
                                *id,
                                cursor,
                                resume,
                                tier_pass,
                                slice_cap,
                                &mut emitted,
                            ),
                            None => TierStep::NsDone,
                        },
                        // Dropped mid-walk (or a plane without tier state
                        // — unreachable when tiered namespaces exist).
                        _ => TierStep::NsDone,
                    };
                    match step {
                        TierStep::Progress => {}
                        TierStep::SealFirst => force_seal = true,
                        TierStep::SealForBound => {
                            force_seal = true;
                            bound_splits += 1;
                        }
                        TierStep::NsDone => {
                            *ns_idx += 1;
                            *cursor = 0;
                            *resume = None;
                            *tier_pass = 0;
                            if *ns_idx == ns_ids.len() {
                                *walk_done = true;
                            }
                        }
                    }
                    continue;
                }
                match ks.ns_store_mut(ns) {
                    // Dropped mid-walk: its records replay as skips anyway.
                    None => {
                        *ns_idx += 1;
                        *cursor = 0;
                        *resume = None;
                    }
                    Some(store) => {
                        let bytes = &mut emitted;
                        let mut at = inf_store::WalkCursor { group: *cursor, chain: resume.take() };
                        let done = store.scan_checkpoint_images_bounded(
                            &mut at,
                            SCAN_CHUNK_ENTRIES,
                            cx.now,
                            |key, image, expire_ms| {
                                let rec = match image {
                                    CheckpointImage::String(value) => {
                                        RecordView::StringPostImage { ns, key, value }
                                    }
                                    #[cfg(feature = "doc")]
                                    CheckpointImage::JsonDoc { lineage, version, idoc } => {
                                        RecordView::DocFull { ns, key, lineage, version, idoc }
                                    }
                                };
                                let expiry = expire_ms.map(|ms| {
                                    let at_unix_ms =
                                        anchor.unix_from_internal(Nanos::from_millis(ms));
                                    RecordView::ExpireAt { ns, at_unix_ms, key }
                                });
                                // ADR-0117 D1: an image and its expiry ride
                                // one section; a pair the pending section
                                // cannot take seals it first (the walk
                                // resumes at this entry — D2).
                                let len = rec.encoded_len() + expiry.map_or(0, |e| e.encoded_len());
                                if !stream.fits(len) {
                                    return false;
                                }
                                *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                stream.stage_record(&rec);
                                if let Some(rec) = expiry {
                                    *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                    stream.stage_record(&rec);
                                }
                                true
                            },
                        );
                        if done {
                            *ns_idx += 1;
                            *cursor = 0;
                        } else {
                            *cursor = at.group;
                            *resume = at.chain;
                            if at.chain.is_some() {
                                force_seal = true;
                                bound_splits += 1;
                            }
                        }
                        if *ns_idx == ns_ids.len() {
                            *walk_done = true;
                        }
                    }
                }
            }
        } else if !*sidecar_done {
            // Sidecar phase (M4.5-S06, ADR-0078 D1): derived data last —
            // converged trees stream after every record image. A v1
            // stream cannot represent them (an index converging mid-walk
            // waits for the next checkpoint, which opens v2).
            if *v2 {
                force_seal = sidecar_walk_step(
                    stream,
                    ks,
                    ns_ids,
                    sidecar_plan,
                    sidecar_at,
                    sidecar_cursor,
                    sidecar_emitted,
                    sidecar_done,
                    slice_cap,
                    &mut emitted,
                );
            } else {
                *sidecar_done = true;
            }
        }
        *walk_bound_splits += bound_splits;
        // Queue at most one block per slice: a full (or final partial)
        // section, a class-boundary seal (M4-S26 tiered passes / S06
        // index boundaries), or — once everything drained — the footer.
        // ADR-0088 D2/D3: each block is offered to the budget at its
        // padded length *before* it seals — `Deferred` leaves the section
        // staged (the walk simply does not advance past it this tick)
        // and the offer repeats next slice.
        if stream.section_full()
            || (force_seal && stream.can_seal())
            || (*walk_done && *sidecar_done && stream.can_seal())
        {
            let block = stream.pending_block_len() as u64;
            if self.budget.admit(IoClass::Checkpoint, block, 1) != Admission::Granted {
                *streamed_bytes += u64::from(emitted);
                return emitted.div_ceil(1024).max(1);
            }
            let lease = stream.seal_section();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                barrier: WriteBarrier::None,
            });
            *in_flight = Some(lease);
        } else if *walk_done && *sidecar_done {
            let block = stream.footer_block_len() as u64;
            if self.budget.admit(IoClass::Checkpoint, block, 1) != Admission::Granted {
                *streamed_bytes += u64::from(emitted);
                return emitted.div_ceil(1024).max(1);
            }
            let lease = stream.finish();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                barrier: WriteBarrier::None,
            });
            *in_flight = Some(lease);
            *footer_staged = true;
        }
        *streamed_bytes += u64::from(emitted);
        emitted.div_ceil(1024).max(1)
    }

    /// REAP: a checkpoint section/header/footer write completed — release
    /// the lease (the `StableBytes` custody point).
    pub fn on_ckpt_written(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptWrite completion with no checkpoint streaming")
        };
        let lease = st.in_flight.take().expect("CkptWrite with no in-flight lease");
        st.stream.release(lease);
    }

    /// REAP: the checkpoint's completion fdatasync landed — publish next
    /// MAINTAIN slice.
    pub fn on_ckpt_synced(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptSync completion with no checkpoint streaming")
        };
        st.sync_done = true;
    }

    /// REAP: a checkpoint op failed — abort the checkpoint, never the
    /// process (the old checkpoint and the whole log stay valid; the
    /// milestone's "checkpoints abort cleanly" rule). `EINVAL` under
    /// `Direct` is the filesystem refusing the direct write the boot
    /// probe could not foresee: the cell downgrades to `Buffered` for
    /// good and retries at once (ADR-0088 D3 as amended); every other
    /// errno — and `EINVAL` under `Buffered` — is an ordinary abort with
    /// the backoff.
    pub fn on_ckpt_error(&mut self, errno: i32) {
        if errno == EINVAL && self.ckpt.io_mode_direct() {
            self.ckpt.abort_refused_direct("I/O");
            return;
        }
        self.ckpt.abort("I/O", &format!("errno {errno}"));
    }

    /// The MANIFEST + truncation slice (M2-S11/S12, ADR-0017), from
    /// MAINTAIN — never the hot path, **never a device barrier on the
    /// loop**, and **never a large unlink on the loop** (freeing a
    /// truncated file's pages is O(size) in the kernel — a measured
    /// multi-ms stall):
    ///
    /// 1. **Swap machine**: `.ick` dir-fsync → watermark guard →
    ///    `MANIFEST.new` stage+fdatasync → rename → shard dir-fsync →
    ///    commit (floor advance). Barriers ride the driver
    ///    (`TokenClass::ManifestSync`); one in flight, ever.
    /// 2. **Truncate**: forget sealed segments below the floor (fully
    ///    covered by the named checkpoint; the M5 retention hook can exempt
    ///    topic segments) and delegate their unlinks to the control thread.
    ///    Budget adapts to the covered backlog (M2.5-S11, ADR-0022 D8.4):
    ///    ≥ [`MAX_UNLINKS_PER_SLICE`], ≤ [`MAX_TRUNC_PER_SLICE_ADAPTIVE`].
    /// 3. **GC**: delegate ≤ [`MAX_UNLINKS_PER_SLICE`] stale-`.ick`/orphan
    ///    unlinks queued at commit.
    ///
    /// `control = None` (planeless/test tiers) falls back to inline
    /// unlinks — those tiers have no foreground tail to protect.
    /// Returns Maintenance units to charge (≈ one per file op).
    pub fn manifest_slice(
        &mut self,
        cx: &mut LoopCx<'_>,
        control: Option<&crate::ControlHandle>,
        unix_now_ms: u64,
        ks: &mut Keyspace,
        tier: Option<&mut crate::tier_cell::TierCell<F>>,
    ) -> u32 {
        if self.failed {
            return 0;
        }
        let watermark = self.commit.watermark().map(|l| l.to_u64());
        let before = self.manifest.stats();
        let mut units =
            self.manifest.swap_slice(cx, watermark, self.rotor.active_segment(), ks, tier);
        // ADR-0088 D5/D7: the swap's barriers and the MANIFEST envelope are
        // metered under `Checkpoint`, never deferred (a checkpoint that
        // wrote its bytes must publish).
        // The envelope bytes ride a synchronous `SegmentFile::write_at`
        // (not the driver): they are disclosed as `manifest_bytes_total`
        // and enter the write-amplification figure, but not the class
        // ledger, whose identity is "what the driver saw".
        let after = self.manifest.stats();
        let syncs = after.syncs_issued.saturating_sub(before.syncs_issued);
        if syncs > 0 {
            self.budget.charge(IoClass::Checkpoint, 0, syncs);
        }
        // A MANIFEST just committed (dir-fsync durable): publish the
        // control-board slot — the `INF.CKPT WAIT`/`LASTSAVE` observable
        // (M2-S20, ADR-0021 D6).
        if let Some((epoch, ckpt_id)) = self.manifest.take_published()
            && let Some(control) = control
        {
            control.ckpt_board().slot(self.manifest.cell()).publish(epoch, ckpt_id, unix_now_ms);
        }
        if let Some(floor) = self.manifest.floor() {
            // Adaptive drain (M2.5-S11, ADR-0022 D8.4): the budget follows
            // the covered backlog — half of it per slice, floored at the
            // fixed cap, ceilinged at MAX_TRUNC_PER_SLICE_ADAPTIVE — so a
            // fast writer cannot grow retained log unboundedly while the
            // drain plods at 2/slice. Still a bounded slice, never a burst.
            let backlog = self
                .rotor
                .sealed_below(floor)
                .iter()
                .filter(|meta| !self.manifest.truncation_exempt(meta.id))
                .count();
            let budget =
                MAX_UNLINKS_PER_SLICE.max(backlog.div_ceil(2)).min(MAX_TRUNC_PER_SLICE_ADAPTIVE);
            for _ in 0..budget {
                let Some(id) = self
                    .rotor
                    .sealed_below(floor)
                    .iter()
                    .map(|meta| meta.id)
                    .find(|&id| !self.manifest.truncation_exempt(id))
                else {
                    break;
                };
                // Forget-then-recycle-or-unlink (ADR-0090 D1): the rotor
                // drops the segment from the live set first so a failed/
                // late unlink can never resurrect it there (boot GC
                // re-collects survivors below the floor — pooled files
                // included). A pooled segment costs no file op here; it
                // is renamed into the next id by a later MAINTAIN prealloc.
                match self.rotor.forget_sealed(id) {
                    SealedDisposal::Recycled => {}
                    SealedDisposal::Unlink(path) => match control {
                        Some(control) => {
                            if !control.request_unlink(path.clone()) {
                                // Queue full: the path joins the GC queue
                                // and retries next slice (bounded, never
                                // a stall).
                                self.manifest.defer_unlink(path);
                            }
                        }
                        None => self.manifest.unlink_now(&path),
                    },
                }
                self.manifest.note_truncated(1);
                units += 1;
            }
        }
        units + self.manifest.gc_slice(MAX_UNLINKS_PER_SLICE, control)
    }

    /// REAP: a MANIFEST-swap barrier (`TokenClass::ManifestSync`) landed —
    /// phase flip only; follow-up metadata ops run next MAINTAIN slice.
    pub fn on_manifest_synced(&mut self) {
        self.manifest.on_synced();
    }

    /// REAP: a MANIFEST-swap barrier failed — the old recovery unit stays
    /// authoritative (the checkpoint-abort class, ADR-0017).
    pub fn on_manifest_error(&mut self, errno: i32) {
        self.manifest.on_sync_error(errno);
    }

    /// Manual trigger latch (`INF.CKPT`/`BGSAVE`, M2-S20): `epoch` is the
    /// control-board request this checkpoint will satisfy; it publishes
    /// back at the MANIFEST swap's dir-fsync commit.
    pub fn request_ckpt(&mut self, epoch: u64) {
        self.ckpt.requested = true;
        self.ckpt.req_epoch = self.ckpt.req_epoch.max(epoch);
    }

    /// Checkpoint gauges for the MAINTAIN stats flush.
    pub fn ckpt_stats(&self) -> CkptStats {
        self.ckpt.stats(self.records_appended)
    }
}
