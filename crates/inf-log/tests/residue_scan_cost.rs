#![allow(
    clippy::disallowed_methods,
    reason = "test target: harness deadlines and timings, not cell code"
)]
//! M4.5-S39b falsifier (c) attribution: the cost of the slack audit over a
//! segment full of recycled-life residue against the same segment full of
//! zeros, on a real filesystem (`INF_RESIDUE_SCAN_DIR` names it; the test
//! is a no-op otherwise). Not a gate — an attribution instrument.

use std::path::PathBuf;
use std::time::Instant;

use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::{
    FRAME_ALIGN, FRAME_HEADER_LEN, FrameBuilder, FrameLayout, FrameStamp, Lsn, NsId, ReaderConfig,
    RecordView, SegmentId, scan_region_evidence, segment_file_name,
};

#[test]
fn residue_scan_cost() {
    let Some(dir) = std::env::var_os("INF_RESIDUE_SCAN_DIR") else { return };
    let dir = PathBuf::from(dir);
    let fs = StdSegmentFs;
    fs.create_dir_all(&dir).expect("dir");
    let size: u64 = 256 << 20;
    let blocks = (size / u64::from(FRAME_ALIGN)) as u32;
    // One residue frame image, re-stamped per block (offset must match).
    let value = vec![0x5Au8; 1000];
    let make = |block: u32| {
        let mut b = FrameBuilder::new();
        b.append(&RecordView::StringPostImage { ns: NsId(1), key: b"key", value: &value });
        b.append(&RecordView::StringPostImage { ns: NsId(1), key: b"key2", value: &value });
        let first = Lsn::new(SegmentId(7), block * FRAME_ALIGN + FRAME_HEADER_LEN as u32);
        b.finalize(
            first,
            FrameStamp { epoch: 1, seq: u64::from(block) + 1, covered_lsn: 0 },
            FrameLayout::Aligned,
        )
        .to_vec()
    };
    for (name, residue) in [("seg-000001.ilog", true), ("seg-000002.ilog", false)] {
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let mut file = fs.create_segment(&path, size).expect("create");
        if residue {
            let mut image = Vec::with_capacity(1 << 20);
            let mut at = 0u64;
            for block in 0..blocks {
                image.extend(make(block));
                if image.len() >= 1 << 20 {
                    file.write_at(at, &image).expect("write");
                    at += image.len() as u64;
                    image.clear();
                }
            }
            file.write_at(at, &image).expect("write");
        } else {
            let zeros = vec![0u8; 1 << 20];
            for i in 0..(size >> 20) {
                file.write_at(i << 20, &zeros).expect("zeros");
            }
        }
        file.sync_data().expect("sync");
    }
    for (id, label) in [
        (SegmentId(1), "residue"),
        (SegmentId(2), "zeros"),
        (SegmentId(1), "residue (2nd)"),
        (SegmentId(2), "zeros (2nd)"),
    ] {
        let t0 = Instant::now();
        let ev = scan_region_evidence(&fs, &dir, id, 0, ReaderConfig::default()).expect("scan");
        eprintln!(
            "scan {label}: {:.3} s (valid {} foreign {})",
            t0.elapsed().as_secs_f64(),
            ev.valid_frames,
            ev.foreign_frames
        );
    }
    let _ = std::fs::remove_file(dir.join(segment_file_name(SegmentId(1))));
    let _ = std::fs::remove_file(dir.join(segment_file_name(SegmentId(2))));
}
