//! M4-S17 — blob-extent reference counting (plan AC 3, ADR-0061): random
//! create/overwrite/delete storms over mixed inline + out-of-line values,
//! with real (MemFs) extent files, the real flush pipeline demoting the
//! referencing records to cold, copy-forward relocations moving
//! references, and the stamped reclaim queue actually unlinking files.
//! Oracles, checked against a shadow model that never trusts the
//! machinery under test:
//!
//! - **refcount exactness:** every live blob key's address maps to
//!   exactly its extent with refcount 1; the live-extent set equals the
//!   model's, always;
//! - **zero early frees:** an extent handed to the unlink slice is never
//!   model-live, and a model-live extent's file is always on disk;
//! - **zero leaks:** at quiescence the extent directory equals the
//!   model's live set exactly — every dead extent's disk came back;
//! - **recovery:** checkpoint (tag-9 images + 0x03 refs + 0x05 map) +
//!   tail replay rebuild the refcounts exactly; the orphan sweep
//!   reclaims unreferenced files; every live blob value reads back
//!   byte-exact through the CRC-verified extent reader (content, never
//!   addresses — §3.1).

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::blob::{ExtentId, ExtentWriter, list_extent_ids, open_extent, unlink_extent_file};
use inf_log::fs::mem::MemFs;
use inf_log::tier::{TIER_FRAME_BYTES, tier_extract, tier_frame_offset, tier_frame_span};
use inf_log::{
    CkptConfig, Lsn, MutationEffect, NsId, RecordView, SegmentId, StagingConfig, StagingRing,
    SyncIckWriter, TierFlush, TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid,
    write_manifest,
};
use inf_store::{
    AddressSpaceConfig, BlobConfig, CompactionWork, DemotionConfig, LogicalAddr, TieredLookup,
    TieredTable, apply_blob_ref_section, apply_live_set_section, apply_ref_section,
    recover_tiered_ns,
};
use proptest::prelude::*;

const NS: NsId = NsId(51);
const SHARD: &str = "shard-0";
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
const FILE_CAPACITY: u64 = 64 << 10;
/// Tiny threshold so the lifecycle is exercised at test scale — the
/// production default (16 MiB) is the u24 ceiling; the mechanism is
/// identical (ADR-0061 D1: construction parameter).
const THRESHOLD: u32 = 64;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// Deterministic value for (key id, generation): recovery regenerates it
/// for the content oracle instead of holding every value in RAM.
fn value_for(id: u64, generation: u64, blob: bool) -> Vec<u8> {
    let len = if blob {
        THRESHOLD as usize + ((id.wrapping_mul(31) ^ generation) % 700) as usize
    } else {
        8 + ((id ^ generation) % 40) as usize
    };
    (0..len).map(|i| (i as u64 ^ id.wrapping_mul(7) ^ generation) as u8).collect()
}

#[derive(Clone, Debug)]
struct Entry {
    addr: u64,
    len: usize,
    version: u32,
    generation: u64,
    /// The referenced extent, when the value is out of line.
    extent_id: Option<u64>,
}

struct Rig {
    fs: MemFs,
    table: TieredTable,
    flush: TierFlush<MemFs>,
    ring: StagingRing,
    /// Encoded record-v1 bytes of everything staged — the modeled WAL
    /// tail the recovery test replays (the `tiered_recovery.rs` shape).
    tail: Vec<u8>,
    model: BTreeMap<u64, Entry>,
    ckpt_id: u64,
}

impl Rig {
    fn new() -> Rig {
        Rig::with_budget(BUDGET)
    }

