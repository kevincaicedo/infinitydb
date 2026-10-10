//! Snapshot parsing is total, byte-exact and consumes one complete frame.
#![no_main]

use inf_server::parse_take_reply;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_take_reply(data);
    let value = &data[..data.len().min(4096)];
    let mut number = [0u8; 8];
    let take = data.len().min(number.len());
    number[..take].copy_from_slice(&data[..take]);
    let deadline = i64::from_le_bytes(number);
    let mut raw = format!("*2\r\n${}\r\n", value.len()).into_bytes();
    raw.extend_from_slice(value);
    raw.extend_from_slice(format!("\r\n:{deadline}\r\n").as_bytes());
    assert_eq!(parse_take_reply(&raw), Some(Some((value.to_vec(), deadline))));
    for cut in [0, raw.len() / 2, raw.len() - 1] {
        assert!(parse_take_reply(&raw[..cut]).is_none());
    }
    for suffix in [b"x".as_slice(), b"\r\n", b"+OK\r\n"] {
        let mut extra = raw.clone();
        extra.extend_from_slice(suffix);
        assert!(parse_take_reply(&extra).is_none());
    }
    for len in [usize::MAX, usize::MAX - 1, usize::MAX - 20] {
        let overflow = format!("*2\r\n${len}\r\nx\r\n:-1\r\n");
        assert!(parse_take_reply(overflow.as_bytes()).is_none());
    }
});
