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

/// Lane L10 perf/DX row (review 2026-08-30, batch 59): the per-object
/// entry scratch is pooled across parses, and one wide object used to
/// pin its peak (12 B per key — about 2× the text of a `{"k":0,…}`
/// shape) for the life of the parser. After a parse the parser keeps at
/// most `OBJ_ENTRIES_RETAIN_MAX` entries per frame, so what it retains
/// is bounded by the text's block masks, not by the widest object it
/// ever saw.
#[test]
fn retained_scratch_after_a_wide_object_is_bounded() {
    use std::io::Write as _;
    const TEXT: usize = 12 << 20;
    let mut text = Vec::with_capacity(TEXT + 16);
    text.push(b'{');
    let mut i = 0u32;
    while text.len() < TEXT {
        if i > 0 {
            text.push(b',');
        }
        write!(text, "\"{i:x}\":0").expect("vec write");
        i += 1;
    }
    text.push(b'}');
    let mut p = JsonParser::new();
    let mut out = Vec::new();
    p.parse_into(&text, &mut out).expect("parses");
    assert!(
        p.scratch_bytes() <= text.len(),
        "retained scratch {} B after one {} B parse of {i} keys",
        p.scratch_bytes(),
        text.len()
    );
}

/// Batch 59 A/B instrument (lane L10 perf rows: pooled dup-scan vectors,
/// post-parse scratch trim): ns per parse on the S20 budget shapes plus
/// two wide objects — 5 000 keys (past `LINEAR_SCAN_MAX`, the two fresh
/// `Vec<u32>` per parse pre-fix) and 200 000 keys (past the retain cap:
/// the regrow-per-parse regime the trim introduces). Run `--release
/// --ignored --nocapture` on both trees, three replicates each.
#[test]
#[ignore = "timing witness: run --release --nocapture on both trees"]
#[allow(
    clippy::disallowed_methods,
    reason = "test-thread instrument: wall time is the measurement"
)]
fn parse_timing_witness() {
    #[allow(dead_code, unused_imports)]
    #[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
    mod doc_corpus;
    use std::io::Write as _;
    let wide = |keys: u32| -> Vec<u8> {
        let mut t = Vec::with_capacity(keys as usize * 10);
        t.push(b'{');
        for i in 0..keys {
            if i > 0 {
                t.push(b',');
            }
            write!(t, "\"k{i}\":1").expect("vec write");
        }
        t.push(b'}');
        t
    };
    let corpus = doc_corpus::generate(doc_corpus::CANONICAL_SEED);
    let mut shapes: Vec<(String, Vec<u8>)> = corpus
        .into_iter()
        .filter(|d| d.name == "gate-1KiB" || d.name == "medium-2KiB" || d.name == "large-64KiB")
        .map(|d| (d.name.to_string(), d.json.into_bytes()))
        .collect();
    shapes.push(("wide-5k-keys".into(), wide(5_000)));
    shapes.push(("wide-200k-keys".into(), wide(200_000)));
    let mut p = JsonParser::new();
    let mut out = Vec::new();
    for (name, text) in &shapes {
        for _ in 0..20 {
            p.parse_into(text, &mut out).expect("parses");
        }
        let iters = (400_000_000 / text.len()).clamp(20, 200_000);
        let start = std::time::Instant::now();
        for _ in 0..iters {
            p.parse_into(text, &mut out).expect("parses");
        }
        let ns = start.elapsed().as_nanos() as f64 / iters as f64;
        println!(
            "{name:>16} {:>9} B  {ns:>12.0} ns/parse  {:>8.1} MB/s  scratch {} B",
            text.len(),
            text.len() as f64 / ns * 1e3,
            p.scratch_bytes()
        );
    }
}
