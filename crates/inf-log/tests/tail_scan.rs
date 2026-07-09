//! M2-S14 taxonomy mechanics: the tail-region scan finds the strongest
//! evidence beyond a segment's data end — a validating self-located frame
//! (⇒ fail-stop corruption), torn-write remnants (⇒ torn tail / inert
//! residue), or pure zeros (⇒ clean) — across chunk boundaries, EOF edges,
//! and window geometries. Recovery *policy* over these facts (torn-tail
//! truncation, sealed-slack tolerance, the begin-LSN guard) is exercised
//! in `inf-server/tests/recover_torn.rs` (ADR-0018).

use std::path::{Path, PathBuf};

use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    FRAME_HEADER_LEN, FrameBuilder, Lsn, NsId, ReaderConfig, RecordView, RegionScan, SegmentId,
    scan_region, segment_file_name,
};

const SEG_BYTES: u64 = 8192;

fn log_dir(fs: &MemFs) -> PathBuf {
    let dir = PathBuf::from("data/shard-0/log");
    fs.create_dir_all(&dir).expect("dirs");
    dir
}

fn prealloc(fs: &MemFs, dir: &Path, id: u32) {
    fs.create_segment(&dir.join(segment_file_name(SegmentId(id))), SEG_BYTES).expect("segment");
}

/// A validating frame built for exactly (`seg`, `offset`).
fn frame_at(seg: u32, offset: u32, key: &[u8]) -> Vec<u8> {
    let mut b = FrameBuilder::new();
    b.append(&RecordView::StringPostImage { ns: NsId(9), key, value: b"v" });
    b.finalize(Lsn::new(SegmentId(seg), offset + FRAME_HEADER_LEN as u32)).to_vec()
}

fn write_at(fs: &MemFs, dir: &Path, id: u32, offset: u32, bytes: &[u8]) {
    let mut file = fs.open_write(&dir.join(segment_file_name(SegmentId(id)))).expect("open");
    file.write_at(u64::from(offset), bytes).expect("write");
}

fn cfg() -> ReaderConfig {
    ReaderConfig::default()
}

#[test]
fn pristine_tail_is_all_zero() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    let frame = frame_at(0, 0, b"a");
    write_at(&fs, &dir, 0, 0, &frame);
    let end = frame.len() as u32;

    assert!(matches!(scan_region(&fs, &dir, SegmentId(0), end, cfg()), Ok(RegionScan::AllZero)));
}

#[test]
fn from_beyond_file_size_is_all_zero() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    assert!(matches!(
        scan_region(&fs, &dir, SegmentId(0), SEG_BYTES as u32 + 512, cfg()),
        Ok(RegionScan::AllZero)
    ));
}

#[test]
fn remnant_bytes_without_a_frame_are_garbage() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    write_at(&fs, &dir, 0, 100, b"partial torn write \xde\xad\xbe\xef");

    match scan_region(&fs, &dir, SegmentId(0), 0, cfg()).expect("scan") {
        RegionScan::Garbage { first_nonzero } => assert_eq!(first_nonzero, 100),
        other => panic!("expected garbage, got {other:?}"),
    }
}

#[test]
fn validating_frame_after_a_zero_gap_is_found() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    // A dropped write left zeros, then a frame written for exactly offset
    // 512 survived — the resurrection hazard recovery must fail-stop on.
    let frame = frame_at(0, 512, b"survivor");
    write_at(&fs, &dir, 0, 512, &frame);

    match scan_region(&fs, &dir, SegmentId(0), 0, cfg()).expect("scan") {
        RegionScan::ValidFrame { offset } => assert_eq!(offset, 512),
        other => panic!("expected a validating frame, got {other:?}"),
    }
}

#[test]
fn garbage_before_a_validating_frame_still_finds_the_frame() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    write_at(&fs, &dir, 0, 40, b"\x49\x46\x52\x31junk\x02\x02");
    let frame = frame_at(0, 700, b"survivor");
    write_at(&fs, &dir, 0, 700, &frame);

    match scan_region(&fs, &dir, SegmentId(0), 0, cfg()).expect("scan") {
        RegionScan::ValidFrame { offset } => assert_eq!(offset, 700),
        other => panic!("expected a validating frame, got {other:?}"),
    }
}

