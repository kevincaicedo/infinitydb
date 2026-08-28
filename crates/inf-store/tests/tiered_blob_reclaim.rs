//! M4-S18 — extent reclaim + compaction interplay (plan AC 1/AC 2's
//! store half; ADR-0061 D5/D8 — the decisions were made in S17, this
//! suite proves them **complete under churn**):
//!
//! - **the leak test:** blob create/overwrite/delete cycles, with the
//!   reclaim slice and S15's compaction slice interleaved in the same
//!   MAINTAIN round, return every dead extent's disk — at quiescence the
//!   extent directory equals the live set exactly, `created ==
//!   reclaimed + live`, and the reclaim backlog reads zero. The AC's
//!   10⁶-cycle row is the release run (`cargo test -p inf-store
//!   --release --test tiered_blob_reclaim`); debug runs a scaled cycle
//!   count so `just check` stays fast — the bound is visible here, not
//!   silent (L10).
//! - **references move, blob bytes never:** across a full compaction
//!   pass, `blob_bytes` does not move, every live extent's file image is
//!   byte-identical, and the relocation volume is record legs only —
//!   the "blob WA ≈ 1× by construction" claim, asserted rather than
//!   hoped.
//! - **no starvation:** with both a reclaim backlog and a compaction
//!   backlog standing, bounded per-round budgets drain both in bounded
//!   rounds — the two MAINTAIN citizens share the round without either
//!   starving the other.
//! - **`statvfs` (the plan's statfs assert):** on the real filesystem,
//!   reclaimed extents are unlinked at the VFS and the blocks return to
//!   the OS — accounting that says "reclaimed" while the filesystem
//!   still holds the bytes is the classic false pass this leg exists to
//!   catch.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::blob::{
    ExtentId, ExtentWriter, extent_file_name, list_extent_ids, unlink_extent_file,
};
use inf_log::flush::unlink_tier_file;
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    MutationEffect, NsId, StagingConfig, StagingRing, TierFlush, TierFlushConfig, TierIoMode,
};
use inf_store::KeyHasher;
use inf_store::{
    AddressSpaceConfig, BLOB_RECLAIM_PER_SLICE_DEFAULT, BlobConfig, CompactionConfig,
    CompactionWork, DemotionConfig, LogicalAddr, TieredTable,
};

const NS: NsId = NsId(53);
const SHARD: &str = "shard-0";
const PAGE: u64 = 4 << 10;
/// Tiny threshold so the lifecycle cycles at test scale (the ADR-0061 D1
/// construction-parameter posture, the `tiered_blob.rs` precedent).
const THRESHOLD: u32 = 64;
/// A small mutable fraction (1.6% instead of the 25% default): blob
/// record legs are ~50 bytes each, so at the default fraction nothing
/// would ever seal at test scale and the compaction interplay under
/// test would be vacuous. The knob is config from day one (ADR-0053 D2).
const MUTABLE_PERMILLE: u32 = 16;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

#[derive(Clone, Debug)]
struct Entry {
    addr: u64,
    len: usize,
    version: u32,
    extent_id: u64,
    value_len: u64,
}

/// A blob-only churn rig (the `tiered_blob.rs` shape, generic over the
/// filesystem so the `statvfs` leg runs the identical workload on
/// `StdSegmentFs`). Values are always out of line — the record leg is
/// key + 24-byte reference by construction.
struct Rig<F: SegmentFs + Clone> {
    fs: F,
    shard: std::path::PathBuf,
    table: TieredTable,
    flush: TierFlush<F>,
    ring: StagingRing,
    model: BTreeMap<u64, Entry>,
    value_len: u64,
    ckpt_id: u64,
}

