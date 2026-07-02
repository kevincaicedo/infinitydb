//! Frame-decoder fuzz target (M2-S01 AC). Three oracles on arbitrary bytes:
//!
//! 1. No panic/UB anywhere in frame or record decoding.
//! 2. One value, one encoding (L7): any frame that decodes cleanly must
//!    re-encode byte-identically through `FrameBuilder` — the property that
//!    caught the non-minimal-varint bug in the fabric codec.
//! 3. Kernel differential: the dispatched CRC32C must agree with the
//!    slicing-by-8 oracle on every input the decoder touches.
#![no_main]

use inf_log::{DEFAULT_MAX_FRAME_LEN, FrameBuilder, FrameIter};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    assert_eq!(
        inf_simd::crc32c(data),
        inf_simd::scalar_crc32c_update(0, data),
        "CRC32C hardware/software divergence"
    );

    let mut builder = FrameBuilder::new();
    for result in FrameIter::new(data, DEFAULT_MAX_FRAME_LEN) {
        let Ok((offset, frame)) = result else { break };
        builder.reset();
        let mut clean = true;
        for record in frame.records() {
            match record {
                Ok((_, view)) => builder.append(&view),
                Err(_) => {
                    clean = false;
                    break;
                }
            }
        }
        if clean && builder.record_count() == frame.record_count() {
            let reencoded = builder.finalize(frame.first_lsn());
            let original = &data[offset..offset + reencoded.len()];
            assert_eq!(reencoded, original, "decode→encode not byte-identical");
        }
    }
});
