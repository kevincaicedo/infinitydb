//! M2-S11 AC: crash at **every** step of the checkpoint-publication +
//! MANIFEST-swap protocol recovers to a valid recovery unit — either the
//! old checkpoint or the new one, never neither, never both-partial
//! (§8.4). The crash model is `MemFs::fail_after_ops`: after N mutating
//! file operations every further op fails, including the dead process's
//! own best-effort cleanup — exactly what a crash leaves behind (fsync
//! reordering/loss of un-synced writes is the M2-S18 sim disk's stricter
//! model; this matrix covers protocol-step atomicity).
//!
//! The resolution contract exercised here is the one `inf-server::recover`
//! runs at boot: `read_manifest` names the unit; the named `.ick` must
//! load digest-clean; the floor segment must be present in a floor-aware
//! scan.

use std::io;
use std::path::Path;

use inf_log::ckpt::{IckReaderConfig, SyncIckWriter, ick_file_name, read_ick};
use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, Lsn, Manifest, NsId, RecordView, SegmentId, create_cell_dirs, read_manifest,
    scan_log_dir, scan_log_dir_from, segment_file_name, write_manifest,
};

const NS: NsId = NsId(7);

fn begin(seg: u32, off: u32) -> Lsn {
    Lsn::new(SegmentId(seg), off)
}

/// A shard with segments 0..=2, checkpoint 1 published and named by a
/// durable manifest {ckpt 1, begin (1, 16), segments [1, 2]} — the OLD
/// recovery unit every crash must fall back to.
fn shard_with_old_unit(fs: &MemFs, shard: &Path) {
    let dirs = create_cell_dirs(fs, shard).expect("dirs");
    for id in 0..=2u32 {
        fs.create_segment(&dirs.log.join(segment_file_name(SegmentId(id))), 64).expect("segment");
    }
    let mut w = SyncIckWriter::create(
        fs.clone(),
        &dirs.ckpt,
        &CkptConfig::default(),
        0,
        1,
        begin(1, 16),
        &[NS.0],
    )
    .expect("old ick create");
    w.append(&RecordView::StringPostImage { ns: NS, key: b"old", value: b"unit" })
        .expect("old ick record");
    w.finish().expect("old ick publish");
    write_manifest(
        fs,
        shard,
        &Manifest {
            ckpt_id: 1,
            begin_lsn: begin(1, 16),
            segments: vec![SegmentId(1), SegmentId(2)],
        },
    )
    .expect("old manifest");
}

/// The publication sequence under test — checkpoint 2's `.ick` staging,
/// footer + fdatasync + rename + dir-fsync (`SyncIckWriter::finish`), then
/// the MANIFEST swap. The server runs exactly these protocol classes
/// (driver ops replace the blocking writes on the reactor tier; the step
/// *sequence* is identical).
fn publish_new_unit(fs: &MemFs, shard: &Path) -> io::Result<()> {
    let ckpt_dir = shard.join("ckpt");
    let mut w = SyncIckWriter::create(
        fs.clone(),
        &ckpt_dir,
        &CkptConfig::default(),
        0,
        2,
        begin(2, 32),
        &[NS.0],
    )?;
    w.append(&RecordView::StringPostImage { ns: NS, key: b"new", value: b"unit" })?;
    w.finish()?;
    write_manifest(
        fs,
        shard,
        &Manifest { ckpt_id: 2, begin_lsn: begin(2, 32), segments: vec![SegmentId(2)] },
    )
}

