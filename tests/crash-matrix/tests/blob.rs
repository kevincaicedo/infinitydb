//! M4-S17 blob-extent crash rows (ADR-0061 D3/D5/D9) — carried at the
//! node tier per `m4.toml`:
//!
//! - `blob-write-fails-typed` — a short device write or a failed
//!   fdatasync abandons the extent typed: no `SealedExtent` exists, so
//!   reference position is unreachable by construction, and the debris
//!   file is the sweep's to reclaim. Never fail-stop (the ADR-0061 D3
//!   narrower behavior), never a dangling reference.
//! - `orphan-reclaimed-never-served` — the AC1 cut, both halves: an
//!   extent whose bytes are durable but whose referencing frame never
//!   made the log recovers as an orphan (no reference anywhere, swept,
//!   unlinked, never resolved by any read), while its referenced twin —
//!   written through exactly the same path with its frame in the tail —
//!   serves byte-exact through the CRC-verified reader.
//! - `reclaim-deferred-nonfatal` — a failed extent unlink is counted
//!   and re-offered; the file outlives the failure, disk returns on the
//!   retry, and a crash instead of a retry hands it to the boot sweep.

use std::path::Path;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::blob::{ExtentId, ExtentWriter, list_extent_ids, open_extent, unlink_extent_file};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentId, StagingConfig,
    StagingRing, SyncIckWriter, TierFlushConfig, TierIoMode, decode_record, read_ick_hybrid,
    read_manifest, write_manifest,
};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, ExtentRef, LogicalAddr, TieredTable,
    apply_blob_ref_section, apply_live_set_section, apply_ref_section, recover_tiered_ns,
};

use crash_matrix::load_matrix;
use inf_store::KeyHasher;

const NS: NsId = NsId(29);
const PAGE: u64 = 4 << 10;
const SHARD: &str = "shard-0";

fn flush_config() -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        file_capacity: 24 << 10,
        slice_bytes: PAGE,
    }
}

fn demote() -> DemotionConfig {
    DemotionConfig { mem_budget_bytes: 1 << 20, mutable_permille: 500, slice_bytes: PAGE }
}

fn table(origin: u64) -> TieredTable {
    let space = AddressSpaceConfig {
        reserve_bytes: demote().ring_reserve_bytes().expect("valid"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    };
    let mut t = TieredTable::new(space, demote(), 256, KeyHasher::default()).expect("ring");
    t.set_blob_config(inf_store::BlobConfig { threshold_bytes: 64, max_bytes: 1 << 20 });
    t
}

fn value(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(43).wrapping_add(seed)).collect()
}

