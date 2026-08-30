//! `m4-recovery` (M4-S12, ADR-0057 D8): the unified recovery picture
//! under seeded power cuts — the never-none invariant checked directly.
//!
//! Each seeded run is a chain of lives on one [`SimDisk`]. Per life:
//! mutate (the modeled WAL tail carries real record-v1 encodings,
//! `ColdDisplace` pairing included) → run the fuzzy hybrid walk (refs
//! below the walk watermark, images above, mutations and demotion
//! interleaved between slices; the release pin holds — D2) → publish
//! `.ick` v2 + MANIFEST v2 as one recovery unit → more tail ops → a
//! seeded **power cut** tears every un-fsynced byte → recover
//! (`recover_tiered_ns` + hybrid checkpoint load + D4 tail replay) →
//! the oracle: every model-live key serves its exact bytes (cold ranges
//! CRC-verified from the recovered catalog), every model-dead key
//! misses. Content — canonical bytes — never addresses, and never
//! string versions (per-life artifacts, ADR-0057 D3).
//!
//! **Shadow-slot ops (M4.5-S37, ADR-0093):** a seeded share of the
//! SETs over a demoted key take the shadow path — the record appends,
//! the cold twin stays slotted as a ticket, no marker is staged — and
//! the harness reconciles tickets at seeded points (some are deliberately
//! left open across the walk and the cut). Recovery re-forms the pairs
//! from the checkpoint's ref + image and the tail's image (the D5
//! rebuild), the harness reconciles them again, and the never-none
//! oracle plus a **cardinality oracle** (`len()` equals the model's key
//! count after reconciliation — no orphan slot) close the row.
//!
//! Seed classes (deterministic per seed, disclosed in the report):
//! - **cut-before-publish** lives: the walk runs but the swap never
//!   lands — recovery resolves the *previous* unit and the WAL tail
//!   keeps accumulating (the truncation rule's negative half: nothing
//!   truncates without a durable name).
//! - **flush-lag** lives: demotion is suppressed during the walk, so
//!   RAM-resident records span checkpoints and must re-image every time
//!   (the D7 falsifier: coverage never leans on a previous checkpoint).
//!
//! Every event folds into `trace_hash`; `--verify-determinism` runs the
//! scenario twice and requires hash identity (L7).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_log::blob::{ExtentId, ExtentWriter, list_extent_ids, open_extent, unlink_extent_file};
use inf_log::ckpt::{IckReaderConfig, ick_file_name};
use inf_log::flush::{TierFileMeta, unlink_tier_file};
use inf_log::fs::SegmentFs;
use inf_log::fs::sim::SimDisk;
use inf_log::manifest::TierNsManifest;
use inf_log::{
    CkptConfig, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentId, StagingConfig,
    StagingRing, SyncIckWriter, TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode,
    decode_record, read_ick_hybrid, read_manifest, tier_extract, tier_frame_offset,
    tier_frame_span, write_manifest,
};
use inf_store::{
    AddressSpaceConfig, BlobConfig, CompactionWork, DemotionConfig, ExtentRef, KeyHasher,
    LogicalAddr, TieredLookup, TieredTable, apply_blob_ref_section, apply_live_set_section,
    apply_ref_section, forced_collision_pair, recover_tiered_ns,
};

const NS: NsId = NsId(88);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small tier files so lives span rotations and gaps.
const FILE_CAPACITY: u64 = 48 << 10;
/// Out-of-line threshold for the blob leg (M4-S17, ADR-0061) — above
/// every inline value the op generator emits, below every blob value.
const BLOB_THRESHOLD: u32 = 256;

/// Scenario knobs — the DSL v0 shape (a struct, not a language).
#[derive(Debug)]
pub struct RecoveryScenario {
    pub seed: u64,
    /// Distinct keys in play.
    pub keys: u64,
    /// Lives (cut + recover cycles) per run.
    pub lives: u64,
    /// Mutations per life phase.
    pub ops_per_phase: u64,
}

impl RecoveryScenario {
    #[must_use]
    pub fn m4_recovery(seed: u64) -> RecoveryScenario {
        // 480 ops/phase (was 320 pre-S17): the blob leg moved a sixth of
        // the op mix out of line, thinning record volume — the bump
        // keeps demotion, flush rotation, and copy-forward relocation
        // coverage on the smoke seed (coverage disclosed, never
        // assumed).
        RecoveryScenario { seed, keys: 800, lives: 4, ops_per_phase: 480 }
    }
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub violations: Vec<String>,
    pub lives: u64,
    pub refs_emitted: u64,
    pub images_emitted: u64,
    pub tail_records: u64,
    pub cut_before_publish: u64,
    pub flush_lag_lives: u64,
    pub keys_audited: u64,
    /// `.ick` 0x04 live-set entries emitted across all publishes
    /// (M4-S14 — coverage disclosed, never assumed).
    pub live_entries_emitted: u64,
    /// Copy-forward records relocated across all lives (M4-S15 —
    /// coverage disclosed: a sweep that never compacted proves nothing).
    pub relocations: u64,
    /// Files copy-forward fully scanned (byte counters finalized).
    pub files_scanned: u64,
    /// Files retired through a landed covering swap (ADR-0059 D3).
    pub files_retired: u64,
    /// Retired files unlinked in-life.
    pub files_unlinked: u64,
    /// Retired files deliberately left for the boot GC (the
    /// swap ↔ unlink crash window, driven).
    pub unlinks_left_to_boot_gc: u64,
    /// Compaction slices that stalled on the tail window (refusal-aware
    /// admission observed working, ADR-0059 D6).
    pub compaction_stalls: u64,
    /// Blob extents written and referenced (M4-S17 — coverage disclosed:
    /// a refcount oracle over zero blobs proves nothing).
    pub blobs_written: u64,
    /// Orphan extents deliberately planted (durable bytes, no reference
    /// — the AC1 cut, seeded).
    pub blob_orphans_planted: u64,
    /// Extents reclaimed in-life (refcount zero, death durable) plus
    /// orphans swept at boot.
    pub blob_extents_reclaimed: u64,
    /// M4.5-S37 (ADR-0093): shadow tickets opened by the op mix, left
    /// open across a cut, re-formed by recovery, and the verdicts.
    pub shadow_opened: u64,
    pub shadow_open_at_cut: u64,
    pub shadow_reformed: u64,
    pub shadow_same_key: u64,
    pub shadow_collision: u64,
    /// ADR-0093 A7: ops on the crafted colliding pairs (two real keys
    /// with one 64-bit hash), rebuilt slots the boot read and settled
    /// by their full key (A4), and the `DBSIZE`-shaped drain checks
    /// (A3: verify every unverified ticket, then `len()` must equal the
    /// model with the verified tickets still open).
    pub shadow_collide_ops: u64,
    pub shadow_settled_at_boot: u64,
    pub shadow_drain_checks: u64,
    pub trace_hash: u64,
}

impl RecoveryReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

#[derive(Clone)]
struct Expect {
    value: Vec<u8>,
    /// The referenced extent when the value is out of line (M4-S17).
    extent: Option<u64>,
}

