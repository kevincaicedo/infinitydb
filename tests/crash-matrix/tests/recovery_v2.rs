//! M4-S12 recovery-unit crash rows (`m4.toml`, ADR-0057 D5/D6/D8): the
//! windows between tier flush, MANIFEST v2 publication, and recovery.
//! Each test injects its named failure, the process "dies" (state
//! drops), and recovery re-proves the never-none contract from whatever
//! manifest survived:
//!
//! - `unit-resolves` — the surviving manifest (old on an aborted swap,
//!   old-or-new on a dir-fsync crash) names a complete recovery unit:
//!   every manifested tier byte reads back exactly, un-manifested tier
//!   state (bytes *and* whole files) is dead-life garbage and is gone
//!   after recovery.
//! - `reseal-at-watermark` — as S11's rows, but the manifested length
//!   now comes from a real epoch-2 MANIFEST read back from disk (the
//!   S11 harness-supplied-length deviation retired).
//!
//! The sixth ADR-0057 D8 window — copy-forward emptying a file vs the
//! checkpoint that unpins it — lands here with M4-S15 (ADR-0059):
//!
//! - `serves-from-prior-unit` — the covering swap aborts after the
//!   file emptied: the old unit (still naming it) recovers and every
//!   manifested range reads back — the row that catches premature
//!   unlink.
//! - `reclaim-deferred-nonfatal` — a failed unlink of a retired file
//!   is typed, counted, and re-driven (retry and boot GC alike); the
//!   one deliberate non-fail-stop posture in the tier pipeline.

use std::path::Path;

use crash_matrix::load_matrix;
use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    Lsn, Manifest, NsId, SealReason, SegmentId, TIER_FRAME_BYTES, TierFlush, TierFlushConfig,
    TierIoMode, read_manifest, tier_extract, tier_frame_offset, tier_frame_span, write_manifest,
};
use inf_store::{
    AddressSpaceConfig, CompactionWork, DemotionConfig, LogicalAddr, TieredTable, recover_tiered_ns,
};

const NS: NsId = NsId(23);
const PAGE: u64 = 4 << 10;
const SHARD: &str = "shard-0";

fn flush_config() -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns: NS,
        mode: TierIoMode::Buffered,
        // Small files so the windows span file boundaries.
        file_capacity: 24 << 10,
        slice_bytes: PAGE,
    }
}

fn demote() -> DemotionConfig {
    // A tiny mutable fraction: everything written becomes seal debt, so
    // flush watermarks move on demand.
    DemotionConfig { mem_budget_bytes: 1 << 20, mutable_permille: 10, slice_bytes: PAGE }
}

