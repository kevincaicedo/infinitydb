//! Declared bounds must reject before buffering a payload or narrowing argv offsets.

use inf_wire::{ConnParser, Parsed, ParserLimits, WireError};

fn parse(input: &[u8], limits: ParserLimits) -> Result<Vec<Vec<u8>>, WireError> {
    let mut parser = ConnParser::new(limits);
    let mut iter = parser.feed(input);
    match iter.next() {
        Some(Parsed::Command(argv) | Parsed::Inline(argv)) => {
            Ok(argv.iter().map(<[u8]>::to_vec).collect())
        }
        Some(Parsed::ProtocolError(error)) => Err(error),
        Some(Parsed::Incomplete) | None => Ok(Vec::new()),
    }
}

#[test]
fn argv_representation_refuses_oversized_declarations_before_payload() {
    let limits = ParserLimits {
        max_bulk_bytes: usize::MAX,
        max_frame_bytes: usize::MAX,
        max_args: usize::MAX,
    };
    for length in [u64::from(u32::MAX), u64::from(u32::MAX) + 1, i64::MAX as u64] {
        let header = format!("*1\r\n${length}\r\n");
        assert!(matches!(parse(header.as_bytes(), limits), Err(WireError::FrameTooLong { .. })));
    }
    for count in [u64::from(u32::MAX) + 1, i64::MAX as u64] {
        let header = format!("*{count}\r\n");
        assert!(matches!(parse(header.as_bytes(), limits), Err(WireError::TooManyArgs { .. })));
    }
    let count_at_cap = format!("*{}\r\n", u32::MAX);
    assert_eq!(parse(count_at_cap.as_bytes(), limits), Ok(Vec::new()));
    let overhead = format!("*1\r\n${}\r\n", u32::MAX).len() + 2;
    let payload_at_cap = u32::MAX as usize - overhead;
    let header = format!("*1\r\n${payload_at_cap}\r\n");
    assert_eq!(parse(header.as_bytes(), limits), Ok(Vec::new()));
    let header = format!("*1\r\n${}\r\n", payload_at_cap + 1);
    assert_eq!(
        parse(header.as_bytes(), limits),
        Err(WireError::FrameTooLong { size: u32::MAX as usize + 1, cap: u32::MAX as usize })
    );
}

#[test]
fn inline_complete_and_partial_frames_obey_the_same_limits() {
    let limits = ParserLimits { max_bulk_bytes: 4, max_frame_bytes: 6, max_args: 1 };
    assert_eq!(parse(b"PING\r\n", limits), Ok(vec![b"PING".to_vec()]));
    for input in [b"HELLO\r\n".as_slice(), b"HELLO".as_slice()] {
        assert_eq!(parse(input, limits), Err(WireError::FrameTooLarge { declared: 5, cap: 4 }));
    }
    let limits = ParserLimits { max_frame_bytes: 5, ..limits };
    assert_eq!(parse(b"PING\r\n", limits), Err(WireError::FrameTooLong { size: 6, cap: 5 }));
}

#[test]
fn multibulk_frame_and_argument_limits_include_equality() {
    let input = b"*1\r\n$4\r\nPING\r\n";
    let limits = ParserLimits { max_bulk_bytes: 4, max_frame_bytes: input.len(), max_args: 1 };
    assert_eq!(parse(input, limits), Ok(vec![b"PING".to_vec()]));
    let short = ParserLimits { max_frame_bytes: input.len() - 1, ..limits };
    assert_eq!(
        parse(input, short),
        Err(WireError::FrameTooLong { size: input.len(), cap: input.len() - 1 })
    );
    let none = ParserLimits { max_args: 0, ..limits };
    assert_eq!(parse(input, none), Err(WireError::TooManyArgs { declared: 1, cap: 0 }));
}

#[test]
fn incomplete_inline_and_empty_multibulk_cannot_bypass_the_frame_cap() {
    let limits = ParserLimits { max_bulk_bytes: 100, max_frame_bytes: 4, max_args: 1 };
    assert_eq!(parse(b"*0\r\n", limits), Ok(Vec::new()));
    assert_eq!(parse(b"PING", limits), Ok(Vec::new()));
    assert_eq!(parse(b"HELLO", limits), Err(WireError::FrameTooLong { size: 5, cap: 4 }));
    let smaller = ParserLimits { max_frame_bytes: 3, ..limits };
    assert_eq!(parse(b"*0\r\n", smaller), Err(WireError::FrameTooLong { size: 4, cap: 3 }));
}

#[test]
fn oversized_declaration_poisons_the_parser_and_releases_carry() {
    let mut parser = ConnParser::new(ParserLimits {
        max_bulk_bytes: usize::MAX,
        max_frame_bytes: usize::MAX,
        max_args: 1,
    });
    let header = b"*1\r\n$4294967296\r\n";
    for byte in &header[..header.len() - 1] {
        let mut iter = parser.feed(core::slice::from_ref(byte));
        assert!(iter.next().is_none());
    }
    {
        let mut iter = parser.feed(&header[header.len() - 1..]);
        assert!(matches!(iter.next(), Some(Parsed::ProtocolError(WireError::FrameTooLong { .. }))));
        assert!(iter.next().is_none());
    }
    assert!(parser.is_poisoned());
    assert_eq!(parser.buffered(), 0);
    assert!(parser.feed(b"PING\r\n").next().is_none());
}
