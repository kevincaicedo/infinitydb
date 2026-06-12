//! RESP2/RESP3 reply serializer (M0-S11). Writes into a caller-provided
//! `Vec<u8>` (a wire send buffer) — no internal allocation; integers format
//! through a stack buffer (no `format!`). The protocol version is chosen per
//! connection at `HELLO` and threaded through [`RespWriter::new`].

/// Negotiated protocol for one connection.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Protocol {
    #[default]
    Resp2,
    Resp3,
}

/// Reply writer over one send buffer.
#[derive(Debug)]
pub struct RespWriter<'b> {
    out: &'b mut Vec<u8>,
    proto: Protocol,
}

impl<'b> RespWriter<'b> {
    pub fn new(out: &'b mut Vec<u8>, proto: Protocol) -> RespWriter<'b> {
        RespWriter { out, proto }
    }

    pub fn protocol(&self) -> Protocol {
        self.proto
    }

    /// `+OK\r\n`. `text` must not contain CR/LF (debug-asserted; replies are
    /// engine-controlled constants).
    pub fn simple(&mut self, text: &str) {
        debug_assert!(!text.bytes().any(|b| b == b'\r' || b == b'\n'));
        self.out.push(b'+');
        self.out.extend_from_slice(text.as_bytes());
        self.out.extend_from_slice(b"\r\n");
    }

    /// `-ERR ...\r\n`.
    pub fn error(&mut self, text: &str) {
        debug_assert!(!text.bytes().any(|b| b == b'\r' || b == b'\n'));
        self.out.push(b'-');
        self.out.extend_from_slice(text.as_bytes());
        self.out.extend_from_slice(b"\r\n");
    }

    /// `:N\r\n`.
    pub fn int(&mut self, value: i64) {
        self.out.push(b':');
        self.raw_int(value);
        self.out.extend_from_slice(b"\r\n");
    }

    /// `$len\r\n<bytes>\r\n`.
    pub fn bulk(&mut self, bytes: &[u8]) {
        self.out.push(b'$');
        self.raw_int(bytes.len() as i64);
        self.out.extend_from_slice(b"\r\n");
        self.out.extend_from_slice(bytes);
        self.out.extend_from_slice(b"\r\n");
    }

    /// Null: `$-1\r\n` (RESP2) / `_\r\n` (RESP3).
    pub fn null(&mut self) {
        match self.proto {
            Protocol::Resp2 => self.out.extend_from_slice(b"$-1\r\n"),
            Protocol::Resp3 => self.out.extend_from_slice(b"_\r\n"),
        }
    }

    /// Null array reply (e.g. timed-out blocking ops): `*-1\r\n` / `_\r\n`.
    pub fn null_array(&mut self) {
        match self.proto {
            Protocol::Resp2 => self.out.extend_from_slice(b"*-1\r\n"),
            Protocol::Resp3 => self.out.extend_from_slice(b"_\r\n"),
        }
    }

    /// `*N\r\n` — N replies follow.
    pub fn array_header(&mut self, n: usize) {
        self.out.push(b'*');
        self.raw_int(n as i64);
        self.out.extend_from_slice(b"\r\n");
    }

    /// Push frame of N elements: `>N\r\n` (RESP3) or a flat array (RESP2) —
    /// pub/sub confirmations and message delivery (M1-S10).
    pub fn push_header(&mut self, n: usize) {
        match self.proto {
            Protocol::Resp2 => self.array_header(n),
            Protocol::Resp3 => {
                self.out.push(b'>');
                self.raw_int(n as i64);
                self.out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Map of N pairs: `%N\r\n` (RESP3) or a flattened `*2N\r\n` (RESP2) —
    /// 2N key/value replies follow either way.
    pub fn map_header(&mut self, pairs: usize) {
        match self.proto {
            Protocol::Resp2 => self.array_header(pairs * 2),
            Protocol::Resp3 => {
                self.out.push(b'%');
                self.raw_int(pairs as i64);
                self.out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Boolean: `#t/#f` (RESP3) or `:1/:0` (RESP2).
    pub fn bool(&mut self, value: bool) {
        match self.proto {
            Protocol::Resp2 => self.int(i64::from(value)),
            Protocol::Resp3 => {
                self.out.extend_from_slice(if value { b"#t\r\n" } else { b"#f\r\n" });
            }
        }
    }

    /// Double: `,3.14\r\n` (RESP3) or a bulk string (RESP2).
    pub fn double(&mut self, value: f64) {
        let mut buf = FmtBuf::default();
        let text = buf.format(format_args!("{value}"));
        match self.proto {
            Protocol::Resp2 => self.bulk(text.as_bytes()),
            Protocol::Resp3 => {
                self.out.push(b',');
                self.out.extend_from_slice(text.as_bytes());
                self.out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Verbatim string `=len\r\nxxx:<text>\r\n` (RESP3) or plain bulk
    /// (RESP2). `kind` is the 3-byte format tag (`txt`, `mkd`).
    pub fn verbatim(&mut self, kind: &[u8; 3], text: &[u8]) {
        match self.proto {
            Protocol::Resp2 => self.bulk(text),
            Protocol::Resp3 => {
                self.out.push(b'=');
                self.raw_int((text.len() + 4) as i64);
                self.out.extend_from_slice(b"\r\n");
                self.out.extend_from_slice(kind);
                self.out.push(b':');
                self.out.extend_from_slice(text);
                self.out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Big number `(N\r\n` (RESP3) or bulk (RESP2) — INCR overflow surface.
    pub fn big_number(&mut self, digits: &str) {
        debug_assert!(digits.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
        match self.proto {
            Protocol::Resp2 => self.bulk(digits.as_bytes()),
            Protocol::Resp3 => {
                self.out.push(b'(');
                self.out.extend_from_slice(digits.as_bytes());
                self.out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Integer → ASCII via a stack buffer (no allocation, no `format!`).
    fn raw_int(&mut self, value: i64) {
        let mut buf = [0u8; 20];
        let text = itoa(value, &mut buf);
        self.out.extend_from_slice(text);
    }
}

/// Minimal signed-integer formatter into a caller stack buffer.
fn itoa(value: i64, buf: &mut [u8; 20]) -> &[u8] {
    let negative = value < 0;
    // Two's-complement-safe magnitude (handles i64::MIN).
    let mut magnitude = value.unsigned_abs();
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        at -= 1;
        buf[at] = b'-';
    }
    &buf[at..]
}

/// Tiny `fmt::Write` sink for double formatting (stack, cold path).
struct FmtBuf {
    buf: [u8; 40],
    len: usize,
}

impl Default for FmtBuf {
    fn default() -> FmtBuf {
        FmtBuf { buf: [0; 40], len: 0 }
    }
}

impl FmtBuf {
    fn format(&mut self, args: core::fmt::Arguments<'_>) -> &str {
        use core::fmt::Write;
        self.len = 0;
        self.write_fmt(args).expect("f64 display fits 40 bytes");
        core::str::from_utf8(&self.buf[..self.len]).expect("Display output is UTF-8")
    }
}

impl core::fmt::Write for FmtBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(proto: Protocol, f: impl FnOnce(&mut RespWriter<'_>)) -> Vec<u8> {
        let mut out = Vec::new();
        let mut writer = RespWriter::new(&mut out, proto);
        f(&mut writer);
        out
    }

    #[test]
    fn resp2_surface_is_byte_exact() {
        assert_eq!(render(Protocol::Resp2, |w| w.simple("OK")), b"+OK\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.error("ERR boom")), b"-ERR boom\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.int(-42)), b":-42\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.int(i64::MIN)), b":-9223372036854775808\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.bulk(b"hi")), b"$2\r\nhi\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.bulk(b"")), b"$0\r\n\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.null()), b"$-1\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.null_array()), b"*-1\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.array_header(3)), b"*3\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.map_header(2)), b"*4\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.bool(true)), b":1\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.double(2.5)), b"$3\r\n2.5\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.verbatim(b"txt", b"hi")), b"$2\r\nhi\r\n");
    }

    #[test]
    fn resp3_surface_is_byte_exact() {
        assert_eq!(render(Protocol::Resp3, |w| w.null()), b"_\r\n");
        assert_eq!(render(Protocol::Resp3, |w| w.null_array()), b"_\r\n");
        assert_eq!(render(Protocol::Resp3, |w| w.map_header(2)), b"%2\r\n");
        assert_eq!(render(Protocol::Resp3, |w| w.bool(true)), b"#t\r\n");
        assert_eq!(render(Protocol::Resp3, |w| w.bool(false)), b"#f\r\n");
        assert_eq!(render(Protocol::Resp3, |w| w.double(2.5)), b",2.5\r\n");
        assert_eq!(
            render(Protocol::Resp3, |w| w.verbatim(b"txt", b"hello")),
            b"=9\r\ntxt:hello\r\n"
        );
        assert_eq!(render(Protocol::Resp3, |w| w.big_number("123456")), b"(123456\r\n");
    }

    #[test]
    fn itoa_edge_values() {
        let mut buf = [0u8; 20];
        assert_eq!(itoa(0, &mut buf), b"0");
        assert_eq!(itoa(7, &mut buf), b"7");
        assert_eq!(itoa(-1, &mut buf), b"-1");
        assert_eq!(itoa(i64::MAX, &mut buf), b"9223372036854775807");
        assert_eq!(itoa(i64::MIN, &mut buf), b"-9223372036854775808");
    }
}
