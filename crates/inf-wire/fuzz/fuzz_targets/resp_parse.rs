//! RESP parser totality (M0-S11): arbitrary bytes in arbitrary chunkings
//! must never panic, never exceed the bounded accumulator, and stay
//! poisoned after a protocol error.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte steers the chunk size so the corpus explores split points.
    let chunk = usize::from(data[0]).max(1);
    let cap = 4096;
    let mut parser =
        inf_wire::ConnParser::new(inf_wire::ParserLimits { max_frame_bytes: cap, max_args: 256 });
    let mut poisoned_seen = false;
    for piece in data[1..].chunks(chunk) {
        let mut iter = parser.feed(piece);
        while let Some(parsed) = iter.next() {
            match parsed {
                inf_wire::Parsed::Command(argv) | inf_wire::Parsed::Inline(argv) => {
                    assert!(!argv.is_empty());
                    let _ = argv.arg(0);
                }
                inf_wire::Parsed::ProtocolError(_) => {
                    poisoned_seen = true;
                }
                inf_wire::Parsed::Incomplete => unreachable!("iterator never yields Incomplete"),
            }
        }
        drop(iter);
        assert!(
            parser.buffered() <= cap + piece.len() + 2,
            "accumulator exceeded its bound"
        );
        if poisoned_seen {
            assert!(parser.is_poisoned());
        }
    }
});