fn space_config(origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote().ring_reserve_bytes().expect("valid"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

fn rig(fs: &MemFs) -> (TieredTable, TierFlush<MemFs>) {
    fs.create_dir_all(Path::new(SHARD)).expect("shard dir");
    let table = TieredTable::new(space_config(0), demote(), 256).expect("ring");
    let flush = TierFlush::new(fs.clone(), flush_config(), 0);
    (table, flush)
}

/// Writes `keys` records (deterministic bytes per index) and drains the
/// demotion pipeline so they land in tier files.
fn fill_and_flush(table: &mut TieredTable, flush: &mut TierFlush<MemFs>, batch: u32, keys: u32) {
    for i in 0..keys {
        let key = format!("r:{batch}:{i:04}");
        let value = vec![0x30 + (i % 40) as u8; 120 + (i as usize % 60)];
        let hash = TieredTable::hash_key(key.as_bytes());
        table.insert(key.as_bytes(), &value, hash).expect("fits");
    }
    loop {
        let sealed = table.seal_slice();
        let f = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// Publishes the recovery unit for the current pipeline state.
fn publish(fs: &MemFs, table: &TieredTable, flush: &TierFlush<MemFs>, ckpt_id: u64) {
    write_manifest(
        fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id,
            begin_lsn: Lsn::new(SegmentId(1), 64),
            segments: vec![SegmentId(1)],
            tiers: vec![table.tier_manifest(NS.0, flush)],
        },
    )
    .expect("manifest swap");
}

/// Reads one manifested cold range through the recovered catalog and
/// CRC-verifies it (`tier_extract`).
fn read_manifested(fs: &MemFs, flush: &TierFlush<MemFs>, addr: u64, len: usize) -> Option<Vec<u8>> {
    let meta = flush
        .sealed()
        .iter()
        .find(|m| addr >= m.base.to_raw() && addr + len as u64 <= m.base.to_raw() + m.data_len)?;
    let image = fs.contents(&meta.path)?;
    let (first, count, skip) = tier_frame_span(addr - meta.base.to_raw(), len);
    let from = tier_frame_offset(first) as usize;
    let to = from + count as usize * TIER_FRAME_BYTES;
    let mut out = Vec::new();
    tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
    Some(out)
}

/// `manifest_rename_fail` over an epoch-2 swap — the flush ↔ manifest
/// window (ADR-0057 D8): tier bytes durable and `flushed` advanced in
/// memory, but the covering swap dies at its commit point. The old unit
/// stays authoritative; recovery truncates every un-manifested tier
/// byte and deletes un-named files (`unit-resolves`).
#[test]
fn manifest_v2_rename_fail_keeps_old_unit() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);
    let old = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    let old_tier = old.tier_ns(NS.0).expect("tier section").clone();
    assert!(old_tier.flushed > 0, "the old unit covers real tier bytes");

    // More durable tier bytes beyond the old unit, then the covering
    // swap dies at the rename (the commit point).
    fill_and_flush(&mut table, &mut flush, 1, 400);
    assert!(table.space().flushed().to_raw() > old_tier.flushed, "the window is real");
    fault::arm("manifest_rename_fail", FaultSpec::Nth(1));
    let tiers = vec![table.tier_manifest(NS.0, &flush)];
    let err = write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 2,
            begin_lsn: Lsn::new(SegmentId(2), 64),
            segments: vec![SegmentId(2)],
            tiers,
        },
    )
    .expect_err("the swap dies at its commit point");
    assert!(err.to_string().contains("manifest_rename_fail"), "typed + named: {err}");
    assert!(fault::fired("manifest_rename_fail") >= 1, "the row is not vacuous");
    fault::disarm_all();
    drop((table, flush)); // crash

    // Recovery: the old manifest is the unit; un-manifested state dies.
    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    assert_eq!(manifest.ckpt_id, 1, "the aborted swap left the old unit");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    assert_eq!(tier, old_tier);
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery");
    assert_eq!(
        recovered.table.space().life_origin().to_raw(),
        old_tier.flushed,
        "the new life starts at the OLD watermark"
    );
    // Every manifested range reads back whole and CRC-clean…
    for range in &tier.files {
        let len = usize::try_from(range.durable_len.min(2048)).expect("fits");
        assert!(
            read_manifested(&fs, &recovered.flush, range.base, len).is_some(),
            "manifested range {}..{} readable",
            range.base,
            range.end()
        );
    }
    // …and every surviving cold file is exactly a manifested one (the
    // batch-1 files beyond the old unit are gone; the old unit's partial
    // file either resealed at its manifested length or — when batch 1's
    // rotation sealed it — survives with inert excess, ADR-0057 D5).
    let survivors: Vec<String> = fs
        .list_dir(&Path::new(SHARD).join("cold"))
        .expect("cold dir")
        .into_iter()
        .filter(|name| inf_log::parse_tier_file_name(name).is_some())
        .collect();
    assert_eq!(
        survivors.len(),
        tier.files.len(),
        "exactly the manifested files survive: {survivors:?}"
    );
    assert!(recovered.stats.files_removed > 0, "the window actually created garbage");
}

