//! Review 2026-08-30, F-L04-02 (ADR-0119 D2): the simulated disk's
//! per-file faults — `EIO` on one file's next reads or writes, every
//! other file and op class untouched — and the product paths they reach.
//! Every test states its goal and method in its first sentence.

use std::path::{Path, PathBuf};

use inf_log::fs::sim::{DeviceFault, SimDisk};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    FrameBuilder, FrameLayout, FrameStamp, NsId, ReadError, ReaderConfig, RecordView,
    SegmentConfig, SegmentId, SegmentReader, SegmentRotor,
};

fn eio(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::EIO)
}

/// A read fault is one file, one op: the armed file's next read answers
/// `EIO`, its following read and every other file's reads succeed, and
/// the writes on the armed file are untouched.
#[test]
fn read_eio_is_one_file_one_op() {
    let disk = SimDisk::new();
    let dir = Path::new("d");
    disk.create_dir_all(dir).expect("dir");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let mut fa = disk.create_meta(&a).expect("a");
    let fb = disk.create_meta(&b).expect("b");
    fa.write_at(0, b"alpha").expect("write");
    disk.inject(&a, DeviceFault::ReadEio, 1).expect("armed");
    let mut buf = [0u8; 5];
    let err = fa.read_at(0, &mut buf).expect_err("the armed read fails");
    assert!(eio(&err), "typed EIO, got {err:?}");
    assert_eq!(disk.faults_fired(), 1);
    assert_eq!(fa.read_at(0, &mut buf).expect("next read"), 5, "one op, not a dead file");
    assert_eq!(&buf, b"alpha");
    assert_eq!(fb.read_at(0, &mut buf).expect("other file"), 0);
    fa.write_at(5, b"!").expect("writes untouched");
    assert_eq!(disk.faults_fired(), 1);
    // The driver tier consumes the same budget.
    disk.inject(&a, DeviceFault::ReadEio, 1).expect("armed");
    let fd = fa.raw_fd().expect("sim fd");
    let err = disk.driver_read_at(fd, 0, &mut buf).expect_err("driver read fails");
    assert!(eio(&err));
    assert_eq!(disk.driver_read_at(fd, 0, &mut buf).expect("next"), 5);
    assert_eq!(disk.faults_fired(), 2);
}

/// A write fault answers `EIO` and lands nothing — the file's bytes are
/// exactly what they were — at the blocking tier and both driver classes.
#[test]
fn write_eio_lands_nothing() {
    let disk = SimDisk::new();
    let dir = Path::new("d");
    disk.create_dir_all(dir).expect("dir");
    let path = dir.join("f");
    let mut file = disk.create_meta(&path).expect("f");
    file.write_at(0, b"keep").expect("write");
    disk.inject(&path, DeviceFault::WriteEio, 3).expect("armed");
    assert!(eio(&file.write_at(0, b"lost").expect_err("blocking")));
    let fd = file.raw_fd().expect("sim fd");
    assert!(eio(&disk.driver_write_at(fd, 0, b"lost").expect_err("plain")));
    assert!(eio(&disk.driver_write_through(fd, 0, b"lost").expect_err("through")));
    assert_eq!(disk.contents(&path).expect("os view"), b"keep");
    assert_eq!(disk.faults_fired(), 3);
    file.write_at(0, b"next").expect("budget spent");
    assert_eq!(disk.contents(&path).expect("os view"), b"next");
}

/// A latent sector error under a segment surfaces at recovery as the
/// typed `ReadError::Io`, never as a torn tail (`ReadEnd`) that replay
/// would silently truncate to — the single-op failure no seed could
/// reach before the injector existed.
#[test]
fn recovery_read_eio_is_typed_never_a_torn_tail() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let cfg = SegmentConfig { segment_bytes: 64 << 10, ..Default::default() };
    let mut rotor = SegmentRotor::create_fresh(disk.clone(), dir.clone(), cfg).expect("fresh");
    for n in 0..8u8 {
        let mut builder = FrameBuilder::new();
        builder.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: &[n; 300] });
        let slot = rotor.begin_frame(builder.frame_len(), 0).expect("reserve");
        let frame = builder.finalize(
            slot.first_record_lsn(),
            FrameStamp { epoch: 1, seq: u64::from(n) + 1, covered_lsn: 0 },
            FrameLayout::Packed,
        );
        rotor.commit_frame(slot, frame).expect("commit");
    }
    drop(rotor);
    let path = dir.join("seg-000000.ilog");
    let replay = |disk: &SimDisk| {
        let mut reader = SegmentReader::open(disk, &dir, SegmentId(0), ReaderConfig::default())
            .expect("open segment");
        let mut frames = 0;
        reader
            .apply_frames(|_| {
                frames += 1;
                Ok::<(), std::convert::Infallible>(())
            })
            .map(|end| (frames, end))
    };
    let (frames, _) = replay(&disk).expect("clean replay");
    assert_eq!(frames, 8);
    disk.inject(&path, DeviceFault::ReadEio, 1).expect("armed");
    match replay(&disk) {
        Err(inf_log::ApplyError::Read(ReadError::Io { segment, source, .. })) => {
            assert_eq!(segment, SegmentId(0));
            assert!(eio(&source), "the device's errno reaches the caller: {source:?}");
        }
        other => panic!("a read EIO must be typed, got {other:?}"),
    }
    assert_eq!(disk.faults_fired(), 1, "the armed fault fired exactly once");
    let (frames, _) = replay(&disk).expect("the file is intact after the fault");
    assert_eq!(frames, 8);
}
