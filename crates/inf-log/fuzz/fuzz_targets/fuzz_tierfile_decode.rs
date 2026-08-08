//! Tier-file v1 decoder fuzz target (M4-S11, ADR-0056 D7, L9): the
//! recovery/verification-side parse of untrusted disk bytes must never
//! panic or run unbounded. Oracles:
//!
//! 1. No panic anywhere — header parse, footer probe, geometry check,
//!    frame-CRC scan — on any byte pattern.
//! 2. A sealed verdict is internally consistent: the footer's frame
//!    count matches the image geometry exactly, and `data_len` fits the
//!    frame span (the decoder never claims more bytes than blocks hold).
//! 3. The block parsers are total on their own inputs (reached directly,
//!    not only through the CRC-gated whole-image path).
#![no_main]

use inf_log::{
    TIER_FRAME_DATA, TIER_HEADER_BYTES, inspect_tier_bytes, parse_tier_footer, parse_tier_header,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Layer 1: the block parsers directly (deep cases the whole-image
    // CRC gate would hide behind 1-in-2³² inputs).
    let _ = parse_tier_header(data);
    let _ = parse_tier_footer(data);
    // Layer 2: the whole-image decoder as recovery sees a file.
    if let Ok(summary) = inspect_tier_bytes(data) {
        if let Some(footer) = summary.sealed {
            let frames = footer.data_len.div_ceil(TIER_FRAME_DATA as u64);
            assert_eq!(summary.frames, frames, "sealed geometry is exact");
            let body = data.len() - TIER_HEADER_BYTES;
            assert_eq!(body as u64 % 4096, 0, "sealed images are block-aligned");
        }
        if let Some(bad) = summary.first_bad_frame {
            assert!(bad < summary.frames, "the bad frame is inside the scanned span");
        }
    }
});
