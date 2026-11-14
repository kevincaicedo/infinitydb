//! A reply's outcome is set by its writer and travels with its bytes
//! (ADR-0190 D1): the transitions of the reply's state table, each read
//! back through `outcome()`. A reply lives beside its buffer (ADR-0190
//! A1.1): a stream opens each reply at its end, an owned reply at 0.

use crate::{
    ErrorPassthrough, OpenReply, OwnedReply, Protocol, ReplyOutcome, ReplyStream, RespWriter,
    SuccessView, ZeroByteReply,
};

/// A stream holding one `+OK` reply, for the rows that open a reply past
/// an earlier one.
fn stream_after_ok() -> ReplyStream {
    let mut stream = ReplyStream::new(Vec::new());
    RespWriter::new(stream.open(), Protocol::Resp2).simple("OK");
    stream
}

#[test]
fn an_error_at_the_start_is_the_outcome() {
    let mut stream = ReplyStream::new(Vec::new());
    let reply = stream.open();
    RespWriter::new(reply, Protocol::Resp2).error("ERR boom");
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    assert_eq!(reply.bytes(), b"-ERR boom\r\n");
}

#[test]
fn a_success_frame_leaves_success() {
    let mut stream = ReplyStream::new(Vec::new());
    let reply = stream.open();
    RespWriter::new(reply, Protocol::Resp3).bulk(b"-ERR not an error");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
}

#[test]
fn a_nested_error_element_leaves_the_outcome() {
    let mut stream = ReplyStream::new(Vec::new());
    let reply = stream.open();
    let mut w = RespWriter::new(reply, Protocol::Resp2);
    w.array_header(1);
    w.error("ERR nested");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
}

#[test]
fn a_rollback_to_the_start_resets_and_past_it_keeps() {
    let mut stream = stream_after_ok();
    let reply = stream.open();
    {
        let mut w = RespWriter::new(reply, Protocol::Resp2);
        let start = w.mark();
        w.error("ERR first");
        w.rollback(&start);
    }
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    {
        let mut w = RespWriter::new(reply, Protocol::Resp2);
        w.error("ERR second");
        let nested = w.mark();
        w.array_header(1);
        w.rollback(&nested);
    }
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    assert_eq!(reply.bytes(), b"-ERR second\r\n");
    assert_eq!(stream.as_bytes(), b"+OK\r\n-ERR second\r\n", "the earlier reply stays");
}

#[test]
fn clear_on_a_streams_second_reply_keeps_the_first() {
    let mut stream = stream_after_ok();
    let reply = stream.open();
    RespWriter::new(reply, Protocol::Resp2).error("ERR gone");
    reply.clear();
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    assert!(reply.bytes().is_empty());
    assert_eq!(stream.as_bytes(), b"+OK\r\n");
}

#[test]
fn a_streams_next_reply_opens_success_after_an_error() {
    let mut stream = ReplyStream::new(Vec::new());
    RespWriter::new(stream.open(), Protocol::Resp2).error("ERR first");
    let second = stream.open();
    assert_eq!(second.outcome(), ReplyOutcome::Success, "a reply opens Success");
    assert!(second.bytes().is_empty(), "a reply opens at the buffer's end");
    RespWriter::new(second, Protocol::Resp2).simple("OK");
    assert_eq!(second.outcome(), ReplyOutcome::Success);
    assert_eq!(second.bytes(), b"+OK\r\n");
    assert_eq!(stream.as_bytes(), b"-ERR first\r\n+OK\r\n");
}

#[test]
fn discard_empties_the_stream_and_the_next_reply_opens_at_zero() {
    let mut stream = ReplyStream::new(b"stale".to_vec());
    assert!(stream.is_empty(), "a stream opens over a cleared buffer");
    RespWriter::new(stream.open(), Protocol::Resp2).error("ERR dropped");
    stream.discard();
    assert!(stream.is_empty());
    let reply = stream.open();
    RespWriter::new(reply, Protocol::Resp2).int(7);
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    assert_eq!(reply.bytes(), b":7\r\n");
    assert_eq!(stream.as_bytes(), b":7\r\n");
}