/// Writes one whole extent through the chunked writer; the caller owns
/// the id (already allocated from the table).
fn write_extent(fs: &MemFs, id: u64, bytes: &[u8]) -> inf_log::SealedExtent {
    let mut w = ExtentWriter::create(
        fs,
        Path::new(SHARD),
        ExtentId(id),
        0,
        NS,
        bytes.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create");
    for chunk in bytes.chunks(900) {
        w.append_chunk(chunk).expect("append");
    }
    w.finish().expect("finish")
}

/// Row: `blob_short_write` + `blob_fsync_err` → blob-write-fails-typed.
/// Either failure abandons the extent: typed error, no token, and the
/// debris is reclaimable — the node never stops and never references.
#[test]
fn blob_write_faults_abandon_the_extent_typed() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let mut t = table(0);

    // Short write mid-append: the id was allocated, the write refuses.
    // The value spans more than one full batch window so a device write
    // happens mid-append (the funnel the point covers).
    let short_id = t.allocate_extent_id();
    let big = value(inf_log::BLOB_CHUNK_BYTES + 4096, 1);
    fault::arm("blob_short_write", FaultSpec::Nth(1));
    let mut w = ExtentWriter::create(
        &fs,
        Path::new(SHARD),
        ExtentId(short_id),
        0,
        NS,
        big.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create");
    let refused = w.append_chunk(&big);
    assert!(refused.is_err(), "short write fails the append whole (typed)");
    fault::disarm_all();
    drop(w);

    // Fsync failure at finish: typed abort, no SealedExtent.
    let abort_id = t.allocate_extent_id();
    let small = value(4 << 10, 2);
    fault::arm("blob_fsync_err", FaultSpec::Nth(1));
    let mut w = ExtentWriter::create(
        &fs,
        Path::new(SHARD),
        ExtentId(abort_id),
        0,
        NS,
        small.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create");
    w.append_chunk(&small).expect("append");
    let aborted = w.finish();
    assert!(aborted.is_err(), "fsync failure is a typed abort, never fail-stop");
    assert_eq!(fault::occurrences("blob_fsync_err"), 1, "the point fired");
    fault::disarm_all();

    // Both ids are quarantined (never reissued) and both debris files
    // are exactly what the sweep reclaims: seed it as a boot would.
    let next = t.allocate_extent_id();
    assert!(next > abort_id, "abandoned ids are never reissued");
    let listed: Vec<u64> =
        list_extent_ids(&fs, Path::new(SHARD)).expect("listing").iter().map(|i| i.0).collect();
    assert!(listed.contains(&short_id) && listed.contains(&abort_id), "debris is on disk");
    t.extent_sweep_seed(&listed);
    let mut swept: Vec<u64> = Vec::new();
    loop {
        let work = t.extent_reclaim_work(0, 8);
        if work.is_empty() {
            break;
        }
        for id in work {
            unlink_extent_file(&fs, Path::new(SHARD), ExtentId(id)).expect("unlink");
            t.extent_reclaim_done(id);
            swept.push(id);
        }
    }
    assert!(swept.contains(&short_id) && swept.contains(&abort_id), "debris reclaimed");
    assert_eq!(list_extent_ids(&fs, Path::new(SHARD)).expect("listing"), Vec::<ExtentId>::new());
}

/// Row: `blob_write_nospace` → blob-write-fails-typed (M4-S21,
/// ADR-0063 D4): a persistent ENOSPC refuses the extent write typed —
/// abandoned, quarantined, sweep-reclaimable, exactly the short-write
/// posture — with **no latch** (per-op refusal is the blob admission),
/// and the first attempt after space frees is its own recovery probe.
#[test]
fn blob_write_nospace_fails_typed_and_next_attempt_recovers() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let mut t = table(0);

    // The disk stays full: every device write refuses `StorageFull`.
    let refused_id = t.allocate_extent_id();
    let bytes = value(4 << 10, 7);
    fault::arm("blob_write_nospace", FaultSpec::FromNth(1));
    let mut w = ExtentWriter::create(
        &fs,
        Path::new(SHARD),
        ExtentId(refused_id),
        0,
        NS,
        bytes.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create precedes the data write");
    w.append_chunk(&bytes).expect("a sub-window chunk only stages");
    let err = w.finish().expect_err("the tail-frame write refuses");
    assert!(err.is_storage_full(), "classified as space exhaustion: {err}");
    assert!(fault::fired("blob_write_nospace") >= 1, "the row is not vacuous");
    fault::disarm_all();

    // Space freed: the retry is a fresh id (allocate-once) and lands;
    // the refused id's debris is exactly what the sweep reclaims.
    let retry_id = t.allocate_extent_id();
    assert!(retry_id > refused_id, "abandoned ids are never reissued");
    let sealed = write_extent(&fs, retry_id, &bytes);
    assert_eq!(sealed.data_len(), bytes.len() as u64);
    let listed: Vec<u64> =
        list_extent_ids(&fs, Path::new(SHARD)).expect("listing").iter().map(|i| i.0).collect();
    assert!(listed.contains(&refused_id), "debris is on disk for the sweep");
    t.extent_sweep_seed(&listed);
    let mut swept: Vec<u64> = Vec::new();
    loop {
        let work = t.extent_reclaim_work(0, 8);
        if work.is_empty() {
            break;
        }
        for id in work {
            unlink_extent_file(&fs, Path::new(SHARD), ExtentId(id)).expect("unlink");
            t.extent_reclaim_done(id);
            swept.push(id);
        }
    }
    assert!(swept.contains(&refused_id), "the refused extent's debris reclaimed");
}

/// Row: `blob_fsync_err` → orphan-reclaimed-never-served — the AC1 cut.
/// One recovery unit holds a referenced blob (extent durable, frame in
/// the tail) and an orphan (extent durable, frame lost to the crash):
/// recovery serves the first byte-exact and sweeps the second without
/// any read ever resolving it.
#[test]
fn orphan_cut_reclaims_never_serves_and_the_referenced_twin_serves() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let mut t = table(0);
    let mut ring = StagingRing::new(StagingConfig::default());
    let mut tail: Vec<u8> = Vec::new();

    // The referenced twin: extent → fdatasync → token → record + frame.
    let live_value = value(9 << 10, 7);
    let live_id = t.allocate_extent_id();
    let sealed = write_extent(&fs, live_id, &live_value);
    let key = b"blob:live".to_vec();
    let hash = KeyHasher::default().hash(&key);
    let effect = MutationEffect::StringSetExtent {
        ns: NS,
        key: &key,
        extent_id: sealed.extent_id().0,
        offset: 0,
        len: sealed.data_len(),
    };
    t.stage_wal(&mut ring, &effect).expect("staged");
    effect.record().encode_into(&mut tail);
    t.insert_extent(&key, hash, &sealed).expect("fits");

    // A checkpoint so the recovery unit exists (empty walk is fine —
    // the record is RAM-resident and rides the tail).
    let ckpt_id = 1u64;
    let w = t.begin_ckpt_walk(ckpt_id);
    let begin_lsn = Lsn::new(SegmentId(1), 64);
    let mut writer = SyncIckWriter::create_v2(
        fs.clone(),
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
        let mut images: Vec<(Vec<u8>, Vec<u8>, Option<ExtentRef>)> = Vec::new();
        cursor = t.ckpt_walk_slice(
            cursor,
            64,
            |h, a| refs.push((h, a.to_raw())),
            |parts| images.push((parts.key.to_vec(), parts.value.to_vec(), parts.extent_ref())),
        );
        for (h, a) in refs {
            writer.append_ref(NS.0, w.to_raw(), h, a).expect("ref");
        }
        for (k, v, ext) in images {
            match ext {
                Some(ext) => writer
                    .append(&RecordView::StringExtentRef {
                        ns: NS,
                        key: &k,
                        extent_id: ext.extent_id,
                        offset: ext.offset,
                        len: ext.len,
                    })
                    .expect("extent image"),
                None => writer
                    .append(&RecordView::StringPostImage { ns: NS, key: &k, value: &v })
                    .expect("image"),
            }
        }
        if cursor == 0 {
            break;
        }
    }
    for (addr, extent_id, len) in t.extent_ckpt_entries().collect::<Vec<_>>() {
        writer.append_blob_ref(NS.0, addr, extent_id, len).expect("0x05");
    }
    writer.finish().expect("finish ick");
    t.end_ckpt_walk();
    let flush = inf_log::TierFlush::new(fs.clone(), flush_config(), 0);
    let manifest = Manifest {
        ckpt_id,
        begin_lsn,
        segments: vec![SegmentId(1)],
        tiers: vec![t.tier_manifest(NS.0, &flush)],
        key_hash_id: KeyHasher::default().identity(),
    };
    write_manifest(&fs, Path::new(SHARD), &manifest).expect("swap");

    // The cut: a second extent reaches durability — and the process
    // dies before its referencing frame ever stages. Nothing durable
    // names it.
    let orphan_value = value(6 << 10, 9);
    let orphan_id = t.allocate_extent_id();
    let _token_lost_to_the_crash = write_extent(&fs, orphan_id, &orphan_value);
    drop((t, ring)); // crash

    // Boot.
    let stored = read_manifest(&fs, Path::new(SHARD)).expect("manifest").expect("present");
    let tier = stored.tier_ns(NS.0).expect("section").clone();
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        stored.ckpt_id,
        flush_config(),
        AddressSpaceConfig {
            reserve_bytes: demote().ring_reserve_bytes().expect("valid"),
            page_bytes: PAGE as usize,
            life_origin: LogicalAddr::ZERO,
        },
        demote(),
        256,
        KeyHasher::default(),
    )
    .expect("recovery");
    assert!(recovered.extents_listed.contains(&orphan_id), "the listing saw the orphan");
    let cell = std::cell::RefCell::new(recovered.table);
    let ick = Path::new(SHARD).join(inf_log::ckpt::ick_file_name(stored.ckpt_id));
    read_ick_hybrid(
        &fs,
        &ick,
        inf_log::ckpt::IckReaderConfig::default(),
        |record| {
            match record {
                RecordView::StringPostImage { key, value, .. } => {
                    cell.borrow_mut()
                        .apply_image(key, value, KeyHasher::default().hash(key))
                        .expect("fits");
                }
                RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                    cell.borrow_mut()
                        .apply_extent_image(
                            key,
                            KeyHasher::default().hash(key),
                            ExtentRef { extent_id, offset, len },
                        )
                        .expect("fits");
                }
                _ => panic!("unexpected image class"),
            }
            Ok::<(), std::convert::Infallible>(())
        },
        |section| {
            apply_ref_section(&mut cell.borrow_mut(), &section, tier.flushed).expect("refs");
            Ok(())
        },
        |section| {
            apply_live_set_section(&mut cell.borrow_mut(), &section);
            Ok(())
        },
        |section| {
            apply_blob_ref_section(&mut cell.borrow_mut(), &section);
            Ok(())
        },
        |_| panic!("no index-sidecar sections in this image"),
    )
    .expect("hybrid load");
    let mut t = cell.into_inner();
    // Tail replay: the referenced twin's frame survived the crash.
    let mut rest: &[u8] = &tail;
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail decodes");
        match record {
            RecordView::StringExtentRef { key, extent_id, offset, len, .. } => {
                t.apply_extent_image(
                    key,
                    KeyHasher::default().hash(key),
                    ExtentRef { extent_id, offset, len },
                )
                .expect("fits");
            }
            _ => panic!("only the blob SET rides this tail"),
        }
        rest = &rest[consumed..];
    }

    // Never served: no reference to the orphan exists anywhere.
    assert_eq!(t.extent_refcount(orphan_id), 0, "nothing references the orphan");
    assert!(
        t.extent_references().all(|(_, ext, _)| ext != orphan_id),
        "no read path can resolve the orphan"
    );
    // The referenced twin serves byte-exact through the verified reader.
    assert_eq!(t.extent_refcount(live_id), 1, "the referenced twin survived");
    let mut reader =
        open_extent(&fs, Path::new(SHARD), ExtentId(live_id), TierIoMode::Buffered).expect("open");
    let mut got = Vec::new();
    let mut offset = 0u64;
    while (offset as usize) < live_value.len() {
        let take = (live_value.len() - offset as usize).min(1000);
        reader.read(offset, take, &mut got).expect("io").expect("crc");
        offset += take as u64;
    }
    assert_eq!(got, live_value, "the referenced blob serves its exact bytes");
    // Reclaimed: the sweep unlinks the orphan and only the orphan.
    t.extent_sweep_seed(&recovered.extents_listed);
    let swept = t.extent_reclaim_work(0, 8);
    assert_eq!(swept, vec![orphan_id], "the sweep hands out exactly the orphan");
    unlink_extent_file(&fs, Path::new(SHARD), ExtentId(orphan_id)).expect("unlink");
    t.extent_reclaim_done(orphan_id);
    assert_eq!(
        list_extent_ids(&fs, Path::new(SHARD)).expect("listing"),
        vec![ExtentId(live_id)],
        "post-sweep: the live extent alone remains"
    );
}