impl<F: SegmentFs + Clone> Rig<F> {
    fn new(fs: F, shard: &Path, value_len: u64, budget: u64, file_capacity: u64) -> Rig<F> {
        let demote = DemotionConfig {
            mem_budget_bytes: budget,
            mutable_permille: MUTABLE_PERMILLE,
            slice_bytes: PAGE,
        };
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            2048,
            KeyHasher::default(),
        )
        .expect("ring");
        table.set_blob_config(BlobConfig { threshold_bytes: THRESHOLD, max_bytes: 1 << 20 });
        table.set_compaction_config(CompactionConfig { dead_ratio_pct: 50, slice_bytes: 1 << 20 });
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: shard.to_path_buf(),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity,
                slice_bytes: PAGE,
            },
            0,
        );
        Rig {
            fs,
            shard: shard.to_path_buf(),
            table,
            flush,
            ring: StagingRing::new(StagingConfig::default()),
            model: BTreeMap::new(),
            value_len,
            ckpt_id: 0,
        }
    }

    fn key(id: u64) -> Vec<u8> {
        format!("k:{id:05}").into_bytes()
    }

    fn stage(&mut self, effect: &MutationEffect<'_>) {
        if self.table.stage_wal(&mut self.ring, effect).is_err() {
            self.ring = StagingRing::new(StagingConfig::default());
            self.table.stage_wal(&mut self.ring, effect).expect("a fresh ring has room");
        }
    }

    /// Deterministic value for (key id, generation) — regenerable, never
    /// held for every key.
    fn value_for(&self, id: u64, generation: u64) -> Vec<u8> {
        let len = self.value_len as usize + ((id.wrapping_mul(31) ^ generation) % 64) as usize;
        (0..len).map(|i| (i as u64 ^ id.wrapping_mul(7) ^ generation) as u8).collect()
    }

    /// SET, always out of line: extent write → fdatasync → sealed token
    /// → reference (the D3 ordering, structural).
    fn set_blob(&mut self, id: u64, generation: u64) {
        let key = Self::key(id);
        let hash = KeyHasher::default().hash(&key);
        let value = self.value_for(id, generation);
        let old = self.model.get(&id).cloned();
        if let Some(old) = &old {
            let addr = LogicalAddr::from_raw(old.addr).expect("48-bit");
            let _ = self.table.take_displacement_origins(hash, addr);
        }
        let extent_id = ExtentId(self.table.allocate_extent_id());
        let mut w = ExtentWriter::create(
            &self.fs,
            &self.shard,
            extent_id,
            0,
            NS,
            value.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create extent");
        w.append_chunk(&value).expect("chunk");
        let sealed = w.finish().expect("finish");
        self.table.note_blob_bytes(sealed.device_bytes());
        self.stage(&MutationEffect::StringSetExtent {
            ns: NS,
            key: &key,
            extent_id: sealed.extent_id().0,
            offset: 0,
            len: sealed.data_len(),
        });
        let place = |table: &mut TieredTable| match &old {
            Some(o) => table.update_extent(
                &key,
                hash,
                &sealed,
                LogicalAddr::from_raw(o.addr).expect("48-bit"),
                o.len,
                o.version,
            ),
            None => table.insert_extent(&key, hash, &sealed),
        };
        let placed = match place(&mut self.table) {
            Ok(addr) => addr,
            Err(_) => {
                self.drain();
                place(&mut self.table).expect("fits after drain")
            }
        };
        let parts = self.table.record(placed);
        self.model.insert(
            id,
            Entry {
                addr: placed.to_raw(),
                len: parts.encoded_len,
                version: parts.version,
                extent_id: sealed.extent_id().0,
                value_len: sealed.data_len(),
            },
        );
    }

    /// DEL: index + accounting only (§3.3) — a cold record's extent
    /// releases through the reference map alone, zero reads.
    fn del(&mut self, id: u64) {
        let Some(entry) = self.model.remove(&id) else { return };
        let key = Self::key(id);
        let hash = KeyHasher::default().hash(&key);
        let addr = LogicalAddr::from_raw(entry.addr).expect("48-bit");
        let _ = self.table.take_displacement_origins(hash, addr);
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

    /// One bounded compaction leg of a MAINTAIN round. Returns the
    /// relocation volume this round moved.
    fn compact_round(&mut self, budget: u64) -> u64 {
        let mut spent = 0u64;
        let mut relocated_bytes = 0u64;
        while spent < budget {
            let work = self.table.compaction_work(&self.flush, false, budget - spent);
            let CompactionWork::Read { file_id, addr, len } = work else { break };
            let Some(bytes) = self.read_chunk(file_id, addr, len) else { break };
            let applied = self.table.compaction_apply(file_id, addr, &bytes);
            spent += applied.consumed.max(applied.need).max(1);
            relocated_bytes += applied.relocated_bytes;
            if applied.stalled {
                break;
            }
        }
        if relocated_bytes > 0 {
            self.refresh_model();
        }
        relocated_bytes
    }

    /// A compaction scan chunk (the S08 cold read, modeled synchronously
    /// — the `tiered_write_amp.rs` helper shape, via the fs seam so the
    /// real-filesystem rig reads real files).
    fn read_chunk(&self, file_id: u32, addr: LogicalAddr, len: u64) -> Option<Vec<u8>> {
        use inf_log::fs::SegmentFile;
        use inf_log::{TIER_FRAME_BYTES, tier_extract, tier_frame_offset, tier_frame_span};
        let meta = self.flush.sealed().iter().find(|m| m.id == file_id)?.clone();
        let file = self.fs.open_tier(&meta.path, TierIoMode::Buffered).ok()?;
        let len = usize::try_from(len).expect("fits");
        let (first, count, skip) = tier_frame_span(addr.to_raw() - meta.base.to_raw(), len);
        let from = tier_frame_offset(first);
        let mut frames = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let mut done = 0usize;
        while done < frames.len() {
            let n = file.read_at(from + done as u64, &mut frames[done..]).ok()?;
            if n == 0 {
                return None;
            }
            done += n;
        }
        let mut out = Vec::new();
        tier_extract(&frames, skip, len, &mut out).ok()?;
        Some(out)
    }

    /// Relocations moved records; blob records refresh exactly through
    /// the reference map (the machinery under test).
    fn refresh_model(&mut self) {
        let references: Vec<(u64, u64, u64)> = self.table.extent_references().collect();
        for entry in self.model.values_mut() {
            let (addr, _, _) = references
                .iter()
                .find(|&&(_, e, _)| e == entry.extent_id)
                .copied()
                .expect("a live blob key's extent stays mapped across relocation");
            entry.addr = addr;
        }
    }

    /// One bounded reclaim leg of a MAINTAIN round: stamps trailing
    /// parked deaths (the D5 idle-cell disclosure — a checkpoint marker
    /// is a legitimate staged non-state record), hands out at most `max`
    /// candidates at full durability, asserts zero early frees, unlinks.
    fn reclaim_round(&mut self, max: usize) -> usize {
        self.stage(&MutationEffect::CkptBegin { ckpt_id: u64::MAX });
        let work = self.table.extent_reclaim_work(self.table.wal_epoch(), max);
        let count = work.len();
        for id in work {
            assert!(
                !self.model.values().any(|e| e.extent_id == id),
                "early free: extent {id} is model-live"
            );
            unlink_extent_file(&self.fs, &self.shard, ExtentId(id)).expect("unlink");
            self.table.extent_reclaim_done(id);
        }
        count
    }

    /// One tier-file publication cycle (the S15 retirement pipeline —
    /// walk stamp, retire scan, manifest exclusion, commit + detach +
    /// unlink). Returns how many tier files retired.
    fn publish_cycle(&mut self) -> usize {
        self.ckpt_id += 1;
        self.table.begin_ckpt_walk(self.ckpt_id);
        self.table.end_ckpt_walk();
        self.table.retire_scan(self.ckpt_id, &self.flush);
        let _section = self.table.tier_manifest(NS.0, &self.flush);
        let ids = self.table.commit_retirement();
        for &id in &ids {
            let meta = self.flush.detach_sealed(id).expect("retired files are sealed");
            unlink_tier_file(&self.fs, &meta).expect("unlink");
        }
        ids.len()
    }

    /// The refcount oracle (the `tiered_blob.rs` shape): live extents
    /// equal the model's exactly, each at refcount 1, each mapped at its
    /// record's current address, each with its file still on disk.
    fn assert_refcounts(&self) {
        let mut model_live: Vec<u64> = Vec::new();
        for entry in self.model.values() {
            model_live.push(entry.extent_id);
            assert_eq!(self.table.extent_refcount(entry.extent_id), 1, "refcount");
            assert_eq!(
                self.table.extent_reference_at(LogicalAddr::from_raw(entry.addr).expect("48-bit")),
                Some((entry.extent_id, entry.value_len)),
                "reference map entry for extent {}",
                entry.extent_id
            );
        }
        model_live.sort_unstable();
        let stats = self.table.extent_stats();
        assert_eq!(stats.live, model_live.len() as u64, "live extent count");
        let on_disk = list_extent_ids(&self.fs, &self.shard).expect("listing");
        for ext in &model_live {
            assert!(on_disk.contains(&ExtentId(*ext)), "extent {ext} unlinked while live");
        }
    }

    /// Drives churn to full quiescence: delete everything, then drain,
    /// compact, publish, and reclaim until every backlog reads zero.
    fn quiesce_empty(&mut self) {
        let ids: Vec<u64> = self.model.keys().copied().collect();
        for id in ids {
            self.del(id);
        }
        self.drain();
        for _ in 0..64 {
            let relocated = self.compact_round(1 << 20);
            let retired = self.publish_cycle();
            let reclaimed = self.reclaim_round(usize::MAX);
            self.drain();
            let stats = self.table.extent_stats();
            if relocated == 0 && retired == 0 && reclaimed == 0 && stats.reclaimable == 0 {
                return;
            }
        }
        panic!("quiescence not reached in 64 rounds — a backlog is stuck");
    }
}

/// The leak test (plan AC 1): blob create/overwrite/delete cycles with
/// compaction and reclaim interleaved in the same MAINTAIN round —
/// at quiescence the extent directory equals the live set, the counters
/// reconcile exactly, and the reclaim backlog is empty. The 10⁶-cycle
/// AC row is the release run; debug and Miri run scaled counts, stated
/// here rather than hidden (L10).
#[test]
fn blob_leak_cycles_return_disk_to_baseline() {
    let cycles: u64 = if cfg!(miri) {
        2_000
    } else if cfg!(debug_assertions) {
        100_000
    } else {
        1_000_000
    };
    let mut rig = Rig::new(MemFs::new(), Path::new(SHARD), 64, 256 << 10, 16 << 10);
    let mut seed = 0x518_5EEDu64;
    let keys = 192u64;
    let mut generation = 0u64;
    for cycle in 0..cycles {
        generation += 1;
        let id = seeded(&mut seed) % keys;
        match seeded(&mut seed) % 16 {
            0..=10 => rig.set_blob(id, generation),
            11..=13 => rig.del(id),
            _ => {}
        }
        // The MAINTAIN round: demotion legs, then one bounded compaction
        // slice, then one bounded reclaim slice — the two S18 citizens
        // share every round, which is exactly the interplay under test.
        if cycle % 64 == 63 {
            rig.drain();
            rig.compact_round(64 << 10);
            rig.reclaim_round(BLOB_RECLAIM_PER_SLICE_DEFAULT);
        }
        if cycle % 1_024 == 1_023 {
            rig.publish_cycle();
        }
        if cycle % 8_192 == 8_191 {
            rig.assert_refcounts();
        }
    }
    rig.assert_refcounts();
    rig.quiesce_empty();

    let stats = rig.table.extent_stats();
    assert!(stats.created > 0 && stats.reclaimed > 0, "the storm exercised the lifecycle");
    assert_eq!(stats.live, 0, "everything was deleted");
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.reclaimable, 0, "no standing backlog at quiescence");
    assert_eq!(stats.created, stats.reclaimed, "every created extent was reclaimed");
    let on_disk = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");
    assert!(on_disk.is_empty(), "the extent directory returned to baseline: {on_disk:?}");
    // Baseline on the mem filesystem: every blob file's bytes are gone.
    for id in 1..=stats.created {
        let path = Path::new(SHARD).join("cold").join(extent_file_name(ExtentId(id)));
        assert!(rig.fs.contents(&path).is_none(), "extent {id} bytes linger");
    }
    println!(
        "leak test: cycles={cycles} created={} reclaimed={} reclaim_slices={}",
        stats.created, stats.reclaimed, stats.reclaim_slices
    );
}

