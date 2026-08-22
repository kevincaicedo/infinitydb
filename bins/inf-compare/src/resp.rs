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

/// Decode a non-null RESP bulk reply into its payload. INFO uses this form
/// on both Redis and InfinityDB.
pub fn bulk_text(reply: &[u8]) -> Result<String, String> {
    if reply.first() != Some(&b'$') {
        return Err(format!("expected bulk reply, got {:?}", String::from_utf8_lossy(reply)));
    }
    let Some(header_end) = reply.windows(2).position(|w| w == b"\r\n") else {
        return Err("bulk reply has no header terminator".into());
    };
    let len: usize = core::str::from_utf8(&reply[1..header_end])
        .map_err(|e| format!("bulk length utf8: {e}"))?
        .parse()
        .map_err(|e| format!("bulk length: {e}"))?;
    let start = header_end + 2;
    let end = start.checked_add(len).ok_or("bulk length overflow")?;
    let payload = reply.get(start..end).ok_or("truncated bulk reply")?;
    if reply.get(end..end + 2) != Some(b"\r\n") {
        return Err("bulk reply has no payload terminator".into());
    }
    String::from_utf8(payload.to_vec()).map_err(|e| format!("INFO utf8: {e}"))
}

/// Send `argvs` in order on **one** connection and return the last reply
/// (connection-state commands — `INF.NS USE` then a probe — need the
/// same socket). Errors like [`command`].
pub fn commands(host: &str, port: u16, argvs: &[&[&[u8]]]) -> Result<Vec<u8>, String> {
    let mut stream = connect(host, port)?;
    let mut last = Vec::new();
    for argv in argvs {
        last = request(&mut stream, argv)?;
        if last.first() == Some(&b'-') {
            let line = String::from_utf8_lossy(&last);
            return Err(format!("server error: {}", line.trim()));
        }
    }
    Ok(last)
}

/// Preload every key of the JSON lanes' keyspace with `doc` via one
/// pipelined connection (`JSON.SET k:<i> $ <doc>`) — the read lane then
/// never measures misses. Replies are drained in bulk and each must be
/// `+OK`; the first error fails the fill loudly (e.g. an engine whose
/// JSON surface was mis-detected).
pub fn json_fill(host: &str, port: u16, keyspace: u64, doc: &str) -> Result<(), String> {
    let mut stream = connect(host, port)?;
    let mut batch = Vec::with_capacity(64 * (doc.len() + 64));
    let mut pending = 0usize;
    for i in 0..keyspace {
        let key = format!("k:{i}");
        batch.extend_from_slice(&encode(&[b"JSON.SET", key.as_bytes(), b"$", doc.as_bytes()]));
        pending += 1;
        // Bounded batches: flush + drain replies every 512 commands so
        // neither side buffers unboundedly (L3 batching, explicit cap).
        if pending == 512 || i + 1 == keyspace {
            stream.write_all(&batch).map_err(|e| format!("json fill write: {e}"))?;
            batch.clear();
            drain_ok(&mut stream, pending)?;
            pending = 0;
        }
    }
    Ok(())
}

/// Read exactly `count` simple replies, requiring `+OK` for each.
fn drain_ok(stream: &mut TcpStream, count: usize) -> Result<(), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut seen = 0usize;
    let mut at = 0usize;
    while seen < count {
        if let Some(end) = frame(&buf, at) {
            if !buf[at..].starts_with(b"+OK") {
                let line = String::from_utf8_lossy(&buf[at..end]);
                return Err(format!("json fill reply: {}", line.trim()));
            }
            at = end;
            seen += 1;
            continue;
        }
        let n = stream.read(&mut chunk).map_err(|e| format!("json fill read: {e}"))?;
        if n == 0 {
            return Err("connection closed mid-fill".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::bulk_text;

    #[test]
    fn decodes_info_bulk_payload_exactly() {
        assert_eq!(bulk_text(b"$6\r\na:1\r\nb\r\n").unwrap(), "a:1\r\nb");
        assert!(bulk_text(b"+OK\r\n").is_err());
        assert!(bulk_text(b"$4\r\nabc\r\n").is_err());
    }
}