/// Row: `blob_unlink_fail` → reclaim-deferred-nonfatal. The failure is
/// typed and counted; the candidate re-offers; the retry returns disk.
#[test]
fn blob_unlink_failure_defers_nonfatally_and_the_retry_reclaims() {
    let fs = MemFs::new();
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let mut t = table(0);
    let mut ring = StagingRing::new(StagingConfig::default());
    let bytes = value(2 << 10, 3);
    let id = t.allocate_extent_id();
    let sealed = write_extent(&fs, id, &bytes);
    let key = b"blob:doomed".to_vec();
    let hash = KeyHasher::default().hash(&key);
    t.stage_wal(
        &mut ring,
        &MutationEffect::StringSetExtent {
            ns: NS,
            key: &key,
            extent_id: sealed.extent_id().0,
            offset: 0,
            len: sealed.data_len(),
        },
    )
    .expect("staged");
    let addr = t.insert_extent(&key, hash, &sealed).expect("fits");
    // Kill it; stamp at the next staging (the delete's own effect).
    t.stage_wal(&mut ring, &MutationEffect::Delete { ns: NS, key: &key }).expect("staged");
    let len = t.record(addr).encoded_len;
    t.delete(hash, addr, len);
    t.stage_wal(&mut ring, &MutationEffect::CkptBegin { ckpt_id: 9 }).expect("stamp carrier");
    let durable = t.wal_epoch();
    assert_eq!(t.extent_reclaim_work(durable, 8), vec![id]);

    // The unlink fails typed — counted, deferred, nothing else changes.
    fault::arm("blob_unlink_fail", FaultSpec::Nth(1));
    let deferred = unlink_extent_file(&fs, Path::new(SHARD), ExtentId(id));
    assert!(deferred.is_err(), "the armed unlink fails typed");
    fault::disarm_all();
    t.extent_reclaim_deferred(id);
    assert_eq!(t.extent_stats().reclaim_deferred, 1, "the failure is counted");
    assert!(
        list_extent_ids(&fs, Path::new(SHARD)).expect("listing").contains(&ExtentId(id)),
        "the file outlives the failed unlink"
    );
    // Re-offered; the retry returns the disk.
    assert_eq!(t.extent_reclaim_work(durable, 8), vec![id], "the candidate re-offers");
    unlink_extent_file(&fs, Path::new(SHARD), ExtentId(id)).expect("retry succeeds");
    t.extent_reclaim_done(id);
    assert_eq!(list_extent_ids(&fs, Path::new(SHARD)).expect("listing"), Vec::<ExtentId>::new());
}

/// The S17 + S21 blob rows are well-formed and carried here
/// (self-policing).
#[test]
fn s17_rows_are_carried_here() {
    let def = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    let here: Vec<_> = def.rows.iter().filter(|r| r.test == "blob.rs").collect();
    assert_eq!(here.len(), 5, "the four S17 rows plus the S21 ENOSPC row are declared");
    for row in &here {
        assert_eq!(row.tier, "node");
        assert!(
            inf_log::fault::ALL.contains(&row.point.as_str()),
            "row {:?} names a declared point",
            row.point
        );
    }
    for expect in
        ["blob-write-fails-typed", "orphan-reclaimed-never-served", "reclaim-deferred-nonfatal"]
    {
        assert!(here.iter().any(|r| r.expect == expect), "the {expect} verdict is carried");
    }
}
