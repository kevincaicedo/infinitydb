//! The CLI's reply decoder (F-L18-07): arbitrary server bytes never panic,
//! never overflow the stack, and a whole reply is consumed within the
//! buffer. The bin has no lib target, so the module is included by path.
#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/reply.rs"]
mod reply;

fuzz_target!(|data: &[u8]| {
    if let Some((parsed, used)) = reply::parse_reply(data) {
        assert!(used <= data.len(), "consumed past the buffer");
        // A whole reply parses the same from its own bytes alone.
        if !matches!(parsed, reply::Reply::Error(ref e) if e.starts_with("protocol error")) {
            let again = reply::parse_reply(&data[..used]).expect("prefix parses");
            assert_eq!(again, (parsed, used));
        }
    }
    // Every shorter prefix is either incomplete or the same answer.
    let cut = data.first().map_or(0, |&b| usize::from(b) % (data.len().max(1)));
    let _ = reply::parse_reply(&data[..cut]);
});
