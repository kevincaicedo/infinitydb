//! RESP *reply* framing totality (review 2026-08-30, finding C6 /
//! ADR-0097): whatever bytes reach the line-framed writers, the buffer they
//! produce must split into exactly one frame per call. `resp_parse` fuzzes
//! the request half; nothing fuzzed the reply half, and that is the half a
//! client's own argument bytes reach through every `-ERR` that quotes them.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_wire::{Protocol, RespWriter};

/// Walks a buffer of line-framed replies, returning how many it contains.
/// Panics with the offending offset when the bytes are not exactly that —
/// which is the C6 signature (a body carrying its own CRLF).
fn count_line_replies(buf: &[u8]) -> usize {
    let mut at = 0;
    let mut replies = 0;
    while at < buf.len() {
        let tag = buf[at];
        assert!(tag == b'+' || tag == b'-', "reply {replies} starts with {tag:#04x} at {at}");
        let mut end = at + 1;
        while end < buf.len() {
            match buf[end] {
                b'\r' => break,
                b'\n' => panic!("bare LF inside reply {replies} at {end}"),
                _ => end += 1,
            }
        }
        assert!(end + 1 < buf.len(), "reply {replies} is unterminated at {end}");
        assert_eq!(buf[end + 1], b'\n', "lone CR inside reply {replies} at {end}");
        at = end + 2;
        replies += 1;
    }
    replies
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // The first byte steers the protocol and how the rest is cut into reply
    // bodies, so the corpus explores boundaries between replies.
    let proto = if data[0] & 1 == 0 { Protocol::Resp2 } else { Protocol::Resp3 };
    let chunk = usize::from(data[0] >> 1).max(1);

    let mut out = Vec::new();
    let mut written = 0usize;
    {
        let mut w = RespWriter::new(&mut out, proto);
        for piece in data[1..].chunks(chunk) {
            w.error_bytes(piece);
            written += 1;
            // The `&str` writers see the same bytes whenever they are UTF-8,
            // and must agree with the raw one.
            if let Ok(text) = core::str::from_utf8(piece) {
                w.error(text);
                w.simple(text);
                written += 2;
            }
        }
    }
    assert_eq!(count_line_replies(&out), written, "reply count changed under the payload");

    // Sanitizing is idempotent: re-writing an already-sanitized body must
    // reproduce it byte for byte (no drift for a proxy that replays errors).
    let mut once = Vec::new();
    RespWriter::new(&mut once, proto).error_bytes(&data[1..]);
    let body = &once[1..once.len() - 2];
    let mut twice = Vec::new();
    RespWriter::new(&mut twice, proto).error_bytes(body);
    assert_eq!(once, twice, "sanitization is not idempotent");

    // ADR-0099 (review 2026-08-30, C9): a patched bulk must equal the
    // plain bulk byte for byte whatever the payload, and a failing
    // builder must leave no trace of the frame. (The ≥ 100 MB header
    // widening is unit-tested — per-exec payloads that size would stall
    // the fuzzer without exploring anything new.)
    let payload = &data[1..];
    let mut plain = Vec::new();
    RespWriter::new(&mut plain, proto).bulk(payload);
    let mut patched = Vec::new();
    RespWriter::new(&mut patched, proto).bulk_patched(|out| out.extend_from_slice(payload));
    assert_eq!(plain, patched, "patched bulk diverged from plain bulk");
    let mut rolled = Vec::new();
    let mut w = RespWriter::new(&mut rolled, proto);
    assert!(
        w.try_bulk_patched(|out| {
            out.extend_from_slice(payload);
            Err::<(), ()>(())
        })
        .is_err()
    );
    w.error("ERR reply too large");
    assert_eq!(rolled, b"-ERR reply too large\r\n", "rollback left frame residue");
});