struct Life {
    table: TieredTable,
    flush: TierFlush<SimDisk>,
    /// Staging admission for the epoch stamp (M4-S17, ADR-0061 D5) —
    /// the harness models WAL durability, but the reclaim gate's epoch
    /// runs through the production `stage_wal` path.
    ring: StagingRing,
    /// Suppress demotion during the walk (the flush-lag class).
    flush_lag: bool,
}

fn tiered_table(origin: u64, hasher: KeyHasher) -> TieredTable {
    let mut table = TieredTable::new(space_config(origin), demote(), 1024, hasher).expect("ring");
    table.set_blob_config(BlobConfig { threshold_bytes: BLOB_THRESHOLD, max_bytes: 1 << 20 });
    // The shadow arm (M4.5-S37, ADR-0093 D8) runs on in this harness —
    // the store-level DST's authority over the mechanism.
    table.set_shadow_enabled(true);
    table
}

fn flush_config(shard: &Path) -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: shard.to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        file_capacity: FILE_CAPACITY,
        slice_bytes: PAGE,
    }
}

fn demote() -> DemotionConfig {
    // A small mutable fraction keeps all three residency classes in
    // play at this corpus size.
    DemotionConfig { mem_budget_bytes: BUDGET, mutable_permille: 40, slice_bytes: PAGE }
}

