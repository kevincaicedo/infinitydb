//! Scatter-reply re-parsers (F-L12-06): the plane re-reads its peers'
//! RESP replies to splice a cross-cell `SCAN`/`INF.PEEK` answer. Every
//! byte string, truncated at every length, must parse totally — never a
//! panic — and a `Some` from the SCAN head must name an offset inside
//! the buffer (`ScanLeg::rest` slices the reply at it unchecked). Each decoder
//! takes a `SuccessView`, the bytes of a reply whose outcome was read, so
//! every cut is read as a `Success` reply; a cut to zero bytes is a reply
//! that wrote nothing, the one empty view a decoder meets.
#![no_main]

use inf_wire::{OpenReply, OwnedReply, ReplyOutcome};
use libfuzzer_sys::fuzz_target;

/// `raw` as a `Success` leg reply, as the decode builds one; zero bytes,
/// which `received` refuses, as a finished reply that wrote nothing.
fn leg(raw: &[u8]) -> OwnedReply {
    OwnedReply::received(raw.to_vec(), ReplyOutcome::Success)
        .unwrap_or_else(|_| OpenReply::new(Vec::new()).finish())
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
    }
});
