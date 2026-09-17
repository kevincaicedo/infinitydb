//! The competitive harness's RESP framer (F-L18-07): arbitrary server bytes
//! never panic, never overflow the stack, and a framed reply lies within
//! the buffer and is stable under extension. The bin has no lib target, so
//! the module is included by path.
#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/resp.rs"]
mod resp;

fuzz_target!(|data: &[u8]| {
    let result = resp::reply_len(data);
    if let Ok(Some(n)) = result {
        assert!(n <= resp::MAX_BYTES);
        assert!(n <= data.len(), "framed past the buffer");
        assert_eq!(resp::reply_len(&data[..n]), Ok(Some(n)), "frame is self-contained");
    }
    let cut = data.first().map_or(0, |&b| usize::from(b) % (data.len().max(1)));
    if let Ok(Some(m)) = resp::reply_len(&data[..cut]) {
        assert_eq!(result, Ok(Some(m)), "extending the buffer keeps the frame");
    }
    let mut decoder = resp::Decoder::default();
    let _ = decoder.advance(&data[..cut]);
    assert_eq!(decoder.advance(data), result, "fragmentation preserves the verdict");
    assert_eq!(decoder.advance(data), result, "terminal verdicts are stable");
});
