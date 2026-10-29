//! The load generator's RESP framer (F-L18-07): arbitrary server bytes
//! never panic, never overflow the stack, and a framed reply lies within
//! the buffer and is stable under extension. The bin has no lib target, so
//! the module is included by path.
#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/resp.rs"]
mod resp;

fuzz_target!(|data: &[u8]| {
    if let Some(n) = resp::reply_len(data) {
        assert!(n <= data.len(), "framed past the buffer");
        assert_eq!(resp::reply_len(&data[..n]), Some(n), "a framed reply is self-contained");
    }
    let cut = data.first().map_or(0, |&b| usize::from(b) % (data.len().max(1)));
    if let Some(m) = resp::reply_len(&data[..cut]) {
        assert_eq!(resp::reply_len(data), Some(m), "extending the buffer keeps the frame");
    }
});
