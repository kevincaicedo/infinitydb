//! A reply's outcome is set by its writer and travels with its bytes
//! (ADR-0190 D1): the transitions of the reply's state table, each read
//! back through `outcome()`.

use crate::{OpenReply, OwnedReply, Protocol, ReplyBuf, ReplyOutcome, RespWriter, ZeroByteReply};

#[test]
fn an_error_at_the_start_is_the_outcome() {
    let mut out = Vec::new();
    let mut reply = ReplyBuf::over(&mut out);
    RespWriter::new(&mut reply, Protocol::Resp2).error("ERR boom");
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    assert_eq!(reply.bytes(), b"-ERR boom\r\n");
}

#[test]
fn a_success_frame_leaves_success() {
    let mut out = Vec::new();
    let mut reply = ReplyBuf::over(&mut out);
    RespWriter::new(&mut reply, Protocol::Resp3).bulk(b"-ERR not an error");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
}

#[test]
fn a_nested_error_element_leaves_the_outcome() {
    let mut out = Vec::new();
    let mut reply = ReplyBuf::over(&mut out);
    let mut w = RespWriter::new(&mut reply, Protocol::Resp2);
    w.array_header(1);
    w.error("ERR nested");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
}

#[test]
fn a_rollback_to_the_start_resets_and_past_it_keeps() {
    let mut out = b"+OK\r\n".to_vec();
    let mut reply = ReplyBuf::over(&mut out);
    {
        let mut w = RespWriter::new(&mut reply, Protocol::Resp2);
        let start = w.mark();
        w.error("ERR first");
        w.rollback(&start);
    }
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    {
        let mut w = RespWriter::new(&mut reply, Protocol::Resp2);
        w.error("ERR second");
        let nested = w.mark();
        w.array_header(1);
        w.rollback(&nested);
    }
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    assert_eq!(reply.bytes(), b"-ERR second\r\n");
    assert_eq!(out, b"+OK\r\n-ERR second\r\n", "the earlier reply stays");
}

#[test]
fn clear_keeps_earlier_bytes_and_resets_success() {
    let mut out = b"+OK\r\n".to_vec();
    let mut reply = ReplyBuf::over(&mut out);
    RespWriter::new(&mut reply, Protocol::Resp2).error("ERR gone");
    reply.clear();
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    assert!(reply.bytes().is_empty());
    assert_eq!(out, b"+OK\r\n");
}

#[test]
fn a_lent_view_writes_its_outcome_back() {
    let mut reply = OpenReply::new(b"stale".to_vec());
    assert!(reply.bytes().is_empty(), "an owned reply opens over a cleared buffer");
    reply.lend(|view| RespWriter::new(view, Protocol::Resp2).error("ERR lent"));
    assert_eq!(reply.outcome(), ReplyOutcome::Error);
    reply.lend(|_| {});
    assert_eq!(reply.outcome(), ReplyOutcome::Error, "a closure that writes nothing keeps it");
    let owned = reply.finish();
    assert!(owned.is_error());
    assert_eq!(owned.bytes(), b"-ERR lent\r\n");
}

#[test]
fn encoded_and_reopen_carry_the_outcome() {
    let mut reply = OpenReply::new(Vec::new());
    RespWriter::new(&mut reply, Protocol::Resp2).error("ERR first");
    let owned = reply.finish();
    let mut again = owned.reopen();
    RespWriter::new(&mut again, Protocol::Resp3).encoded(b">1\r\n+x\r\n");
    let owned = again.finish();
    assert_eq!(owned.outcome(), ReplyOutcome::Error);
    assert_eq!(owned.bytes(), b"-ERR first\r\n>1\r\n+x\r\n");
    let mut header = OpenReply::new(Vec::new());
    let mut w = RespWriter::new(&mut header, Protocol::Resp2);
    w.array_header(1);
    w.encoded(b"-ERR element\r\n");
    assert_eq!(header.outcome(), ReplyOutcome::Success);
}

#[test]
fn detach_copies_the_reply_and_leaves_the_buffer_at_its_start() {
    let mut out = b"+OK\r\n".to_vec();
    let mut reply = ReplyBuf::over(&mut out);
    RespWriter::new(&mut reply, Protocol::Resp2).error("ERR gated");
    let owned = reply.detach();
    assert_eq!(owned.outcome(), ReplyOutcome::Error);
    assert_eq!(owned.bytes(), b"-ERR gated\r\n");
    assert_eq!(reply.outcome(), ReplyOutcome::Success);
    assert_eq!(out, b"+OK\r\n");
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
