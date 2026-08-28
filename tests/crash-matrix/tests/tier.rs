//! M4-S11 tier-file crash rows (`m4.toml`, ADR-0056 D6): each named
//! tier fault point injects its documented failure, the process "dies"
//! (the writer/pipeline drops), and recovery re-proves the D5 contract —
//! the file is valid exactly to the **manifested** watermark: retained
//! frames CRC-verify, everything beyond (torn or clean alike) is
//! dead-life garbage and is gone, and the file reseals `Recovered`.
//!
//! The claim rule makes the torn-tail argument structural: `flushed`
//! (hence the manifested watermark) only ever covers full, final frames
//! while a file is unsealed, and the partial tail frame — the only frame
//! ever rewritten — is claimable strictly at seal. A torn write can
//! therefore never land under the watermark; these tests assert the
//! recovery half of that pair.

use std::path::Path;

use crash_matrix::load_matrix;
use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::manifest::{TierFileRange, TierNsManifest};
use inf_log::{
    NsId, SealReason, TIER_FOOTER_BYTES, TIER_FRAME_BYTES, TIER_FRAME_DATA, TierDrive, TierFlush,
    TierFlushConfig, TierIdentity, TierIoMode, TierWriter, inspect_tier_bytes,
};
use inf_store::KeyHasher;
use inf_store::{AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig};

const NS: NsId = NsId(21);
const PAGE: u64 = 4 << 10;