    fn with_budget(budget: u64) -> Rig {
        let demote = DemotionConfig::for_budget(budget, PAGE);
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            2048,
        )
        .expect("ring");
        table.set_blob_config(BlobConfig { threshold_bytes: THRESHOLD, max_bytes: 1 << 20 });
        let fs = MemFs::new();
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new(SHARD).to_path_buf(),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: FILE_CAPACITY,
                slice_bytes: PAGE,
            },
            0,
        );
        Rig {
            fs,
            table,
            flush,
            ring: StagingRing::new(StagingConfig::default()),
            tail: Vec::new(),
            model: BTreeMap::new(),
            ckpt_id: 0,
        }
    }

    fn key(id: u64) -> Vec<u8> {
        format!("k:{id:05}").into_bytes()
    }

    /// Stages one effect into the (recycled) ring — so `stage_wal`'s
    /// epoch stamping runs through the production path — and appends its
    /// record encoding to the modeled tail.
    fn stage(&mut self, effect: &MutationEffect<'_>) {
        self.stage_epoch_only(effect);
        effect.record().encode_into(&mut self.tail);
    }

    /// The staging half alone (no tail record) — quiesce stamping.
    fn stage_epoch_only(&mut self, effect: &MutationEffect<'_>) {
        if self.table.stage_wal(&mut self.ring, effect).is_err() {
            // The modeled tail is the durable record; the ring is only
            // the staging admission — recycle it wholesale.
            self.ring = StagingRing::new(StagingConfig::default());
            self.table.stage_wal(&mut self.ring, effect).expect("a fresh ring has room");
        }
    }

    /// Encodes the ADR-0059 D9 origin markers plus the ordinary
    /// displacement marker for a mutation displacing `addr` (markers are
    /// record-vocabulary, not effects — the harness tiers encode them
    /// directly, the `tiered_recovery.rs` shape).
    fn stage_displacement(&mut self, hash: u64, addr: LogicalAddr) {
        for (origin, _stamp) in self.table.take_displacement_origins(hash, addr) {
            RecordView::ColdDisplace { ns: NS, old_addr: origin }.encode_into(&mut self.tail);
        }
        RecordView::ColdDisplace { ns: NS, old_addr: addr.to_raw() }.encode_into(&mut self.tail);
    }

    /// SET routed by the blob threshold (the plane's contract): inline
    /// under it, extent write → fdatasync → sealed token → reference at
    /// or above it (ADR-0061 D3 — the token is the ordering proof).
    fn set(&mut self, id: u64, generation: u64, blob: bool) {
        let key = Self::key(id);
        let hash = TieredTable::hash_key(&key);
        let value = value_for(id, generation, blob);
        let old = self.model.get(&id).cloned();
        if let Some(old) = &old {
            let addr = LogicalAddr::from_raw(old.addr).expect("48-bit");
            self.stage_displacement(hash, addr);
        }
        let placed = if blob {
            let extent_id = ExtentId(self.table.allocate_extent_id());
            let mut w = ExtentWriter::create(
                &self.fs,
                Path::new(SHARD),
                extent_id,
                0,
                NS,
                value.len() as u64,
                TierIoMode::Buffered,
            )
            .expect("create extent");
            for chunk in value.chunks(37) {
                w.append_chunk(chunk).expect("chunk");
            }
            let sealed = w.finish().expect("finish");
            self.table.note_blob_bytes(sealed.device_bytes());
            self.stage(&MutationEffect::StringSetExtent {
                ns: NS,
                key: &key,
                extent_id: sealed.extent_id().0,
                offset: 0,
                len: sealed.data_len(),
            });
            let result = match &old {
                Some(o) => self.table.update_extent(
                    &key,
                    hash,
                    &sealed,
                    LogicalAddr::from_raw(o.addr).expect("48-bit"),
                    o.len,
                    o.version,
                ),
                None => self.table.insert_extent(&key, hash, &sealed),
            };
            match result {
                Ok(addr) => addr,
                Err(_) => {
                    self.drain();
                    match &old {
                        Some(o) => self
                            .table
                            .update_extent(
                                &key,
                                hash,
                                &sealed,
                                LogicalAddr::from_raw(o.addr).expect("48-bit"),
                                o.len,
                                o.version,
                            )
                            .expect("fits after maintain"),
                        None => {
                            self.table.insert_extent(&key, hash, &sealed).expect("fits after drain")
                        }
                    }
                }
            }
        } else {
            self.stage(&MutationEffect::StringSet { ns: NS, key: &key, value: &value });
            let result = match &old {
                Some(o) => self.table.update(
                    &key,
                    &value,
                    hash,
                    LogicalAddr::from_raw(o.addr).expect("48-bit"),
                    o.len,
                    o.version,
                ),
                None => self.table.insert(&key, &value, hash),
            };
            match result {
                Ok(addr) => addr,
                Err(_) => {
                    self.drain();
                    match &old {
                        Some(o) => self
                            .table
                            .update(
                                &key,
                                &value,
                                hash,
                                LogicalAddr::from_raw(o.addr).expect("48-bit"),
                                o.len,
                                o.version,
                            )
                            .expect("fits after maintain"),
                        None => self.table.insert(&key, &value, hash).expect("fits after drain"),
                    }
                }
            }
        };
        let TieredLookup::Ram(addr) = self.table.lookup(&key, hash, &[]) else {
            unreachable!("a fresh write is RAM-resident");
        };
        assert_eq!(addr, placed);
        let parts = self.table.record(addr);
        let extent_id = parts.extent_ref().map(|e| e.extent_id);
        assert_eq!(extent_id.is_some(), blob, "routing wrote the right record kind");
        self.model.insert(
            id,
            Entry {
                addr: placed.to_raw(),
                len: parts.encoded_len,
                version: parts.version,
                generation,
                extent_id,
            },
        );
    }

    /// DEL: index + accounting only (§3.3) — the model supplies the
    /// length; a cold record's extent releases through the map alone.
    fn del(&mut self, id: u64) {
        let Some(entry) = self.model.remove(&id) else { return };
        let key = Self::key(id);
        let hash = TieredTable::hash_key(&key);
        let addr = LogicalAddr::from_raw(entry.addr).expect("48-bit");
        self.stage_displacement(hash, addr);
        self.stage(&MutationEffect::Delete { ns: NS, key: &key });
        self.table.delete(hash, addr, entry.len);
    }

    /// Seal → flush → release to quiescence.
    fn drain(&mut self) {
        loop {
            let sealed = self.table.seal_slice();
            let flushed = self.table.flush_slice(&mut self.flush).expect("flush slice");
            let released = self.table.release_slice();
            if sealed + released + flushed.appended_bytes + u64::from(flushed.gaps_crossed) == 0 {
                break;
            }
        }
    }

    /// One compaction slice (relocations move references — the storm's
    /// relocation coverage). Model addresses update from the index.
    fn compact_slice(&mut self, budget: u64) {
        let mut spent = 0u64;
        while spent < budget {
            let work = self.table.compaction_work(&self.flush, false, budget - spent);
            let CompactionWork::Read { file_id, addr, len } = work else { break };
            let Some(bytes) = self.read_chunk(file_id, addr, len) else { break };
            let applied = self.table.compaction_apply(file_id, addr, &bytes);
            spent += applied.consumed.max(applied.need).max(1);
            if applied.stalled {
                break;
            }
        }
        // Relocations moved records; refresh model addresses. Blob
        // records refresh exactly through the reference map (the moved
        // entry is the machinery under test — asserted, not assumed);
        // inline records refresh through lookup (the full-hash sidecar
        // makes the candidate the key's own slot at this corpus size —
        // the existing suites' documented assumption).
        let references: Vec<(u64, u64, u64)> = self.table.extent_references().collect();
        let entries: Vec<u64> = self.model.keys().copied().collect();
        for id in entries {
            let key = Self::key(id);
            let hash = TieredTable::hash_key(&key);
            let entry = self.model.get_mut(&id).expect("just listed");
            if let Some(ext) = entry.extent_id {
                let (addr, _, _) = references
                    .iter()
                    .find(|&&(_, e, _)| e == ext)
                    .copied()
                    .expect("a live blob key's extent stays mapped across relocation");
                entry.addr = addr;
            } else {
                match self.table.lookup(&key, hash, &[]) {
                    TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => {
                        entry.addr = addr.to_raw();
                    }
                    TieredLookup::Miss => panic!("live inline key {id} lost by relocation"),
                }
            }
        }
    }

    /// A compaction scan chunk (the S08 cold read, modeled synchronously
    /// — the `tiered_write_amp.rs` helper).
    fn read_chunk(&self, file_id: u32, addr: LogicalAddr, len: u64) -> Option<Vec<u8>> {
        let meta = self.flush.sealed().iter().find(|m| m.id == file_id)?.clone();
        let image = self.fs.contents(&meta.path)?;
        let len = usize::try_from(len).expect("fits");
        let (first, count, skip) = tier_frame_span(addr.to_raw() - meta.base.to_raw(), len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// Drives the reclaim slice at full durability (everything staged is
    /// treated as committed — the quiesce shape) and unlinks. Asserts
    /// zero early frees: no handed-out candidate is model-live.
    fn reclaim(&mut self) {
        // Trailing parked deaths stamp at the next staging — give them
        // one (the D5 disclosure: an idle cell defers to its next
        // write). A checkpoint marker is a legitimate staged non-state
        // record, so the modeled tail stays a pure mutation stream.
        self.stage_epoch_only(&MutationEffect::CkptBegin { ckpt_id: u64::MAX });
        loop {
            let work = self.table.extent_reclaim_work(self.table.wal_epoch(), 8);
            if work.is_empty() {
                break;
            }
            for id in work {
                assert!(
                    !self.model.values().any(|e| e.extent_id == Some(id)),
                    "early free: extent {id} is model-live"
                );
                unlink_extent_file(&self.fs, Path::new(SHARD), ExtentId(id)).expect("unlink");
                self.table.extent_reclaim_done(id);
            }
        }
    }

    /// The refcount oracle: live extents equal the model's exactly, each
    /// at refcount 1, each mapped at its record's current address with
    /// its regenerated value length, each with its file still on disk.
    fn assert_refcounts(&self) {
        let mut model_live: Vec<u64> = Vec::new();
        for (id, entry) in &self.model {
            let Some(ext) = entry.extent_id else { continue };
            model_live.push(ext);
            let declared = value_for(*id, entry.generation, true).len() as u64;
            assert_eq!(self.table.extent_refcount(ext), 1, "extent {ext} refcount");
            assert_eq!(
                self.table.extent_reference_at(LogicalAddr::from_raw(entry.addr).expect("48-bit")),
                Some((ext, declared)),
                "reference map entry for extent {ext} (key {id})"
            );
        }
        model_live.sort_unstable();
        let stats = self.table.extent_stats();
        assert_eq!(stats.live, model_live.len() as u64, "live extent count");
        let on_disk = list_extent_ids(&self.fs, Path::new(SHARD)).expect("listing");
        for ext in &model_live {
            assert!(on_disk.contains(&ExtentId(*ext)), "extent {ext} unlinked while live");
        }
    }
}

#[test]
fn blob_lifecycle_refcounts_are_exact_and_reclaim_is_complete() {
    // Goal (AC 3): a deterministic mixed storm — inline + blob writes,
    // overwrites both directions, deletes, demotion to cold, compaction
    // relocations, retirement, reclaim — keeps refcounts exact against
    // the model at every checkpointed step, never frees early, and
    // leaks nothing at quiescence.
    let mut rig = Rig::new();
    let mut seed = 0x517_B10Bu64;
    let keys = 96u64;
    let mut generation = 0u64;
    for step in 0..6_000u64 {
        generation += 1;
        let id = seeded(&mut seed) % keys;
        match seeded(&mut seed) % 10 {
            0..=3 => rig.set(id, generation, true),
            4..=6 => rig.set(id, generation, false),
            7 => rig.del(id),
            8 => rig.drain(),
            _ => {
                if step % 40 == 0 {
                    rig.drain();
                    rig.compact_slice(1 << 20);
                }
            }
        }
        if step % 97 == 0 {
            rig.assert_refcounts();
        }
    }
    rig.drain();
    rig.reclaim();
    rig.assert_refcounts();
    // Zero leaks: at quiescence the directory holds exactly the live set.
    let mut model_live: Vec<ExtentId> =
        rig.model.values().filter_map(|e| e.extent_id).map(ExtentId).collect();
    model_live.sort_unstable();
    model_live.dedup();
    let on_disk = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");
    assert_eq!(on_disk, model_live, "extent directory equals the model's live set");
    assert!(rig.table.extent_stats().reclaimed > 0, "the storm exercised reclaim");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]
    /// Randomized op sequences reconcile exactly (the proptest arm of
    /// AC 3 — seeds beyond the deterministic storm's).
    #[test]
    fn blob_refcounts_match_the_model(ops in proptest::collection::vec((0u8..10, 0u64..48), 1..400)) {
        let mut rig = Rig::new();
        let mut generation = 0u64;
        for (op, id) in ops {
            generation += 1;
            match op {
                0..=3 => rig.set(id, generation, true),
                4..=6 => rig.set(id, generation, false),
                7 => rig.del(id),
                8 => rig.drain(),
                _ => { rig.drain(); rig.compact_slice(1 << 18); }
            }
        }
        rig.drain();
        rig.reclaim();
        rig.assert_refcounts();
        let mut model_live: Vec<ExtentId> =
            rig.model.values().filter_map(|e| e.extent_id).map(ExtentId).collect();
        model_live.sort_unstable();
        let on_disk = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");
        prop_assert_eq!(on_disk, model_live);
    }
}

