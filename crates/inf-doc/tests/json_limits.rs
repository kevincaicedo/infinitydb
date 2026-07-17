//! M3-S07 ACs: bounded everything at document ingest.
//!
//! - Oversize / overdeep inputs reject with the documented errors, and
//!   peak memory during a rejection stays bounded — asserted through the
//!   attribution surfaces (`JsonParser::scratch_bytes`, the caller-owned
//!   `parse_into` buffer capacity), not eyeballed.
//! - The **dual bound** is proven in its pathological direction: a
//!   small-token document that PASSES the text cap and FAILS the
//!   idoc-byte cap (`1e1` is 3 text bytes and 9 tape bytes) is rejected
//!   by the incremental stage-2 guard with memory still bounded by the
//!   caps — the text pre-check alone does not bound memory, exactly as
//!   the plan's risk row states.
//! - Limits clamp to the format ceilings (config lowers, never raises).
//!
//! RESP-layer phrasing (`ERR document too large`) binds at S11 against
//! the oracle; what is pinned here is the typed kind and the library
//! Display line.

use inf_doc::{JsonErrorKind, JsonParser, ParseLimits};

/// `[1e1,1e1,…]`: ~4 text bytes but 9 idoc bytes per element.
fn small_token_array(elements: usize) -> Vec<u8> {
    let mut text = Vec::with_capacity(4 * elements + 2);
    text.push(b'[');
    for i in 0..elements {
        if i > 0 {
            text.push(b',');
        }
        text.extend_from_slice(b"1e1");
    }
    text.push(b']');
    text
}

#[test]
fn text_cap_rejects_before_any_allocation() {
    let mut p = JsonParser::with_limits(ParseLimits { max_text: 1024, ..ParseLimits::default() });
    let input = vec![b'x'; 4096]; // not even valid JSON — never inspected
    let mut out = Vec::new();
    let e = p.parse_into(&input, &mut out).unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::DocumentTooLarge);
    assert_eq!(e.offset, 0);
    // Reject-before-allocate, observably: no scratch, no output buffer.
    assert_eq!(p.scratch_bytes(), 0);
    assert_eq!(out.capacity(), 0);
}

#[test]
fn pathological_small_token_corpus_hits_the_idoc_bound() {
    const CAP: usize = 64 << 10;
    let limits = ParseLimits { max_depth: 128, max_text: CAP, max_body: CAP };
    let mut p = JsonParser::with_limits(limits);
    // ~48 KiB of text (passes the text cap) that would encode to
    // ~108 KiB of tape (fails the idoc cap): the text-cap-passes /
    // idoc-cap-fails case, proven explicitly.
    let text = small_token_array(12_000);
    assert!(text.len() <= CAP, "corpus must pass the text cap");
    let mut out = Vec::new();
    let e = p.parse_into(&text, &mut out).unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::DocumentTooLarge);
    // The incremental guard aborted the build mid-stream: held memory is
    // bounded by the caps (+ Vec doubling slack), never by the would-be
    // 108 KiB document.
    assert!(
        out.len() <= inf_doc::HEADER_LEN + CAP + 16,
        "output length {} exceeds cap + one token",
        out.len()
    );
    assert!(
        out.capacity() <= 2 * (CAP + 64),
        "output capacity {} exceeds cap + growth slack",
        out.capacity()
    );
    // Scratch is proportional to the (text-capped) input, not the output:
    // the structural index is ≤ 1 entry per text byte plus growth slack.
    assert!(
        p.scratch_bytes() <= 8 * text.len() + 4096,
        "scratch {} not bounded by the text cap",
        p.scratch_bytes()
    );
}

#[test]
fn configured_depth_rejects_downward() {
    let mut p = JsonParser::with_limits(ParseLimits { max_depth: 4, ..ParseLimits::default() });
    assert!(p.parse(b"[[[[1]]]]").is_ok(), "depth 4 fits a depth-4 limit");
    let e = p.parse(b"[[[[[1]]]]]").unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::DepthExceeded);
    assert_eq!(e.offset, 4, "the fifth opener is the offending byte");
}

#[test]
fn limits_clamp_to_format_ceilings() {
    // Raising past the ceilings is silently clamped: a 129-deep document
    // still rejects, and the body cap stays the u24 ceiling.
    let mut p = JsonParser::with_limits(ParseLimits {
        max_depth: 100_000,
        max_text: usize::MAX,
        max_body: usize::MAX,
    });
    let too_deep = format!("{}1{}", "[".repeat(129), "]".repeat(129));
    let e = p.parse(too_deep.as_bytes()).unwrap_err();
    assert_eq!(e.kind, JsonErrorKind::DepthExceeded);
}

#[test]
fn rejection_error_lines_are_documented() {
    let mut p = JsonParser::with_limits(ParseLimits { max_text: 8, ..ParseLimits::default() });
    let e = p.parse(b"[1,2,3,4,5]").unwrap_err();
    assert_eq!(e.to_string(), "document too large at offset 0");

    let mut p = JsonParser::with_limits(ParseLimits { max_depth: 2, ..ParseLimits::default() });
    let e = p.parse(b"[[[1]]]").unwrap_err();
    assert_eq!(e.to_string(), "document nesting too deep at offset 2");
}

/// The recycled ingest buffer keeps serving after a rejection — a refused
/// document must not poison the seam (the S11 command path reuses one
/// buffer per cell).
#[test]
fn buffer_reuse_survives_rejection() {
    const CAP: usize = 4 << 10;
    let mut p = JsonParser::with_limits(ParseLimits {
        max_text: CAP,
        max_body: CAP,
        ..ParseLimits::default()
    });
    let mut out = Vec::new();
    let reject = small_token_array(1000); // ~3.9 KiB text → ~9 KiB idoc
    assert!(reject.len() <= CAP);
    assert_eq!(p.parse_into(&reject, &mut out).unwrap_err().kind, JsonErrorKind::DocumentTooLarge);
    p.parse_into(b"{\"ok\":true}", &mut out).expect("parses after a rejection");
    let doc = inf_doc::TapeDoc::from_bytes(&out).expect("valid canonical idoc");
    let mut text = Vec::new();
    inf_doc::serialize_canonical_into(doc.root().into(), &mut text);
    assert_eq!(text, b"{\"ok\":true}");
}
