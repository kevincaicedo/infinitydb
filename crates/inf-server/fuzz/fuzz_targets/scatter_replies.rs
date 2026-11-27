//! Scatter-reply re-parsers (F-L12-06): the plane re-reads its peers'
//! RESP replies to splice a cross-cell `SCAN`/`INF.PEEK`/`JSON.MGET`
//! answer. Every byte string, truncated at every length, must parse
//! totally — never a panic — and a `Some` must name offsets inside the
//! buffer (`ScanLeg::rest` slices the reply at its offset unchecked, and a
//! `JSON.MGET` gather pastes each element between the ends its decoder
//! wrote). Each decoder takes a `SuccessView`, the bytes of a reply whose
//! outcome was read, so every cut is read as a `Success` reply; a cut to
//! zero bytes is a reply that wrote nothing, the one empty view a decoder
//! meets. The `JSON.MGET` leg decoder reads the input after its first byte,
//! which draws the leg's key count (its low seven bits) and protocol (its
//! high bit), so a count that matches the header is reached.
#![no_main]

use inf_wire::{OpenReply, OwnedReply, Protocol, ReplyOutcome};
use libfuzzer_sys::fuzz_target;

/// `raw` as a `Success` leg reply, as the decode builds one; zero bytes,
/// which `received` refuses, as a finished reply that wrote nothing.
fn leg(raw: &[u8]) -> OwnedReply {
    OwnedReply::received(raw.to_vec(), ReplyOutcome::Success)
        .unwrap_or_else(|_| OpenReply::new(Vec::new()).finish())
}

/// On `Some(body_at)` the elements tile the body: `body_at ≤ ends[0] ≤ … ≤
/// ends[n − 1] = len`, each element at least one frame.
fn json_mget_leg(draw: u8, raw: &[u8]) {
    let proto = if draw & 0x80 == 0 { Protocol::Resp2 } else { Protocol::Resp3 };
    let mut ends = vec![0usize; usize::from(draw & 0x7F)];
    let reply = leg(raw);
    let view = reply.success().expect("a Success reply has a view");
    if let Some(body_at) = inf_server::parse_json_mget_leg(view, proto, &mut ends) {
        let mut at = body_at;
        for &end in &ends {
            assert!(at < end, "an element ends after it begins: {at} {end}");
            at = end;
        }
        assert_eq!(at, raw.len(), "the elements end at the reply's end");
    }
}

fuzz_target!(|data: &[u8]| {
    for n in [data.len(), data.len().saturating_sub(1), data.len().saturating_sub(2)] {
        let raw = &data[..n];
        let reply = leg(raw);
        let view = reply.success().expect("a Success reply has a view");
        if let Some((_, at)) = inf_server::parse_scan_head(view) {
            assert!(at <= raw.len(), "scan head offset {at} past {}", raw.len());
        }
        if let Some((_, at)) = inf_server::parse_array_header(view) {
            assert!(at <= raw.len(), "array header offset {at} past {}", raw.len());
        }
        let _ = inf_server::parse_take_reply(view);
        if let Some((&draw, rest)) = raw.split_first() {
            json_mget_leg(draw, rest);
        }
    }
});
