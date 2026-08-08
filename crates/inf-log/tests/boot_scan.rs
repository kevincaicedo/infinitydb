//! M2-S02 AC: proptest over random gap/duplicate/truncated-name corpora —
//! every anomaly produces its documented named error, never a silent skip.

use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{ScanError, SegmentId, scan_log_dir, segment_file_name};
use proptest::prelude::*;
use std::path::{Path, PathBuf};

fn fs_with_segments(ids: impl IntoIterator<Item = SegmentId>) -> (MemFs, PathBuf) {
    let fs = MemFs::new();
    let log_dir = PathBuf::from("data/shard-0/log");
    fs.create_dir_all(&log_dir).expect("dir");
    for id in ids {
        fs.create_segment(&log_dir.join(segment_file_name(id)), 64).expect("segment");
    }
    (fs, log_dir)
}

fn touch(fs: &MemFs, log_dir: &Path, name: &str) {
    fs.create_segment(&log_dir.join(name), 8).expect("file");
}

proptest! {
    /// Contiguous ranges — starting anywhere (truncation deletes prefixes)
    /// — scan cleanly and in ascending order.
    #[test]
    fn contiguous_ranges_scan_clean(start in 0u32..1000, len in 0u32..40) {
        let ids = (start..start + len).map(SegmentId);
        let (fs, log_dir) = fs_with_segments(ids);
        let scan = scan_log_dir(&fs, &log_dir).expect("contiguous range is valid");
        let expected: Vec<SegmentId> = (start..start + len).map(SegmentId).collect();
        prop_assert_eq!(scan.segments(), expected.as_slice());
        prop_assert_eq!(scan.tail(), (len > 0).then(|| SegmentId(start + len - 1)));
    }

    /// Removing one interior segment produces the exact `Gap` error.
    #[test]
    fn interior_gap_is_named(start in 0u32..1000, len in 3u32..40, hole in any::<prop::sample::Index>()) {
        let missing = SegmentId(start + 1 + hole.index(len as usize - 2) as u32);
        let ids = (start..start + len).map(SegmentId).filter(|id| *id != missing);
        let (fs, log_dir) = fs_with_segments(ids);
        prop_assert_eq!(
            scan_log_dir(&fs, &log_dir),
            Err(ScanError::Gap { expected: missing, found: SegmentId(missing.0 + 1) })
        );
    }

    /// A second, non-canonically padded file for an existing id is the
    /// documented `Duplicate` error.
    #[test]
    fn padded_duplicate_is_named(start in 0u32..100, len in 1u32..20, dup in any::<prop::sample::Index>()) {
        let target = SegmentId(start + dup.index(len as usize) as u32);
        let (fs, log_dir) = fs_with_segments((start..start + len).map(SegmentId));
        let padded = format!("seg-{:09}.ilog", target.0);
        prop_assume!(padded != segment_file_name(target));
        touch(&fs, &log_dir, &padded);
        match scan_log_dir(&fs, &log_dir) {
            Err(ScanError::Duplicate { id, .. }) => prop_assert_eq!(id, target),
            other => prop_assert!(false, "expected Duplicate, got {:?}", other),
        }
    }

    /// Foreign, truncated, or out-of-range names are `BadName` — never
    /// skipped.
    #[test]
    fn foreign_files_are_named(
        start in 0u32..100,
        len in 0u32..20,
        bad in prop_oneof![
            Just("seg-00001.ilog".to_string()),          // 5 digits: truncated
            Just("seg-.ilog".to_string()),
            Just("seg-000001.ilog.tmp".to_string()),     // rename debris
            Just("seg-4294967296.ilog".to_string()),     // > u32::MAX
            Just("seg-00000042.ilog2".to_string()),
            Just("MANIFEST-old".to_string()),
            "[a-z]{1,12}\\.dat",                          // arbitrary junk
        ],
    ) {
        prop_assume!(inf_log::parse_segment_file_name(&bad).is_none());
        let (fs, log_dir) = fs_with_segments((start..start + len).map(SegmentId));
        touch(&fs, &log_dir, &bad);
        prop_assert_eq!(scan_log_dir(&fs, &log_dir), Err(ScanError::BadName { name: bad }));
    }
}

#[test]
fn empty_dir_is_a_clean_fresh_boot() {
    let (fs, log_dir) = fs_with_segments([]);
    let scan = scan_log_dir(&fs, &log_dir).expect("empty is fine");
    assert!(scan.is_empty());
    assert_eq!(scan.tail(), None);
}

#[test]
fn missing_dir_is_io_error() {
    let fs = MemFs::new();
    let log_dir = PathBuf::from("data/shard-9/log");
    assert!(matches!(scan_log_dir(&fs, &log_dir), Err(ScanError::Io { .. })));
}
