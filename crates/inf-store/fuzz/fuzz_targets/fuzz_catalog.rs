//! Catalog-payload fuzz (M4.5-S03, ADR-0075 D2, L9): the `META` payload
//! decoder grew the v3 index section, so the whole decoder gets its
//! target in the same PR. Laws:
//!
//! - **Totality:** `NsCatalog::decode` never panics on arbitrary bytes —
//!   every refusal is a typed [`CatalogError`].
//! - **Normalization fixpoint:** for accepted input, `encode` is a
//!   normalizer (v1 upgrades, pristine index sections drop to v2) and
//!   re-decoding the normalized bytes reproduces the same catalog:
//!   `decode(encode(decode(b))) == decode(b)`.
//! - **Version pivot:** the normalized payload is v3 exactly when the
//!   index feature has been used (ADR-0075 D2.2), v2 otherwise.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_store::NsCatalog;

fuzz_target!(|data: &[u8]| {
    let Ok(decoded) = NsCatalog::decode(data) else {
        return; // typed rejection — panics are the bug
    };
    let normalized = decoded.encode();
    let expected_version = if decoded.index.is_pristine() { 2 } else { 3 };
    assert_eq!(normalized[0], expected_version, "the D2.2 version pivot");
    let reencoded = NsCatalog::decode(&normalized).expect("normalized bytes decode");
    assert_eq!(reencoded, decoded, "encode is a normalizer, not a mutator");
    assert_eq!(reencoded.encode(), normalized, "normalization is a fixpoint");
});
