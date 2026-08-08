//! MANIFEST parser fuzz target (M2-S11, milestone §5 test plan): the
//! recovery-unit manifest is boot-path input, so both layers — the
//! META/MANIFEST envelope (magic/length/CRC) and the schema-v1 payload —
//! decode arbitrary bytes without panic/UB. Oracles:
//!
//! 1. No panic anywhere in envelope or payload decoding.
//! 2. Canonicality (L7, one value ↔ one encoding): a payload that decodes
//!    cleanly re-encodes byte-identically.
//! 3. A manifest that decodes cleanly satisfies its documented invariants
//!    (non-empty, strictly ascending, floor == begin segment); an epoch-2
//!    manifest (M4-S12, ADR-0057 D5) additionally satisfies the tier
//!    invariants — ascending namespaces, ascending non-overlapping file
//!    ranges tiling inside `[0, flushed)`, 48-bit addresses.
#![no_main]

use inf_log::manifest::Manifest;
use inf_log::meta::decode_envelope;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Layer 1: the envelope (as read_manifest would see the file bytes).
    if let Ok(payload) = decode_envelope(data) {
        let _ = Manifest::decode(payload);
    }
    // Layer 2: the payload directly (reaches deep cases the envelope CRC
    // would otherwise gate on 1-in-2³² of inputs).
    if let Ok(m) = Manifest::decode(data) {
        assert!(!m.segments.is_empty());
        assert!(m.segments.windows(2).all(|p| p[0] < p[1]), "strictly ascending");
        assert_eq!(m.segments[0], m.floor(), "floor is the first live segment");
        for tier in &m.tiers {
            assert!(tier.flushed < (1 << 48), "48-bit watermark");
            let mut prev_end = 0u64;
            for file in &tier.files {
                assert!(file.durable_len >= 1, "empty ranges are never named");
                assert!(file.base >= prev_end, "ranges never overlap");
                assert!(file.end() <= tier.flushed, "ranges tile inside [0, flushed)");
                prev_end = file.end();
            }
            assert_eq!(m.tier_ns(tier.ns), Some(tier), "sections resolvable by ns");
        }
        assert!(m.tiers.windows(2).all(|p| p[0].ns < p[1].ns), "tier ns ascend");
        assert_eq!(m.encode(), data, "canonical: decode → encode is identity");
    }
});
