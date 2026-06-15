//! Tiny client-side RESP — only what the harness needs: a readiness `PING`
//! and a one-shot command (FLUSHALL, CONFIG SET) between benchmark rows. The
//! actual load is driven by memtier/redis-benchmark; this never touches a hot
//! path. Independent of `inf-wire` on purpose: the orchestrator shares no code
//! with the system under test.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// `true` iff `host:port` answers `PING` with `+PONG`.
pub fn ping(host: &str, port: u16) -> bool {
    let Ok(mut stream) = connect(host, port) else { return false };
    matches!(request(&mut stream, &[b"PING"]), Ok(reply) if reply.starts_with(b"+PONG"))
}

/// Send one command and return its raw reply. Errors on transport failure or a
/// RESP error reply (`-ERR ...`), so callers can fail loudly.
pub fn command(host: &str, port: u16, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    let mut stream = connect(host, port)?;
    let reply = request(&mut stream, argv)?;
    if reply.first() == Some(&b'-') {
        let line = String::from_utf8_lossy(&reply);
        return Err(format!("server error: {}", line.trim()));
    }
    Ok(reply)
}

fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let stream =
        TcpStream::connect((host, port)).map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| format!("timeout: {e}"))?;
    Ok(stream)
}

fn request(stream: &mut TcpStream, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    stream.write_all(&encode(argv)).map_err(|e| format!("write: {e}"))?;
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

fn encode(argv: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + argv.iter().map(|a| a.len() + 16).sum::<usize>());
    out.extend_from_slice(format!("*{}\r\n", argv.len()).as_bytes());
    for arg in argv {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// `Some(n)` when `buf[..n]` is exactly one complete reply; `None` = need more.
fn reply_len(buf: &[u8]) -> Option<usize> {
    frame(buf, 0)
}

fn frame(buf: &[u8], at: usize) -> Option<usize> {
    let tag = *buf.get(at)?;
    match tag {
        b'+' | b'-' | b':' | b',' | b'#' | b'(' | b'_' => line_end(buf, at),
        b'$' | b'=' => {
            let header_end = line_end(buf, at)?;
            let n = parse_len(&buf[at + 1..header_end - 2])?;
            if n < 0 {
                return Some(header_end);
            }
            let total = header_end + n as usize + 2;
            (buf.len() >= total).then_some(total)
        }
        b'*' | b'%' | b'~' | b'>' => {
            let header_end = line_end(buf, at)?;
            let n = parse_len(&buf[at + 1..header_end - 2])?;
            if n < 0 {
                return Some(header_end);
            }
            let items = if tag == b'%' { n as usize * 2 } else { n as usize };
            let mut pos = header_end;
            for _ in 0..items {
                pos = frame(buf, pos)?;
            }
            Some(pos)
        }
        _ => None,
    }
}

fn line_end(buf: &[u8], at: usize) -> Option<usize> {
    let nl = buf[at..].windows(2).position(|w| w == b"\r\n")?;
    Some(at + nl + 2)
}

fn parse_len(digits: &[u8]) -> Option<i64> {
    core::str::from_utf8(digits).ok()?.parse().ok()
}
