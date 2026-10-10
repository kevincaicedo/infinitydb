//! The sim clients' RESP2 value parser (review B64-65-R04, closing the
//! F-L18-07 coverage gap): `try_parse_reply` is total on arbitrary bytes
//! (a typed `Malformed`, never a panic or a stack overflow), an accepted
//! reply re-encodes to bytes that parse back to the same value, and a
//! generated RESP2 model — a chain of single-element arrays around a
//! flat array of leaves, `data[0]` choosing the depth — parses back to
//! itself at every depth up to `MAX_DEPTH` and is refused with
//! `Malformed::Nesting` past it.
#![no_main]

use inf_sim::resp::{MAX_DEPTH, Malformed, Reply, encode_reply, try_parse_reply, try_reply_len};
use libfuzzer_sys::fuzz_target;

/// Leaves from the tail of the input: one per byte, kind from the byte.
fn leaves(bytes: &[u8]) -> Vec<Reply> {
    bytes
        .iter()
        .take(16)
        .map(|&b| match b % 6 {
            0 => Reply::Simple(vec![b'O'; usize::from(b % 5)]),
            1 => Reply::Error(b"ERR".to_vec()),
            2 => Reply::Int(i64::from(b) * 1_000_003 - 128),
            3 => Reply::Bulk(vec![b; usize::from(b % 7)]),
            4 => Reply::Nil,
            _ => Reply::Array(Vec::new()),
        })
        .collect()
}

fuzz_target!(|data: &[u8]| {
    // Arm 1: total on arbitrary bytes; agrees with the framer.
    match try_parse_reply(data) {
        Ok(reply) => {
            assert_eq!(try_reply_len(data), Ok(Some(data.len())), "framer disagrees");
            let mut again = Vec::with_capacity(data.len());
            encode_reply(&reply, &mut again);
            assert_eq!(try_parse_reply(&again), Ok(reply), "re-encode must parse back");
        }
        Err(Malformed::Nesting) => {
            assert_eq!(try_reply_len(data), Err(Malformed::Nesting));
        }
        Err(_) => {}
    }
    // Arm 2: the generated model at and past the depth cap.
    let Some((&first, rest)) = data.split_first() else { return };
    let depth = usize::from(first) % (MAX_DEPTH + 4);
    // The innermost array is never empty: `*0` opens no aggregate, so
    // an empty chain end would sit one level shallower than `depth`.
    let mut items = leaves(rest);
    if items.is_empty() {
        items.push(Reply::Nil);
    }
    let mut model = Reply::Array(items);
    for _ in 1..depth {
        model = Reply::Array(vec![model]);
    }
    let mut bytes = Vec::new();
    encode_reply(&model, &mut bytes);
    if depth <= MAX_DEPTH {
        assert_eq!(try_parse_reply(&bytes), Ok(model), "model at depth {depth}");
    } else {
        assert_eq!(try_parse_reply(&bytes), Err(Malformed::Nesting), "depth {depth}");
    }
});
