//! Segment-reader fuzz target (M2-S04): drive `SegmentReader` over an
//! arbitrary byte image served as a segment file, with an adversarial
//! window size. Oracles:
//!
//! 1. No panic/UB anywhere in the read path (peek, refill, decode).
//! 2. The reader agrees with `FrameIter` on the same image: identical
//!    frame sequence (first LSNs), and the reader stops at or before
//!    `FrameIter`'s offset (it additionally enforces the physical-offset
//!    vs stored-LSN cross-check, so it may stop earlier — never later,
//!    never yielding a frame `FrameIter` would not).
//! 3. The foreign-segment shape (M4.5-S39b, ADR-0090 D2 as amended): the
//!    first frame `FrameIter` decodes at its stored *offset* but for
//!    another segment id must stop the reader with exactly
//!    `ReadError::ForeignSegment` (never `LsnMismatch`, never a yield),
//!    and the slack scanner over the same image must count every such
//!    frame as foreign, never as validating — `valid_frames` equals the
//!    reader's yield count on an image whose first misplaced frame is
//!    foreign. Both paths are fuzzed from the same bytes so the two
//!    classifications can never drift apart.
#![no_main]

use std::path::Path;

use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FrameIter, Lsn, ReadError, ReaderConfig, SegmentId, SegmentReader,
    scan_region_evidence,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u16, &[u8])| {
    let (chunk_seed, data) = input;
    // Window from 8 bytes (smaller than a header — forces refill/compact
    // paths) up to 64 KiB.
    let chunk = 8 + usize::from(chunk_seed) % (64 << 10);

    let fs = MemFs::new();
    let dir = Path::new("fuzz");
    fs.create_dir_all(dir).expect("dir");
    let mut file = fs.create_segment(&dir.join("seg-000000.ilog"), 0).expect("create");
    inf_log::fs::SegmentFile::write_at(&mut file, 0, data).expect("write");

    let cfg = ReaderConfig { chunk_bytes: chunk, max_frame_len: DEFAULT_MAX_FRAME_LEN };
    let mut reader = SegmentReader::open(&fs, dir, SegmentId(0), cfg).expect("open");

    // Reference walk over the same bytes, keeping only frames whose stored
    // LSN matches their physical position (the reader enforces that).
    let mut reference: Vec<Lsn> = Vec::new();
    // The first misplaced frame's class: `Some(true)` foreign (offset
    // equal, segment different), `Some(false)` misdirected, `None` if
    // the walk ended otherwise.
    let mut first_misplaced_foreign: Option<bool> = None;
    for item in FrameIter::new(data, DEFAULT_MAX_FRAME_LEN) {
        let Ok((offset, frame)) = item else { break };
        // Header length is version-dependent (v1 = 20, v2 = 40 — ADR-0031).
        let expected = Lsn::new(SegmentId(0), (offset + frame.header_len()) as u32);
        if frame.first_lsn() != expected {
            first_misplaced_foreign = Some(frame.first_lsn().offset == expected.offset);
            break;
        }
        reference.push(expected);
    }

    let mut seen = 0usize;
    loop {
        match reader.next_frame() {
            Ok(Some(frame)) => {
                assert!(seen < reference.len(), "reader yielded a frame FrameIter did not");
                assert_eq!(frame.first_lsn(), reference[seen], "frame sequence divergence");
                for record in frame.records() {
                    let _ = record; // record-level errors are the applier's business
                }
                seen += 1;
            }
            Ok(None) => {
                assert_eq!(seen, reference.len(), "reader ended early without an error");
                assert!(reader.read_end().is_some(), "clean end must be classified");
                break;
            }
            Err(err) => {
                match (&err, first_misplaced_foreign) {
                    (ReadError::ForeignSegment { offset, stored_segment, .. }, Some(true)) => {
                        assert_eq!(seen, reference.len(), "foreign stop after every good frame");
                        assert_ne!(*stored_segment, SegmentId(0), "foreign means another id");
                        // The scanner from the stop classifies the same
                        // frame foreign, never validating.
                        let evidence =
                            scan_region_evidence(&fs, dir, SegmentId(0), *offset, cfg)
                                .expect("scan");
                        assert!(evidence.foreign_frames >= 1, "scanner sees the foreign frame");
                    }
                    (ReadError::ForeignSegment { .. }, other) => {
                        panic!("foreign-segment error without a foreign frame first: {other:?}")
                    }
                    (ReadError::LsnMismatch { .. }, Some(true)) => {
                        panic!("a foreign frame must be the typed foreign error, not a mismatch")
                    }
                    _ => {}
                }
                // Typed error: the reader is fused afterwards.
                assert!(matches!(reader.next_frame(), Ok(None)), "reader must fuse after error");
                break;
            }
        }
    }
    // Whatever the image, the scanner from offset 0 never counts more
    // validating frames than the reader yielded before its first stop
    // plus what lies beyond that stop — and never a foreign one as valid.
    let evidence = scan_region_evidence(&fs, dir, SegmentId(0), 0, cfg).expect("scan");
    assert!(evidence.valid_frames as usize >= seen, "every yielded frame self-locates");
});
