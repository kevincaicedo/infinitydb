//! Scatter-reply re-parsers (F-L12-06): the plane re-reads its peers'
//! RESP replies to splice a cross-cell `SCAN`/`INF.PEEK` answer. Every
//! byte string, truncated at every length, must parse totally — never a
//! panic — and a `Some` from the SCAN head must name an offset inside
//! the buffer (the caller slices `&raw[at..]` unchecked).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for n in [data.len(), data.len().saturating_sub(1), data.len().saturating_sub(2)] {
        let raw = &data[..n];
        if let Some((_, at)) = inf_server::parse_scan_head(raw) {
            assert!(at <= raw.len(), "scan head offset {at} past {}", raw.len());
        }
        if let Some((_, at)) = inf_server::parse_array_header(raw) {
            assert!(at <= raw.len(), "array header offset {at} past {}", raw.len());
        }
        let _ = inf_server::parse_take_reply(raw);
    }
});
