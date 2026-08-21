//! M2-S02 ACs: rotation is a pointer swap onto a preallocated next segment,
//! disk-full is surfaced loudly before writes need the space, fsync failure
//! is fatal, and the whole lifecycle behaves identically over the real
//! filesystem (StdSegmentFs smoke) and the deterministic in-memory tier.

use inf_log::FrameLayout;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, StdSegmentFs};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FrameBuilder, FrameIter, FrameStamp, LogError, Lsn,
    NsId, RecordView, SegmentConfig, SegmentId, SegmentRotor, create_cell_dirs, scan_log_dir,
};
use std::path::PathBuf;

const SEGMENT_BYTES: u32 = 4096;

/// Canonical v2 stamp for hand-built test frames (epoch 1, covered 0 —
/// attests nothing). `seq` matters only where a test builds sequential
/// frames the recovery policy will walk; readers/scanners ignore it.
fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn cfg() -> SegmentConfig {
    SegmentConfig { segment_bytes: SEGMENT_BYTES, ..Default::default() }
}

fn mem_rotor(fs: &MemFs) -> SegmentRotor<MemFs> {
    let dirs = create_cell_dirs(fs, &PathBuf::from("data/shard-0")).expect("dirs");
    SegmentRotor::create_fresh(fs.clone(), dirs.log, cfg()).expect("fresh rotor")
}

/// Append one frame with `filler` bytes of value payload; returns its base LSN.
fn append_frame(rotor: &mut SegmentRotor<MemFs>, filler: usize, now_ms: u64) -> Lsn {
    let value = vec![0xAB; filler];
    let mut builder = FrameBuilder::new();
    builder.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: &value });
    let slot = rotor.begin_frame(builder.frame_len(), now_ms).expect("reserve");
    let first_record_lsn = slot.first_record_lsn();
    let frame = builder.finalize(first_record_lsn, stamp(1), FrameLayout::Packed);
    rotor.commit_frame(slot, frame).expect("commit")
}

#[test]
fn rotation_is_pointer_swap_with_preallocated_next() {
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);

    let mut appended = 0u32;
    while rotor.stats().rotations < 3 {
        append_frame(&mut rotor, 600, 0);
        appended += 1;
        // MAINTAIN keeps the next segment ready well before the seal.
        rotor.maintain(0).expect("maintain");
    }
    assert!(appended > 3, "several frames per segment");
    assert_eq!(rotor.sealed(), &[SegmentId(0), SegmentId(1), SegmentId(2)]);
    assert_eq!(rotor.active_segment(), SegmentId(3));
    let stats = rotor.stats();
    assert_eq!(stats.rotations, 3);
    assert_eq!(stats.inline_preallocs, 0, "maintained rotor never preallocates inline");
    assert!(stats.preallocs >= 3);
    assert_eq!(rotor.next_ready(), Some(SegmentId(4)));

    // Every sealed segment replays cleanly through the frame iterator.
    for &sealed in rotor.sealed() {
        let file = rotor.open_segment_read(sealed).expect("open sealed");
        let len = file.file_size().expect("len");
        let mut image = vec![0u8; usize::try_from(len).expect("fits")];
        let mut read = 0;
        while read < image.len() {
            let n = file.read_at(read as u64, &mut image[read..]).expect("read");
            assert!(n > 0, "EOF before preallocated length");
            read += n;
        }
        let mut frames = 0;
        for frame in FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN) {
            let (_, frame) = frame.expect("sealed segment frames are valid");
            for record in frame.records() {
                record.expect("records valid");
            }
            frames += 1;
        }
        assert!(frames > 0, "sealed segment {sealed} holds frames");
    }
}

#[test]
fn unmaintained_rotation_counts_inline_prealloc() {
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    while rotor.stats().rotations == 0 {
        append_frame(&mut rotor, 600, 0);
    }
    assert_eq!(rotor.stats().inline_preallocs, 1, "slow path taken and counted");
}

#[test]
fn enospc_surfaces_in_maintain_before_writes_need_it() {
    let fs = MemFs::new();
    // Room for exactly the fresh segment, nothing more.
    fs.set_capacity(Some(u64::from(SEGMENT_BYTES)));
    let mut rotor = mem_rotor(&fs);

    let report = rotor.maintain(0).expect("maintain never hard-fails on ENOSPC");
    assert!(report.prealloc_failed);
    assert!(rotor.space_exhausted(), "early warning raised before any write needs space");
    assert_eq!(rotor.stats().prealloc_failures, 1);

    // The active segment still accepts frames (degrade loudly ≠ stop early)…
    append_frame(&mut rotor, 600, 0);

    // …but once it is full, the named error surfaces instead of corruption.
    let err = loop {
        let value = vec![0xAB; 600];
        let mut builder = FrameBuilder::new();
        builder.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: &value });
        match rotor.begin_frame(builder.frame_len(), 0) {
            Ok(slot) => {
                let lsn = slot.first_record_lsn();
                let bytes = builder.finalize(lsn, stamp(1), FrameLayout::Packed);
                rotor.commit_frame(slot, bytes).expect("commit within active");
            }
            Err(err) => break err,
        }
    };
    assert!(matches!(err, LogError::NoSpace { segment: SegmentId(1) }), "got {err}");

    // Space returns → maintain recovers, appends flow again.
    fs.set_capacity(None);
    let report = rotor.maintain(0).expect("maintain");
    assert_eq!(report.preallocated, Some(SegmentId(1)));
    assert!(!rotor.space_exhausted());
    append_frame(&mut rotor, 600, 0);
    assert_eq!(rotor.active_segment(), SegmentId(1));
}

