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
    AddressSpaceConfig, BlobConfig, CompactionWork, DemotionConfig, ExtentRef, LogicalAddr,
    TieredLookup, TieredTable, apply_blob_ref_section, apply_live_set_section, apply_ref_section,
    recover_tiered_ns,
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
    encoded_len: usize,
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

fn tiered_table(origin: u64) -> TieredTable {
    let mut table = TieredTable::new(space_config(origin), demote(), 1024).expect("ring");
    table.set_blob_config(BlobConfig { threshold_bytes: BLOB_THRESHOLD, max_bytes: 1 << 20 });
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
        let hash = TieredTable::hash_key(key);
        let displaced = match life.table.lookup(key, hash, &[]) {
            TieredLookup::Ram(addr) => {
                let parts = life.table.record(addr);
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Cold(addr) => {
                let expect = self.model.get(key).expect("cold candidate implies model entry");
                let bytes = read_cold(&self.disk, &life.flush, addr.to_raw(), expect.encoded_len)
                    .expect("cold record readable");
                let parts = TieredTable::decode_record(&bytes);
                assert_eq!(parts.key, key, "no full-hash collisions at this corpus size");
                Some((addr, parts.encoded_len, parts.version))
            }
            TieredLookup::Miss => None,
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
                let encoded_len = match life.table.lookup(key, hash, &[]) {
                    TieredLookup::Ram(addr) => life.table.record(addr).encoded_len,
                    _ => unreachable!("a fresh write is RAM-resident"),
                };
                self.model.insert(key.to_vec(), Expect { value, encoded_len, extent: None });
            }
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
                let encoded_len = match life.table.lookup(key, hash, &[]) {
                    TieredLookup::Ram(addr) => life.table.record(addr).encoded_len,
                    _ => unreachable!("a fresh write is RAM-resident"),
                };
                self.model
                    .insert(key.to_vec(), Expect { value, encoded_len, extent: Some(extent_id.0) });
            }
            Op::Del => {
                if let Some((addr, len, _)) = displaced {
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

    /// The oracle: every model key serves its exact bytes; every other
    /// key misses. Content only — versions are per-life (D3), addresses
    /// are per-life (§3.1).
    fn audit(&mut self, life: &Life, when: &str) {
        for (key, expect) in &self.model {
            let hash = TieredTable::hash_key(key);
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
                        match read_cold(&self.disk, &life.flush, addr.to_raw(), expect.encoded_len)
                        {
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
    match rng.next_u64() % 6 {
        0 => Op::Del,
        // The blob leg (M4-S17): values at or above the threshold, small
        // enough that the extent lifecycle churns at DST scale.
        1 => {
            let len = BLOB_THRESHOLD as usize + (rng.next_u64() % 300) as usize;
            Op::SetBlob(vec![(rng.next_u64() % 251) as u8; len])
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
        table: tiered_table(0),
        flush: TierFlush::new(disk.clone(), flush_config(&shard), 0),
        ring: StagingRing::new(StagingConfig::default()),
        flush_lag: false,
    };
    let mut ckpt_id = 0u64;

    for life_index in 0..scenario.lives {
        run.report.lives += 1;
        life.flush_lag = life_index > 0 && rng.next_u64().is_multiple_of(3);
        if life.flush_lag {
            run.report.flush_lag_lives += 1;
        }
        // Phase A: mutations (into the tail — everything since the last
        // durable publish replays).
        for _ in 0..scenario.ops_per_phase {
            let idx = rng.next_u64() % scenario.keys;
            let key = format!("rec:{idx:05}").into_bytes();
            let op = seeded_op(&mut rng);
            run.apply_op(&mut life, &key, op);
            if !life.flush_lag && rng.next_u64().is_multiple_of(32) {
                run.maintain(&mut life);
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
                let idx = rng.next_u64() % scenario.keys;
                let key = format!("rec:{idx:05}").into_bytes();
                let op = seeded_op(&mut rng);
                run.apply_op(&mut life, &key, op);
            }
            if !life.flush_lag && rng.next_u64().is_multiple_of(4) {
                run.maintain(&mut life);
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
                let idx = rng.next_u64() % scenario.keys;
                let key = format!("rec:{idx:05}").into_bytes();
                let op = seeded_op(&mut rng);
                run.apply_op(&mut life, &key, op);
            }
            if !life.flush_lag {
                run.maintain(&mut life);
            }
        } else {
            run.report.cut_before_publish += 1;
        }

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
                        table
                            .borrow_mut()
                            .apply_image(key, value, TieredTable::hash_key(key))
                            .expect("fits");
                    }
                    RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                        table
                            .borrow_mut()
                            .apply_extent_image(
                                key,
                                TieredTable::hash_key(key),
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
        );
        if let Err(e) = loaded {
            run.report.violations.push(format!("checkpoint load failed: {e:?}"));
            return run.report;
        }
        let mut table = table.into_inner();
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
                    let hash = TieredTable::hash_key(key);
                    for old in pending.drain(..) {
                        table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                    }
                    table.apply_image(key, value, hash).expect("fits");
                }
                RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                    let hash = TieredTable::hash_key(key);
                    for old in pending.drain(..) {
                        table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                    }
                    table
                        .apply_extent_image(key, hash, ExtentRef { extent_id, offset, len })
                        .expect("fits");
                }
                RecordView::Delete { key, .. } => {
                    let hash = TieredTable::hash_key(key);
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
        // The M4-S17 refcount reconciliation oracle + the boot sweep
        // (ADR-0061 D6): exact counts, orphans reclaimed, disk equals
        // the live set.
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
            ]
            .concat(),
            run.report.trace_hash,
        );
    }
    run.report
}