fn space_config(origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote().ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

/// Reads one cold record straight from the tier bytes through the
/// catalog (CRC-verified; the read path the §3.1 oracle stands on).
fn read_cold(disk: &SimDisk, flush: &TierFlush<SimDisk>, addr: u64, len: usize) -> Option<Vec<u8>> {
    let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
    let (base, path) = flush
        .sealed()
        .iter()
        .find(|m| contains(m.base.to_raw(), m.data_len))
        .map(|m| (m.base.to_raw(), m.path.clone()))
        .or_else(|| {
            let (_, base, _, durable_len, path) = flush.active()?;
            contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
        })?;
    let file = disk.open_read(&path).ok()?;
    let (first, count, skip) = tier_frame_span(addr - base, len);
    let from = tier_frame_offset(first);
    let span = count as usize * TIER_FRAME_BYTES;
    let mut window = vec![0u8; span];
    let mut done = 0usize;
    while done < span {
        use inf_log::fs::SegmentFile;
        let n = file.read_at(from + done as u64, &mut window[done..]).ok()?;
        if n == 0 {
            return None;
        }
        done += n;
    }
    let mut out = Vec::new();
    tier_extract(&window, skip, len, &mut out).ok()?;
    Some(out)
}

/// Reads one whole cold record: the header window sizes it, the exact
/// span follows (never a model-length read — a colliding cold candidate
/// is another key of another length, ADR-0093 A7).
fn read_cold_record(disk: &SimDisk, flush: &TierFlush<SimDisk>, addr: u64) -> Option<Vec<u8>> {
    let head = read_cold(disk, flush, addr, TieredTable::RECORD_HEADER_LEN)?;
    let len = TieredTable::record_len_from_header(&head);
    read_cold(disk, flush, addr, len)
}

/// The op mix's key (ADR-0093 A7): one in sixteen is a crafted colliding
/// key — either side of one of `pairs` — so the shadow, `DEL`, walk and
/// recovery paths meet two real keys with one hash on every seed.
fn seeded_key(rng: &mut SplitMix64, keys: u64, pairs: &[([u8; 48], [u8; 48])]) -> Vec<u8> {
    if rng.next_u64().is_multiple_of(16) {
        let pair = &pairs[(rng.next_u64() % pairs.len() as u64) as usize];
        return if rng.next_u64().is_multiple_of(2) { pair.0.to_vec() } else { pair.1.to_vec() };
    }
    let idx = rng.next_u64() % keys;
    format!("rec:{idx:05}").into_bytes()
}

/// Tag spread for the crafted pairs (four unrelated pairs per seed).
const P_TAG: u64 = 0x9E37_79B9_7F4A_7C15;

struct Run {
    disk: SimDisk,
    shard: PathBuf,
    model: BTreeMap<Vec<u8>, Expect>,
    /// Encoded record-v1 tail since the last durable publish (the WAL's
    /// covered suffix; a publish truncates the checkpoint-covered
    /// prefix — the D7 rule made literal).
    tail: Vec<u8>,
    /// Retired-and-detached files awaiting unlink (the plane's
    /// pin-analog queue; some are deliberately left for the boot GC —
    /// the swap ↔ unlink crash window).
    pending_unlink: Vec<TierFileMeta>,
    report: RecoveryReport,
}

impl Run {
    fn maintain(&mut self, life: &mut Life) {
        loop {
            let sealed = life.table.seal_slice();
            let f = life.table.flush_slice(&mut life.flush).expect("flush slice");
            let released = life.table.release_slice();
            if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
                break;
            }
        }
        self.reclaim_blobs(life, "maintain");
    }

    /// The extent-reclaim slice (M4-S17, ADR-0061 D5): candidates whose
    /// killing record's staging epoch is covered unlink here — with the
    /// early-free oracle armed (a model-live extent handed out is a
    /// violation, immediately).
    fn reclaim_blobs(&mut self, life: &mut Life, when: &str) {
        let durable = life.table.wal_epoch();
        loop {
            let work = life.table.extent_reclaim_work(durable, 4);
            if work.is_empty() {
                break;
            }
            for id in work {
                if self.model.values().any(|e| e.extent == Some(id)) {
                    self.report
                        .violations
                        .push(format!("{when}: early free — extent {id} is model-live"));
                    life.table.extent_reclaim_done(id);
                    continue;
                }
                unlink_extent_file(&self.disk, &self.shard, ExtentId(id)).expect("sim unlink");
                life.table.extent_reclaim_done(id);
                self.report.blob_extents_reclaimed += 1;
            }
        }
    }

    /// Stages one effect through the production `stage_wal` path (the
    /// reclaim-gate epoch), recycling the ring on admission refusal (the
    /// modeled tail is the durable record here, not the ring).
    fn stage(&mut self, life: &mut Life, effect: &MutationEffect<'_>) {
        if life.table.stage_wal(&mut life.ring, effect).is_err() {
            life.ring = StagingRing::new(StagingConfig::default());
            life.table.stage_wal(&mut life.ring, effect).expect("a fresh ring has room");
        }
    }

    /// A bounded burst of copy-forward slices (M4-S15, ADR-0059 D2):
    /// work request → catalog cold read → apply, with the cold floor
    /// asserted monotone. A stalled slice runs one maintain round (the
    /// refusal-aware admission's resolver) and continues.
    fn compact(&mut self, life: &mut Life, pressure: bool, rounds: u32, when: &str) {
        let floor_before = life.table.cold_floor();
        let mut budget = PAGE * 4;
        for _ in 0..rounds {
            match life.table.compaction_work(&life.flush, pressure, budget) {
                CompactionWork::Read { file_id, addr, len } => {
                    let Some(bytes) =
                        self.read_scan_chunk(&life.flush, file_id, addr.to_raw(), len)
                    else {
                        self.report
                            .violations
                            .push(format!("{when}: compaction read failed (file {file_id})"));
                        return;
                    };
                    let applied = life.table.compaction_apply(file_id, addr, &bytes);
                    self.report.relocations += u64::from(applied.relocated);
                    if applied.file_scanned {
                        self.report.files_scanned += 1;
                    }
                    // An oversized record re-reads at exactly its length
                    // (minimum-one-record progress, bounded at one).
                    budget = if applied.need > 0 { applied.need } else { PAGE * 4 };
                    if applied.stalled {
                        self.report.compaction_stalls += 1;
                        self.maintain(life);
                    }
                }
                CompactionWork::Idle => break,
            }
        }
        if life.table.cold_floor() < floor_before {
            self.report.violations.push(format!("{when}: the cold floor moved backwards"));
        }
    }

    /// One reconciliation (ADR-0093 D4) played by the harness: the
    /// ticket's cold record read through the catalog, the verdict
    /// applied. A read that fails (a file the pin-analog unlink took —
    /// impossible while the slot is live) is a violation.
    fn reconcile_ticket(&mut self, life: &mut Life, ticket: inf_store::ShadowTicket, when: &str) {
        let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw()) else {
            self.report.violations.push(format!(
                "{when}: shadow twin at {} unreadable while its slot is live",
                ticket.cold.to_raw()
            ));
            life.table.shadow_read_failed(ticket.cold);
            return;
        };
        let same_key = self.twin_is_winner_key(life, &ticket, &image);
        match life.table.resolve_shadow(ticket.hash, ticket.cold, &image) {
            inf_store::ShadowVerdict::SameKey => {
                self.report.shadow_same_key += 1;
                if !same_key {
                    self.report
                        .violations
                        .push(format!("{when}: same-key verdict on a collision twin"));
                }
            }
            inf_store::ShadowVerdict::Collision => {
                // ADR-0093 A7: legal exactly when the twin's full key is
                // not the winner's — the crafted pairs; on equal keys it
                // is a wrong comparison.
                self.report.shadow_collision += 1;
                if same_key {
                    self.report
                        .violations
                        .push(format!("{when}: collision verdict on a same-key twin"));
                }
            }
            inf_store::ShadowVerdict::Stale | inf_store::ShadowVerdict::Deferred => {}
        }
    }

    /// The oracle's own comparison: the twin's decoded key against the
    /// ticket's current winner's key (the verdict is checked, never
    /// trusted).
    fn twin_is_winner_key(
        &self,
        life: &Life,
        ticket: &inf_store::ShadowTicket,
        image: &[u8],
    ) -> bool {
        let Some(current) = life.table.shadow_tickets().find(|t| t.cold == ticket.cold) else {
            return false;
        };
        TieredTable::decode_record(image).key == life.table.record(current.winner).key
    }

    /// The `DEL` path's verify (ADR-0093 D3): the twin read and
    /// key-compared; a same-key twin is deleted through its own marker
    /// (into the tail) and `delete` — never `resolve_shadow`, whose
    /// death attribution is deferred under a pinned walk.
    fn verify_twin_for_delete(
        &mut self,
        life: &mut Life,
        ticket: inf_store::ShadowTicket,
        when: &str,
    ) {
        // A verified ticket needs no read (ADR-0093 A1): the exact
        // length is on the ticket — the plane's `delete_one` rule.
        if let Some(len) = ticket.verified_len {
            RecordView::ColdDisplace { ns: NS, old_addr: ticket.cold.to_raw() }
                .encode_into(&mut self.tail);
            life.table.delete(ticket.hash, ticket.cold, len as usize);
            self.report.shadow_same_key += 1;
            return;
        }
        let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw()) else {
            self.report.violations.push(format!(
                "{when}: shadow twin at {} unreadable while its slot is live",
                ticket.cold.to_raw()
            ));
            return;
        };
        let same_key = self.twin_is_winner_key(life, &ticket, &image);
        match life.table.verify_shadow(ticket.hash, ticket.cold, &image) {
            inf_store::ShadowVerdict::SameKey => {
                if !same_key {
                    self.report
                        .violations
                        .push(format!("{when}: same-key verdict on a collision twin"));
                }
                RecordView::ColdDisplace { ns: NS, old_addr: ticket.cold.to_raw() }
                    .encode_into(&mut self.tail);
                life.table.delete(ticket.hash, ticket.cold, image.len());
                self.report.shadow_same_key += 1;
            }
            inf_store::ShadowVerdict::Collision => {
                self.report.shadow_collision += 1;
                if same_key {
                    self.report
                        .violations
                        .push(format!("{when}: collision verdict on a same-key twin"));
                }
            }
            _ => {}
        }
    }

    /// The `DBSIZE` drain played by the harness (ADR-0093 A3): verify
    /// every unverified ticket (reads only — no settle, so a pinned walk
    /// is no obstacle), then `len()` must equal the model with the
    /// verified tickets still open. An unverified collision ticket that
    /// survived verification, or a count off by one, is the review's
    /// finding reconstructed.
    fn audit_len_after_drain(&mut self, life: &mut Life, when: &str) {
        for ticket in life.table.shadow_unverified_tickets() {
            let Some(image) = read_cold_record(&self.disk, &life.flush, ticket.cold.to_raw())
            else {
                self.report.violations.push(format!(
                    "{when}: drain — shadow twin at {} unreadable",
                    ticket.cold.to_raw()
                ));
                return;
            };
            let same_key = self.twin_is_winner_key(life, &ticket, &image);
            match life.table.verify_shadow(ticket.hash, ticket.cold, &image) {
                inf_store::ShadowVerdict::SameKey if !same_key => self
                    .report
                    .violations
                    .push(format!("{when}: drain — same-key verdict on a collision twin")),
                inf_store::ShadowVerdict::Collision if same_key => self
                    .report
                    .violations
                    .push(format!("{when}: drain — collision verdict on a same-key twin")),
                inf_store::ShadowVerdict::Collision => self.report.shadow_collision += 1,
                _ => {}
            }
        }
        self.report.shadow_drain_checks += 1;
        if life.table.shadow_unverified() != 0 {
            self.report
                .violations
                .push(format!("{when}: drain left {} unverified", life.table.shadow_unverified()));
        }
        if life.table.len() != self.model.len() {
            self.report.violations.push(format!(
                "{when}: DBSIZE EXACTNESS VIOLATION — len {} vs model {} with {} verified \
                 tickets open",
                life.table.len(),
                self.model.len(),
                life.table.shadow_pending()
            ));
        }
    }

    /// Reconciles up to `max` tickets (the MAINTAIN pump played by the
    /// harness) — oldest winner first, the store's own work list.
    fn reconcile(&mut self, life: &mut Life, max: usize, when: &str) {
        for read in life.table.shadow_work(max) {
            self.reconcile_ticket(life, read.ticket, when);
        }
    }

    /// Reconciles every open ticket; a round that resolves nothing is a
    /// wedged reconciler — a violation, never a spin.
    fn reconcile_all(&mut self, life: &mut Life, when: &str) {
        while life.table.shadow_pending() > 0 {
            let before = life.table.shadow_pending();
            self.reconcile(life, 16, when);
            if life.table.shadow_pending() >= before {
                self.report.violations.push(format!(
                    "{when}: reconciliation made no progress with {before} tickets open"
                ));
                return;
            }
        }
    }

    /// Reads one scan chunk of a sealed catalog file (compaction
    /// candidates are sealed by eligibility, so the range path resolves).
    fn read_scan_chunk(
        &self,
        flush: &TierFlush<SimDisk>,
        file_id: u32,
        addr: u64,
        len: u64,
    ) -> Option<Vec<u8>> {
        debug_assert!(flush.sealed().iter().any(|m| m.id == file_id), "candidates are sealed");
        read_cold(&self.disk, flush, addr, usize::try_from(len).expect("chunk fits"))
    }

    /// One live-path mutation, recorded into the modeled tail with its
    /// displacement marker (ADR-0057 D4 — unconditional for displacing
    /// mutations).
    fn apply_op(&mut self, life: &mut Life, key: &[u8], op: Op) {
        let hash = life.table.hash_key(key);
        if key.starts_with(inf_store::COLLISION_KEY_PREFIX) {
            self.report.shadow_collide_ops += 1;
        }
        // The shadow path (ADR-0093 D2): probe → admit → insert →
        // register → the image alone into the tail (no marker). Any
        // other probe answer or a refusal is a plain SET.
        let op = match op {
            Op::SetShadow(value) => {
                let record_len = TieredTable::RECORD_HEADER_LEN + key.len() + value.len();
                match life.table.shadow_probe(key, hash) {
                    inf_store::ShadowProbe::One(cold)
                        if life.table.shadow_admit(hash, cold, record_len).is_ok() =>
                    {
                        let winner = life.table.insert(key, &value, hash).expect("fits");
                        life.table.register_shadow(hash, cold, winner);
                        self.stage(life, &MutationEffect::StringSet { ns: NS, key, value: &value });
                        RecordView::StringPostImage { ns: NS, key, value: &value }
                            .encode_into(&mut self.tail);
                        self.report.tail_records += 1;
                        self.report.shadow_opened += 1;
                        self.model.insert(key.to_vec(), Expect { value, extent: None });
                        return;
                    }
                    _ => Op::Set(value),
                }
            }
            other => other,
        };
        // The plane's resolve: RAM verifies in place; a cold candidate is
        // read and its full key compared, a mismatch (a fingerprint false
        // positive, or a crafted collision — ADR-0093 A7) excluded and
        // the probe retried.
        let mut exclude: Vec<LogicalAddr> = Vec::new();
        let displaced = loop {
            match life.table.lookup(key, hash, &exclude) {
                TieredLookup::Ram(addr) => {
                    let parts = life.table.record(addr);
                    break Some((addr, parts.encoded_len, parts.version));
                }
                TieredLookup::Cold(addr) => {
                    let bytes = read_cold_record(&self.disk, &life.flush, addr.to_raw())
                        .expect("cold record readable");
                    let parts = TieredTable::decode_record(&bytes);
                    if parts.key == key {
                        break Some((addr, parts.encoded_len, parts.version));
                    }
                    exclude.push(addr);
                }
                TieredLookup::Miss => break None,
            }
        };
        match op {
            Op::Set(value) => {
                match displaced {
                    Some((old, old_len, old_version)) => {
                        // Relocation-origin markers first (ADR-0059 D9):
                        // one ColdDisplace per address an un-superseded
                        // checkpoint may still ref this record by, then
                        // the ordinary marker for the live address.
                        for (origin, _) in life.table.take_displacement_origins(hash, old) {
                            RecordView::ColdDisplace { ns: NS, old_addr: origin }
                                .encode_into(&mut self.tail);
                        }
                        RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                            .encode_into(&mut self.tail);
                        life.table
                            .update(key, &value, hash, old, old_len, old_version)
                            .expect("fits");
                    }
                    None => {
                        life.table.insert(key, &value, hash).expect("fits");
                    }
                }
                self.stage(life, &MutationEffect::StringSet { ns: NS, key, value: &value });
                RecordView::StringPostImage { ns: NS, key, value: &value }
                    .encode_into(&mut self.tail);
                self.report.tail_records += 1;
                self.model.insert(key.to_vec(), Expect { value, extent: None });
            }
            Op::SetShadow(_) => unreachable!("rewritten to Set above"),
            Op::SetBlob(value) => {
                // M4-S17 (ADR-0061 D3): extent bytes → fdatasync →
                // sealed token → only then the referencing record. The
                // cut physics are real — an unfsynced extent would tear,
                // and only the token proves it cannot be referenced.
                let extent_id = ExtentId(life.table.allocate_extent_id());
                let mut w = ExtentWriter::create(
                    &self.disk,
                    &self.shard,
                    extent_id,
                    0,
                    NS,
                    value.len() as u64,
                    TierIoMode::Buffered,
                )
                .expect("extent create");
                for chunk in value.chunks(29) {
                    w.append_chunk(chunk).expect("extent chunk");
                }
                let sealed = w.finish().expect("extent fsync");
                life.table.note_blob_bytes(sealed.device_bytes());
                if let Some((old, _, _)) = displaced {
                    for (origin, _) in life.table.take_displacement_origins(hash, old) {
                        RecordView::ColdDisplace { ns: NS, old_addr: origin }
                            .encode_into(&mut self.tail);
                    }
                    RecordView::ColdDisplace { ns: NS, old_addr: old.to_raw() }
                        .encode_into(&mut self.tail);
                }
                self.stage(
                    life,
                    &MutationEffect::StringSetExtent {
                        ns: NS,
                        key,
                        extent_id: sealed.extent_id().0,
                        offset: 0,
                        len: sealed.data_len(),
                    },
                );
                match displaced {
                    Some((old, old_len, old_version)) => {
                        life.table
                            .update_extent(key, hash, &sealed, old, old_len, old_version)
                            .expect("fits");
                    }
                    None => {
                        life.table.insert_extent(key, hash, &sealed).expect("fits");
                    }
                }
                RecordView::StringExtentRef {
                    ns: NS,
                    key,
                    extent_id: sealed.extent_id().0,
                    offset: 0,
                    len: sealed.data_len(),
                }
                .encode_into(&mut self.tail);
                self.report.tail_records += 1;
                self.report.blobs_written += 1;
                self.model.insert(key.to_vec(), Expect { value, extent: Some(extent_id.0) });
            }
            Op::Del => {
                if let Some((addr, len, _)) = displaced {
                    // ADR-0093 D3: a winner's ticket is verified before
                    // its delete and the same-key twin takes the marker
                    // path (its own `ColdDisplace` + `delete`) — the
                    // plane's `delete_one` rule, played here.
                    if let Some(ticket) = life.table.shadow_of_winner(addr) {
                        self.verify_twin_for_delete(life, ticket, "del");
                    }
                    for (origin, _) in life.table.take_displacement_origins(hash, addr) {
                        RecordView::ColdDisplace { ns: NS, old_addr: origin }
                            .encode_into(&mut self.tail);
                    }
                    RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }
                        .encode_into(&mut self.tail);
                    self.stage(life, &MutationEffect::Delete { ns: NS, key });
                    RecordView::Delete { ns: NS, key }.encode_into(&mut self.tail);
                    self.report.tail_records += 1;
                    life.table.delete(hash, addr, len);
                    self.model.remove(key);
                }
            }
        }
    }

    /// The cardinality oracle (ADR-0093 I5): with no ticket open, the
    /// keys the table counts are exactly the model's — an orphan slot
    /// (a twin that survived its key) or a lost key would show here
    /// before any read does.
    fn audit_cardinality(&mut self, life: &Life, when: &str) {
        if life.table.shadow_pending() != 0 {
            return;
        }
        if life.table.len() != self.model.len() {
            // Name the anomaly: every slot decoded (RAM directly, cold
            // through the catalog), counted per key, compared to the
            // model — the diagnosis a bare count cannot give.
            let mut slots: Vec<(u64, LogicalAddr)> = Vec::new();
            let mut cursor = 0u64;
            loop {
                cursor = life.table.scan_slots(cursor, 256, |hash, addr| slots.push((hash, addr)));
                if cursor == 0 {
                    break;
                }
            }
            let mut per_key: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
            for (_, addr) in slots {
                let (key, class) = if life.table.space().resolve(addr) == inf_store::AddrClass::Cold
                {
                    let head = read_cold(
                        &self.disk,
                        &life.flush,
                        addr.to_raw(),
                        TieredTable::RECORD_HEADER_LEN,
                    );
                    let image = head
                        .map(|h| TieredTable::record_len_from_header(&h))
                        .and_then(|len| read_cold(&self.disk, &life.flush, addr.to_raw(), len));
                    match image {
                        Some(image) => (TieredTable::decode_record(&image).key.to_vec(), "cold"),
                        None => (b"<unreadable>".to_vec(), "cold"),
                    }
                } else {
                    (life.table.record(addr).key.to_vec(), "ram")
                };
                per_key.entry(key).or_default().push(format!("{class}@{}", addr.to_raw()));
            }
            let anomalies: Vec<String> = per_key
                .iter()
                .filter(|(key, at)| at.len() != 1 || !self.model.contains_key(*key))
                .map(|(key, at)| format!("{}: {at:?}", String::from_utf8_lossy(key)))
                .take(6)
                .collect();
            self.report.violations.push(format!(
                "{when}: cardinality — the table counts {} keys, the model {} — {anomalies:?}",
                life.table.len(),
                self.model.len()
            ));
        }
    }

    /// The oracle: every model key serves its exact bytes; every other
    /// key misses. Content only — versions are per-life (D3), addresses
    /// are per-life (§3.1).
    fn audit(&mut self, life: &Life, when: &str) {
        for (key, expect) in &self.model {
            let hash = life.table.hash_key(key);
            let mut exclude: Vec<LogicalAddr> = Vec::new();
            let got = loop {
                match life.table.lookup(key, hash, &exclude) {
                    TieredLookup::Ram(addr) => {
                        let parts = life.table.record(addr);
                        match parts.extent_ref() {
                            // M4-S17: the record carries a reference —
                            // the value serves through the CRC-verified
                            // extent reader (chunked, the cold-read
                            // shape), and the refcount map must agree.
                            Some(ext) => {
                                if life.table.extent_reference_at(addr)
                                    != Some((ext.extent_id, ext.len))
                                {
                                    break None; // map desync = a loss shape
                                }
                                break read_blob(&self.disk, &self.shard, ext);
                            }
                            None => break Some(parts.value.to_vec()),
                        }
                    }
                    TieredLookup::Cold(addr) => {
                        match read_cold_record(&self.disk, &life.flush, addr.to_raw()) {
                            Some(bytes) => {
                                let parts = TieredTable::decode_record(&bytes);
                                if parts.key == key.as_slice() {
                                    match parts.extent_ref() {
                                        Some(ext) => {
                                            break read_blob(&self.disk, &self.shard, ext);
                                        }
                                        None => break Some(parts.value.to_vec()),
                                    }
                                }
                                exclude.push(addr); // fingerprint false positive
                            }
                            None => break None,
                        }
                    }
                    TieredLookup::Miss => break None,
                }
            };
            self.report.keys_audited += 1;
            match got {
                Some(value) if value == expect.value => {
                    self.report.trace_hash = hash64(&value, self.report.trace_hash);
                }
                Some(_) => self
                    .report
                    .violations
                    .push(format!("{when}: wrong bytes for {}", String::from_utf8_lossy(key))),
                None => {
                    let shape = match life.table.lookup(key, hash, &[]) {
                        TieredLookup::Miss => "index miss".to_string(),
                        TieredLookup::Cold(addr) => {
                            format!("dangling cold ref at addr {}", addr.to_raw())
                        }
                        TieredLookup::Ram(_) => "ram (transient?)".to_string(),
                    };
                    self.report.violations.push(format!(
                        "{when}: never-none violated — {} lost ({shape})",
                        String::from_utf8_lossy(key)
                    ));
                }
            }
        }
    }
}

