//! Schema-1 INFO decoding is bounded and never accepts an inconsistent count.
#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/loop_histogram.rs"]
mod loop_histogram;

fuzz_target!(|data: &[u8]| {
    let _ = loop_histogram::Snapshot::decode_info(data);
    if let Ok(text) = core::str::from_utf8(data) {
        if let Ok(counts) = loop_histogram::decode_counts(text) {
            assert_eq!(counts.len(), inf_foundation::LogHistogram::BUCKET_COUNT);
            assert_eq!(text.split(',').count(), counts.len());
        }
    }
});