#[test]
fn crc_damaged_frame_is_garbage_not_data() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    let mut frame = frame_at(0, 512, b"survivor");
    let last = frame.len() - 1;
    frame[last] ^= 0x01; // CRC trailer no longer matches
    write_at(&fs, &dir, 0, 512, &frame);

    assert!(matches!(
        scan_region(&fs, &dir, SegmentId(0), 0, cfg()),
        Ok(RegionScan::Garbage { first_nonzero: 512 })
    ));
}

#[test]
fn mislocated_remnant_frame_is_garbage_not_data() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    // A whole frame from a previous life, built for offset 256, now
    // sitting at 512: decodes, but fails self-location (the LSN check).
    let frame = frame_at(0, 256, b"remnant");
    write_at(&fs, &dir, 0, 512, &frame);

    assert!(matches!(
        scan_region(&fs, &dir, SegmentId(0), 0, cfg()),
        Ok(RegionScan::Garbage { .. })
    ));
}

#[test]
fn frame_spanning_the_chunk_boundary_is_found() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    // Tiny scan window; a frame with a fat record straddles several
    // refills and must still validate.
    let mut b = FrameBuilder::new();
    let value = vec![0xAB; 300];
    b.append(&RecordView::StringPostImage { ns: NsId(9), key: b"fat", value: &value });
    let offset = 50u32;
    let frame = b.finalize(Lsn::new(SegmentId(0), offset + FRAME_HEADER_LEN as u32)).to_vec();
    assert!(frame.len() > 64, "frame must exceed the scan chunk");
    write_at(&fs, &dir, 0, offset, &frame);

    let small = ReaderConfig { chunk_bytes: 64, ..ReaderConfig::default() };
    match scan_region(&fs, &dir, SegmentId(0), 0, small).expect("scan") {
        RegionScan::ValidFrame { offset: at } => assert_eq!(at, offset),
        other => panic!("expected valid frame, got {other:?}"),
    }
}

#[test]
fn magic_at_eof_without_room_is_garbage() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    // The magic bytes end the file exactly — no header can follow.
    write_at(&fs, &dir, 0, SEG_BYTES as u32 - 4, b"IFR1");

    match scan_region(&fs, &dir, SegmentId(0), 0, cfg()).expect("scan") {
        RegionScan::Garbage { first_nonzero } => {
            assert_eq!(first_nonzero, SEG_BYTES as u32 - 4);
        }
        other => panic!("expected garbage, got {other:?}"),
    }
}

#[test]
fn magic_claiming_more_bytes_than_the_file_holds_is_garbage() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    // Valid magic + a length that runs past EOF: not a frame.
    let mut junk = Vec::new();
    junk.extend_from_slice(b"IFR1");
    junk.extend_from_slice(&(4096u32).to_le_bytes());
    write_at(&fs, &dir, 0, SEG_BYTES as u32 - 64, &junk);

    assert!(matches!(
        scan_region(&fs, &dir, SegmentId(0), 0, cfg()),
        Ok(RegionScan::Garbage { .. })
    ));
}

#[test]
fn zero_runs_larger_than_the_window_are_skipped() {
    let fs = MemFs::new();
    let dir = log_dir(&fs);
    prealloc(&fs, &dir, 0);
    write_at(&fs, &dir, 0, SEG_BYTES as u32 - 8, b"\xffjunk");

    let small = ReaderConfig { chunk_bytes: 64, ..ReaderConfig::default() };
    match scan_region(&fs, &dir, SegmentId(0), 0, small).expect("scan") {
        RegionScan::Garbage { first_nonzero } => assert_eq!(first_nonzero, SEG_BYTES as u32 - 8),
        other => panic!("expected garbage, got {other:?}"),
    }
}