enum Op {
    Set(Vec<u8>),
    /// An out-of-line value (M4-S17): always at or above the threshold.
    SetBlob(Vec<u8>),
    /// A SET that takes the shadow path when the key's only exact
    /// candidate is cold (M4.5-S37); otherwise a plain `Set`.
    SetShadow(Vec<u8>),
    Del,
}

/// Streams one blob value through the chunked, CRC-verified extent
/// reader (M4-S17) — `None` on any read or CRC failure (a loss shape
/// the audit reports as never-none).
fn read_blob(disk: &SimDisk, shard: &Path, ext: ExtentRef) -> Option<Vec<u8>> {
    let mut reader =
        open_extent(disk, shard, ExtentId(ext.extent_id), TierIoMode::Buffered).ok()?;
    let len = usize::try_from(ext.len).expect("fits");
    let mut out = Vec::with_capacity(len);
    let mut offset = 0usize;
    while offset < len {
        let take = (len - offset).min(1000);
        match reader.read(offset as u64, take, &mut out) {
            Ok(Ok(())) => offset += take,
            Ok(Err(_)) | Err(_) => return None,
        }
    }
    Some(out)
}

/// The M4-S17 post-recovery refcount oracle (ADR-0061 D6): the
/// reference map equals the model's live blob set exactly — every live
/// blob key maps to its extent at count 1, and no extra reference
/// exists. Then the sweep: every listed-but-unreferenced extent
/// reclaims (orphans and stale alike), never a live one.
fn check_blob_refs(run: &mut Run, life: &mut Life, listed: &[u64], life_index: u64) {
    let mut model_live: Vec<u64> = run.model.values().filter_map(|e| e.extent).collect();
    model_live.sort_unstable();
    let mapped: Vec<u64> = life.table.extent_references().map(|(_, ext, _)| ext).collect();
    let mut mapped_sorted = mapped.clone();
    mapped_sorted.sort_unstable();
    if mapped_sorted != model_live {
        run.report.violations.push(format!(
            "life {life_index}: reference map {mapped_sorted:?} != model {model_live:?}"
        ));
    }
    for ext in &model_live {
        if life.table.extent_refcount(*ext) != 1 {
            run.report.violations.push(format!(
                "life {life_index}: extent {ext} refcount {} != 1",
                life.table.extent_refcount(*ext)
            ));
        }
    }
    life.table.extent_sweep_seed(listed);
    run.reclaim_blobs(life, &format!("life {life_index} boot sweep"));
    // Post-sweep, the directory is exactly the live set (zero leaks,
    // zero early frees — checked against the disk, not the accounting).
    let on_disk: Vec<u64> =
        list_extent_ids(&run.disk, &run.shard).expect("listing").iter().map(|i| i.0).collect();
    if on_disk != model_live {
        run.report.violations.push(format!(
            "life {life_index}: post-sweep directory {on_disk:?} != live set {model_live:?}"
        ));
    }
    run.report.trace_hash = hash64(&(model_live.len() as u64).to_le_bytes(), run.report.trace_hash);
}

