//! Minimal RESP reply framing for the sim clients: find where one reply
//! ends. Handles RESP2 + the RESP3 types the M0 surface emits.

/// `Some(n)` when `buf[..n]` is one complete reply; `None` = need more.
///
/// # Panics
/// Panics on malformed framing — the server under test produced it, which
/// is itself a finding the panic surfaces with the seed.
pub fn reply_len(buf: &[u8]) -> Option<usize> {
    frame(buf, 0)
}

fn frame(buf: &[u8], at: usize) -> Option<usize> {
    let tag = *buf.get(at)?;
    match tag {
        b'+' | b'-' | b':' | b',' | b'#' | b'(' | b'_' => line_end(buf, at),
        b'$' | b'=' => {
            let header_end = line_end(buf, at)?;
            let n = parse_len(&buf[at + 1..header_end - 2]);
            if n < 0 {
                return Some(header_end); // RESP2 null bulk
            }
            let total = header_end + n as usize + 2;
            (buf.len() >= total).then_some(total)
        }
        b'*' | b'%' | b'~' | b'>' => {
            let header_end = line_end(buf, at)?;
            let n = parse_len(&buf[at + 1..header_end - 2]);
            if n < 0 {
                return Some(header_end); // null array
            }
            let items = if tag == b'%' { n as usize * 2 } else { n as usize };
            let mut pos = header_end;
            for _ in 0..items {
                pos = frame(buf, pos)?;
            }
            Some(pos)
        }
        other => panic!("sim client saw invalid RESP tag {other:#04x}"),
    }
}

fn line_end(buf: &[u8], at: usize) -> Option<usize> {
    let nl = buf[at..].windows(2).position(|w| w == b"\r\n")?;
    Some(at + nl + 2)
}

fn parse_len(digits: &[u8]) -> i64 {
    let text = core::str::from_utf8(digits).expect("RESP length is ASCII");
    text.parse().expect("RESP length parses")
}

/// One parsed RESP2 reply — the shapes the sim's surface clients and the
/// audit oracle read (`SCAN` pages, `KEYS`, `DBSIZE`, `RANDOMKEY`, status
/// and error replies). Review of 2026-08-30, F-L19-05.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Simple(Vec<u8>),
    Error(Vec<u8>),
    Int(i64),
    Bulk(Vec<u8>),
    Nil,
    Array(Vec<Reply>),
}

/// Parses exactly one complete RESP2 reply.
///
/// # Panics
/// Panics on a malformed, incomplete, or over-long frame — the server
/// under test produced it, which is itself a finding the panic surfaces
/// with the seed.
pub fn parse_reply(raw: &[u8]) -> Reply {
    let (reply, end) = parse_at(raw, 0);
    assert_eq!(end, raw.len(), "trailing bytes after one reply: {raw:?}");
    reply
}

fn parse_at(buf: &[u8], at: usize) -> (Reply, usize) {
    let end = frame(buf, at).expect("complete frame");
    match buf[at] {
        b'+' => (Reply::Simple(buf[at + 1..end - 2].to_vec()), end),
        b'-' => (Reply::Error(buf[at + 1..end - 2].to_vec()), end),
        b':' => {
            let text = core::str::from_utf8(&buf[at + 1..end - 2]).expect("int ASCII");
            (Reply::Int(text.parse().expect("int parses")), end)
        }
        b'$' => {
            let header = line_end(buf, at).expect("bulk header");
            if parse_len(&buf[at + 1..header - 2]) < 0 {
                (Reply::Nil, end)
            } else {
                (Reply::Bulk(buf[header..end - 2].to_vec()), end)
            }
        }
        b'*' => {
            let header = line_end(buf, at).expect("array header");
            let n = parse_len(&buf[at + 1..header - 2]);
            if n < 0 {
                return (Reply::Nil, end);
            }
            let mut items = Vec::with_capacity(n as usize);
            let mut pos = header;
            for _ in 0..n {
                let (item, next) = parse_at(buf, pos);
                items.push(item);
                pos = next;
            }
            (Reply::Array(items), end)
        }
        other => panic!("sim reply parser saw RESP tag {other:#04x}"),
    }
}

/// One frame on a subscriber connection, classified (M1-S15 pub/sub
/// delivery oracle). RESP2 only — sim clients never HELLO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubFrame {
    /// `[subscribe|unsubscribe|psubscribe|punsubscribe, name|nil, :count]`.
    Confirm { verb: Vec<u8>, count: i64 },
    /// `[message, channel, payload]`.
    Message { channel: Vec<u8>, payload: Vec<u8> },
    /// `[pmessage, pattern, channel, payload]`.
    PMessage { channel: Vec<u8>, payload: Vec<u8> },
}

