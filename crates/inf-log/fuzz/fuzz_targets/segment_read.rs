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
#![no_main]

use std::path::Path;

use inf_log::fs::SegmentFs;
use inf_log::fs::mem::MemFs;
use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FrameIter, Lsn, ReaderConfig, SegmentId,
    SegmentReader,
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
    for item in FrameIter::new(data, DEFAULT_MAX_FRAME_LEN) {
        let Ok((offset, frame)) = item else { break };
        let expected = Lsn::new(SegmentId(0), (offset + FRAME_HEADER_LEN) as u32);
        if frame.first_lsn() != expected {
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
            Err(_) => {
                // Typed error: the reader is fused afterwards.
                assert!(matches!(reader.next_frame(), Ok(None)), "reader must fuse after error");
                break;
            }
        }
    }
});