/// `dir_fsync_fail` at the epoch-2 swap's step 6 — the checkpoint ↔
/// truncation window: the rename landed, the name's durability barrier
/// died. Old-or-new are both complete units; whichever the boot reads
/// resolves (`unit-resolves`). MemFs kill semantics surface the NEW
/// name; the old-name half of the ambiguity is exactly the
/// rename-failure row above.
#[test]
fn manifest_v2_dir_fsync_crash_resolves_new_unit() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);
    fill_and_flush(&mut table, &mut flush, 1, 400);
    fault::arm("dir_fsync_fail", FaultSpec::Nth(1));
    let tiers = vec![table.tier_manifest(NS.0, &flush)];
    let new_flushed = tiers[0].flushed;
    let err = write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 2,
            begin_lsn: Lsn::new(SegmentId(2), 64),
            segments: vec![SegmentId(2)],
            tiers,
        },
    )
    .expect_err("the barrier after the rename dies");
    assert!(err.to_string().contains("dir_fsync_fail"), "typed + named: {err}");
    assert!(fault::fired("dir_fsync_fail") >= 1, "the row is not vacuous");
    fault::disarm_all();
    drop((table, flush)); // crash

    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    assert_eq!(manifest.ckpt_id, 2, "the renamed unit is visible after this crash shape");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    assert_eq!(tier.flushed, new_flushed);
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery");
    assert_eq!(recovered.table.space().life_origin().to_raw(), new_flushed);
    // A trailing file with nothing confirmed is legally un-named by the
    // unit (durable_len 0 entries are never manifested) and is removed
    // as dead-life garbage; everything named survives.
    assert!(recovered.stats.files_removed <= 1, "only a zero-confirmed trailing file may go");
    for range in &tier.files {
        let len = usize::try_from(range.durable_len.min(2048)).expect("fits");
        assert!(read_manifested(&fs, &recovered.flush, range.base, len).is_some());
    }
}

/// `tier_torn_frame` driven end-to-end through MANIFEST v2 — the
/// mid-tier-file window with the manifested length read from disk, not
/// supplied by the harness (the S11 deviation retired): recovery
/// truncates the torn tail to the manifested watermark, reseals
/// `Recovered`, and the manifested prefix reads back clean
/// (`reseal-at-watermark`).
#[test]
fn tier_torn_frame_reseal_from_manifest_v2() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);
    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();

    // Un-manifested appends after publication, one of which tears (the
    // lying-disk shape: the call succeeds).
    fault::arm("tier_torn_frame", FaultSpec::Nth(1));
    for i in 0..200u32 {
        let key = format!("torn:{i:04}");
        let hash = TieredTable::hash_key(key.as_bytes());
        table.insert(key.as_bytes(), &[0x77; 200], hash).expect("fits");
    }
    loop {
        let sealed = table.seal_slice();
        let f = match table.flush_slice(&mut flush) {
            Ok(f) => f,
            Err(_) => break, // a torn write may also surface typed later
        };
        if sealed + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
    assert!(fault::fired("tier_torn_frame") >= 1, "the row is not vacuous");
    fault::disarm_all();
    drop((table, flush)); // crash

    // Recovery from the durable manifest alone.
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery");
    assert!(
        recovered.stats.files_resealed + recovered.stats.files_removed > 0,
        "the torn window left un-manifested state to clean"
    );
    // The manifested catalog carries exactly the manifested ranges, and
    // every one reads back CRC-clean; the resealed file's footer sits at
    // its manifested length.
    for (range, meta) in tier.files.iter().zip(recovered.flush.sealed()) {
        assert_eq!(meta.id, range.id);
        assert_eq!(meta.data_len, range.durable_len, "catalog carries manifested lengths");
        if meta.reason == SealReason::Recovered {
            let image = fs.contents(&meta.path).expect("file exists");
            let summary = inf_log::inspect_tier_bytes(&image).expect("sealed image parses");
            assert_eq!(summary.sealed.expect("sealed").data_len, range.durable_len);
            assert_eq!(summary.first_bad_frame, None, "every retained frame verifies");
        }
        let len = usize::try_from(range.durable_len.min(2048)).expect("fits");
        assert!(read_manifested(&fs, &recovered.flush, range.base, len).is_some());
    }
    // Recovery of recovery: running it again from the same durable state
    // is a no-op fast path (the reseal is terminal and idempotent).
    let again = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery is idempotent");
    assert_eq!(again.stats.files_resealed, 0, "second boot takes the sealed fast path");
    assert_eq!(again.stats.files_removed, 0);
}