/// Classifies one complete frame from a subscriber connection.
///
/// # Panics
/// Panics on anything that is not a well-formed RESP2 pub/sub frame — the
/// server produced it on a subscribed connection, which is itself a finding
/// the panic surfaces with the seed.
pub fn parse_sub_frame(raw: &[u8]) -> SubFrame {
    let items = parse_array(raw);
    let verb = items.first().and_then(Item::bulk).expect("frame verb is a bulk").to_vec();
    if verb == b"message" {
        assert_eq!(items.len(), 3, "message frame arity");
        return SubFrame::Message {
            channel: items[1].bulk().expect("channel is a bulk").to_vec(),
            payload: items[2].bulk().expect("payload is a bulk").to_vec(),
        };
    }
    if verb == b"pmessage" {
        assert_eq!(items.len(), 4, "pmessage frame arity");
        return SubFrame::PMessage {
            channel: items[2].bulk().expect("channel is a bulk").to_vec(),
            payload: items[3].bulk().expect("payload is a bulk").to_vec(),
        };
    }
    let confirms: [&[u8]; 4] = [b"subscribe", b"unsubscribe", b"psubscribe", b"punsubscribe"];
    if confirms.contains(&verb.as_slice()) {
        assert_eq!(items.len(), 3, "confirmation frame arity");
        let Item::Int(count) = items[2] else {
            panic!("confirmation count is {:?}, want integer", items[2]);
        };
        return SubFrame::Confirm { verb, count };
    }
    panic!("unexpected frame on subscriber connection: {:?}", String::from_utf8_lossy(&verb));
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Bulk(Vec<u8>),
    Nil,
    Int(i64),
}

impl Item {
    fn bulk(&self) -> Option<&[u8]> {
        match self {
            Item::Bulk(b) => Some(b),
            _ => None,
        }
    }
}

/// Flat RESP2 array of bulks/integers (the only shapes pub/sub frames use).
fn parse_array(buf: &[u8]) -> Vec<Item> {
    assert_eq!(buf.first(), Some(&b'*'), "pub/sub frame is an array");
    let header = line_end(buf, 0).expect("complete frame");
    let n = parse_len(&buf[1..header - 2]);
    assert!(n >= 0, "pub/sub frame is non-null");
    let mut items = Vec::with_capacity(n as usize);
    let mut at = header;
    for _ in 0..n {
        let end = frame(buf, at).expect("element of a complete frame is complete");
        match buf[at] {
            b'$' => {
                let h = line_end(buf, at).expect("bulk header");
                if parse_len(&buf[at + 1..h - 2]) < 0 {
                    items.push(Item::Nil);
                } else {
                    items.push(Item::Bulk(buf[h..end - 2].to_vec()));
                }
            }
            b':' => {
                let text = core::str::from_utf8(&buf[at + 1..end - 2]).expect("int ASCII");
                items.push(Item::Int(text.parse().expect("int parses")));
            }
            other => panic!("unexpected element tag {other:#04x} in pub/sub frame"),
        }
        at = end;
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_surface_reply_shapes() {
        assert_eq!(parse_reply(b"+OK\r\n"), Reply::Simple(b"OK".to_vec()));
        assert_eq!(parse_reply(b"-ERR x\r\n"), Reply::Error(b"ERR x".to_vec()));
        assert_eq!(parse_reply(b":7\r\n"), Reply::Int(7));
        assert_eq!(parse_reply(b"$-1\r\n"), Reply::Nil);
        assert_eq!(parse_reply(b"$3\r\nfoo\r\n"), Reply::Bulk(b"foo".to_vec()));
        assert_eq!(
            parse_reply(b"*2\r\n$1\r\n0\r\n*2\r\n$1\r\na\r\n$1\r\nb\r\n"),
            Reply::Array(vec![
                Reply::Bulk(b"0".to_vec()),
                Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Bulk(b"b".to_vec())]),
            ])
        );
        assert_eq!(parse_reply(b"*0\r\n"), Reply::Array(Vec::new()));
    }

    #[test]
    fn frames_every_m0_reply_shape() {
        for (reply, want) in [
            (&b"+OK\r\n"[..], 5),
            (b"-ERR x\r\n", 8),
            (b":42\r\n", 5),
            (b"$3\r\nfoo\r\n", 9),
            (b"$-1\r\n", 5),
            (b"_\r\n", 3),
            (b"*2\r\n:1\r\n:2\r\n", 12),
            (b"%1\r\n$1\r\na\r\n:1\r\n", 15),
            (b"=8\r\ntxt:abcd\r\n", 14),
        ] {
            assert_eq!(reply_len(reply), Some(want), "reply {reply:?}");
            assert_eq!(reply_len(&reply[..want - 1]), None, "partial {reply:?}");
        }
    }
}
