//! Minimal RESP reply framing for the sim clients: find where one reply
//! ends. Handles RESP2 + the RESP3 types the M0 surface emits.

/// The deepest reply nesting the sim accepts (F-L18-07, review of
/// 2026-08-30): the framer is iterative with an explicit stack and this
/// explicit depth limit; past it the reply is a finding (a panic naming
/// it), never a stack overflow that aborts the sweep without its seed.
pub const MAX_DEPTH: usize = 32;

/// Why bytes could not be framed as one RESP reply — the server under
/// test produced them, which is itself a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// A byte that is not a RESP type tag where one was expected.
    Tag(u8),
    /// A length header that is not ASCII digits (or overflows).
    Length,
    /// Nested past [`MAX_DEPTH`].
    Nesting,
}

impl core::fmt::Display for Malformed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Malformed::Tag(tag) => write!(f, "invalid RESP tag {tag:#04x}"),
            Malformed::Length => f.write_str("RESP length is not ASCII digits"),
            Malformed::Nesting => write!(f, "reply nesting exceeds {MAX_DEPTH}"),
        }
    }
}

/// `Some(n)` when `buf[..n]` is one complete reply; `None` = need more.
///
/// # Panics
/// Panics on malformed framing — the server under test produced it, which
/// is itself a finding the panic surfaces with the seed.
pub fn reply_len(buf: &[u8]) -> Option<usize> {
    match frame(buf, 0) {
        Ok(len) => len,
        Err(err) => panic!("sim client saw malformed RESP: {err}"),
    }
}

/// The total framer behind [`reply_len`]: `Ok(None)` = need more bytes,
/// `Err` = the bytes can never frame (the fuzz target drives this one).
///
/// # Errors
/// [`Malformed`] names the first violation.
pub fn try_reply_len(buf: &[u8]) -> Result<Option<usize>, Malformed> {
    frame(buf, 0)
}

fn frame(buf: &[u8], at: usize) -> Result<Option<usize>, Malformed> {
    // Remaining items per open aggregate — the explicit stack (F-L18-07).
    let mut pending = [0usize; MAX_DEPTH];
    let mut depth = 0usize;
    let mut pos = at;
    loop {
        let Some(&tag) = buf.get(pos) else { return Ok(None) };
        let items = match tag {
            b'+' | b'-' | b':' | b',' | b'#' | b'(' | b'_' => {
                let Some(end) = line_end(buf, pos) else { return Ok(None) };
                pos = end;
                0
            }
            b'$' | b'=' => {
                let Some(header_end) = line_end(buf, pos) else { return Ok(None) };
                let n = parse_len(&buf[pos + 1..header_end - 2])?;
                pos = if n < 0 {
                    header_end // RESP2 null bulk
                } else {
                    let len = usize::try_from(n).map_err(|_| Malformed::Length)?;
                    let total = header_end
                        .checked_add(len)
                        .and_then(|t| t.checked_add(2))
                        .ok_or(Malformed::Length)?;
                    if buf.len() < total {
                        return Ok(None);
                    }
                    total
                };
                0
            }
            b'*' | b'%' | b'~' | b'>' => {
                let Some(header_end) = line_end(buf, pos) else { return Ok(None) };
                let n = parse_len(&buf[pos + 1..header_end - 2])?;
                pos = header_end;
                if n < 0 {
                    0 // null array
                } else {
                    let n = usize::try_from(n).map_err(|_| Malformed::Length)?;
                    if tag == b'%' { n.checked_mul(2).ok_or(Malformed::Length)? } else { n }
                }
            }
            other => return Err(Malformed::Tag(other)),
        };
        if items > 0 {
            if depth == MAX_DEPTH {
                return Err(Malformed::Nesting);
            }
            pending[depth] = items;
            depth += 1;
            continue;
        }
        // One element is complete: close every aggregate it finishes.
        while depth > 0 {
            pending[depth - 1] -= 1;
            if pending[depth - 1] > 0 {
                break;
            }
            depth -= 1;
        }
        if depth == 0 {
            return Ok(Some(pos));
        }
    }
}

fn line_end(buf: &[u8], at: usize) -> Option<usize> {
    let nl = buf[at..].windows(2).position(|w| w == b"\r\n")?;
    Some(at + nl + 2)
}

