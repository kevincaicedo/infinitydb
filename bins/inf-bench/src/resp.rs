//! Client-side RESP for the load generator and INFO scrapes. Deliberately
//! independent of `inf-wire`. Other harness primitives come from `inf-foundation`;
//! the RESP encoder and parser share no implementation with the server.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Encodes one command as a RESP array of bulk strings.
pub fn encode_command(argv: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + argv.iter().map(|a| a.len() + 16).sum::<usize>());
    out.extend_from_slice(format!("*{}\r\n", argv.len()).as_bytes());
    for arg in argv {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The deepest reply nesting the framer accepts (F-L18-07, review of
/// 2026-08-30): the framer is iterative with an explicit stack and this
/// explicit depth limit; past it the reply is treated as malformed.
pub const MAX_DEPTH: usize = 32;

/// `Some(n)` when `buf[..n]` is one complete reply; `None` = need more bytes.
pub fn reply_len(buf: &[u8]) -> Option<usize> {
    frame(buf, 0)
}

fn frame(buf: &[u8], at: usize) -> Option<usize> {
    // Remaining items per open aggregate — the explicit stack (F-L18-07).
    let mut pending = [0usize; MAX_DEPTH];
    let mut depth = 0usize;
    let mut pos = at;
    loop {
        let tag = *buf.get(pos)?;
        let items = match tag {
            b'+' | b'-' | b':' | b',' | b'#' | b'(' | b'_' => {
                pos = line_end(buf, pos)?;
                0
            }
            b'$' | b'=' => {
                let header_end = line_end(buf, pos)?;
                let n = parse_len(&buf[pos + 1..header_end - 2])?;
                pos = if n < 0 {
                    header_end // RESP2 null bulk
                } else {
                    let total = header_end.checked_add(usize::try_from(n).ok()?)?.checked_add(2)?;
                    if buf.len() < total {
                        return None;
                    }
                    total
                };
                0
            }
            b'*' | b'%' | b'~' | b'>' => {
                let header_end = line_end(buf, pos)?;
                let n = parse_len(&buf[pos + 1..header_end - 2])?;
                pos = header_end;
                if n < 0 {
                    0 // null array
                } else {
                    let n = usize::try_from(n).ok()?;
                    if tag == b'%' { n.checked_mul(2)? } else { n }
                }
            }
            _ => return None, // malformed: caller treats as a protocol error
        };
        if items > 0 {
            if depth == MAX_DEPTH {
                return None; // nested past the cap: malformed
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
            return Some(pos);
        }
    }
}

fn line_end(buf: &[u8], at: usize) -> Option<usize> {
    let nl = buf[at..].windows(2).position(|w| w == b"\r\n")?;
    Some(at + nl + 2)
}

fn parse_len(digits: &[u8]) -> Option<i64> {
    core::str::from_utf8(digits).ok()?.parse().ok()
}

/// One blocking request/response exchange (cold-path helper: INFO scrapes).
pub fn request(stream: &mut TcpStream, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    stream.write_all(&encode_command(argv)).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        if let Some(n) = reply_len(&buf) {
            buf.truncate(n);
            return Ok(buf);
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed mid-reply".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parses `key:value` lines out of an INFO reply (bulk or verbatim).
pub fn parse_info(reply: &[u8]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let text = String::from_utf8_lossy(reply);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':')
            && !key.starts_with(['#', '$', '=', '*'])
        {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

/// Connects with timeouts + NODELAY.
pub fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let stream = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    stream.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| format!("timeout: {e}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH, reply_len};

    /// F-L18-07 (review of 2026-08-30): a reply nested past the depth cap
    /// is refused (`None`, the framer's malformed answer), never a stack
    /// overflow.
    #[test]
    fn framing_past_the_cap_is_refused() {
        let deep = b"*1\r\n".repeat(200_000);
        assert_eq!(reply_len(&deep), None);
        let mut at_cap = b"*1\r\n".repeat(MAX_DEPTH);
        at_cap.extend_from_slice(b":1\r\n");
        assert_eq!(reply_len(&at_cap), Some(at_cap.len()));
        let mut past = b"*1\r\n".repeat(MAX_DEPTH + 1);
        past.extend_from_slice(b":1\r\n");
        assert_eq!(reply_len(&past), None);
    }
}
