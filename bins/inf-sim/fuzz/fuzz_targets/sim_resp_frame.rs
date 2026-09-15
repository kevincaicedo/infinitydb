//! The sim clients' RESP framer (F-L18-07): the total framer behind
//! `reply_len` never panics and never overflows the stack on arbitrary
//! bytes; a framed reply lies within the buffer and is stable under
//! extension. (`reply_len` and `parse_reply` panic on malformed input by
//! design — the server under test produced it — so the fuzz target drives
//! the framer's `Result` form only.)
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(Some(n)) = inf_sim::resp::try_reply_len(data) {
        assert!(n <= data.len(), "framed past the buffer");
        assert_eq!(inf_sim::resp::try_reply_len(&data[..n]), Ok(Some(n)));
        // `parse_reply` is deliberately not driven: it panics on anything
        // the M0 surface never emits (a non-integer `:` line, a RESP3 tag),
        // and libFuzzer's panic hook makes every such panic a finding.
    }
    let cut = data.first().map_or(0, |&b| usize::from(b) % (data.len().max(1)));
    if let Ok(Some(m)) = inf_sim::resp::try_reply_len(&data[..cut]) {
        assert_eq!(inf_sim::resp::try_reply_len(data), Ok(Some(m)));
    }
});
