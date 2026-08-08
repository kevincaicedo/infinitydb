//! Frame-decoder fuzz target (M2-S01 AC; both formats since M2.5-S12,
//! ADR-0031). Three oracles on arbitrary bytes:
//!
//! 1. No panic/UB anywhere in frame or record decoding — v1 (`IFR1`) and
//!    v2 (`IFR2`) headers both explored from raw bytes.
//! 2. One value, one encoding (L7): any v2 frame that decodes cleanly must
//!    re-encode byte-identically through `FrameBuilder` (stamp included) —
//!    the property that caught the non-minimal-varint bug in the fabric
//!    codec. v1 frames have no writer anymore (read-only format), so the
//!    oracle checks their decoded stamp is `None` instead.
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
        let Some(stamp) = frame.stamp() else {
            assert_eq!(frame.header_len(), 20, "v1 frames carry the 20-byte header");
            continue;
        };
        assert!(stamp.epoch > 0, "decoder admitted a reserved epoch");
        assert!(stamp.seq > 0, "decoder admitted a reserved seq");
        assert!(
            stamp.covered_lsn <= frame.first_lsn().to_u64(),
            "decoder admitted an attestation past the frame's own records"
        );
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
            let reencoded = builder.finalize(frame.first_lsn(), stamp);
            let original = &data[offset..offset + reencoded.len()];
            assert_eq!(reencoded, original, "decode→encode not byte-identical");
        }
    }
});