/// Boot-time resolution: the manifest names the unit; everything it names
/// must be whole. Returns the named ckpt id.
fn resolve(fs: &MemFs, shard: &Path) -> u64 {
    let manifest = read_manifest(fs, shard)
        .expect("manifest readable — never torn (envelope CRC + atomic swap)")
        .expect("a manifest exists — the old unit was published before the crash");
    assert!(
        manifest.ckpt_id == 1 || manifest.ckpt_id == 2,
        "old or new, never anything else: {manifest:?}"
    );
    let ick = shard.join("ckpt").join(ick_file_name(manifest.ckpt_id));
    let (info, _audit) = read_ick(fs, &ick, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
        .unwrap_or_else(|err| {
            panic!("manifest names ckpt {} but its .ick does not load: {err:?}", manifest.ckpt_id)
        });
    assert_eq!(info.ckpt_id, manifest.ckpt_id, "named id matches the file header");
    assert_eq!(info.begin_lsn, manifest.begin_lsn, "begin LSN agrees");
    let outcome =
        scan_log_dir_from(fs, &shard.join("log"), manifest.floor()).expect("floor-aware scan");
    assert_eq!(
        outcome.scan.segments().first(),
        Some(&manifest.floor()),
        "the floor segment is present"
    );
    manifest.ckpt_id
}

#[test]
fn crash_at_every_swap_step_recovers_old_or_new_never_both_partial() {
    let mut steps_covered = 0u64;
    loop {
        let fs = MemFs::new();
        let shard = Path::new("/data/shard-0");
        shard_with_old_unit(&fs, shard);

        fs.fail_after_ops(steps_covered);
        let crashed = publish_new_unit(&fs, shard).is_err();
        fs.clear_op_fault(); // the reboot

        let named = resolve(&fs, shard);
        if crashed {
            // Old *or* new: a crash after the manifest rename may still
            // have failed only the trailing dir-fsync — MemFs renames are
            // immediately visible, so the new unit is legal here too. What
            // is never legal: a manifest naming a half-written checkpoint
            // (resolve() would have panicked).
            assert!(named == 1 || named == 2, "crash at step {steps_covered}: unit {named}");
        } else {
            assert_eq!(named, 2, "no crash ⇒ the new unit is named");
            break;
        }
        steps_covered += 1;
        assert!(steps_covered < 64, "publication sequence never completed");
    }
    // The sequence spans ≥ 8 mutating ops (ick create/writes/sync/rename/
    // dir-sync + manifest staging remove-or-create/write/sync/rename/
    // dir-sync) — the matrix must have exercised each one.
    assert!(steps_covered >= 8, "only {steps_covered} crash points exercised");
}

/// Crash mid-truncation: un-fsynced unlinks may survive in any subset, so
/// gaps BELOW the floor are legal and ignored, while the un-manifested
/// tier keeps rejecting gaps (honesty does not regress).
#[test]
fn gaps_below_the_floor_are_stale_not_errors() {
    let fs = MemFs::new();
    let shard = Path::new("/data/shard-0");
    shard_with_old_unit(&fs, shard);
    publish_new_unit(&fs, shard).expect("publish");

    // Truncation unlinked seg-1 but the power cut hit before seg-0's
    // unlink persisted (un-fsynced unlinks survive in any subset): the
    // survivors below floor 2 are {0} with a hole at 1.
    fs.remove_file(&shard.join("log").join(segment_file_name(SegmentId(1)))).expect("unlink");

    let outcome = scan_log_dir_from(&fs, &shard.join("log"), SegmentId(2)).expect("floored scan");
    assert_eq!(outcome.stale, vec![SegmentId(0)], "the survivor below the floor is stale");
    assert_eq!(outcome.scan.segments(), &[SegmentId(2)], "live set starts at the floor");

    // Without a manifest floor the same directory is still a hard error.
    let err = scan_log_dir(&fs, &shard.join("log")).expect_err("gap without a floor");
    assert!(matches!(err, inf_log::ScanError::Gap { .. }), "got {err:?}");
}

/// The old checkpoint + segments must be retained until the NEW manifest
/// is durable: with the swap crashing at its very first op, the old unit
/// resolves untouched (nothing may delete it before the swap returns Ok).
#[test]
fn old_unit_survives_until_the_new_manifest_is_durable() {
    let fs = MemFs::new();
    let shard = Path::new("/data/shard-0");
    shard_with_old_unit(&fs, shard);

    // The new .ick publishes fine; the manifest swap never starts.
    let ckpt_dir = shard.join("ckpt");
    let mut w = SyncIckWriter::create(
        fs.clone(),
        &ckpt_dir,
        &CkptConfig::default(),
        0,
        2,
        begin(2, 32),
        &[NS.0],
    )
    .expect("create");
    w.append(&RecordView::StringPostImage { ns: NS, key: b"new", value: b"unit" }).expect("record");
    w.finish().expect("ick publish");
    fs.fail_after_ops(0);
    let err = write_manifest(
        &fs,
        shard,
        &Manifest { ckpt_id: 2, begin_lsn: begin(2, 32), segments: vec![SegmentId(2)] },
    );
    assert!(err.is_err(), "swap crashed at step 0");
    fs.clear_op_fault();

    assert_eq!(resolve(&fs, shard), 1, "old unit intact; ckpt-2.ick is an unnamed orphan");
}