/// Compaction moves references, never blob bytes (ADR-0061 D2/D4, the
/// reason blob WA ≈ 1×): across a full compaction pass `blob_bytes`
/// does not move, every live extent's file image is byte-identical, the
/// extent directory is untouched, and the relocation volume is record
/// legs only — a blob payload riding a relocation would show up as
/// hundreds of bytes per record against the ~64-byte record leg.
#[test]
fn compaction_relocates_references_never_blob_bytes() {
    // 512-byte values make the separation crisp: a record leg is
    // key (7) + reference (24) + header, far under 128; one leaked
    // payload would exceed it several times over.
    // Enough records to span commit pages: seal marks land on
    // page-first-record boundaries (ADR-0053 D2), so a workload smaller
    // than a page never seals and the pass under test would be vacuous.
    let mut rig = Rig::new(MemFs::new(), Path::new(SHARD), 512, 64 << 10, 4 << 10);
    for id in 0..512u64 {
        rig.set_blob(id, 1);
    }
    rig.drain();
    // Overwrites kill the cold copies: the old files go ≥ 50% dead and
    // become compaction candidates; the old extents park for reclaim.
    for id in 0..384u64 {
        rig.set_blob(id, 2);
    }
    rig.drain();

    let before = rig.table.write_accounting();
    let images: BTreeMap<u64, Vec<u8>> = rig
        .model
        .values()
        .map(|e| {
            let path = Path::new(SHARD).join("cold").join(extent_file_name(ExtentId(e.extent_id)));
            (e.extent_id, rig.fs.contents(&path).expect("live extent on disk"))
        })
        .collect();
    let listed_before = list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing");

    let mut relocated_bytes = 0u64;
    for _ in 0..64 {
        let moved = rig.compact_round(1 << 20);
        rig.drain();
        if moved == 0 {
            break;
        }
        relocated_bytes += moved;
    }
    assert!(relocated_bytes > 0, "the pass relocated live records");

    let after = rig.table.write_accounting();
    assert_eq!(after.blob_bytes, before.blob_bytes, "compaction wrote zero extent device bytes");
    assert_eq!(after.blob_user_bytes, before.blob_user_bytes);
    assert_eq!(
        after.compaction_bytes - before.compaction_bytes,
        relocated_bytes,
        "the volume counter is exactly the relocated record legs"
    );
    // Records may relocate more than once across the pass; 256 B/record
    // of headroom still sits far under one 512-byte payload per record.
    let record_leg_cap = 256 * rig.model.len() as u64;
    assert!(
        relocated_bytes <= record_leg_cap,
        "relocation volume {relocated_bytes} exceeds the record-leg bound {record_leg_cap} — \
         blob payload rode a relocation"
    );
    assert_eq!(
        list_extent_ids(&rig.fs, Path::new(SHARD)).expect("listing"),
        listed_before,
        "compaction never unlinks an extent"
    );
    for (extent_id, image) in &images {
        let path = Path::new(SHARD).join("cold").join(extent_file_name(ExtentId(*extent_id)));
        assert_eq!(
            rig.fs.contents(&path).as_ref(),
            Some(image),
            "extent {extent_id} bytes moved during compaction"
        );
    }
    rig.assert_refcounts();
}

