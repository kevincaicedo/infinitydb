//! Tiny client-side RESP — only what the harness needs: a readiness `PING`
//! and a one-shot command (FLUSHALL, CONFIG SET) between benchmark rows. The
//! actual load is driven by memtier/redis-benchmark; this never touches a hot
//! path. Independent of `inf-wire` on purpose: the orchestrator shares no code
//! with the system under test.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

#[path = "resp/frame.rs"]
mod frame;
pub use frame::{Decoder, Error, MAX_BYTES};

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
    if reply_len(reply).map_err(|error| error.to_string())? != Some(reply.len()) {
        return Err("expected one complete bulk reply".into());
    }
    if reply.first() != Some(&b'$') {
        return Err("expected bulk reply".into());
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
    let mut reader = BufReader::new(stream);
    for _ in 0..count {
        let reply = read_reply(&mut reader)?;
        if reply != b"+OK\r\n" {
            return Err("json fill expected +OK".into());
        }
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
    read_reply(&mut BufReader::new(stream))
}

/// Keep at most one bounded reply; leave pipelined successors in the reader.
fn read_reply(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut decoder = Decoder::default();
    loop {
        let chunk = reader.fill_buf().map_err(|error| format!("read: {error}"))?;
        if chunk.is_empty() {
            return Err("connection closed mid-reply".into());
        }
        let previous = buf.len();
        let count = chunk.len().min(MAX_BYTES - previous);
        buf.extend_from_slice(&chunk[..count]);
        if let Some(end) = decoder.advance(&buf).map_err(|error| error.to_string())? {
            reader.consume(end - previous);
            buf.truncate(end);
            return Ok(buf);
        }
        reader.consume(count);
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

/// Complete frame end, incomplete input, or a terminal protocol/limit error.
pub fn reply_len(buf: &[u8]) -> Result<Option<usize>, Error> {
    Decoder::default().advance(buf)
}

#[cfg(test)]
mod tests {
    use super::bulk_text;

    #[test]
    fn buffered_reader_preserves_pipelined_replies_and_refuses_oversize_headers() {
        let mut reader = std::io::Cursor::new(b"+OK\r\n:2\r\n");
        assert_eq!(super::read_reply(&mut reader).unwrap(), b"+OK\r\n");
        assert_eq!(super::read_reply(&mut reader).unwrap(), b":2\r\n");
        let mut reader = std::io::Cursor::new(b"$999999999\r\n");
        assert!(super::read_reply(&mut reader).unwrap_err().contains("byte limit"));
    }

    #[test]
    fn empty_containers_count_toward_depth_and_payloads_are_bounded() {
        let mut reply = b"*1\r\n".repeat(super::frame::MAX_DEPTH);
        reply.extend_from_slice(b"*0\r\n");
        assert!(super::reply_len(&reply).is_err());
        let bytes = 1024 * 1024;
        let reply = format!("${bytes}\r\n{}\r\n", "x".repeat(bytes));
        assert!(super::reply_len(reply.as_bytes()).is_err());
        assert!(super::reply_len(b"$1\r\nx!!").is_err());
    }

    #[test]
    fn socket_reports_depth_refusal_instead_of_waiting_for_more_bytes() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sender = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 64];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(&b"*1\r\n".repeat(super::frame::MAX_DEPTH + 1)).unwrap();
        });
        let error = super::command("127.0.0.1", port, &[b"PING"]).unwrap_err();
        sender.join().unwrap();
        assert!(error.contains("nesting"), "{error}");
    }

    #[test]
    fn decodes_info_bulk_payload_exactly() {
        assert_eq!(bulk_text(b"$6\r\na:1\r\nb\r\n").unwrap(), "a:1\r\nb");
        assert!(bulk_text(b"+OK\r\n").is_err());
        assert!(bulk_text(b"$4\r\nabc\r\n").is_err());
    }

    /// F-L18-07 (review of 2026-08-30): a reply nested past the depth cap
    /// is refused with a depth error, never a stack
    /// overflow.
    #[test]
    fn framing_past_the_cap_is_refused() {
        let deep = b"*1\r\n".repeat(200_000);
        assert_eq!(super::reply_len(&deep), Err(super::Error::DepthLimit));
        let mut at_cap = b"*1\r\n".repeat(super::frame::MAX_DEPTH);
        at_cap.extend_from_slice(b":1\r\n");
        assert_eq!(super::reply_len(&at_cap), Ok(Some(at_cap.len())));
        let mut past = b"*1\r\n".repeat(super::frame::MAX_DEPTH + 1);
        past.extend_from_slice(b":1\r\n");
        assert_eq!(super::reply_len(&past), Err(super::Error::DepthLimit));
    }
}
