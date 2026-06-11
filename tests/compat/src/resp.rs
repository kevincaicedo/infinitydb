//! Minimal RESP2/RESP3 wire support: command encoding and byte-exact reply
//! framing.
//!
//! The framing parser understands just enough structure to know where one
//! reply ends; the raw bytes are returned untouched so the diff is byte-exact.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Wire-level framing failure: the byte stream cannot be valid RESP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    /// Byte offset where framing broke.
    pub at: usize,
    /// Human-readable cause.
    pub what: String,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid RESP at byte {}: {}", self.at, self.what)
    }
}

impl std::error::Error for FrameError {}

/// Encodes `argv` as a RESP array of bulk strings (the client request form).
pub fn encode_command(argv: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 16 * argv.len());
    out.extend_from_slice(format!("*{}\r\n", argv.len()).as_bytes());
    for arg in argv {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Returns `Ok(Some(n))` when `buf[..n]` is exactly one complete RESP reply,
/// `Ok(None)` when more bytes are needed, and `Err` when the buffer cannot be
/// a valid RESP reply.
///
/// Handles RESP2 and RESP3 framing: `+ - : $ *` plus `# , ( _ = % ~ > |`.
/// An attribute (`|`) is framed together with the reply it annotates, so one
/// logical reply is always returned as one unit.
///
/// # Errors
///
/// Returns [`FrameError`] on an unknown type tag or a malformed length /
/// bulk-payload terminator.
pub fn frame_len(buf: &[u8]) -> Result<Option<usize>, FrameError> {
    parse_reply(buf, 0)
}

/// First RESP type tag of a complete reply, if any.
pub fn first_tag(reply: &[u8]) -> Option<u8> {
    reply.first().copied()
}

/// Parses a RESP integer reply (`:<n>\r\n`). Returns `None` for any other
/// reply shape.
pub fn parse_integer(reply: &[u8]) -> Option<i64> {
    let digits = reply.strip_prefix(b":")?.strip_suffix(b"\r\n")?;
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn parse_reply(buf: &[u8], pos: usize) -> Result<Option<usize>, FrameError> {
    let Some(&tag) = buf.get(pos) else {
        return Ok(None);
    };
    match tag {
        // Single-line types: simple string, error, integer, boolean, double,
        // big number, RESP3 null.
        b'+' | b'-' | b':' | b'#' | b',' | b'(' | b'_' => Ok(line_end(buf, pos + 1)),
        // Length-prefixed payloads: bulk string, verbatim string.
        b'$' | b'=' => parse_bulk(buf, pos),
        // Counted aggregates: array, set, push.
        b'*' | b'~' | b'>' => parse_aggregate(buf, pos, 1),
        // Map: count is in pairs.
        b'%' => parse_aggregate(buf, pos, 2),
        // Attribute: a map followed by the reply it annotates.
        b'|' => parse_attribute(buf, pos),
        other => Err(FrameError { at: pos, what: format!("unknown RESP type tag 0x{other:02x}") }),
    }
}

/// Absolute offset just past the `\r\n` that terminates the line starting at
/// `from`, or `None` if the line is still incomplete.
fn line_end(buf: &[u8], from: usize) -> Option<usize> {
    let rel = buf[from..].windows(2).position(|w| w == b"\r\n")?;
    Some(from + rel + 2)
}

/// Parses the `<tag><signed int>\r\n` header at `pos`. Returns the absolute
/// offset past the header and the integer.
fn parse_header(buf: &[u8], pos: usize) -> Result<Option<(usize, i64)>, FrameError> {
    let Some(end) = line_end(buf, pos + 1) else {
        return Ok(None);
    };
    let line = &buf[pos + 1..end - 2];
    let value = std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .ok_or_else(|| FrameError {
            at: pos,
            what: format!("header is not an integer: {:?}", String::from_utf8_lossy(line)),
        })?;
    Ok(Some((end, value)))
}

fn parse_bulk(buf: &[u8], pos: usize) -> Result<Option<usize>, FrameError> {
    let Some((header_end, len)) = parse_header(buf, pos)? else {
        return Ok(None);
    };
    if len < 0 {
        // RESP2 null bulk `$-1\r\n`.
        return Ok(Some(header_end));
    }
    let total = header_end + len as usize + 2;
    if buf.len() < total {
        return Ok(None);
    }
    if &buf[total - 2..total] != b"\r\n" {
        return Err(FrameError {
            at: total - 2,
            what: "bulk payload not terminated by CRLF".to_string(),
        });
    }
    Ok(Some(total))
}

fn parse_aggregate(
    buf: &[u8],
    pos: usize,
    per_element: usize,
) -> Result<Option<usize>, FrameError> {
    let Some((mut cursor, count)) = parse_header(buf, pos)? else {
        return Ok(None);
    };
    if count < 0 {
        // RESP2 null array `*-1\r\n`.
        return Ok(Some(cursor));
    }
    for _ in 0..(count as usize * per_element) {
        match parse_reply(buf, cursor)? {
            Some(next) => cursor = next,
            None => return Ok(None),
        }
    }
    Ok(Some(cursor))
}

fn parse_attribute(buf: &[u8], pos: usize) -> Result<Option<usize>, FrameError> {
    match parse_aggregate(buf, pos, 2)? {
        Some(end) => parse_reply(buf, end),
        None => Ok(None),
    }
}

/// One client connection speaking raw RESP over TCP.
#[derive(Debug)]
pub struct RespConn {
    stream: TcpStream,
    buf: Vec<u8>,
    consumed: usize,
}

impl RespConn {
    /// Connects with 10s read/write timeouts and `TCP_NODELAY`.
    ///
    /// # Errors
    ///
    /// Propagates socket errors.
    pub fn connect(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self { stream, buf: Vec::new(), consumed: 0 })
    }

    /// Sends one command.
    ///
    /// # Errors
    ///
    /// Propagates socket errors.
    pub fn send(&mut self, argv: &[String]) -> io::Result<()> {
        self.stream.write_all(&encode_command(argv))
    }

    /// Reads exactly one complete raw reply (RESP2 or RESP3 framing).
    ///
    /// # Errors
    ///
    /// `InvalidData` when the stream is not valid RESP, `UnexpectedEof` when
    /// the server closes mid-reply, plus ordinary socket errors/timeouts.
    pub fn read_reply(&mut self) -> io::Result<Vec<u8>> {
        loop {
            let pending = &self.buf[self.consumed..];
            let framed = frame_len(pending)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            if let Some(n) = framed {
                let reply = pending[..n].to_vec();
                self.consumed += n;
                if self.consumed == self.buf.len() {
                    self.buf.clear();
                    self.consumed = 0;
                }
                return Ok(reply);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-reply",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Sends one command and reads its raw reply.
    ///
    /// # Errors
    ///
    /// See [`RespConn::send`] and [`RespConn::read_reply`].
    pub fn roundtrip(&mut self, argv: &[String]) -> io::Result<Vec<u8>> {
        self.send(argv)?;
        self.read_reply()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(input: &[u8]) -> usize {
        frame_len(input).expect("valid RESP").expect("complete reply")
    }

    #[test]
    fn encodes_argv_as_bulk_array() {
        let argv: Vec<String> = ["SET", "k", "v"].iter().map(|s| s.to_string()).collect();
        assert_eq!(encode_command(&argv), b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }

    #[test]
    fn frames_line_types() {
        assert_eq!(complete(b"+OK\r\n"), 5);
        assert_eq!(complete(b"-ERR boom\r\n"), 11);
        assert_eq!(complete(b":-2\r\n"), 5);
        assert_eq!(complete(b"#t\r\n"), 4);
        assert_eq!(complete(b",3.15\r\n"), 7);
        assert_eq!(complete(b"(3492890328409238509324850943850943825024385\r\n"), 46);
        assert_eq!(complete(b"_\r\n"), 3);
    }

    #[test]
    fn frames_bulk_variants() {
        assert_eq!(complete(b"$5\r\nhello\r\n"), 11);
        assert_eq!(complete(b"$0\r\n\r\n"), 6);
        assert_eq!(complete(b"$-1\r\n"), 5);
        assert_eq!(complete(b"$7\r\na\r\nb\r\nc\r\n"), 13); // CRLF inside payload
        assert_eq!(complete(b"=15\r\ntxt:Some string\r\n"), 22);
    }

    #[test]
    fn frames_aggregates() {
        assert_eq!(complete(b"*2\r\n+a\r\n:1\r\n"), 12);
        assert_eq!(complete(b"*-1\r\n"), 5);
        assert_eq!(complete(b"*0\r\n"), 4);
        assert_eq!(complete(b"%1\r\n+key\r\n+value\r\n"), 18);
        assert_eq!(complete(b"~2\r\n:1\r\n:2\r\n"), 12);
        assert_eq!(complete(b">2\r\n+a\r\n+b\r\n"), 12);
        assert_eq!(complete(b"*1\r\n*1\r\n$2\r\nab\r\n"), 16); // nested
    }

    #[test]
    fn frames_attribute_with_its_payload() {
        assert_eq!(complete(b"|1\r\n+k\r\n+v\r\n:9\r\n"), 16);
    }

    #[test]
    fn incomplete_input_needs_more_bytes() {
        for input in [
            &b"+OK"[..],
            b"$5\r\nhel",
            b"$5\r\nhello\r",
            b"*2\r\n+a\r\n",
            b"%1\r\n+key\r\n",
            b"|1\r\n+k\r\n+v\r\n",
            b"",
        ] {
            assert_eq!(frame_len(input).expect("valid prefix"), None, "input {input:?}");
        }
    }

    #[test]
    fn trailing_bytes_are_not_consumed() {
        assert_eq!(complete(b"+OK\r\n:1\r\n"), 5);
    }

    #[test]
    fn rejects_garbage() {
        assert!(frame_len(b"?x\r\n").is_err());
        assert!(frame_len(b"$abc\r\n").is_err());
    }

    #[test]
    fn parses_integer_replies() {
        assert_eq!(parse_integer(b":-2\r\n"), Some(-2));
        assert_eq!(parse_integer(b":100\r\n"), Some(100));
        assert_eq!(parse_integer(b"+OK\r\n"), None);
        assert_eq!(parse_integer(b":1"), None);
    }
}