#[test]
fn oversized_frame_is_rejected() {
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    let err = rotor.begin_frame(SEGMENT_BYTES + 1, 0).expect_err("too large");
    assert!(matches!(err, LogError::FrameTooLarge { len, max }
        if len == SEGMENT_BYTES + 1 && max == SEGMENT_BYTES));
}

#[test]
fn fsync_failure_on_seal_is_fatal() {
    let fs = MemFs::new();
    let mut rotor = mem_rotor(&fs);
    append_frame(&mut rotor, 600, 0);
    fs.fail_next_sync_data();
    // Force a rotation: the seal fsync fails and must surface as the
    // non-recoverable FsyncFailed type (§8.4) — callers fail-stop on it.
    let err = rotor.begin_frame(SEGMENT_BYTES, 0).expect_err("seal fsync fails");
    assert!(matches!(err, LogError::Fsync(_)), "got {err}");
}

#[test]
fn time_seal_fires_only_when_configured_and_dirty() {
    let fs = MemFs::new();
    let dirs = create_cell_dirs(&fs, &PathBuf::from("data/shard-0")).expect("dirs");
    let config = SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        seal_after_ms: Some(1_000),
        ..Default::default()
    };
    let mut rotor = SegmentRotor::create_fresh(fs.clone(), dirs.log, config).expect("rotor");

    // Empty active segment never time-seals.
    let report = rotor.maintain(10_000).expect("maintain");
    assert!(!report.time_sealed);

    append_frame(&mut rotor, 100, 20_000);
    assert!(!rotor.maintain(20_999).expect("maintain").time_sealed, "bound not reached");
    let report = rotor.maintain(21_000).expect("maintain");
    assert!(report.time_sealed);
    assert_eq!(rotor.stats().time_seals, 1);
    assert_eq!(rotor.active_segment(), SegmentId(1));
    // The fresh (still-empty) active segment does not seal again.
    assert!(!rotor.maintain(90_000).expect("maintain").time_sealed);
}

/// StdSegmentFs smoke: the same lifecycle against the real filesystem —
/// create, append, reopen from a boot scan, append again, replay.
#[test]
fn std_fs_lifecycle_smoke() {
    let root = std::env::temp_dir().join(format!("inf-log-s02-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear stale test dir");
    }
    let shard = root.join("shard-0");
    let fs = StdSegmentFs;
    let dirs = create_cell_dirs(&fs, &shard).expect("dirs");

    let config = SegmentConfig { segment_bytes: 8192, ..Default::default() };
    let mut rotor = SegmentRotor::create_fresh(fs, dirs.log.clone(), config).expect("fresh rotor");
    let mut builder = FrameBuilder::new();
    builder.append(&RecordView::StringPostImage { ns: NsId(0), key: b"boot", value: b"one" });
    let slot = rotor.begin_frame(builder.frame_len(), 0).expect("reserve");
    let lsn = slot.first_record_lsn();
    let first_end = {
        let frame = builder.finalize(lsn, stamp(1), FrameLayout::Packed);
        let base = rotor.commit_frame(slot, frame).expect("commit");
        assert_eq!(base, Lsn::new(SegmentId(0), 0));
        rotor.active_written()
    };
    drop(rotor);

    // Reboot: scan, reopen at the recovered tail, append a second frame.
    let scan = scan_log_dir(&fs, &dirs.log).expect("scan");
    assert_eq!(scan.segments(), &[SegmentId(0)]);
    let mut rotor = SegmentRotor::open_existing(fs, dirs.log.clone(), config, &scan, first_end)
        .expect("reopen");
    builder.reset();
    builder.append(&RecordView::Delete { ns: NsId(0), key: b"boot" });
    let slot = rotor.begin_frame(builder.frame_len(), 0).expect("reserve");
    let lsn = slot.first_record_lsn();
    assert_eq!(lsn, Lsn::new(SegmentId(0), first_end + FRAME_HEADER_LEN as u32));
    let frame = builder.finalize(lsn, stamp(2), FrameLayout::Packed);
    rotor.commit_frame(slot, frame).expect("commit");

    // Replay the segment: exactly the two frames, in order.
    let file = rotor.open_segment_read(SegmentId(0)).expect("read");
    let len = usize::try_from(file.file_size().expect("len")).expect("fits");
    let mut image = vec![0u8; len];
    let mut read = 0;
    while read < image.len() {
        let n = file.read_at(read as u64, &mut image[read..]).expect("read");
        assert!(n > 0);
        read += n;
    }
    let decoded: Vec<_> = FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN)
        .map(|frame| frame.expect("valid"))
        .flat_map(|(_, frame)| frame.records().map(|r| r.expect("valid").0).collect::<Vec<_>>())
        .collect();
    assert_eq!(decoded.len(), 2);

    std::fs::remove_dir_all(&root).expect("cleanup");
}
