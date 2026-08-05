//! Blob-extent v1 decoder fuzz target (M4-S17, ADR-0061 D9, L9): the
//! sweep/read-side parse of untrusted disk bytes must never panic or run
//! unbounded. Oracles:
//!
//! 1. No panic anywhere — header parse, geometry, frame-CRC scan — on
//!    any byte pattern.
//! 2. A summary is internally consistent: `expected_frames` is exactly
//!    the declared length's frame span; `complete` implies every
//!    expected frame is present and none scanned bad; a reported bad
//!    frame sits inside the scanned span and forbids completeness.
//! 3. The header parser is total on its own input (reached directly,
//!    not only through the whole-image path).
#![no_main]

use inf_log::{BLOB_HEADER_BYTES, inspect_extent_bytes, parse_extent_header};
use libfuzzer_sys::fuzz_target;

const TIER_FRAME_DATA: u64 = 4092;

fuzz_target!(|data: &[u8]| {
    // Layer 1: the block parser directly.
    let _ = parse_extent_header(data);
    // Layer 2: the whole-image decoder as the sweep sees a file.
    if let Ok(summary) = inspect_extent_bytes(data) {
        assert!(summary.header.data_len > 0, "a parsed header names at least one byte");
        assert_eq!(
            summary.expected_frames,
            summary.header.data_len.div_ceil(TIER_FRAME_DATA),
            "expected frames follow the declared length exactly"
        );
        let body = data.len() - BLOB_HEADER_BYTES;
        assert!(summary.frames <= (body / 4096) as u64 + 1, "frames fit the image");
        if summary.complete {
            assert!(summary.frames >= summary.expected_frames, "complete implies coverage");
            assert_eq!(summary.first_bad_frame, None, "complete implies clean CRCs");
        }
        if let Some(bad) = summary.first_bad_frame {
            assert!(bad < summary.frames.min(summary.expected_frames), "bad frame in span");
            assert!(!summary.complete, "a bad frame forbids completeness");
        }
    }
});
