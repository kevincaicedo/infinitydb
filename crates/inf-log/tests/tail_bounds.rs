//! Sparse-file boundary tests for the tail scanner's u32 segment cursor.

use std::path::{Path, PathBuf};

use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::{
    MAX_SEGMENT_LEN, ReadError, ReaderConfig, RegionScan, SegmentId, SegmentReader, scan_region,
    scan_region_evidence, segment_file_name,
};

struct SparseLog(PathBuf);

impl SparseLog {
    fn new(name: &str, len: u64) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("tail-bounds-{name}-{}", std::process::id()));
        std::fs::create_dir(&path).expect("unique scratch directory");
        let file = std::fs::File::create(path.join(segment_file_name(SegmentId(0))))
            .expect("sparse segment");
        file.set_len(len).expect("sparse length");
        Self(path)
    }
}

impl Drop for SparseLog {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove scratch directory");
    }
}

/// Both public scanners must refuse an unrepresentable end, even at EOF.
#[test]
fn oversized_segment_is_invalid_data() {
    for (index, len) in [(1u64 << 32), (1u64 << 32) + 8192].into_iter().enumerate() {
        let log = SparseLog::new(&format!("oversized-{index}"), len);
        let cfg = ReaderConfig { chunk_bytes: 64, ..ReaderConfig::default() };
        let from = u32::MAX - 31;
        let error = scan_region(&StdSegmentFs, &log.0, SegmentId(0), from, cfg)
            .expect_err("oversized segment must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds u32 segment address limit"), "{error}");
        let error = scan_region_evidence(&StdSegmentFs, &log.0, SegmentId(0), from, cfg)
            .expect_err("evidence scanner must refuse the same file");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::metadata(log.0.join(segment_file_name(SegmentId(0)))).unwrap().len(),
            len
        );
    }
    println!("tail bounds: oversized files refused by both scanners; files unchanged");
}

/// The largest representable end still scans zeros and the final garbage byte.
#[test]
fn largest_representable_segment_keeps_exact_offsets() {
    let log = SparseLog::new("maximum", u64::from(u32::MAX));
    let cfg = ReaderConfig { chunk_bytes: 64, ..ReaderConfig::default() };
    let from = u32::MAX - 31;
    let evidence = scan_region_evidence(&StdSegmentFs, &log.0, SegmentId(0), from, cfg)
        .expect("representable end");
    assert_eq!(evidence.summary(), RegionScan::AllZero);
    assert_eq!(evidence.bytes_read, 31);
    let mut file = StdSegmentFs.open_write(&log.0.join(segment_file_name(SegmentId(0)))).unwrap();
    file.write_at(u64::from(u32::MAX - 1), b"x").unwrap();
    let evidence = scan_region_evidence(&StdSegmentFs, &log.0, SegmentId(0), from, cfg).unwrap();
    assert_eq!(evidence.summary(), RegionScan::Garbage { first_nonzero: u32::MAX - 1 });
    assert_eq!(evidence.bytes_read, 31);
    assert_eq!(
        scan_region(&StdSegmentFs, &log.0, SegmentId(0), u32::MAX, cfg).unwrap(),
        RegionScan::AllZero
    );
}

/// The replay reader owns the same bound: an unaddressable file is a named
/// refusal, not a silently readable one whose tail no LSN can name.
#[test]
fn oversized_segment_is_refused_by_the_replay_reader() {
    let log = SparseLog::new("reader", MAX_SEGMENT_LEN + 4096);
    let error = SegmentReader::open(&StdSegmentFs, &log.0, SegmentId(0), ReaderConfig::default())
        .expect_err("oversized segment must be refused");
    match &error {
        ReadError::Io { segment, offset, source } => {
            assert_eq!((*segment, *offset), (SegmentId(0), 0));
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
            assert!(source.to_string().contains("exceeds u32 segment address limit"), "{source}");
        }
        other => panic!("wrong refusal: {other}"),
    }
    let representable = SparseLog::new("reader-max", MAX_SEGMENT_LEN);
    let mut reader =
        SegmentReader::open(&StdSegmentFs, &representable.0, SegmentId(0), ReaderConfig::default())
            .expect("the representable maximum still opens");
    assert!(reader.next_frame().expect("clean zero tail").is_none());
    println!("tail bounds: replay reader refuses the unaddressable file, reads the maximum");
}