fn seed_count(default: u64) -> u64 {
    std::env::var("CRASH_MATRIX_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn identity() -> TierIdentity {
    TierIdentity { cell: 0, ns: NS, base: LogicalAddr::ZERO }
}

fn writer(fs: &MemFs, id: u32) -> TierWriter<MemFs> {
    TierWriter::create(fs, Path::new("shard-0"), id, 0, NS, LogicalAddr::ZERO, TierIoMode::Buffered)
        .expect("create tier file")
}

/// A small tiered table + pipeline pair for the pipeline-level rows.
fn table_and_flush(fs: &MemFs) -> (Keyspace, TierFlush<MemFs>) {
    let demote = DemotionConfig::for_budget(4 << 20, PAGE);
    let ring = demote.ring_reserve_bytes().expect("valid budget");
    let mut ks = Keyspace::new(StoreConfig::default());
    assert!(
        ks.materialize_tiered(
            NS,
            AddressSpaceConfig {
                reserve_bytes: ring,
                page_bytes: PAGE as usize,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            128,
        )
        .is_ok()
    );
    let flush = TierFlush::new(
        fs.clone(),
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: 1 << 20,
            slice_bytes: PAGE,
        },
        0,
    );
    (ks, flush)
}

fn fill(ks: &mut Keyspace, keys: u32) {
    fill_batch(ks, keys, 0);
}

fn fill_batch(ks: &mut Keyspace, keys: u32, batch: u32) {
    let table = ks.tiered_store_mut(NS).expect("materialized");
    for i in 0..keys {
        let key = format!("crash:{batch}:{i:04}");
        let value = vec![0x61 + (i % 20) as u8; 100 + (i as usize % 60)];
        let hash = KeyHasher::default().hash(key.as_bytes());
        table.insert(key.as_bytes(), &value, hash).expect("fits");
    }
    let tail = table.space().tail();
    table.space_mut().advance_ro_boundary(tail);
}

/// `tier_short_write` — the write is cut short and the append FAILS
/// typed; the flushed watermark does not move (`append-fails-typed`).
#[test]
fn tier_short_write_append_fails_typed() {
    let fs = MemFs::new();
    let (mut ks, mut flush) = table_and_flush(&fs);
    fill(&mut ks, 64);
    fault::arm("tier_short_write", FaultSpec::Nth(1));
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let flushed_before = table.space().flushed();
    let err = table.flush_slice(&mut flush).expect_err("short write fails the slice");
    assert!(!err.is_fatal(), "a short write is the I/O class, not the fsync class");
    assert!(err.to_string().contains("tier_short_write"), "typed + named: {err}");
    assert_eq!(table.space().flushed(), flushed_before, "watermark unmoved on failure");
    assert!(fault::fired("tier_short_write") >= 1, "the row is not vacuous");
    fault::disarm_all();
}

/// `tier_torn_frame` — a prefix lands and the call SUCCEEDS (lying-disk
/// physics); after the crash, recovery truncates to the manifested
/// watermark, every retained frame verifies, and the file seals
/// `Recovered` (`reseal-at-watermark`). Seeds vary which append tears.
#[test]
fn tier_torn_frame_reseal_at_watermark() {
    for seed in 0..seed_count(3) {
        let fs = MemFs::new();
        let mut w = writer(&fs, 0);
        // Manifested prefix: three full frames, synced — the claim rule
        // means the manifested watermark covers only these.
        let manifested = 3 * TIER_FRAME_DATA as u64;
        w.append(LogicalAddr::ZERO, &vec![0x11; manifested as usize]).expect("append");
        w.sync().expect("sync");
        assert_eq!(w.confirmable_len(), manifested);
        // Un-manifested appends, one of which tears (seed-picked) — the
        // call succeeds, the disk lies.
        fault::arm("tier_torn_frame", FaultSpec::Nth(seed + 1));
        let mut at = manifested;
        // Per-append barriers so each staged batch reaches the device as
        // its own write: the seed-picked one tears, and a later clean
        // write extends the image past it — the torn frame sits inside
        // the whole-block span (CRC-detectable, not just truncated),
        // the lying-disk shape where later writes land around the tear.
        for _ in 0..seed + 2 {
            w.append(LogicalAddr::from_raw(at).expect("fits"), &[0x22; TIER_FRAME_DATA])
                .expect("append");
            w.sync().expect("torn write still reports success");
            at += TIER_FRAME_DATA as u64;
        }
        assert!(fault::fired("tier_torn_frame") >= 1, "the row is not vacuous");
        fault::disarm_all();
        let path = w.path().to_path_buf();
        drop(w); // crash
        // Pre-recovery: the torn frame is CRC-detectable in the image.
        let image = fs.contents(&path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("unsealed image parses");
        assert!(summary.sealed.is_none(), "no footer — the file is unsealed");
        assert!(
            summary.first_bad_frame.is_some_and(|f| f >= 3),
            "the torn frame is beyond the manifested prefix (claim rule) and CRC-detected"
        );
        // Recovery: valid exactly to the manifested watermark.
        let path = TierWriter::<MemFs>::recover_seal_existing(
            &fs,
            Path::new("shard-0"),
            0,
            identity(),
            manifested,
            TierIoMode::Buffered,
        )
        .expect("recover");
        let image = fs.contents(&path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("sealed image parses");
        let footer = summary.sealed.expect("resealed");
        assert_eq!(footer.data_len, manifested);
        assert_eq!(footer.reason, SealReason::Recovered);
        assert_eq!(summary.first_bad_frame, None, "every retained frame verifies");
        assert_eq!(
            image.len(),
            4096 + 3 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES,
            "torn/un-manifested frames are gone"
        );
    }
}

/// `tier_fsync_err` — the barrier fails: the fatal typed class surfaces,
/// and the flushed watermark freezes at the last good barrier
/// (`fail-stop`).
#[test]
fn tier_fsync_err_fail_stop() {
    let fs = MemFs::new();
    let (mut ks, mut flush) = table_and_flush(&fs);
    fill(&mut ks, 64);
    let table = ks.tiered_store_mut(NS).expect("materialized");
    // One clean slice establishes a good barrier.
    let outcome = table.flush_slice(&mut flush).expect("clean slice");
    assert!(outcome.appended_bytes > 0);
    let frozen = table.space().flushed();
    // A second sealed batch gives the failing slice real work.
    fill_batch(&mut ks, 64, 1);
    let table = ks.tiered_store_mut(NS).expect("materialized");
    fault::arm("tier_fsync_err", FaultSpec::Nth(1));
    let err = table.flush_slice(&mut flush).expect_err("fsync fails the slice");
    assert!(err.is_fatal(), "the §8.4 class");
    assert!(err.to_string().contains("FATAL"), "the message says stop: {err}");
    assert_eq!(table.space().flushed(), frozen, "watermark frozen at the last good barrier");
    assert!(fault::fired("tier_fsync_err") >= 1, "the row is not vacuous");
    fault::disarm_all();
}

/// `tier_footer_torn` — crash between data durability and footer
/// durability: the file recovers as unsealed, then reseals at the
/// manifested watermark (`reseal-at-watermark`); the torn footer block
/// is dropped by the truncation.
#[test]
fn tier_footer_torn_reseal_at_watermark() {
    let fs = MemFs::new();
    let mut w = writer(&fs, 0);
    let manifested = 2 * TIER_FRAME_DATA as u64;
    w.append(LogicalAddr::ZERO, &vec![0x33; (manifested + 500) as usize]).expect("append");
    w.sync().expect("sync");
    assert_eq!(w.confirmable_len(), manifested, "partial tail frame not claimable");
    fault::arm("tier_footer_torn", FaultSpec::Nth(1));
    let path = w.path().to_path_buf();
    let err = w.seal(SealReason::Shutdown).expect_err("the seal dies mid-footer");
    assert!(err.to_string().contains("tier_footer_torn"), "typed + named: {err}");
    assert!(fault::fired("tier_footer_torn") >= 1, "the row is not vacuous");
    fault::disarm_all();
    // Pre-recovery: no valid footer — the image is unsealed.
    let image = fs.contents(&path).expect("file exists");
    let summary = inspect_tier_bytes(&image).expect("unsealed image parses");
    assert!(summary.sealed.is_none(), "a torn footer never reads as sealed");
    // Recovery reseals at the manifested watermark; the seal is redone.
    let path = TierWriter::<MemFs>::recover_seal_existing(
        &fs,
        Path::new("shard-0"),
        0,
        identity(),
        manifested,
        TierIoMode::Buffered,
    )
    .expect("recover");
    let image = fs.contents(&path).expect("file exists");
    let summary = inspect_tier_bytes(&image).expect("sealed image parses");
    let footer = summary.sealed.expect("resealed");
    assert_eq!(footer.data_len, manifested);
    assert_eq!(footer.reason, SealReason::Recovered);
    assert_eq!(summary.first_bad_frame, None);
}

/// `tier_write_nospace` — the M4-S21 ENOSPC row
/// (`diskfull-typed-then-automatic-recovery`, ADR-0063 D4): the disk
/// stays full (`FromNth`), the slice fails typed **non-fatal** with the
/// watermark frozen, foreground admission latches `DISKFULL (Device)`,
/// and once space frees the ordinary MAINTAIN retry relands the
/// retained backlog, clears the latch, and admission resumes — no
/// operator step.
#[test]
fn tier_write_nospace_diskfull_typed_then_automatic_recovery() {
    let fs = MemFs::new();
    let (mut ks, mut flush) = table_and_flush(&fs);
    fill(&mut ks, 64);
    fault::arm("tier_write_nospace", FaultSpec::FromNth(1));
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let flushed_before = table.space().flushed();
    let err = table.flush_slice(&mut flush).expect_err("ENOSPC fails the slice");
    assert!(!err.is_fatal(), "write-time exhaustion is the graceful leg (M2 precedent)");
    assert!(err.is_storage_full(), "classified: {err}");
    assert_eq!(table.space().flushed(), flushed_before, "watermark frozen on failure");
    assert_eq!(
        table.disk_full(),
        Some(inf_store::DiskFullCause::Device),
        "foreground latches DISKFULL instead of an opaque stall"
    );
    let refusal = table
        .insert(b"crash:more", &[0x33; 128], KeyHasher::default().hash(b"crash:more"))
        .expect_err("foreground refuses while latched");
    assert!(matches!(refusal, inf_store::OpError::DiskFull(inf_store::DiskFullCause::Device)));
    assert!(fault::fired("tier_write_nospace") >= 1, "the row is not vacuous");
    // Space frees: the same MAINTAIN retry is the recovery probe — the
    // latch-probe barrier rewrites the retained frames at their own
    // offsets. (This rig's coarse single-chunk shape leaves the one
    // recorded chunk end inside the partial tail frame, so `flushed`
    // itself advances at the seal — the ADR-0056 D5 holdback.)
    fault::disarm_all();
    let _ = table.flush_slice(&mut flush).expect("the retry succeeds");
    assert!(table.disk_full().is_none(), "the latch cleared — admission resumed");
    table
        .insert(b"crash:more", &[0x33; 128], KeyHasher::default().hash(b"crash:more"))
        .expect("writes resume with no operator step");
    // The relanded backlog is durable: the shutdown drain seals, and the
    // watermark advances over every sealed byte.
    let ro = table.space().ro_boundary().to_raw();
    table.flush_drain(&mut flush).expect("drain");
    assert_eq!(table.space().flushed().to_raw(), ro, "the whole backlog became durable");
}

/// `tier_write_nospace` — the kill-mid-exhaustion row
/// (`reseal-at-watermark`): ENOSPC adds no new crash shape, because
/// nothing a refused write touched was ever claimable — after the kill,
/// the file recovers valid exactly to the manifested watermark, the
/// standing D5 contract.
#[test]
fn tier_write_nospace_kill_recovers_at_watermark() {
    for seed in 0..seed_count(3) {
        let fs = MemFs::new();
        let mut w = writer(&fs, 0);
        let manifested = 3 * TIER_FRAME_DATA as u64;
        w.append(LogicalAddr::ZERO, &vec![0x11; manifested as usize]).expect("append");
        w.sync().expect("sync");
        assert_eq!(w.confirmable_len(), manifested);
        // The disk fills at a seed-picked write; every later attempt
        // refuses too (exhaustion persists until the crash).
        fault::arm("tier_write_nospace", FaultSpec::FromNth(seed + 1));
        let mut at = manifested;
        for _ in 0..seed + 2 {
            match w.append(LogicalAddr::from_raw(at).expect("fits"), &[0x22; TIER_FRAME_DATA]) {
                Ok(()) => {
                    at += TIER_FRAME_DATA as u64;
                    if w.sync().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(fault::fired("tier_write_nospace") >= 1, "the row is not vacuous");
        fault::disarm_all();
        drop(w); // crash with the device still full
        let path = TierWriter::<MemFs>::recover_seal_existing(
            &fs,
            Path::new("shard-0"),
            0,
            identity(),
            manifested,
            TierIoMode::Buffered,
        )
        .expect("recover");
        let image = fs.contents(&path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("sealed image parses");
        let footer = summary.sealed.expect("resealed");
        assert_eq!(footer.data_len, manifested, "valid exactly to the manifested watermark");
        assert_eq!(footer.reason, SealReason::Recovered);
        assert_eq!(summary.first_bad_frame, None, "every retained frame verifies");
    }
}

// ---- M4.5-S31 reactor-drive cut windows (ADR-0084 D4) ----
//
// The two windows the driver-op conversion opens are cut-timing
// windows, not fault points, so they carry as named power-cut tests
// (the `documents.rs` precedent) rather than `[[row]]` entries — the
// row schema requires a declared fault point. m4.toml documents the
// pair next to the tier rows.

fn reactor_pipeline(disk: &SimDisk) -> TierFlush<SimDisk> {
    let mut flush = TierFlush::new(
        disk.clone(),
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: 1 << 20,
            slice_bytes: PAGE,
        },
        0,
    );
    flush.set_drive(TierDrive::Reactor);
    flush
}

/// Executes the staged round's writes; barriers only when `sync_too`.
fn run_reactor_round(disk: &SimDisk, flush: &mut TierFlush<SimDisk>, sync_too: bool) {
    let writes = flush.round_write_count();
    for index in 0..writes {
        let op = flush.round_op(index);
        disk.driver_write_at(op.fd, op.offset, op.bytes).expect("driver write");
    }
    if sync_too {
        for index in writes..flush.round_op_count() {
            let op = flush.round_op(index);
            disk.driver_fdatasync(op.fd).expect("driver barrier");
        }
    }
}

fn apply_effects(flush: &mut TierFlush<SimDisk>) {
    for effect in flush.finish_round() {
        match effect {
            inf_log::RoundEffect::DurableTo { data_len } => flush.confirm_durable_to(data_len),
            inf_log::RoundEffect::SealCommit => flush.commit_oldest_seal(),
            inf_log::RoundEffect::GapCross { .. } => {}
        }
    }
}

fn sim_image(disk: &SimDisk, path: &Path) -> Vec<u8> {
    let file = disk.open_read(path).expect("file exists");
    let size = file.file_size().expect("size") as usize;
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = file.read_at(read as u64, &mut bytes[read..]).expect("read");
        assert!(n > 0, "no EOF inside the image");
        read += n;
    }
    bytes
}

/// Window 1: the seal's footer **write** completed but its fdatasync
/// never did — the cut lands between wave 1 and wave 2 of a sealing
/// round. On disk a plausible (never-synced) footer may survive the
/// cut; the catalog never committed, so recovery's authority is the
/// manifested watermark: the file reseals `Recovered` exactly there,
/// dropping the unsynced footer and tail (ADR-0056 D5 at the reactor
/// tier — the same window the seam drive had between `write_at(footer)`
/// and `sync_data()`).
#[test]
fn reactor_seal_barrier_lost_reseals_at_the_manifested_watermark() {
    for seed in 0..seed_count(3) {
        let disk = SimDisk::new();
        let mut flush = reactor_pipeline(&disk);
        // Round 1 completes whole: two full frames + a partial tail are
        // durable; the manifested watermark is the full-frame floor.
        let round1 = vec![0x11u8; 2 * TIER_FRAME_DATA + 500];
        flush.append_range_queued(LogicalAddr::ZERO, &round1).expect("stage");
        flush.sync_queued();
        run_reactor_round(&disk, &mut flush, true);
        apply_effects(&mut flush);
        let manifested = flush.confirmable_end().expect("round 1 confirmed");
        assert_eq!(manifested, (2 * TIER_FRAME_DATA) as u64, "full-frame claim floor");
        // Round 2 stages more bytes and a gap seal — footer included —
        // and the cut lands after the writes, before any barrier.
        let at = LogicalAddr::from_raw(round1.len() as u64).expect("fits");
        flush.append_range_queued(at, &[0x22u8; 700]).expect("stage");
        flush.seal_for_gap_queued(90_000);
        assert_eq!(flush.pending_seal_count(), 1, "the seal is staged, not committed");
        run_reactor_round(&disk, &mut flush, false);
        disk.power_cut(0x51EA_1000 ^ seed);

        let path = TierWriter::<SimDisk>::recover_seal_existing(
            &disk,
            Path::new("shard-0"),
            0,
            identity(),
            manifested,
            TierIoMode::Buffered,
        )
        .expect("recovery reseals at the manifested watermark");
        let image = sim_image(&disk, &path);
        let summary = inspect_tier_bytes(&image).expect("valid sealed image");
        let footer = summary.sealed.expect("resealed");
        assert_eq!(footer.data_len, manifested, "sealed exactly at the watermark");
        assert_eq!(footer.reason, SealReason::Recovered);
        assert_eq!(summary.first_bad_frame, None, "every retained frame verifies");
        assert_eq!(
            image.len(),
            inf_log::TIER_HEADER_BYTES + 2 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES,
            "the unsynced footer and tail are gone (seed {seed})"
        );
    }
}

/// Window 2: the seal **completed** (footer durable, catalog committed)
/// but no MANIFEST ever named it — the cut lands between the round's
/// completion and the next checkpoint. Recovery takes the older
/// manifest's unsealed range and the sealed fast path keeps the file
/// **clamped to the manifested prefix** — the physical excess is inert,
/// never served, never corruption (ADR-0057 D5; the WAL still covers
/// the excess records, so replay reproduces them in the new life).
#[test]
fn reactor_sealed_file_beyond_the_manifest_is_clamped_inert() {
    let disk = SimDisk::new();
    let mut flush = reactor_pipeline(&disk);
    // Round 1 completes; a checkpoint at this instant manifests the
    // full-frame floor of the round's bytes.
    let round1 = vec![0x33u8; 2 * TIER_FRAME_DATA + 400];
    flush.append_range_queued(LogicalAddr::ZERO, &round1).expect("stage");
    flush.sync_queued();
    run_reactor_round(&disk, &mut flush, true);
    apply_effects(&mut flush);
    let manifested = flush.confirmable_end().expect("round 1 confirmed");
    let manifest = TierNsManifest {
        ns: NS.0,
        flushed: manifested,
        files: vec![TierFileRange { id: 0, base: 0, durable_len: manifested }],
    };
    // Round 2 seals the file whole — durable on disk, committed to the
    // in-memory catalog — and the cut lands before any covering swap.
    let at = LogicalAddr::from_raw(round1.len() as u64).expect("fits");
    flush.append_range_queued(at, &[0x44u8; 600]).expect("stage");
    flush.seal_for_gap_queued(90_000);
    run_reactor_round(&disk, &mut flush, true);
    apply_effects(&mut flush);
    let sealed_len = flush.sealed()[0].data_len;
    assert!(sealed_len > manifested, "the window is real: sealed past the manifest");
    disk.power_cut(0x51EA_2000);

    let recovered = inf_store::recover_tiered_ns(
        disk.clone(),
        &manifest,
        1,
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: 1 << 20,
            slice_bytes: PAGE,
        },
        AddressSpaceConfig {
            reserve_bytes: DemotionConfig::for_budget(4 << 20, PAGE)
                .ring_reserve_bytes()
                .expect("valid budget"),
            page_bytes: PAGE as usize,
            life_origin: LogicalAddr::from_raw(manifested).expect("fits"),
        },
        DemotionConfig::for_budget(4 << 20, PAGE),
        128,
        KeyHasher::default(),
    )
    .expect("recovery keeps the sealed file");
    let metas = recovered.flush.sealed();
    assert_eq!(metas.len(), 1, "the sealed file survives");
    assert_eq!(
        metas[0].data_len, manifested,
        "the catalog clamps to the manifested prefix — the excess is inert"
    );
    assert_eq!(
        recovered.table.space().life_origin().to_raw(),
        manifested,
        "the new life starts at the manifested watermark"
    );
}

/// The m4.toml definition itself stays well-formed and every row names
/// a carrying test file (self-policing, the m2 pattern; S12's rows are
/// carried by `recovery_v2.rs`, M4.5-S34/S35's by `fua.rs`, M4.5-S36's
/// by `ickv3.rs` — each polices its own subset).
#[test]
fn m4_rows_are_carried_here() {
    let def = load_matrix(&Path::new(env!("CARGO_MANIFEST_DIR")).join("m4.toml"));
    assert!(def.rows.len() >= 11);
    for row in &def.rows {
        assert_eq!(row.tier, "node", "tier rows are carried by a named test");
        assert!(
            row.test == "tier.rs"
                || row.test == "recovery_v2.rs"
                || row.test == "blob.rs"
                || row.test == "fua.rs"
                || row.test == "ickv3.rs",
            "row {:?} names an unknown carrier {:?}",
            row.point,
            row.test
        );
        assert!(
            inf_log::fault::ALL.contains(&row.point.as_str()),
            "row {:?} names a declared point",
            row.point
        );
    }
}