#[test]
fn recovery_rebuilds_refcounts_serves_content_and_sweeps_orphans() {
    // Goal: the full unit — hybrid checkpoint (tag-9 images, 0x03 refs,
    // 0x04 live set, 0x05 blob refs) + MANIFEST v2 + modeled tail —
    // crashes and recovers with exact refcounts, byte-exact blob
    // content through the CRC-verified reader, and the orphan (extent
    // durable, referencing frame lost — the AC 1 cut) reclaimed by the
    // sweep, never served.
    // A small budget so demotion pressure is reachable at test volume.
    let mut rig = Rig::with_budget(128 << 10);
    let mut generation = 0u64;
    // Phase A: one-shot keys first — untouched afterwards, they age to
    // cold under the later volume (the emergent hot/cold filter, §9);
    // some blob records must be genuinely cold or the 0x05 section (the
    // story's recovery crux) would go untested — the assert below
    // refuses a silently RAM-resident corpus.
    for id in 0..120u64 {
        generation += 1;
        rig.set(id, generation, id % 3 != 2);
    }
    rig.drain();
    // Volume on a disjoint key range pushes the watermarks past them.
    for _round in 0..4u64 {
        for id in 1000..1300u64 {
            generation += 1;
            rig.set(id, generation, id % 5 == 0);
            if id % 16 == 15 {
                rig.drain();
            }
        }
    }
    rig.drain();
    // Some churn: overwrites across kinds, deletes, a compaction pass.
    for id in [3u64, 6, 9, 12] {
        generation += 1;
        rig.set(id, generation, id % 2 == 0);
    }
    rig.del(15);
    rig.del(21);
    rig.drain();
    rig.compact_slice(1 << 20);
    rig.drain();

    // The checkpoint: walk → images (tag 9 for extent records) + refs,
    // then 0x04 + 0x05 at walk end, then the manifest swap.
    rig.ckpt_id += 1;
    let ckpt_id = rig.ckpt_id;
    let w = rig.table.begin_ckpt_walk(ckpt_id).to_raw();
    let begin_lsn = Lsn::new(SegmentId(1), 64);
    let mut writer = SyncIckWriter::create_v2(
        rig.fs.clone(),
        Path::new(SHARD),
        &CkptConfig::default(),
        0,
        ckpt_id,
        begin_lsn,
        &[NS.0],
    )
    .expect("create ick");
    let mut cursor = 0u64;
    loop {
        let mut refs: Vec<(u64, u64)> = Vec::new();
        let mut images: Vec<(Vec<u8>, Vec<u8>, Option<inf_store::ExtentRef>)> = Vec::new();
        cursor = rig.table.ckpt_walk_slice(
            cursor,
            64,
            |hash, addr| refs.push((hash, addr.to_raw())),
            |parts| images.push((parts.key.to_vec(), parts.value.to_vec(), parts.extent_ref())),
        );
        for (hash, addr) in refs {
            writer.append_ref(NS.0, w, hash, addr).expect("ref");
        }
        for (key, value, ext) in images {
            match ext {
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
        }
        if cursor == 0 {
            break;
        }
    }
    for f in rig.table.live_set().files().to_vec() {
        writer.append_live_set(NS.0, f.id, f.data_len, f.dead_bytes, f.byte_exact).expect("0x04");
    }
    let blob_entries: Vec<(u64, u64, u64)> = rig.table.extent_ckpt_entries().collect();
    assert!(!blob_entries.is_empty(), "the corpus demoted blob records below the watermark");
    for (addr, extent_id, len) in &blob_entries {
        writer.append_blob_ref(NS.0, *addr, *extent_id, *len).expect("0x05");
    }
    writer.finish().expect("finish ick");
    rig.table.end_ckpt_walk();
    let manifest = inf_log::Manifest {
        ckpt_id,
        begin_lsn,
        segments: vec![SegmentId(1)],
        tiers: vec![rig.table.tier_manifest(NS.0, &rig.flush)],
    };
    write_manifest(&rig.fs, Path::new(SHARD), &manifest).expect("manifest swap");
    // Recovery replays from begin-LSN: everything before the walk is
    // covered by the checkpoint. No mutation interleaved this walk, so
    // the post-swap tail is exactly the replay window.
    rig.tail.clear();

    // Post-checkpoint tail: more churn the replay must reproduce.
    for id in [2u64, 5, 30] {
        generation += 1;
        rig.set(id, generation, true);
    }
    rig.del(9);
    // The AC 1 cut, store-level: an extent durable on disk whose
    // referencing frame never made the log — an orphan, never a ref.
    let orphan_id = ExtentId(rig.table.allocate_extent_id());
    let mut orphan = ExtentWriter::create(
        &rig.fs,
        Path::new(SHARD),
        orphan_id,
        0,
        NS,
        128,
        TierIoMode::Buffered,
    )
    .expect("orphan create");
    orphan.append_chunk(&[0xAB; 128]).expect("orphan bytes");
    let _sealed_but_never_referenced = orphan.finish().expect("orphan fsync");

    let model = rig.model.clone();
    let tail = rig.tail.clone();
    let fs = rig.fs.clone();
    drop(rig); // crash

    // Boot: manifest → tier files + extent listing → checkpoint → tail.
    let stored = inf_log::read_manifest(&fs, Path::new(SHARD)).expect("manifest").expect("present");
    let tier = stored.tier_ns(NS.0).expect("tier section").clone();
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        stored.ckpt_id,
        TierFlushConfig {
            shard_dir: Path::new(SHARD).to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: FILE_CAPACITY,
            slice_bytes: PAGE,
        },
        AddressSpaceConfig {
            reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
            page_bytes: PAGE as usize,
            life_origin: LogicalAddr::ZERO,
        },
        demote,
        2048,
    )
    .expect("recovery");
    assert!(
        recovered.extents_listed.contains(&orphan_id.0),
        "the listing collected the orphan (names only)"
    );
    let table = std::cell::RefCell::new(recovered.table);
    let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(stored.ckpt_id));
    read_ick_hybrid(
        &fs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
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
                            inf_store::ExtentRef { extent_id, offset, len },
                        )
                        .expect("fits");
                }
                _ => panic!("unexpected image class in this checkpoint"),
            }
            Ok::<(), std::convert::Infallible>(())
        },
        |section| {
            apply_ref_section(&mut table.borrow_mut(), &section, tier.flushed).expect("refs");
            Ok(())
        },
        |section| {
            apply_live_set_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
        |section| {
            assert_eq!(section.ns, NS.0);
            apply_blob_ref_section(&mut table.borrow_mut(), &section);
            Ok(())
        },
    )
    .expect("hybrid load");
    let mut table = table.into_inner();
    replay_tail(&mut table, &tail);
    // The sweep: orphans reclaim through ordinary slices; nothing live
    // is ever handed out.
    table.extent_sweep_seed(&recovered.extents_listed);
    let mut swept: Vec<u64> = Vec::new();
    loop {
        let work = table.extent_reclaim_work(0, 8);
        if work.is_empty() {
            break;
        }
        for id in work {
            assert!(
                !model.values().any(|e| e.extent_id == Some(id)),
                "sweep handed out live extent {id}"
            );
            unlink_extent_file(&fs, Path::new(SHARD), ExtentId(id)).expect("unlink");
            table.extent_reclaim_done(id);
            swept.push(id);
        }
    }
    assert!(swept.contains(&orphan_id.0), "the orphan was reclaimed");

    // Refcount reconciliation + content: every model-live blob key's
    // extent exists, counts exactly 1, and serves its exact bytes
    // through the CRC-verified reader.
    let on_disk = list_extent_ids(&fs, Path::new(SHARD)).expect("listing");
    let mut model_live: Vec<ExtentId> = Vec::new();
    for (id, entry) in &model {
        let Some(ext) = entry.extent_id else { continue };
        model_live.push(ExtentId(ext));
        assert_eq!(table.extent_refcount(ext), 1, "post-recovery refcount of extent {ext}");
        assert!(on_disk.contains(&ExtentId(ext)), "live extent {ext} present");
        let expected = value_for(*id, entry.generation, true);
        let mut reader =
            open_extent(&fs, Path::new(SHARD), ExtentId(ext), TierIoMode::Buffered).expect("open");
        let mut got = Vec::new();
        let mut offset = 0u64;
        while (offset as usize) < expected.len() {
            let take = (expected.len() - offset as usize).min(1000);
            reader.read(offset, take, &mut got).expect("io").expect("crc");
            offset += take as u64;
        }
        assert_eq!(got, expected, "blob content for key {id} (content, never addresses)");
    }
    model_live.sort_unstable();
    let on_disk = list_extent_ids(&fs, Path::new(SHARD)).expect("listing");
    assert_eq!(on_disk, model_live, "post-sweep directory equals the live set");
}