#[test]
fn consume_drops_sent_bytes_and_emit_queues_a_closed_reply() {
    let mut stream = stream_after_ok();
    let mut closed = OpenReply::new(Vec::new());
    RespWriter::new(closed.reply(), Protocol::Resp2).error("ERR queued");
    stream.emit(&closed.finish());
    assert_eq!(stream.as_bytes(), b"+OK\r\n-ERR queued\r\n");
    stream.consume(5);
    assert_eq!(stream.as_bytes(), b"-ERR queued\r\n", "the sent prefix is gone");
    assert_eq!(stream.len(), 13);
    let reply = stream.open();
    assert_eq!(reply.outcome(), ReplyOutcome::Success, "an emitted error is not the next reply's");
    RespWriter::new(reply, Protocol::Resp2).simple("OK");
    assert_eq!(stream.into_vec(), b"-ERR queued\r\n+OK\r\n");
}

#[test]
fn an_error_through_an_open_replys_borrow_is_its_outcome() {
    let mut reply = OpenReply::new(b"stale".to_vec());
    assert!(reply.bytes().is_empty(), "an owned reply opens over a cleared buffer");
    RespWriter::new(reply.reply(), Protocol::Resp2).error("ERR lent");
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    let _ = reply.reply();
    assert_eq!(reply.outcome(), ReplyOutcome::Error, "a borrow that writes nothing keeps it");
    let owned = reply.finish();
    assert!(owned.is_error());
    assert_eq!(owned.bytes(), b"-ERR lent\r\n");
}

/// The one move no type stops: `mem::swap` of two live replies exchanges
/// whole buffers with their starts. An owned reply that took a stream's
/// second reply would carry a pipelined prefix; `finish` trips on it.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "an owned reply starts at 0")]
fn finish_trips_on_a_reply_swapped_in_from_a_stream() {
    let mut stream = stream_after_ok();
    let mut reply = OpenReply::new(Vec::new());
    core::mem::swap(stream.open(), reply.reply());
    let _ = reply.finish();
}

#[test]
fn encoded_and_reopen_carry_the_outcome() {
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(reply.reply(), Protocol::Resp2).error("ERR first");
    let owned = reply.finish();
    let mut again = owned.reopen();
    RespWriter::new(again.reply(), Protocol::Resp3).encoded(b">1\r\n+x\r\n");
    let owned = again.finish();
    assert_eq!(owned.outcome(), ReplyOutcome::Error);
    assert_eq!(owned.bytes(), b"-ERR first\r\n>1\r\n+x\r\n");
    let mut header = OpenReply::new(Vec::new());
    let mut w = RespWriter::new(header.reply(), Protocol::Resp2);
    w.array_header(1);
    w.encoded(b"-ERR element\r\n");
    assert_eq!(header.outcome(), ReplyOutcome::Success);
}

#[test]
fn detach_copies_the_reply_and_leaves_the_buffer_at_its_start() {
    let mut stream = stream_after_ok();
    let reply = stream.open();
    RespWriter::new(reply, Protocol::Resp2).error("ERR gated");
    let owned = reply.detach();
    assert_eq!(owned.outcome(), ReplyOutcome::Error);
    assert_eq!(owned.bytes(), b"-ERR gated\r\n");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    assert_eq!(stream.as_bytes(), b"+OK\r\n");
}

#[test]
fn received_refuses_zero_bytes_and_returns_the_buffer() {
    let buf = Vec::with_capacity(64);
    let Err(ZeroByteReply(back)) = OwnedReply::received(buf, ReplyOutcome::Success) else {
        panic!("a zero-byte reply is refused");
    };
    assert_eq!(back.capacity(), 64, "the pool buffer comes back");
    let owned = OwnedReply::received(b"-ERR x\r\n".to_vec(), ReplyOutcome::Error)
        .expect("a non-empty reply is received");
    assert!(owned.is_error());
    assert_eq!(owned.into_bytes(), b"-ERR x\r\n");
}

#[test]
fn success_views_a_success_reply_and_passes_an_error_through() {
    let leg = |bytes: &[u8], outcome| {
        OwnedReply::received(bytes.to_vec(), outcome).expect("a non-empty reply")
    };
    let success = leg(b"-ERR a value\r\n", ReplyOutcome::Success);
    let view = success.success().expect("a Success reply has a view");
    assert_eq!(view.bytes(), b"-ERR a value\r\n", "the outcome decides, never the bytes");
    let error = leg(b"*1\r\n$1\r\nk\r\n", ReplyOutcome::Error);
    assert_eq!(error.success().map(SuccessView::bytes), Err(ErrorPassthrough));
    let nothing = OpenReply::new(Vec::new()).finish();
    let view = nothing.success().map(SuccessView::bytes);
    assert_eq!(view, Ok(&b""[..]), "a reply that wrote nothing is an empty success");
}
