//! Frame-decoder fuzz target (M2-S01 AC; both formats since M2.5-S12,
//! ADR-0031; v3 since M4.5-S34, ADR-0086 D3). Three oracles on arbitrary
//! bytes:
//!
//! 1. No panic/UB anywhere in frame or record decoding — v1 (`IFR1`),
//!    v2 (`IFR2`), and v3 (`IFR3`) headers all explored from raw bytes,
//!    including v3 frames whose padding region is garbage or truncated
//!    (the reader skips padding, never validates it).
//! 2. One value, one encoding (L7): any v2/v3 frame that decodes cleanly
//!    must re-encode byte-identically through `FrameBuilder` (stamp and
//!    layout included; a v3 re-encoding compares the frame bytes, not the
//!    padding) — the property that caught the non-minimal-varint bug in
//!    the fabric codec. v1 frames have no writer anymore (read-only
//!    format), so the oracle checks their decoded stamp is `None` instead.
//! 3. Kernel differential: the dispatched CRC32C must agree with the
//!    slicing-by-8 oracle on every input the decoder touches.
//!
//! The successor invariant rides along: a v3 frame's `padded_len` is the
//! 4 KiB round-up of its `frame_len`, and `FrameIter` advances by it.
#![no_main]

use inf_log::{DEFAULT_MAX_FRAME_LEN, FRAME_ALIGN, FrameBuilder, FrameIter, FrameLayout};
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
        let layout = frame.layout();
        let frame_len = frame.frame_len();
        match layout {
            FrameLayout::Packed => assert_eq!(frame.padded_len(), frame_len),
            FrameLayout::Aligned => {
                assert_eq!(frame.padded_len(), frame_len.div_ceil(FRAME_ALIGN) * FRAME_ALIGN);
                assert_eq!(frame.padded_len() % FRAME_ALIGN, 0);
            }
        }
        if clean && builder.record_count() == frame.record_count() {
            let reencoded = builder.finalize(frame.first_lsn(), stamp, layout);
            let frame_bytes = &reencoded[..frame_len as usize];
            let original = &data[offset..offset + frame_bytes.len()];
            assert_eq!(frame_bytes, original, "decode→encode not byte-identical");
            assert!(
                reencoded[frame_len as usize..].iter().all(|&b| b == 0),
                "writer padding is zeroed"
            );
        }
    }
});