#[test]
fn reclaim_gates_on_the_deaths_durability() {
    // Goal (ADR-0061 D5): an extent whose killing record is not yet
    // durable never unlinks — the everysec dangling-reference window is
    // closed by the epoch stamp, and the file stays until the gate.
    let mut rig = Rig::new();
    rig.set(1, 1, true);
    let ext = rig.model.get(&1).expect("live").extent_id.expect("blob");
    rig.del(1);
    // The death parked; it stamps at the *next* successful staging (the
    // conservative direction under stage-then-apply — deferral, never
    // early release; ADR-0061 D5's disclosure). Before any stamp,
    // nothing reclaims at any epoch.
    assert!(
        rig.table.extent_reclaim_work(u64::MAX, 8).is_empty(),
        "an unstamped death never reclaims"
    );
    rig.stage_epoch_only(&MutationEffect::CkptBegin { ckpt_id: u64::MAX });
    let stamped = rig.table.wal_epoch();
    // Stamped, but the plane's durability has not reached that epoch.
    assert!(
        rig.table.extent_reclaim_work(stamped - 1, 8).is_empty(),
        "an un-durable death never reclaims"
    );
    let on_disk = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");
    assert!(on_disk.contains(&ExtentId(ext)), "the file waits for the gate");
    // Durability reaches the death: the extent reclaims and the disk
    // returns.
    let work = rig.table.extent_reclaim_work(stamped, 8);
    assert_eq!(work, vec![ext]);
    unlink_extent_file(&rig.fs, Path::new(SHARD), ExtentId(ext)).expect("unlink");
    rig.table.extent_reclaim_done(ext);
    let on_disk = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");
    assert!(!on_disk.contains(&ExtentId(ext)), "reclaimed after the gate");
}

/// Replays one modeled WAL tail through the ADR-0057 D4 rules plus the
/// tag-9 arm (the `tiered_recovery.rs` replayer with the extent kind).
fn replay_tail(table: &mut TieredTable, tail: &[u8]) {
    let mut rest = tail;
    let mut pending: Vec<u64> = Vec::new();
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail decodes");
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
                    .apply_extent_image(key, hash, inf_store::ExtentRef { extent_id, offset, len })
                    .expect("fits");
            }
            RecordView::Delete { key, .. } => {
                let hash = TieredTable::hash_key(key);
                for old in pending.drain(..) {
                    table.apply_displace(hash, LogicalAddr::from_raw(old).expect("48-bit"));
                }
                table.apply_delete(key, hash);
            }
            _ => panic!("unexpected record class in the modeled tail"),
        }
        rest = &rest[consumed..];
    }
    assert!(pending.is_empty(), "a displace marker with no paired mutation");
}
