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

#[cfg(test)]
mod tests {
    use super::*;

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