/// The two MAINTAIN citizens share the round without starving each
/// other: with a standing reclaim backlog **and** a standing compaction
/// backlog, bounded per-round budgets make forward progress on both
/// every round until both drain.
#[test]
fn reclaim_and_compaction_share_the_round_without_starvation() {
    let mut rig = Rig::new(MemFs::new(), Path::new(SHARD), 256, 64 << 10, 4 << 10);
    for id in 0..512u64 {
        rig.set_blob(id, 1);
    }
    rig.drain();
    // Deleting three of every four keys builds the reclaim backlog AND
    // leaves the survivors interleaved through every cold file: ~75%
    // dead with live records to move — genuine copy-forward work, not
    // the fully-dead skip-to-retirement arm (ADR-0059 D1).
    for id in 0..512u64 {
        if id % 4 != 0 {
            rig.del(id);
        }
    }
    rig.drain();
    let backlog = rig.table.extent_stats().reclaimable;
    assert!(backlog >= 384, "the deletes parked a real backlog: {backlog}");

    let mut reclaim_progress = 0u64;
    let mut compact_progress = 0u64;
    for round in 0..256 {
        let compacted = rig.compact_round(16 << 10);
        let reclaimed = rig.reclaim_round(4);
        rig.drain();
        compact_progress += compacted;
        reclaim_progress += reclaimed as u64;
        let stats = rig.table.extent_stats();
        // While a durable backlog stands, the bounded slice still moves:
        // a round that reclaims nothing against a standing backlog is
        // starvation, which is exactly what this test exists to refuse.
        if stats.reclaimable > 0 {
            assert!(
                reclaimed > 0,
                "round {round}: reclaim starved with {} candidates standing",
                stats.reclaimable
            );
        }
        if stats.reclaimable == 0 && compacted == 0 {
            break;
        }
    }
    assert!(reclaim_progress >= 384, "every deleted extent reclaimed: {reclaim_progress}");
    assert!(compact_progress > 0, "compaction progressed in the same rounds");
    assert_eq!(rig.table.extent_stats().reclaimable, 0);
    rig.assert_refcounts();
}

