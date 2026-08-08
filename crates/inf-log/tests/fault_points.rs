//! M2-S16 AC: every named fault point demonstrably fires and produces its
//! documented failure path (one test per point — the CI inventory check
//! `scripts/check-fault-points.sh` keys on these names). Triggers are the
//! registry's deterministic specs (L7); the storage tier is the
//! fault-injectable `MemFs`. Recovery-policy legs (torn truncation, seal
//! boundary resume through `open_cell_log`) live in
//! `inf-server/tests/fault_recovery.rs`; the crash matrix drives both at
//! M2-S17.

use std::path::PathBuf;

use inf_foundation::fault::{self, FaultSpec};
use inf_log::fs::mem::MemFs;
use inf_log::meta::{read_meta, write_meta};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FrameBuilder, FrameIter, FrameStamp, LogError, Lsn, Manifest, NsId,
    RecordView, SegmentConfig, SegmentId, SegmentRotor, create_cell_dirs, read_manifest,
    scan_log_dir, segment_file_name, write_manifest,
};

/// Canonical v2 stamp for hand-built test frames (epoch 1, covered 0 —
/// attests nothing). `seq` matters only where a test builds sequential
/// frames the recovery policy will walk; readers/scanners ignore it.
fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

const SEGMENT_BYTES: u32 = 4096;

fn cfg() -> SegmentConfig {
    SegmentConfig { segment_bytes: SEGMENT_BYTES, seal_after_ms: None }
}

fn mem_rotor(fs: &MemFs) -> SegmentRotor<MemFs> {
    let dirs = create_cell_dirs(fs, &PathBuf::from("data/shard-0")).expect("dirs");
    SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg()).expect("fresh rotor")
}

fn append_frame(rotor: &mut SegmentRotor<MemFs>, filler: usize) -> Result<Lsn, LogError> {
    let value = vec![0xAB; filler];
    let mut builder = FrameBuilder::new();
    builder.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: &value });
    let slot = rotor.begin_frame(builder.frame_len(), 0)?;
    let first_record_lsn = slot.first_record_lsn();
    let frame = builder.finalize(first_record_lsn, stamp(1));
    rotor.commit_frame(slot, frame)
}

/// Frames decoded from the segment image: `(valid_count, ended_in_error)`.
fn decode_segment(fs: &MemFs, seg: SegmentId) -> (usize, bool) {
    let image = fs
        .contents(&PathBuf::from("data/shard-0/log").join(segment_file_name(seg)))
        .expect("segment exists");
    let mut valid = 0;
    for frame in FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN) {
        match frame {
            Ok((_, frame)) => {
                for record in frame.records() {
                    record.expect("records in a validated frame decode");
                }
                valid += 1;
            }
            Err(_) => return (valid, true),
        }
    }
    (valid, false)
}

#[test]
fn prealloc_no_space_fires_the_enospc_discipline() {
    fault::disarm_all();
    // At creation: the very first prealloc refuses with the typed error.
    fault::arm(inf_log::fault::PREALLOC_NO_SPACE, FaultSpec::Nth(1));
    let fs = MemFs::new();
    let dirs = create_cell_dirs(&fs, &PathBuf::from("data/shard-0")).expect("dirs");
    let err = SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg())
        .expect_err("prealloc refused");
    assert!(matches!(err, LogError::NoSpace { .. }), "{err:?}");

    // In steady state: MAINTAIN surfaces the exhaustion early
    // (`space_exhausted`), the admission hook refuses with `NoSpace`, and
    // space returning clears it — the S02 discipline through the named
    // point instead of a MemFs capacity budget.
    fault::disarm_all();
    let mut rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg()).expect("rotor");
    fault::arm(inf_log::fault::PREALLOC_NO_SPACE, FaultSpec::FromNth(1));
    while !rotor.space_exhausted() {
        rotor.maintain(0).ok();
        if !rotor.space_exhausted() {
            append_frame(&mut rotor, 600).expect("writes admitted until exhaustion surfaces");
        }
    }
    let err = loop {
        match append_frame(&mut rotor, 600) {
            Ok(_) => {} // room left in the active segment
            Err(err) => break err,
        }
    };
    assert!(matches!(err, LogError::NoSpace { .. }), "{err:?}");
    // Space returns (disarm): the rotor recovers without restart.
    fault::disarm_all();
    rotor.maintain(0).expect("maintain after space returns");
    assert!(!rotor.space_exhausted(), "exhaustion clears when prealloc succeeds");
    append_frame(&mut rotor, 600).expect("writes resume");
}

