//! RESP2/RESP3 reply decoding and rendering for the CLI.
//!
//! Total over arbitrary prefixes: `parse_reply` returns `None` while the
//! reply is incomplete and `Some((reply, bytes_consumed))` once whole.

#[derive(Debug, PartialEq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Null,
    Array(Vec<Reply>),
    Map(Vec<(Reply, Reply)>),
    Set(Vec<Reply>),
    Double(String),
    Bool(bool),
    BigNumber(String),
    Verbatim(String),
    Push(Vec<Reply>),
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn parse_line(buf: &[u8]) -> Option<(&[u8], usize)> {
    let end = find_crlf(buf)?;
    Some((&buf[..end], end + 2))
}

fn parse_int_line(buf: &[u8]) -> Option<(i64, usize)> {
    let (line, used) = parse_line(buf)?;
    let s = std::str::from_utf8(line).ok()?;
    Some((s.parse().ok()?, used))
}

/// The deepest reply nesting the CLI accepts (F-L18-07, review of
/// 2026-08-30): the parser is iterative with an explicit stack and this
/// explicit depth limit. No server reply nests past a handful of levels;
/// past the cap the stream is treated as desynced and reported as a
/// protocol error that consumes everything buffered.
pub const MAX_DEPTH: usize = 32;

/// One aggregate still collecting its items on the explicit stack.
struct Open {
    tag: u8,
    want: usize,
    items: Vec<Reply>,
}

/// What sits at the front of a buffer: a whole scalar, or the header of an
/// aggregate whose `items` follow.
enum Head {
    Value(Reply, usize),
    Open { tag: u8, items: usize, used: usize },
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_head(buf: &[u8]) -> Option<Head> {
    let (&tag, rest) = buf.split_first()?;
    let value = |reply, used| Some(Head::Value(reply, 1 + used));
    match tag {
        b'+' => {
            let (line, used) = parse_line(rest)?;
            value(Reply::Simple(lossy(line)), used)
        }
        b'-' => {
            let (line, used) = parse_line(rest)?;
            value(Reply::Error(lossy(line)), used)
        }
        b':' => {
            let (v, used) = parse_int_line(rest)?;
            value(Reply::Int(v), used)
        }
        b'$' | b'=' => {
            let (len, head) = parse_int_line(rest)?;
            if len < 0 {
                return value(Reply::Null, head); // RESP2 null bulk
            }
            let len = usize::try_from(len).ok()?;
            let body = rest.get(head..head.checked_add(len)?.checked_add(2)?)?;
            if &body[len..] != b"\r\n" {
                return None;
            }
            let bytes = body[..len].to_vec();
            let reply =
                if tag == b'=' { Reply::Verbatim(lossy(&bytes)) } else { Reply::Bulk(bytes) };
            value(reply, head + len + 2)
        }
        b'*' | b'>' | b'~' => {
            let (n, head) = parse_int_line(rest)?;
            if n < 0 {
                return value(Reply::Null, head); // RESP2 null array
            }
            Some(Head::Open { tag, items: usize::try_from(n).ok()?, used: 1 + head })
        }
        b'%' => {
            let (n, head) = parse_int_line(rest)?;
            let n = usize::try_from(n).ok()?;
            Some(Head::Open { tag, items: n.checked_mul(2)?, used: 1 + head })
        }
        b'_' => {
            let (_, used) = parse_line(rest)?;
            value(Reply::Null, used)
        }
        b'#' => {
            let (line, used) = parse_line(rest)?;
            value(Reply::Bool(line == b"t"), used)
        }
        b',' => {
            let (line, used) = parse_line(rest)?;
            value(Reply::Double(lossy(line)), used)
        }
        b'(' => {
            let (line, used) = parse_line(rest)?;
            value(Reply::BigNumber(lossy(line)), used)
        }
        _ => {
            // Unknown type tag: surface as a protocol error to the user.
            value(Reply::Error(format!("protocol error: unknown reply tag {:?}", tag as char)), 0)
        }
    }
}

/// Closes a collected aggregate.
fn close(tag: u8, items: Vec<Reply>) -> Reply {
    match tag {
        b'*' => Reply::Array(items),
        b'>' => Reply::Push(items),
        b'~' => Reply::Set(items),
        _ => {
            let mut pairs = Vec::with_capacity(items.len() / 2);
            let mut it = items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                pairs.push((k, v));
            }
            Reply::Map(pairs)
        }
    }
}