/// `tier_fsync_err` with a published unit — the fatal class freezes the
/// watermark, and a manifest built from the frozen state never names a
/// byte past it (publication cannot outrun a frozen watermark).
#[test]
fn tier_fsync_frozen_watermark_bounds_manifest() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    let frozen = table.space().flushed().to_raw();
    for i in 0..32u32 {
        let key = format!("f:{i:04}");
        let hash = TieredTable::hash_key(key.as_bytes());
        table.insert(key.as_bytes(), &[0x55; 200], hash).expect("fits");
    }
    table.seal_slice();
    fault::arm("tier_fsync_err", FaultSpec::Nth(1));
    let err = table.flush_slice(&mut flush).expect_err("the barrier dies");
    assert!(err.is_fatal(), "the §8.4 class");
    assert!(fault::fired("tier_fsync_err") >= 1, "the row is not vacuous");
    fault::disarm_all();
    assert_eq!(table.space().flushed().to_raw(), frozen, "watermark frozen");
    let section = table.tier_manifest(NS.0, &flush);
    assert_eq!(section.flushed, frozen);
    assert!(
        section.files.iter().all(|f| f.end() <= frozen),
        "no manifested range outruns the frozen watermark"
    );
}

/// Deletes a fraction of file 0's cold records through the live path
/// (index + accounting only, §3.3 — lengths read from the manifested
/// bytes' headers, the caller's verified view).
fn kill_cold_prefix(
    fs: &MemFs,
    table: &mut TieredTable,
    flush: &TierFlush<MemFs>,
    keys: u32,
    fraction_pct: u32,
) -> u32 {
    let file = table.live_set().files()[0].clone();
    let mut killed = 0u32;
    let target = u64::from(fraction_pct) * file.data_len / 100;
    let mut dead = 0u64;
    for i in 0..keys {
        if dead >= target {
            break;
        }
        let key = format!("r:0:{i:04}");
        let hash = TieredTable::hash_key(key.as_bytes());
        let inf_store::TieredLookup::Cold(addr) = table.lookup(key.as_bytes(), hash, &[]) else {
            continue;
        };
        if addr.to_raw() >= file.base + file.data_len {
            continue;
        }
        let head = read_manifested(fs, flush, addr.to_raw(), TieredTable::RECORD_HEADER_LEN)
            .expect("record header readable");
        let len = TieredTable::record_len_from_header(&head);
        table.delete(hash, addr, len);
        dead += len as u64;
        killed += 1;
    }
    assert!(killed > 0, "the kill pass found cold records in file 0");
    killed
}

/// Runs copy-forward to completion over whatever is eligible, feeding
/// chunks from the manifested bytes.
fn compact_to_idle(fs: &MemFs, table: &mut TieredTable, flush: &TierFlush<MemFs>) -> u64 {
    let mut relocated = 0u64;
    let mut budget = PAGE * 2;
    while let CompactionWork::Read { file_id, addr, len } =
        table.compaction_work(flush, false, budget)
    {
        let chunk =
            read_manifested(fs, flush, addr.to_raw(), len as usize).expect("scan chunk readable");
        let applied = table.compaction_apply(file_id, addr, &chunk);
        relocated += u64::from(applied.relocated);
        budget = if applied.need > 0 { applied.need } else { PAGE * 2 };
        assert!(!applied.stalled, "this workload never fills the window");
    }
    relocated
}