/// The M4-S14 post-recovery consistency oracle: enumerate the recovered
/// index through a pinned walk (nothing has flushed in the new life, so
/// every pre-life slot emits as a ref), bucket slots by manifested file,
/// and require the live-set counts to match exactly; byte counters obey
/// the sound-direction rule (`dead ≤ len`; byte-exact means fully dead).
fn check_live_set(
    table: &mut TieredTable,
    tier: &TierNsManifest,
    walk_id: u64,
    report: &mut RecoveryReport,
    life_index: u64,
) {
    let mut truth: BTreeMap<u32, u64> = BTreeMap::new();
    let w = table.begin_ckpt_walk(walk_id).to_raw();
    if w != tier.flushed {
        report.violations.push(format!("life {life_index}: new life not at the watermark"));
    }
    let mut cursor = 0u64;
    loop {
        let mut refs: Vec<u64> = Vec::new();
        cursor = table.ckpt_walk_slice(cursor, 256, |_, addr| refs.push(addr.to_raw()), |_| {});
        for addr in refs {
            match tier.files.iter().find(|f| addr >= f.base && addr < f.base + f.durable_len) {
                Some(range) => *truth.entry(range.id).or_default() += 1,
                None => report
                    .violations
                    .push(format!("life {life_index}: slot {addr} outside every manifested file")),
            }
        }
        if cursor == 0 {
            break;
        }
    }
    table.end_ckpt_walk();
    for f in table.live_set().files() {
        let want = truth.get(&f.id).copied().unwrap_or(0);
        if f.live_count != want {
            report.violations.push(format!(
                "life {life_index}: file {} live count {} but the index holds {want}",
                f.id, f.live_count
            ));
        }
        if f.dead_bytes > f.data_len {
            report.violations.push(format!(
                "life {life_index}: file {} dead {} exceeds its {} bytes",
                f.id, f.dead_bytes, f.data_len
            ));
        }
        if f.byte_exact && f.dead_bytes != f.data_len {
            report.violations.push(format!(
                "life {life_index}: file {} restored byte-exact without being fully dead",
                f.id
            ));
        }
        report.trace_hash = hash64(
            &[
                u64::from(f.id).to_le_bytes(),
                f.live_count.to_le_bytes(),
                f.dead_bytes.to_le_bytes(),
            ]
            .concat(),
            report.trace_hash,
        );
    }
}

