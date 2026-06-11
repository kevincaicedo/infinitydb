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

fn parse_items(mut buf: &[u8], count: usize) -> Option<(Vec<Reply>, usize)> {
    let mut items = Vec::with_capacity(count.min(64));
    let mut total = 0;
    for _ in 0..count {
        let (item, used) = parse_reply(buf)?;
        items.push(item);
        buf = &buf[used..];
        total += used;
    }
    Some((items, total))
}

/// Parse one complete reply from the front of `buf`.
pub fn parse_reply(buf: &[u8]) -> Option<(Reply, usize)> {
    let (&tag, rest) = buf.split_first()?;
    match tag {
        b'+' => {
            let (line, used) = parse_line(rest)?;
            Some((Reply::Simple(String::from_utf8_lossy(line).into_owned()), 1 + used))
        }
        b'-' => {
            let (line, used) = parse_line(rest)?;
            Some((Reply::Error(String::from_utf8_lossy(line).into_owned()), 1 + used))
        }
        b':' => {
            let (v, used) = parse_int_line(rest)?;
            Some((Reply::Int(v), 1 + used))
        }
        b'$' | b'=' => {
            let (len, head) = parse_int_line(rest)?;
            if len < 0 {
                return Some((Reply::Null, 1 + head)); // RESP2 null bulk
            }
            let len = usize::try_from(len).ok()?;
            let body = rest.get(head..head + len + 2)?;
            if &body[len..] != b"\r\n" {
                return None;
            }
            let bytes = body[..len].to_vec();
            let reply = if tag == b'=' {
                Reply::Verbatim(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                Reply::Bulk(bytes)
            };
            Some((reply, 1 + head + len + 2))
        }
        b'*' | b'>' | b'~' => {
            let (n, head) = parse_int_line(rest)?;
            if n < 0 {
                return Some((Reply::Null, 1 + head)); // RESP2 null array
            }
            let (items, body) = parse_items(&rest[head..], usize::try_from(n).ok()?)?;
            let reply = match tag {
                b'*' => Reply::Array(items),
                b'>' => Reply::Push(items),
                _ => Reply::Set(items),
            };
            Some((reply, 1 + head + body))
        }
        b'%' => {
            let (n, head) = parse_int_line(rest)?;
            let n = usize::try_from(n).ok()?;
            let (items, body) = parse_items(&rest[head..], n * 2)?;
            let mut pairs = Vec::with_capacity(n);
            let mut it = items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                pairs.push((k, v));
            }
            Some((Reply::Map(pairs), 1 + head + body))
        }
        b'_' => {
            let (_, used) = parse_line(rest)?;
            Some((Reply::Null, 1 + used))
        }
        b'#' => {
            let (line, used) = parse_line(rest)?;
            Some((Reply::Bool(line == b"t"), 1 + used))
        }
        b',' => {
            let (line, used) = parse_line(rest)?;
            Some((Reply::Double(String::from_utf8_lossy(line).into_owned()), 1 + used))
        }
        b'(' => {
            let (line, used) = parse_line(rest)?;
            Some((Reply::BigNumber(String::from_utf8_lossy(line).into_owned()), 1 + used))
        }
        _ => {
            // Unknown type tag: surface as a protocol error to the user.
            Some((Reply::Error(format!("protocol error: unknown reply tag {:?}", tag as char)), 1))
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
}