/// The sixth ADR-0057 D8 window, abort half (`manifest_rename_fail`
/// over the *covering* swap — `serves-from-prior-unit`): copy-forward
/// empties a file and the checkpoint that would cover its retirement
/// dies at the commit point. The old unit — which still names the
/// file — stays authoritative, and every one of its manifested ranges
/// (the emptied file included) reads back exactly. An unlink before
/// the landed swap would brick exactly this boot; the deferred
/// pipeline never unlinks here because commit never runs.
#[test]
fn s15_covering_swap_abort_serves_from_prior_unit() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);
    let old_tier = {
        let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
        manifest.tier_ns(NS.0).expect("tier section").clone()
    };

    // Empty file 0: 60% by user deletes, the rest by copy-forward.
    kill_cold_prefix(&fs, &mut table, &flush, 400, 60);
    let relocated = compact_to_idle(&fs, &mut table, &flush);
    assert!(relocated > 0, "the survivors relocated");
    let first = table.live_set().files()[0].clone();
    assert!(first.is_dead() && first.byte_exact, "file 0 emptied and finalized");

    // The covering checkpoint begins after the last removal, marks the
    // file retiring — and its swap dies at the commit point.
    table.begin_ckpt_walk(2);
    table.end_ckpt_walk();
    assert_eq!(table.retire_scan(2, &flush), 1);
    fault::arm("manifest_rename_fail", FaultSpec::Nth(1));
    let err = write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 2,
            begin_lsn: Lsn::new(SegmentId(1), 64),
            segments: vec![SegmentId(1)],
            tiers: vec![table.tier_manifest(NS.0, &flush)],
        },
    )
    .expect_err("the covering swap dies at its commit point");
    assert!(err.to_string().contains("manifest_rename_fail"), "typed + named: {err}");
    assert!(fault::fired("manifest_rename_fail") >= 1, "the row is not vacuous");
    fault::disarm_all();
    table.abort_retirement();
    drop((table, flush)); // crash — nothing was unlinked

    // The old unit recovers; the emptied file is still named, still
    // present, and every manifested byte still serves.
    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    assert_eq!(manifest.ckpt_id, 1, "the aborted swap left the old unit");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    assert_eq!(tier, old_tier, "the old unit still names the emptied file");
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery");
    for range in &tier.files {
        let len = usize::try_from(range.durable_len.min(2048)).expect("fits");
        assert!(
            read_manifested(&fs, &recovered.flush, range.base, len).is_some(),
            "manifested range of file {} reads back",
            range.id
        );
    }
}

/// The sixth window, landed half (`dir_fsync_fail` over the covering
/// swap — `unit-resolves`): the rename lands but the barrier fails.
/// The new unit — which excludes the retired file — resolves; boot
/// GC deletes the on-disk orphan (the swap ↔ unlink crash cover), and
/// every named range reads back.
#[test]
fn s15_covering_swap_dir_fsync_resolves_and_boot_gc_reclaims() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);

    kill_cold_prefix(&fs, &mut table, &flush, 400, 60);
    compact_to_idle(&fs, &mut table, &flush);
    let first = table.live_set().files()[0].clone();
    let first_path = flush.sealed()[0].path.clone();
    assert!(first.is_dead());

    table.begin_ckpt_walk(2);
    table.end_ckpt_walk();
    assert_eq!(table.retire_scan(2, &flush), 1);
    fault::arm("dir_fsync_fail", FaultSpec::Nth(1));
    let _err = write_manifest(
        &fs,
        Path::new(SHARD),
        &Manifest {
            ckpt_id: 2,
            begin_lsn: Lsn::new(SegmentId(1), 64),
            segments: vec![SegmentId(1)],
            tiers: vec![table.tier_manifest(NS.0, &flush)],
        },
    )
    .expect_err("the barrier fails after the rename");
    assert!(fault::fired("dir_fsync_fail") >= 1, "the row is not vacuous");
    fault::disarm_all();
    // The plane cannot know the rename landed: it aborts (marks roll
    // back, nothing unlinks) and the process dies.
    table.abort_retirement();
    drop((table, flush)); // crash — the retired file is still on disk

    let manifest = read_manifest(&fs, Path::new(SHARD)).expect("read").expect("present");
    assert_eq!(manifest.ckpt_id, 2, "the renamed unit resolves after this crash shape");
    let tier = manifest.tier_ns(NS.0).expect("tier section").clone();
    assert!(tier.files.iter().all(|f| f.id != first.id), "the new unit excludes the file");
    let recovered = recover_tiered_ns(
        fs.clone(),
        &tier,
        manifest.ckpt_id,
        flush_config(),
        space_config(0),
        demote(),
        256,
    )
    .expect("recovery");
    assert!(recovered.stats.files_removed >= 1, "boot GC reclaimed the un-named file");
    assert!(fs.contents(&first_path).is_none(), "the orphan's bytes are gone");
    for range in &tier.files {
        let len = usize::try_from(range.durable_len.min(2048)).expect("fits");
        assert!(read_manifested(&fs, &recovered.flush, range.base, len).is_some());
    }
}