/// The plan's statfs assert, on the real filesystem: reclaim is
/// `statvfs`-visible — the unlinked extents are gone at the VFS and the
/// blocks return to the OS. Reduced cycle count (stated, not hidden):
/// the 10⁶-cycle exactness row runs on the mem filesystem above; this
/// leg proves the filesystem half the mem rig cannot.
#[cfg(unix)]
#[test]
fn statvfs_confirms_blob_reclaim_returns_disk_to_the_os() {
    use inf_log::fs::StdSegmentFs;

    let root = std::env::temp_dir().join(format!("inf-s18-reclaim-{}", std::process::id()));
    let shard = root.join(SHARD);
    std::fs::create_dir_all(shard.join("cold")).expect("tempdir");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    fn avail_bytes(path: &Path) -> u64 {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
        // SAFETY: `statvfs` is a plain-old-data C struct; the all-zero
        // bit pattern is a valid (if meaningless) value for every field,
        // and the FFI call below overwrites it before any read.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a live NUL-terminated path and `stat` is a valid
        // exclusive out-pointer for the duration of the call.
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
        assert_eq!(rc, 0, "statvfs");
        stat.f_bavail as u64 * stat.f_frsize as u64
    }

    fn blob_dir_bytes(shard: &Path) -> u64 {
        let mut total = 0u64;
        let Ok(entries) = std::fs::read_dir(shard.join("cold")) else { return 0 };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".iblob") {
                total += entry.metadata().expect("metadata").len();
            }
        }
        total
    }

    let cycles: u64 = if cfg!(debug_assertions) { 256 } else { 768 };
    let avail_start = avail_bytes(&root);
    // ~24 KiB values: each extent is a header block plus ~6 frames, so
    // the run cycles real multi-frame files without hammering the box.
    let mut rig = Rig::new(StdSegmentFs, &shard, 24 << 10, 64 << 10, 16 << 10);
    let mut seed = 0x518_D15Cu64;
    let keys = 24u64;
    let mut generation = 0u64;
    let mut peak_blob_bytes = 0u64;
    for cycle in 0..cycles {
        generation += 1;
        let id = seeded(&mut seed) % keys;
        match seeded(&mut seed) % 8 {
            0..=5 => rig.set_blob(id, generation),
            _ => rig.del(id),
        }
        if cycle % 16 == 15 {
            rig.drain();
            rig.compact_round(64 << 10);
            rig.reclaim_round(BLOB_RECLAIM_PER_SLICE_DEFAULT);
            peak_blob_bytes = peak_blob_bytes.max(blob_dir_bytes(&shard));
        }
        if cycle % 128 == 127 {
            rig.publish_cycle();
        }
    }
    rig.quiesce_empty();

    let stats = rig.table.extent_stats();
    assert_eq!(stats.created, stats.reclaimed, "every extent reclaimed");
    assert!(list_extent_ids(&rig.fs, &shard).expect("listing").is_empty());
    assert_eq!(blob_dir_bytes(&shard), 0, "no .iblob bytes linger at the VFS");
    assert!(peak_blob_bytes > 0, "the run held real extent bytes at its peak");
    // statvfs: the blocks came back to the OS. Generous slack absorbs
    // unrelated churn on a shared box; the direction is what the AC
    // names — space returns, not merely accounting (the false pass the
    // plan's pitfall row warns about).
    let residual = blob_dir_bytes(&shard);
    let avail_end = avail_bytes(&root);
    let slack = 16u64 << 20;
    assert!(
        avail_end + slack >= avail_start.saturating_sub(residual),
        "statvfs shows the reclaimed bytes returned (start {avail_start}, end {avail_end})"
    );
    println!(
        "statvfs leg: cycles={cycles} created={} reclaimed={} peak_blob_bytes={peak_blob_bytes} \
         avail_start={avail_start} avail_end={avail_end}",
        stats.created, stats.reclaimed
    );
}