fn parse_len(digits: &[u8]) -> Result<i64, Malformed> {
    core::str::from_utf8(digits).ok().and_then(|t| t.parse().ok()).ok_or(Malformed::Length)
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

/// Parses exactly one complete RESP2 reply. Iterative: the open arrays
/// live on an explicit stack, bounded by [`MAX_DEPTH`] through
/// [`reply_len`] (F-L18-07).
///
/// # Panics
/// Panics on a malformed, incomplete, or over-long frame — the server
/// under test produced it, which is itself a finding the panic surfaces
/// with the seed.
pub fn parse_reply(raw: &[u8]) -> Reply {
    let end = reply_len(raw).expect("complete frame");
    assert_eq!(end, raw.len(), "trailing bytes after one reply: {raw:?}");
    let len = |digits: &[u8]| parse_len(digits).expect("framed length");
    let mut stack: Vec<(usize, Vec<Reply>)> = Vec::new();
    let mut at = 0;
    loop {
        let header = line_end(raw, at).expect("framed line");
        let mut value = match raw[at] {
            b'+' => {
                let line = raw[at + 1..header - 2].to_vec();
                at = header;
                Reply::Simple(line)
            }
            b'-' => {
                let line = raw[at + 1..header - 2].to_vec();
                at = header;
                Reply::Error(line)
            }
            b':' => {
                let text = core::str::from_utf8(&raw[at + 1..header - 2]).expect("int ASCII");
                at = header;
                Reply::Int(text.parse().expect("int parses"))
            }
            b'$' => {
                let n = len(&raw[at + 1..header - 2]);
                if n < 0 {
                    at = header;
                    Reply::Nil
                } else {
                    let end = header + n as usize + 2;
                    at = end;
                    Reply::Bulk(raw[header..end - 2].to_vec())
                }
            }
            b'*' => {
                let n = len(&raw[at + 1..header - 2]);
                at = header;
                if n < 0 {
                    Reply::Nil
                } else if n == 0 {
                    Reply::Array(Vec::new())
                } else {
                    stack.push((n as usize, Vec::with_capacity(n as usize)));
                    continue;
                }
            }
            other => panic!("sim reply parser saw RESP tag {other:#04x}"),
        };
        loop {
            let Some((want, items)) = stack.last_mut() else { return value };
            items.push(value);
            if items.len() < *want {
                break;
            }
            let (_, items) = stack.pop().expect("checked non-empty");
            value = Reply::Array(items);
        }
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
    let n = parse_len(&buf[1..header - 2]).expect("array length");
    assert!(n >= 0, "pub/sub frame is non-null");
    let mut items = Vec::with_capacity(n as usize);
    let mut at = header;
    for _ in 0..n {
        let end = frame(buf, at)
            .expect("element of a complete frame is well formed")
            .expect("element of a complete frame is complete");
        match buf[at] {
            b'$' => {
                let h = line_end(buf, at).expect("bulk header");
                if parse_len(&buf[at + 1..h - 2]).expect("bulk length") < 0 {
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

    /// F-L18-07 (review of 2026-08-30): a reply nested past the depth cap
    /// is the sim's usual finding — a panic naming it — never a stack
    /// overflow (which aborts the whole sweep without a seed).
    #[test]
    #[should_panic(expected = "nesting")]
    fn framing_past_the_cap_panics_typed() {
        let deep = b"*1\r\n".repeat(200_000);
        let _ = reply_len(&deep);
    }

    #[test]
    #[should_panic(expected = "nesting")]
    fn parsing_past_the_cap_panics_typed() {
        let mut deep = b"*1\r\n".repeat(200_000);
        deep.extend_from_slice(b":1\r\n");
        let _ = parse_reply(&deep);
    }

    #[test]
    fn framing_at_the_cap_is_legal() {
        let mut at_cap = b"*1\r\n".repeat(MAX_DEPTH);
        at_cap.extend_from_slice(b":1\r\n");
        assert_eq!(reply_len(&at_cap), Some(at_cap.len()));
        let mut inner = parse_reply(&at_cap);
        for _ in 0..MAX_DEPTH {
            let Reply::Array(mut items) = inner else { panic!("{inner:?}") };
            inner = items.remove(0);
        }
        assert_eq!(inner, Reply::Int(1));
    }
}