fn seeded_op(rng: &mut SplitMix64) -> Op {
    match rng.next_u64() % 8 {
        0 => Op::Del,
        // The blob leg (M4-S17): values at or above the threshold, small
        // enough that the extent lifecycle churns at DST scale.
        1 => {
            let len = BLOB_THRESHOLD as usize + (rng.next_u64() % 300) as usize;
            Op::SetBlob(vec![(rng.next_u64() % 251) as u8; len])
        }
        // The shadow leg (M4.5-S37): a quarter of the inline SETs.
        2 | 3 => {
            let len = 24 + (rng.next_u64() % 140) as usize;
            Op::SetShadow(vec![(rng.next_u64() % 251) as u8; len])
        }
        _ => {
            let len = 24 + (rng.next_u64() % 140) as usize;
            Op::Set(vec![(rng.next_u64() % 251) as u8; len])
        }
    }
}

/// Runs the scenario once. Deterministic from `scenario.seed` (L7).
#[must_use]
pub fn run_recovery_scenario(scenario: &RecoveryScenario) -> RecoveryReport {
    let mut rng = SplitMix64::new(scenario.seed ^ 0x4EC0_7E4Fu64);
    // The table's key hasher (ADR-0094 D2): seed-derived, one value for
    // every life of the run (the checkpoint's refs are its outputs).
    let hasher = KeyHasher::from_seed(scenario.seed ^ 0x4B45_5948);
    let disk = SimDisk::new();
    let shard = PathBuf::from("node/shard-0");
    disk.create_dir_all(&shard).expect("shard dir");
    let mut run = Run {
        disk: disk.clone(),
        shard: shard.clone(),
        model: BTreeMap::new(),
        tail: Vec::new(),
        pending_unlink: Vec::new(),
        report: RecoveryReport::default(),
    };
    let mut life = Life {
        table: tiered_table(0, hasher),
        flush: TierFlush::new(disk.clone(), flush_config(&shard), 0),
        ring: StagingRing::new(StagingConfig::default()),
        flush_lag: false,
    };
    let mut ckpt_id = 0u64;
    // ADR-0093 A7: four crafted colliding pairs per seed — two real keys
    // with one 64-bit hash each, routed by the shared hashtag.
    let pairs: Vec<([u8; 48], [u8; 48])> =
        (0..4u64).map(|i| forced_collision_pair(scenario.seed ^ i.wrapping_mul(P_TAG))).collect();

    for life_index in 0..scenario.lives {
        run.report.lives += 1;
        life.flush_lag = life_index > 0 && rng.next_u64().is_multiple_of(3);
        if life.flush_lag {
            run.report.flush_lag_lives += 1;
        }
        // Phase A: mutations (into the tail — everything since the last
        // durable publish replays).
        for _ in 0..scenario.ops_per_phase {
            let key = seeded_key(&mut rng, scenario.keys, &pairs);
            let op = seeded_op(&mut rng);
            run.apply_op(&mut life, &key, op);
            if !life.flush_lag && rng.next_u64().is_multiple_of(32) {
                run.maintain(&mut life);
            }
            // The reconciler's cadence (ADR-0093 D4), seeded: most
            // tickets resolve in-life, some stay open into the walk and
            // the cut.
            if rng.next_u64().is_multiple_of(5) {
                run.reconcile(&mut life, 2, &format!("life {life_index} phase A"));
            }
            // The seeded AC1 cut (M4-S17): occasionally an extent
            // reaches durability and the "process dies" before its
            // referencing record exists — a durable orphan nothing can
            // ever resolve; the boot sweep must reclaim it.
            if rng.next_u64().is_multiple_of(48) {
                let orphan_id = ExtentId(life.table.allocate_extent_id());
                let len = BLOB_THRESHOLD as usize + (rng.next_u64() % 64) as usize;
                let mut w = ExtentWriter::create(
                    &run.disk,
                    &run.shard,
                    orphan_id,
                    0,
                    NS,
                    len as u64,
                    TierIoMode::Buffered,
                )
                .expect("orphan create");
                w.append_chunk(&vec![(rng.next_u64() % 251) as u8; len]).expect("orphan bytes");
                let _token_lost = w.finish().expect("orphan fsync");
                run.report.blob_orphans_planted += 1;
            }
        }
        if !life.flush_lag {
            run.maintain(&mut life);
        }
        // Copy-forward burst before the walk (M4-S15): the dead-ratio
        // arm normally; the pressure arm on seeded lives. Relocated
        // bytes flush with the next maintain round.
        let pressure = rng.next_u64().is_multiple_of(5);
        if !life.flush_lag {
            run.compact(&mut life, pressure, 8, &format!("life {life_index} pre-walk"));
            run.maintain(&mut life);
        }

        // The fuzzy hybrid walk, slice-interleaved with mutations. The
        // tail prefix covered by this checkpoint is truncated only if
        // the publish lands (cut-before-publish keeps it — D7).
        let cut_before_publish = life_index > 0 && rng.next_u64().is_multiple_of(4);
        let covered = run.tail.len();
        let w = life.table.begin_ckpt_walk(ckpt_id + 1).to_raw();
        let begin_lsn = Lsn::new(SegmentId(u32::try_from(life_index + 1).expect("small")), 64);
        let mut writer = SyncIckWriter::create_v2(
            disk.clone(),
            &shard,
            &CkptConfig::default(),
            0,
            ckpt_id + 1,
            begin_lsn,
            &[NS.0],
        )
        .expect("create ick");
        let mut cursor = 0u64;
        loop {
            let cold_before = life.table.space().counters().cold_resolves;
            let mut refs: Vec<(u64, u64)> = Vec::new();
            let mut images: Vec<(Vec<u8>, Vec<u8>, Option<ExtentRef>)> = Vec::new();
            cursor = life.table.ckpt_walk_slice(
                cursor,
                48,
                |hash, addr| refs.push((hash, addr.to_raw())),
                |parts| {
                    images.push((parts.key.to_vec(), parts.value.to_vec(), parts.extent_ref()));
                },
            );
            if life.table.space().counters().cold_resolves != cold_before {
                run.report.violations.push("walker resolved a cold address".into());
            }
            for (hash, addr) in refs {
                writer.append_ref(NS.0, w, hash, addr).expect("ref");
                run.report.refs_emitted += 1;
            }
            for (key, value, ext) in images {
                match ext {
                    // M4-S17 (ADR-0061 D2): resident extent records
                    // image as tag-9 — the reference, never the value.
                    Some(ext) => writer
                        .append(&RecordView::StringExtentRef {
                            ns: NS,
                            key: &key,
                            extent_id: ext.extent_id,
                            offset: ext.offset,
                            len: ext.len,
                        })
                        .expect("extent image"),
                    None => writer
                        .append(&RecordView::StringPostImage { ns: NS, key: &key, value: &value })
                        .expect("image"),
                }
                run.report.images_emitted += 1;
            }
            if cursor == 0 {
                break;
            }
            for _ in 0..4 {
                let key = seeded_key(&mut rng, scenario.keys, &pairs);
                let op = seeded_op(&mut rng);
                run.apply_op(&mut life, &key, op);
            }
            if !life.flush_lag && rng.next_u64().is_multiple_of(4) {
                run.maintain(&mut life);
            }
            // Mid-walk reconciliation (ADR-0093 D5): a resolution under
            // a pinned walk records the walk's own id as the origin's
            // stamp, so the ref this walk may have emitted for the twin
            // is covered by the origin until the next checkpoint lands.
            if rng.next_u64().is_multiple_of(3) {
                run.reconcile(&mut life, 1, &format!("life {life_index} mid-walk"));
            }
            // Mid-walk copy-forward attempt (ADR-0059 D9-1): the pin
            // pauses compaction — a mid-walk relocation would let this
            // walk emit a ref and an image for one key. The call
            // exercises the pause path; relocations must not move.
            if !life.flush_lag && rng.next_u64().is_multiple_of(3) {
                let before = run.report.relocations;
                run.compact(&mut life, false, 2, &format!("life {life_index} mid-walk"));
                if run.report.relocations != before {
                    run.report
                        .violations
                        .push(format!("life {life_index}: compaction ran under a pinned walk"));
                }
            }
        }
        // Live-set emission (M4-S14, ADR-0058 D3): one 0x04 section per
        // namespace, after its record/ref emission — recovered files'
        // lower bounds carry forward, this life's files serialize exact.
        for f in life.table.live_set().files() {
            writer
                .append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact)
                .expect("live set");
            run.report.live_entries_emitted += 1;
        }
        // Blob-reference emission (M4-S17, ADR-0061 D6): the reference
        // map's cold entries — the identity a released record's death
        // decrements by after recovery.
        for (addr, extent_id, len) in life.table.extent_ckpt_entries().collect::<Vec<_>>() {
            writer.append_blob_ref(NS.0, addr, extent_id, len).expect("blob ref");
        }
        writer.finish().expect("finish ick");
        life.table.end_ckpt_walk();

        if !cut_before_publish {
            ckpt_id += 1;
            // Retirement (M4-S15, ADR-0059 D3): mark before the section
            // builds — the manifest under construction excludes retiring
            // files, and this walk provably emitted no ref into them.
            life.table.retire_scan(ckpt_id, &life.flush);
            let section = life.table.tier_manifest(NS.0, &life.flush);
            if section.flushed < w {
                run.report.violations.push("publication does not cover the walk watermark".into());
            }
            write_manifest(
                &disk,
                &shard,
                &Manifest {
                    ckpt_id,
                    begin_lsn,
                    segments: vec![begin_lsn.segment],
                    tiers: vec![section],
                    key_hash_id: hasher.identity(),
                },
            )
            .expect("manifest swap");
            // The swap landed: retiring files leave the table, detach
            // from the catalog, and unlink — except when the seed drives
            // the swap ↔ unlink crash window, leaving them on disk for
            // the boot GC to prove D6-1 covers it.
            let leave_for_boot_gc = rng.next_u64().is_multiple_of(3);
            for id in life.table.commit_retirement() {
                let Some(meta) = life.flush.detach_sealed(id) else { continue };
                run.report.files_retired += 1;
                if leave_for_boot_gc {
                    run.report.unlinks_left_to_boot_gc += 1;
                } else {
                    run.pending_unlink.push(meta);
                }
            }
            // The pin-analog drain: no reads are in flight between ops
            // in this harness, so queued unlinks execute here.
            for meta in run.pending_unlink.drain(..) {
                unlink_tier_file(&disk, &meta).expect("sim unlink");
                run.report.files_unlinked += 1;
            }
            // WAL truncation (D7): drop exactly the covered prefix.
            run.tail.drain(..covered);
            // In-life dangling oracle: every model key still serves with
            // the retired files gone (a slot naming a detached file
            // surfaces here as a read failure, before any crash).
            run.audit(&life, &format!("life {life_index} post-retirement"));
            // Post-publish tail ops.
            for _ in 0..scenario.ops_per_phase / 4 {
                let key = seeded_key(&mut rng, scenario.keys, &pairs);
                let op = seeded_op(&mut rng);
                run.apply_op(&mut life, &key, op);
            }
            if !life.flush_lag {
                run.maintain(&mut life);
            }
        } else {
            run.report.cut_before_publish += 1;
        }

        // Tickets deliberately left open across the cut (ADR-0093 D5):
        // recovery must re-form them from the checkpoint/tail.
        run.report.shadow_open_at_cut += life.table.shadow_pending() as u64;
        // The cut: every un-fsynced byte tears (seeded physics).
        disk.power_cut(scenario.seed ^ (0xC07_0000 + life_index));
        drop(life);

        // ---- recovery (ADR-0057 D6) ----
        let manifest = match read_manifest(&disk, &shard) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                run.report.violations.push("published manifest lost".into());
                return run.report;
            }
            Err(e) => {
                run.report.violations.push(format!("manifest unreadable: {e}"));
                return run.report;
            }
        };
        let Some(tier) = manifest.tier_ns(NS.0).cloned() else {
            run.report.violations.push("manifest lost its tier section".into());
            return run.report;
        };
        let recovered = match recover_tiered_ns(
            disk.clone(),
            &tier,
            manifest.ckpt_id,
            flush_config(&shard),
            space_config(0),
            demote(),
            1024,
            hasher,
        ) {
            Ok(recovered) => recovered,
            Err(e) => {
                run.report.violations.push(format!("tier recovery failed: {e}"));
                return run.report;
            }
        };
        let table = std::cell::RefCell::new(recovered.table);
        let ick = shard.join(ick_file_name(manifest.ckpt_id));
        let loaded = read_ick_hybrid(
            &disk,
            &ick,
            IckReaderConfig::default(),
            |record| {
                match record {
                    RecordView::StringPostImage { key, value, .. } => {
                        table.borrow_mut().apply_image(key, value, hasher.hash(key)).expect("fits");
                    }
                    RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                        table
                            .borrow_mut()
                            .apply_extent_image(
                                key,
                                hasher.hash(key),
                                ExtentRef { extent_id, offset, len },
                            )
                            .expect("fits");
                    }
                    _ => {}
                }
                Ok::<(), std::convert::Infallible>(())
            },
            |section| {
                apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed)
                    .expect("refs inside the unit");
                Ok(())
            },
            |section| {
                apply_live_set_section(&mut table.borrow_mut(), &section);
                Ok(())
            },
            |section| {
                apply_blob_ref_section(&mut table.borrow_mut(), &section);
                Ok(())
            },
            |_| panic!("no index-sidecar sections in this image"),
        );
        if let Err(e) = loaded {
            run.report.violations.push(format!("checkpoint load failed: {e:?}"));
            return run.report;
        }
        let mut table = table.into_inner();
        table.set_shadow_enabled(true);
        // D4 tail replay: displacement markers pair with their mutation
        // — a bounded list since ADR-0059 D9 (origin markers stack atop
        // the ordinary one; each removal is exact-pair).
        let mut rest: &[u8] = &run.tail;
        let mut pending: Vec<u64> = Vec::new();
        while !rest.is_empty() {
            let (record, consumed) = decode_record(rest).expect("tail records decode");
            match record {
                RecordView::ColdDisplace { old_addr, .. } => {
                    pending.push(old_addr);
                    assert!(pending.len() <= 4, "displace register exceeds the D9 bound");
                }
                RecordView::StringPostImage { key, value, .. } => {
                    let hash = table.hash_key(key);
                    for old in pending.drain(..) {
                        table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                    }
                    table.apply_image(key, value, hash).expect("fits");
                }
                RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                    let hash = table.hash_key(key);
                    for old in pending.drain(..) {
                        table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                    }
                    table
                        .apply_extent_image(key, hash, ExtentRef { extent_id, offset, len })
                        .expect("fits");
                }
                RecordView::Delete { key, .. } => {
                    let hash = table.hash_key(key);
                    for old in pending.drain(..) {
                        table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                    }
                    table.apply_delete(key, hash);
                }
                other => {
                    run.report.violations.push(format!("modeled tail carries {other:?}"));
                    return run.report;
                }
            }
            rest = &rest[consumed..];
        }
        // M4.5-S37 (ADR-0093 D5/A4): the shadow ticket set is rebuilt
        // from the finished index at recovery-complete — the plane's
        // `finish_tier_replay` does this; the harness plays it here,
        // including the settle list: the slots the rebuild cannot pair
        // by construction (two RAM keys with one hash beside a cold
        // twin) or beyond the cap, read and settled by their full key
        // before the life serves.
        let settled_at_boot = &mut run.report.shadow_settled_at_boot;
        if let Err(err) = table.rebuild_shadow_tickets(|slot| -> Result<Vec<u8>, String> {
            let image = read_cold_record(&disk, &recovered.flush, slot.cold.to_raw())
                .ok_or_else(|| "unreadable while its slot is live".to_owned())?;
            *settled_at_boot += 1;
            Ok(image)
        }) {
            run.report.violations.push(format!("life {life_index}: {err}"));
            return run.report;
        }
        // The M4-S14 oracle (ADR-0058 D4): by replay-complete, every
        // recovered file's slot count equals the index's ground truth,
        // and byte counters never over-count dead — asserted per life,
        // folded into the determinism trace. The ground-truth walk uses
        // the id the next real checkpoint would (monotone past boot).
        check_live_set(&mut table, &tier, manifest.ckpt_id + 1, &mut run.report, life_index);
        table.set_blob_config(BlobConfig { threshold_bytes: BLOB_THRESHOLD, max_bytes: 1 << 20 });
        life = Life {
            table,
            flush: recovered.flush,
            ring: StagingRing::new(StagingConfig::default()),
            flush_lag: false,
        };
        // The D5 rebuild: every pair the checkpoint and the tail restored
        // is a ticket again; the never-none audit runs with them open
        // (the winner serves), then they reconcile and the cardinality
        // oracle closes the life.
        run.report.shadow_reformed += life.table.shadow_pending() as u64;
        run.audit(&life, &format!("life {life_index} (tickets open)"));
        run.audit_len_after_drain(&mut life, &format!("life {life_index} (tickets open)"));
        run.reconcile_all(&mut life, &format!("life {life_index} post-recovery"));
        run.audit_cardinality(&life, &format!("life {life_index}"));
        // The M4-S17 refcount reconciliation oracle + the boot sweep
        // (ADR-0061 D6): exact counts, orphans reclaimed, disk equals
        // the live set. After reconciliation: an open ticket's twin may
        // be a blob record whose extent stays referenced — legitimately
        // live until the twin is verified — while the model already
        // holds the key's inline winner (ADR-0093 D4: the death, and the
        // refcount decrement, happen at the verdict).
        check_blob_refs(&mut run, &mut life, &recovered.extents_listed, life_index);
        run.audit(&life, &format!("life {life_index}"));
        run.report.trace_hash = hash64(
            &[
                run.report.refs_emitted.to_le_bytes(),
                run.report.images_emitted.to_le_bytes(),
                run.report.tail_records.to_le_bytes(),
                run.report.relocations.to_le_bytes(),
                run.report.files_retired.to_le_bytes(),
                run.report.files_unlinked.to_le_bytes(),
                run.report.blobs_written.to_le_bytes(),
                run.report.blob_extents_reclaimed.to_le_bytes(),
                life.table.cold_floor().to_le_bytes(),
                run.report.shadow_opened.to_le_bytes(),
                run.report.shadow_reformed.to_le_bytes(),
                run.report.shadow_same_key.to_le_bytes(),
                run.report.shadow_collision.to_le_bytes(),
                run.report.shadow_settled_at_boot.to_le_bytes(),
                run.report.shadow_collide_ops.to_le_bytes(),
            ]
            .concat(),
            run.report.trace_hash,
        );
    }
    run.report
}
