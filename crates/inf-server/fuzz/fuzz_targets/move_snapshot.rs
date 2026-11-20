//! Snapshot parsing is total, byte-exact and consumes one complete frame.
//! The decoder takes a `SuccessView`, so every input is read as a `Success`
//! reply's bytes; an empty one is a reply that wrote nothing.
#![no_main]

use inf_server::parse_take_reply;
use inf_wire::{OpenReply, OwnedReply, ReplyOutcome};
use libfuzzer_sys::fuzz_target;

/// The snapshot decoder over `raw` as a `Success` leg reply; zero bytes,
/// which `received` refuses, as a finished reply that wrote nothing.
fn decode(raw: &[u8]) -> Option<Option<(Vec<u8>, i64)>> {
    let reply = OwnedReply::received(raw.to_vec(), ReplyOutcome::Success)
        .unwrap_or_else(|_| OpenReply::new(Vec::new()).finish());
    parse_take_reply(reply.success().expect("a Success reply has a view"))
}

fuzz_target!(|data: &[u8]| {
    let _ = decode(data);
    let value = &data[..data.len().min(4096)];
    let mut number = [0u8; 8];
    let take = data.len().min(number.len());
    number[..take].copy_from_slice(&data[..take]);
    let deadline = i64::from_le_bytes(number);
    let mut raw = format!("*2\r\n${}\r\n", value.len()).into_bytes();
    raw.extend_from_slice(value);
    raw.extend_from_slice(format!("\r\n:{deadline}\r\n").as_bytes());
    assert_eq!(decode(&raw), Some(Some((value.to_vec(), deadline))));
    for cut in [0, raw.len() / 2, raw.len() - 1] {
        assert!(decode(&raw[..cut]).is_none());
    }
    for suffix in [b"x".as_slice(), b"\r\n", b"+OK\r\n"] {
        let mut extra = raw.clone();
        extra.extend_from_slice(suffix);
        assert!(decode(&extra).is_none());
    }
    for len in [usize::MAX, usize::MAX - 1, usize::MAX - 20] {
        let overflow = format!("*2\r\n${len}\r\nx\r\n:-1\r\n");
        assert!(decode(overflow.as_bytes()).is_none());
    }
});
