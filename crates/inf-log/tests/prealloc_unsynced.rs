//! Review 2026-08-30, F-L04-05 (ADR-0119 A2): a `Buffered` deferred
//! prealloc (M2.5-S01) reboots short after a cut that beat its first data
//! barrier — the regime the simulator could not produce while the
//! unsynced create routed to the synced one — and the boot path takes
//! the short file as the empty tail it is. Every test states its goal
//! and method in its first sentence.

use std::path::PathBuf;

use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs, SegmentIoMode};
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FrameBuilder, FrameIter, FrameLayout, FrameStamp, NsId, RecordView,
    SegmentConfig, SegmentId, SegmentRotor, scan_log_dir,
};

const SEGMENT_BYTES: u32 = 64 << 10;

fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

fn cfg() -> SegmentConfig {
    SegmentConfig {
        segment_bytes: SEGMENT_BYTES,
        io_mode: SegmentIoMode::Buffered,
        prealloc: inf_log::PreallocPolicy::Immediate,
        ..Default::default()
    }
}

/// One packed frame written through the driver at its reserved base.
fn frame(rotor: &mut SegmentRotor<SimDisk>, disk: &SimDisk, seq: u64) {
    let mut b = FrameBuilder::new();
    b.append(&RecordView::StringPostImage { ns: NsId(1), key: b"k", value: &[seq as u8; 300] });
    let (slot, _) = rotor.begin_frame_deferred(b.frame_len(), 0).expect("reserve");
    let bytes = b.finalize(slot.first_record_lsn(), stamp(seq), FrameLayout::Packed);
    let fd = rotor.active_raw_fd().expect("fd");
    disk.driver_write_at(fd, u64::from(slot.base().offset), bytes).expect("frame");
    rotor.commit_frame_queued(slot);
}

fn image(disk: &SimDisk, dir: &std::path::Path, id: SegmentId) -> Vec<u8> {
    disk.contents(&dir.join(inf_log::segment_file_name(id))).expect("named ⇒ survives")
}

/// Goal: the deferred next segment reboots at length 0 when the cut
/// lands after its dir barrier and before any barrier on its fd, the
/// active segment (barriered by its group commits) keeps its length,
/// and the rotor reopens on the short tail and appends. Method: two
/// frames + fdatasync on segment 0, MAINTAIN with the dir barrier only,
/// cut, scan, `open_existing`, one more frame, replay.
#[test]
fn deferred_prealloc_reboots_short_and_reopens_as_the_empty_tail() {
    let disk = SimDisk::new();
    let dir = PathBuf::from("log");
    disk.create_dir_all(&dir).expect("dir");
    let mut rotor =
        SegmentRotor::create_fresh_deferred(disk.clone(), dir.clone(), cfg()).expect("fresh");
    frame(&mut rotor, &disk, 1);
    frame(&mut rotor, &disk, 2);
    let fd = rotor.active_raw_fd().expect("fd");
    disk.driver_fdatasync(fd).expect("group-commit barrier: data and length");
    let (report, barrier) = rotor.maintain_deferred(0).expect("maintain");
    assert!(report.preallocated.is_some(), "segment 1 preallocated");
    let barrier = barrier.expect("the prealloc's dir barrier");
    disk.driver_fdatasync(barrier.dir.raw_fd().expect("dir fd")).expect("dir barrier");
    drop(barrier);
    drop(rotor);
    disk.power_cut(0xD5EE);

    assert_eq!(image(&disk, &dir, SegmentId(0)).len() as u32, SEGMENT_BYTES, "barriered length");
    assert_eq!(image(&disk, &dir, SegmentId(1)).len(), 0, "unsynced prealloc: length lost");

    let scan = scan_log_dir(&disk, &dir).expect("scan");
    assert_eq!(scan.segments(), &[SegmentId(0), SegmentId(1)]);
    let mut rotor = SegmentRotor::open_existing(disk.clone(), dir.clone(), cfg(), &scan, 0)
        .expect("the short tail reopens");
    assert_eq!(rotor.active_segment(), SegmentId(1));
    frame(&mut rotor, &disk, 3);
    let fd = rotor.active_raw_fd().expect("fd");
    disk.driver_fdatasync(fd).expect("barrier");
    let replayed: Vec<u64> = [SegmentId(0), SegmentId(1)]
        .into_iter()
        .flat_map(|id| {
            FrameIter::new(&image(&disk, &dir, id), DEFAULT_MAX_FRAME_LEN)
                .map(|f| f.expect("valid").1.stamp().expect("stamped").seq)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(replayed, [1, 2, 3], "every committed frame replays across the short tail");
}