#[test]
fn dir_fsync_fail_fires_at_every_barrier_class() {
    fault::disarm_all();
    let fs = MemFs::new();

    // Boot dir creation barrier.
    fault::arm(inf_log::fault::DIR_FSYNC_FAIL, FaultSpec::Nth(1));
    let err = create_cell_dirs(&fs, &PathBuf::from("data/shard-0")).expect_err("barrier fails");
    assert!(err.to_string().contains("injected fault: dir_fsync_fail"), "{err}");

    // Segment-prealloc barrier (the entry must be durable before use).
    fault::disarm_all();
    let dirs = create_cell_dirs(&fs, &PathBuf::from("data/shard-0")).expect("dirs");
    fault::arm(inf_log::fault::DIR_FSYNC_FAIL, FaultSpec::Nth(1));
    let err = SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg()).expect_err("barrier fails");
    assert!(matches!(err, LogError::Fsync(_)), "{err:?}");
    assert!(err.to_string().contains("injected fault: dir_fsync_fail"), "{err}");

    // Envelope-swap step 6 (META class): the swap fails, the old
    // committed envelope stays authoritative.
    fault::disarm_all();
    write_meta(&fs, &PathBuf::from("data"), b"old-catalog").expect("first swap");
    fault::arm(inf_log::fault::DIR_FSYNC_FAIL, FaultSpec::Nth(1));
    let err = write_meta(&fs, &PathBuf::from("data"), b"new-catalog").expect_err("barrier fails");
    assert!(err.to_string().contains("injected fault: dir_fsync_fail"), "{err}");
    fault::disarm_all();
    // NOTE: the rename (step 5) already happened when step 6 fails — the
    // new payload may be visible; what the protocol guarantees is
    // old-XOR-new, never neither/corrupt (manifest_swap.rs owns that
    // sweep; here the point just demonstrably fires at its barrier).
    let survivor = read_meta(&fs, &PathBuf::from("data")).expect("readable").expect("present");
    assert!(survivor == b"old-catalog" || survivor == b"new-catalog");
}

