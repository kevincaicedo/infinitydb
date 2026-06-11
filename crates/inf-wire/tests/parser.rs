//! Parser correctness (M0-S11): command corpus, resumability under
//! arbitrary chunking (the provided-buffer reality), bounded-accumulator
//! enforcement, and protocol-error semantics.

use inf_wire::{ConnParser, Parsed, ParserLimits, WireError};

/// Drains every complete command from one feed into owned argv vectors.
fn drain(parser: &mut ConnParser, input: &[u8]) -> (Vec<Vec<Vec<u8>>>, Option<WireError>) {
    let mut commands = Vec::new();
    let mut error = None;
    let mut iter = parser.feed(input);
    while let Some(parsed) = iter.next() {
        match parsed {
            Parsed::Command(argv) | Parsed::Inline(argv) => {
                commands.push(argv.iter().map(<[u8]>::to_vec).collect());
            }
            Parsed::ProtocolError(err) => {
                error = Some(err);
                break;
            }
            Parsed::Incomplete => unreachable!("iterator never yields Incomplete"),
        }
    }
    (commands, error)
}

fn args(list: &[&str]) -> Vec<Vec<u8>> {
    list.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn parses_single_and_pipelined_commands() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let (cmds, err) = drain(&mut parser, b"*2\r\n$3\r\nGET\r\n$4\r\nuser\r\n");
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["GET", "user"])]);

    let pipeline = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nvalue\r\n*1\r\n$4\r\nPING\r\n*2\r\n$6\r\nEXISTS\r\n$1\r\nk\r\n";
    let (cmds, err) = drain(&mut parser, pipeline);
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["SET", "k", "value"]), args(&["PING"]), args(&["EXISTS", "k"])]);
    assert_eq!(parser.buffered(), 0, "complete pipeline leaves nothing buffered");
}

#[test]
fn binary_payloads_pass_through_untouched() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let value: Vec<u8> = (0u8..=255).collect();
    let mut frame = format!("*3\r\n$3\r\nSET\r\n$3\r\nbin\r\n${}\r\n", value.len()).into_bytes();
    frame.extend_from_slice(&value);
    frame.extend_from_slice(b"\r\n");
    let (cmds, err) = drain(&mut parser, &frame);
    assert_eq!(err, None);
    assert_eq!(cmds[0][2], value, "payload bytes (incl. CR/LF) must be opaque");
}

#[test]
fn empty_bulk_and_zero_array_are_handled() {
    let mut parser = ConnParser::new(ParserLimits::default());
    // Empty bulk argument.
    let (cmds, err) = drain(&mut parser, b"*2\r\n$3\r\nGET\r\n$0\r\n\r\n");
    assert_eq!(err, None);
    assert_eq!(cmds, vec![vec![b"GET".to_vec(), Vec::new()]]);
    // `*0` is consumed silently.
    let (cmds, err) = drain(&mut parser, b"*0\r\n*1\r\n$4\r\nPING\r\n");
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["PING"])]);
}

#[test]
fn frame_split_across_feeds_resumes_via_accumulator() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let frame = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nhello\r\n";
    // Split inside the bulk header, then inside the payload.
    let (cmds, err) = drain(&mut parser, &frame[..9]);
    assert_eq!((cmds.len(), err), (0, None));
    assert!(parser.buffered() > 0, "partial tail must be carried");
    let (cmds, err) = drain(&mut parser, &frame[9..27]);
    assert_eq!((cmds.len(), err), (0, None));
    let (cmds, err) = drain(&mut parser, &frame[27..]);
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["SET", "key", "hello"])]);
    assert_eq!(parser.buffered(), 0, "spanning-frame storage released after drain");
}

#[test]
fn inline_commands_split_on_whitespace() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let (cmds, err) = drain(&mut parser, b"PING\r\n  GET   key1\t\r\n\r\n");
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["PING"]), args(&["GET", "key1"])]);
}

#[test]
fn oversized_bulk_header_is_rejected_immediately_with_bounded_memory() {
    // M0-S11 AC: a 100 MB single bulk string on a 1 MiB-cap connection is
    // rejected with the documented error, memory bounded — the rejection
    // fires from the header line, before any payload is buffered.
    let cap = 1024 * 1024;
    let mut parser = ConnParser::new(ParserLimits { max_frame_bytes: cap, max_args: 1024 });
    let declared = 100 * 1024 * 1024;
    let header = format!("*2\r\n$3\r\nSET\r\n${declared}\r\n");
    let (cmds, err) = drain(&mut parser, header.as_bytes());
    assert_eq!(cmds.len(), 0);
    assert_eq!(err, Some(WireError::FrameTooLarge { declared, cap }));
    assert!(parser.is_poisoned());
    assert_eq!(parser.buffered(), 0, "nothing may be accumulated after rejection");
    // Poisoned parser ignores everything that follows.
    let (cmds, err) = drain(&mut parser, b"*1\r\n$4\r\nPING\r\n");
    assert_eq!((cmds.len(), err), (0, None));
}