/// `tier_unlink_fail` (`reclaim-deferred-nonfatal`): the unlink of a
/// retired, detached file fails typed — nothing else changes, and both
/// re-drives work (the in-life retry and the boot GC).
#[test]
fn s15_unlink_failure_is_nonfatal_and_redriven() {
    let fs = MemFs::new();
    let (mut table, mut flush) = rig(&fs);
    fill_and_flush(&mut table, &mut flush, 0, 400);
    publish(&fs, &table, &flush, 1);

    // Fully dead by user deletes alone — no relocation involved (the
    // stamp generalization: any slot-removal stamps, ADR-0059 D3).
    kill_cold_prefix(&fs, &mut table, &flush, 400, 100);
    let first = table.live_set().files()[0].clone();
    assert!(first.is_dead());
    table.begin_ckpt_walk(2);
    table.end_ckpt_walk();
    assert_eq!(table.retire_scan(2, &flush), 1);
    publish(&fs, &table, &flush, 2); // the covering swap lands
    let ids = table.commit_retirement();
    assert_eq!(ids, vec![first.id]);
    let meta = flush.detach_sealed(first.id).expect("detach");

    fault::arm("tier_unlink_fail", FaultSpec::Nth(1));
    let err = inf_log::flush::unlink_tier_file(&fs, &meta).expect_err("unlink fails typed");
    assert!(err.to_string().contains("tier_unlink_fail"), "typed + named: {err}");
    assert!(fault::fired("tier_unlink_fail") >= 1, "the row is not vacuous");
    fault::disarm_all();
    assert!(fs.contents(&meta.path).is_some(), "the failure changed nothing on disk");

    // Re-drive 1: the in-life retry.
    inf_log::flush::unlink_tier_file(&fs, &meta).expect("the retry succeeds");
    assert!(fs.contents(&meta.path).is_none(), "space returned");

    // Re-drive 2: a crash before any retry — boot GC (the manifest no
    // longer names the file, ADR-0057 D6-1) — is proven by
    // `s15_covering_swap_dir_fsync_resolves_and_boot_gc_reclaims`.
}

/// The S15 rows are well-formed and carried here (self-policing).
#[test]
fn s15_rows_are_carried_here() {
    let def = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    let expects = ["serves-from-prior-unit", "reclaim-deferred-nonfatal"];
    for expect in expects {
        assert!(
            def.rows.iter().any(|r| r.test == "recovery_v2.rs" && r.expect == expect),
            "the {expect} row is declared"
        );
    }
    assert!(
        def.rows.iter().any(|r| r.point == "tier_unlink_fail"),
        "the new fault point has its row"
    );
}

/// The S12 rows are well-formed and carried here (self-policing).
#[test]
fn s12_rows_are_carried_here() {
    let def = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    let here: Vec<_> = def.rows.iter().filter(|r| r.test == "recovery_v2.rs").collect();
    assert!(here.len() >= 3, "the S12 windows have rows");
    for row in here {
        assert_eq!(row.tier, "node");
        assert!(
            inf_log::fault::ALL.contains(&row.point.as_str()),
            "row {:?} names a declared point",
            row.point
        );
    }
}