/// Parse one complete reply from the front of `buf`. Iterative: the open
/// aggregates live on an explicit stack bounded by [`MAX_DEPTH`].
pub fn parse_reply(buf: &[u8]) -> Option<(Reply, usize)> {
    let mut stack: Vec<Open> = Vec::new();
    let mut pos = 0;
    loop {
        let mut value = match parse_head(&buf[pos..])? {
            Head::Value(reply, used) => {
                pos += used;
                reply
            }
            Head::Open { tag, items, used } => {
                pos += used;
                if items == 0 {
                    close(tag, Vec::new())
                } else {
                    if stack.len() == MAX_DEPTH {
                        let err = format!("protocol error: reply nesting exceeds {MAX_DEPTH}");
                        return Some((Reply::Error(err), buf.len()));
                    }
                    stack.push(Open { tag, want: items, items: Vec::with_capacity(items.min(64)) });
                    continue;
                }
            }
        };
        // Deliver the value upward, closing every aggregate it completes.
        loop {
            let Some(top) = stack.last_mut() else { return Some((value, pos)) };
            top.items.push(value);
            if top.items.len() < top.want {
                break;
            }
            let Open { tag, items, .. } = stack.pop().expect("checked non-empty");
            value = close(tag, items);
        }
    }
}

impl Reply {
    /// redis-cli-flavored rendering.
    pub fn render(&self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        match self {
            Reply::Simple(s) => format!("{pad}{s}"),
            Reply::Error(e) => format!("{pad}(error) {e}"),
            Reply::Int(v) => format!("{pad}(integer) {v}"),
            Reply::Bulk(b) => format!("{pad}\"{}\"", String::from_utf8_lossy(b)),
            Reply::Null => format!("{pad}(nil)"),
            Reply::Bool(v) => format!("{pad}({v})"),
            Reply::Double(d) => format!("{pad}(double) {d}"),
            Reply::BigNumber(n) => format!("{pad}(big number) {n}"),
            Reply::Verbatim(s) => format!("{pad}{s}"),
            Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
                if items.is_empty() {
                    return format!("{pad}(empty array)");
                }
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| format!("{pad}{}) {}", i + 1, item.render(0)))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Reply::Map(pairs) => pairs
                .iter()
                .map(|(k, v)| format!("{pad}{} => {}", k.render(0), v.render(0)))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse_reply(b"+OK\r\n"), Some((Reply::Simple("OK".into()), 5)));
        assert_eq!(parse_reply(b":42\r\n"), Some((Reply::Int(42), 5)));
        assert_eq!(parse_reply(b"$3\r\nfoo\r\n"), Some((Reply::Bulk(b"foo".to_vec()), 9)));
        assert_eq!(parse_reply(b"$-1\r\n"), Some((Reply::Null, 5)));
        assert_eq!(parse_reply(b"_\r\n"), Some((Reply::Null, 3)));
        assert_eq!(parse_reply(b"#t\r\n"), Some((Reply::Bool(true), 4)));
    }

    #[test]
    fn incomplete_returns_none() {
        assert_eq!(parse_reply(b"$3\r\nfo"), None);
        assert_eq!(parse_reply(b"*2\r\n:1\r\n"), None);
        assert_eq!(parse_reply(b"+OK"), None);
    }

    #[test]
    fn nested_array_and_map() {
        let (reply, used) = parse_reply(b"*2\r\n:1\r\n$2\r\nhi\r\n").expect("whole");
        assert_eq!(used, 16);
        assert_eq!(reply, Reply::Array(vec![Reply::Int(1), Reply::Bulk(b"hi".to_vec())]));

        let (reply, _) = parse_reply(b"%1\r\n+server\r\n+infinity\r\n").expect("map");
        assert_eq!(
            reply,
            Reply::Map(vec![(Reply::Simple("server".into()), Reply::Simple("infinity".into()))])
        );
    }

    #[test]
    fn pipelined_consumes_exactly_one() {
        let buf = b"+OK\r\n:7\r\n";
        let (first, used) = parse_reply(buf).expect("first");
        assert_eq!(first, Reply::Simple("OK".into()));
        assert_eq!(parse_reply(&buf[used..]), Some((Reply::Int(7), 4)));
    }

    /// F-L18-07 (review of 2026-08-30): a server reply nested past the
    /// depth cap is a typed protocol error, never a stack overflow.
    #[test]
    fn nesting_past_the_cap_is_a_typed_protocol_error() {
        let deep = b"*1\r\n".repeat(200_000);
        let (reply, used) = parse_reply(&deep).expect("a typed answer");
        assert!(matches!(&reply, Reply::Error(e) if e.contains("nesting")), "{reply:?}");
        assert_eq!(used, deep.len(), "a desynced stream is dropped whole");
        // Exactly at the cap: legal.
        let mut at_cap = b"*1\r\n".repeat(MAX_DEPTH);
        at_cap.extend_from_slice(b":1\r\n");
        let (reply, used) = parse_reply(&at_cap).expect("whole");
        assert_eq!(used, at_cap.len());
        let mut inner = &reply;
        for _ in 0..MAX_DEPTH {
            let Reply::Array(items) = inner else { panic!("{inner:?}") };
            inner = &items[0];
        }
        assert_eq!(*inner, Reply::Int(1));
        // One past: refused.
        let mut past = b"*1\r\n".repeat(MAX_DEPTH + 1);
        past.extend_from_slice(b":1\r\n");
        let (reply, _) = parse_reply(&past).expect("typed");
        assert!(matches!(&reply, Reply::Error(e) if e.contains("nesting")), "{reply:?}");
    }
}