#[test]
fn protocol_errors_poison_the_connection() {
    let cases: &[(&[u8], WireError)] = &[
        (b"*notanumber\r\n", WireError::BadMultibulkLen),
        (b"*-1\r\n", WireError::BadMultibulkLen),
        (b"*1\r\n+OK\r\n", WireError::ExpectedBulk { found: b'+' }),
        (b"*1\r\n$-5\r\n", WireError::BadBulkLen),
        (b"*1\r\n$3\r\nabcXY", WireError::ExpectedCrlf),
        (b"*1x\r\n", WireError::BadMultibulkLen),
    ];
    for (input, expected) in cases {
        let mut parser = ConnParser::new(ParserLimits::default());
        let (_, err) = drain(&mut parser, input);
        assert_eq!(err.as_ref(), Some(expected), "input {input:?}");
        assert!(parser.is_poisoned());
    }
}

#[test]
fn arg_count_over_limit_is_rejected() {
    let mut parser = ConnParser::new(ParserLimits { max_frame_bytes: 1024, max_args: 4 });
    let (_, err) = drain(&mut parser, b"*5\r\n");
    assert_eq!(err, Some(WireError::TooManyArgs { declared: 5, cap: 4 }));
}

#[test]
fn more_than_inline_args_spill_correctly() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let n = 40; // > INLINE_ARGS
    let mut frame = format!("*{n}\r\n").into_bytes();
    frame.extend_from_slice(b"$3\r\nDEL\r\n");
    for i in 1..n {
        let key = format!("k{i:02}");
        frame.extend_from_slice(format!("${}\r\n{key}\r\n", key.len()).as_bytes());
    }
    let (cmds, err) = drain(&mut parser, &frame);
    assert_eq!(err, None);
    assert_eq!(cmds[0].len(), n);
    assert_eq!(cmds[0][0], b"DEL");
    assert_eq!(cmds[0][39], b"k39");
}

#[test]
fn early_drop_carries_unconsumed_bytes() {
    let mut parser = ConnParser::new(ParserLimits::default());
    let two = b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nECHO\r\n";
    {
        let mut iter = parser.feed(two);
        let first = iter.next();
        assert!(matches!(first, Some(Parsed::Command(_))));
        // Budget exhausted: drop the iterator with one command unparsed.
    }
    let (cmds, err) = drain(&mut parser, b"");
    assert_eq!(err, None);
    assert_eq!(cmds, vec![args(&["ECHO"])], "dropped iterator must not lose frames");
}

// ---- Resumability proptest: arbitrary chunkings ⇒ identical commands ------

mod chunking {
    use proptest::prelude::*;

    use super::*;

    fn reference_pipeline() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$5\r\nkey:1\r\n$11\r\nhello world\r\n");
        bytes.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$5\r\nkey:1\r\n");
        bytes.extend_from_slice(b"PING\r\n");
        bytes.extend_from_slice(b"*2\r\n$6\r\nINCRBY\r\n$3\r\nctr\r\n");
        bytes.extend_from_slice(b"*4\r\n$3\r\nDEL\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n");
        bytes
    }

    fn expected_commands() -> Vec<Vec<Vec<u8>>> {
        vec![
            args(&["SET", "key:1", "hello world"]),
            args(&["GET", "key:1"]),
            args(&["PING"]),
            args(&["INCRBY", "ctr"]),
            args(&["DEL", "a", "b", "c"]),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// M0-S11: the provided-buffer reality — frames arrive in arbitrary
        /// splits; every chunking must parse to the identical command list.
        #[test]
        fn any_chunking_parses_identically(
            cuts in proptest::collection::vec(0usize..=130, 0..12)
        ) {
            let stream = reference_pipeline();
            prop_assert_eq!(stream.len(), 131); // keep `cuts` range honest
            let mut boundaries: Vec<usize> = cuts;
            boundaries.push(0);
            boundaries.push(stream.len());
            boundaries.sort_unstable();
            boundaries.dedup();

            let mut parser = ConnParser::new(ParserLimits::default());
            let mut all = Vec::new();
            for window in boundaries.windows(2) {
                let (commands, err) = drain(&mut parser, &stream[window[0]..window[1]]);
                prop_assert_eq!(err, None);
                all.extend(commands);
            }
            prop_assert_eq!(all, expected_commands());
            prop_assert_eq!(parser.buffered(), 0);
        }

        /// Arbitrary bytes never panic and never grow the accumulator past
        /// cap + one input buffer (the bounded-accumulator invariant).
        #[test]
        fn arbitrary_bytes_are_total_and_bounded(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..128), 1..8
            )
        ) {
            let cap = 256;
            let mut parser = ConnParser::new(ParserLimits { max_frame_bytes: cap, max_args: 64 });
            for chunk in &chunks {
                let mut iter = parser.feed(chunk);
                while let Some(parsed) = iter.next() {
                    if matches!(parsed, Parsed::ProtocolError(_)) {
                        break;
                    }
                }
                drop(iter);
                prop_assert!(
                    parser.buffered() <= cap + 128 + 2,
                    "accumulator exceeded its bound: {}",
                    parser.buffered()
                );
            }
        }
    }
}