#[test]
fn log_append_short_write_fails_the_append_and_leaves_a_short_prefix() {
    fault::disarm_all();
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    append_frame(&mut rotor, 600).expect("good frame");

    fault::arm(inf_log::fault::LOG_APPEND_SHORT_WRITE, FaultSpec::Nth(1));
    let err = append_frame(&mut rotor, 600).expect_err("short write fails the append");
    assert!(matches!(err, LogError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("injected fault: log_append_short_write"), "{err}");
    fault::disarm_all();

    // On disk: the good frame validates; the short prefix does not — the
    // exact input the M2-S14 taxonomy classifies at the next boot.
    let (valid, ended_in_error) = decode_segment(&fs, SegmentId(0));
    assert_eq!(valid, 1, "only the pre-fault frame validates");
    assert!(ended_in_error, "the short prefix is not a clean end");
}

#[test]
fn torn_frame_lands_a_prefix_and_lies() {
    fault::disarm_all();
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    append_frame(&mut rotor, 600).expect("good frame");

    fault::arm(inf_log::fault::TORN_FRAME, FaultSpec::Nth(1));
    // The lying-disk shape: the append SUCCEEDS while only a prefix lands
    // (power-cut physics — meaningful only as the final write before a
    // crash; recovery truncates it, fault_recovery.rs proves that leg).
    append_frame(&mut rotor, 600).expect("the torn append lies and succeeds");
    assert_eq!(fault::fired(inf_log::fault::TORN_FRAME), 1);
    fault::disarm_all();

    let (valid, ended_in_error) = decode_segment(&fs, SegmentId(0));
    assert_eq!(valid, 1, "the torn frame must not validate");
    assert!(ended_in_error, "torn bytes are not a clean end");
}

#[test]
fn fsync_err_fails_the_seal_typed_and_non_recoverable() {
    fault::disarm_all();
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    fault::arm(inf_log::fault::FSYNC_ERR, FaultSpec::Always);
    // Fill the active segment until a seal is due; the seal fsync fails.
    let err = loop {
        match append_frame(&mut rotor, 600) {
            Ok(_) => {}
            Err(err) => break err,
        }
    };
    fault::disarm_all();
    assert!(matches!(err, LogError::Fsync(_)), "typed, §8.4-fatal class: {err:?}");
    assert!(err.to_string().contains("injected fault: fsync_err"), "{err}");
}

#[test]
fn power_cut_after_seal_leaves_a_durable_sealed_segment() {
    fault::disarm_all();
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    fault::arm(inf_log::fault::POWER_CUT_AFTER_SEAL, FaultSpec::Always);
    let mut appended = 0u32;
    let err = loop {
        match append_frame(&mut rotor, 600) {
            Ok(_) => appended += 1,
            Err(err) => break err,
        }
    };
    fault::disarm_all();
    assert!(err.to_string().contains("injected fault: power_cut_after_seal"), "{err}");
    drop(rotor); // the process is dead past the seal

    // The seal was durable: every appended frame survives and validates;
    // the scan sees a well-formed directory the next boot resumes from
    // (the open_cell_log leg lives in fault_recovery.rs).
    let scan = scan_log_dir(&fs, &PathBuf::from("data/shard-0/log")).expect("clean scan");
    assert_eq!(scan.segments().first(), Some(&SegmentId(0)));
    let (valid, ended_in_error) = decode_segment(&fs, SegmentId(0));
    assert_eq!(valid as u32, appended, "every pre-seal frame survives");
    assert!(!ended_in_error, "a sealed segment ends cleanly");
}

#[test]
fn manifest_rename_fail_leaves_the_old_unit_authoritative() {
    fault::disarm_all();
    let fs = MemFs::new();
    let shard = PathBuf::from("data/shard-0");
    create_cell_dirs(&fs, &shard).expect("dirs");
    let old = Manifest {
        ckpt_id: 1,
        begin_lsn: Lsn::new(SegmentId(0), 64),
        segments: vec![SegmentId(0)],
        tiers: Vec::new(),
    };
    write_manifest(&fs, &shard, &old).expect("first manifest");

    fault::arm(inf_log::fault::MANIFEST_RENAME_FAIL, FaultSpec::Nth(1));
    let newer = Manifest {
        ckpt_id: 2,
        begin_lsn: Lsn::new(SegmentId(1), 64),
        segments: vec![SegmentId(1)],
        tiers: Vec::new(),
    };
    let err = write_manifest(&fs, &shard, &newer).expect_err("rename refused");
    assert!(err.to_string().contains("injected fault: manifest_rename_fail"), "{err}");
    fault::disarm_all();

    // The old recovery unit stays authoritative (swap aborts, never
    // neither) — and the swap succeeds once the fault clears.
    let survivor = read_manifest(&fs, &shard).expect("readable").expect("present");
    assert_eq!(survivor.ckpt_id, 1, "old unit authoritative after failed swap");
    write_manifest(&fs, &shard, &newer).expect("swap succeeds after the fault clears");
    assert_eq!(read_manifest(&fs, &shard).expect("readable").expect("present").ckpt_id, 2);
}
