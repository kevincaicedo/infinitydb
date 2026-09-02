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

    /// `+OK\r\n`. The text is written as one protocol line — see
    /// [`RespWriter::error`] for the sanitization both line-framed writers
    /// share.
    #[inline]
    pub fn simple(&mut self, text: &str) {
        self.push_line(b'+', text.as_bytes());
    }

    /// `-ERR ...\r\n`.
    ///
    /// Simple strings and errors are the only replies whose frame boundary
    /// **is** a CRLF in the payload; every other reply this writer emits is
    /// length-prefixed. So `text` is sanitized on the way out rather than
    /// asserted clean: a leading/trailing run of CR/LF is trimmed and any
    /// remaining CR or LF becomes a space — byte-for-byte what
    /// redis-server 8.0.5 does in `addReplyErrorFormatInternal`
    /// (`sdstrim(s, "\r\n")` then `sdsmapchars(s, "\r\n", "  ", 2)`).
    ///
    /// ADR-0097 (review 2026-08-30, finding C6). The previous contract —
    /// *"`text` must not contain CR/LF (debug-asserted; replies are
    /// engine-controlled constants)"* — was false for twelve live call
    /// sites that interpolate client bytes, and `debug_assert!` is compiled
    /// out of the shipping profile: a client could open a second RESP frame
    /// inside its own error reply and so forge the reply to the *next*
    /// pipelined command. Enforcing the invariant here closes the class for
    /// every caller, present and future.
    #[inline]
    pub fn error(&mut self, text: &str) {
        self.error_bytes(text.as_bytes());
    }

    /// [`RespWriter::error`] for error text that interpolates raw client
    /// bytes: argv contents need not be UTF-8, and a lossy conversion would
    /// both change the byte budget a caller bounds against and diverge from
    /// the oracle's bytes.
    #[inline]
    pub fn error_bytes(&mut self, text: &[u8]) {
        self.push_line(b'-', text);
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

    /// Bulk string of unknown length (ADR-0039 D1's wire half, built at
    /// M3-S11 per ADR-0041 D10): reserve the common length-header width,
    /// let `build` append the payload once, back-patch the digits, and
    /// close the gap with one overlapping `copy_within` of the payload.
    /// The move is O(payload) — ~tens of ns at the 1 KiB gate shape;
    /// rejected alternatives (scratch-buffer double write; deferred-length
    /// iovec chains) are recorded in the ADR.
    ///
    /// The header patch is **total** (ADR-0099, review 2026-08-30 C9): a
    /// serialized reply is not bounded by the 16 MiB−1 document cap
    /// (multi-path `JSON.GET`, formatting separators, `\u00xx` escape
    /// amplification), so a payload needing more than the reserved
    /// 8 digits takes a cold path that widens the header in place. The
    /// previous `debug_assert!` on the payload size documented the bound
    /// while release builds wrapped `MAX_DIGITS - text.len()` and
    /// panicked inside `copy_within` — a client-drivable quantity is not
    /// an internal invariant.
    pub fn bulk_patched(&mut self, build: impl FnOnce(&mut Vec<u8>)) {
        let Ok(()) = self.try_bulk_patched(|out| {
            build(out);
            Ok::<(), core::convert::Infallible>(())
        });
    }

    /// [`RespWriter::bulk_patched`] with a fallible builder (ADR-0099 D2):
    /// when `build` errors — a reply-byte budget refusing mid-serialization
    /// — the buffer is truncated back to the frame start, so no partial
    /// frame can escape and the caller answers an error reply instead.
    pub fn try_bulk_patched<E>(
        &mut self,
        build: impl FnOnce(&mut Vec<u8>) -> Result<(), E>,
    ) -> Result<(), E> {
        let frame_at = self.out.len();
        self.out.push(b'$');
        let digits_at = self.out.len();
        self.out.extend_from_slice(b"00000000\r\n");
        let payload_at = digits_at + PATCHED_DIGITS + 2;
        if let Err(e) = build(self.out) {
            self.out.truncate(frame_at);
            return Err(e);
        }
        let len = self.out.len() - payload_at;
        let mut buf = [0u8; 20];
        let text = itoa(len as i64, &mut buf);
        if text.len() <= PATCHED_DIGITS {
            let gap = PATCHED_DIGITS - text.len();
            self.out[digits_at..digits_at + text.len()].copy_from_slice(text);
            self.out[digits_at + text.len()..digits_at + text.len() + 2].copy_from_slice(b"\r\n");
            if gap > 0 {
                self.out.copy_within(payload_at.., payload_at - gap);
                self.out.truncate(self.out.len() - gap);
            }
        } else {
            widen_patched_header(self.out, digits_at, payload_at, text);
        }
        self.out.extend_from_slice(b"\r\n");
        Ok(())
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

    /// Writes one line-framed reply: `tag`, the body, `\r\n`. The body is
    /// sanitized so it cannot open a second frame (see [`RespWriter::error`]).
    ///
    /// Clean text — every engine constant, and so the whole hot reply path
    /// (`+OK`, `+PONG`) — takes the straight-line branch: one scan, then the
    /// same three appends the writer did before sanitization existed. Text
    /// that really carries CR/LF goes to a cold, out-of-line path.
    #[inline]
    fn push_line(&mut self, tag: u8, text: &[u8]) {
        if contains_line_break(text) {
            self.push_line_sanitized(tag, text);
            return;
        }
        self.out.push(tag);
        self.out.extend_from_slice(text);
        self.out.extend_from_slice(b"\r\n");
    }

    /// The CR/LF-carrying half of [`RespWriter::push_line`]: trim a
    /// leading/trailing run, map what is left to spaces. Out of line — no
    /// engine-controlled reply reaches it, only client bytes do.
    #[cold]
    #[inline(never)]
    fn push_line_sanitized(&mut self, tag: u8, text: &[u8]) {
        let start = text.iter().position(|b| *b != b'\r' && *b != b'\n').unwrap_or(text.len());
        let end = text.iter().rposition(|b| *b != b'\r' && *b != b'\n').map_or(start, |i| i + 1);
        self.out.push(tag);
        let at = self.out.len();
        self.out.extend_from_slice(&text[start..end]);
        for byte in &mut self.out[at..] {
            if *byte == b'\r' || *byte == b'\n' {
                *byte = b' ';
            }
        }
        // Postcondition: the terminator below is the only CRLF in this
        // reply — the property the whole connection's framing rests on.
        debug_assert!(!self.out[at..].iter().any(|b| *b == b'\r' || *b == b'\n'));
        self.out.extend_from_slice(b"\r\n");
    }

    /// Integer → ASCII via a stack buffer (no allocation, no `format!`).
    fn raw_int(&mut self, value: i64) {
        let mut buf = [0u8; 20];
        let text = itoa(value, &mut buf);
        self.out.extend_from_slice(text);
    }
}

/// Reserved length-header width of a patched bulk: 8 digits cover every
/// payload under 100 MB — all of them but the near-boundary amplified
/// replies that take [`widen_patched_header`].
const PATCHED_DIGITS: usize = 8;

/// The ≥ 100 MB half of the total header patch (ADR-0099 D1): extend by
/// the extra digit count, shift the payload right once, write the digits
/// and CRLF over the old reserve. O(payload), on a frame that is ≥ 100 MB
/// by construction — out of line so the fast path stays the D10 bytes.
#[cold]
#[inline(never)]
fn widen_patched_header(out: &mut Vec<u8>, digits_at: usize, payload_at: usize, text: &[u8]) {
    let extra = text.len() - PATCHED_DIGITS;
    let old_len = out.len();
    out.resize(old_len + extra, 0);
    out.copy_within(payload_at..old_len, payload_at + extra);
    out[digits_at..digits_at + text.len()].copy_from_slice(text);
    out[digits_at + text.len()..digits_at + text.len() + 2].copy_from_slice(b"\r\n");
}

/// Whether `text` holds a CR or an LF — the only bytes that can end a
/// line-framed reply early.
///
/// SWAR, eight bytes per step: the fast path of every `+`/`-` reply runs
/// this, and a short-circuiting `iter().any()` costs a cycle per byte
/// (measured: +12.6 ns on a 44-byte error line, +580 % on the write bench).
#[inline(always)]
fn contains_line_break(text: &[u8]) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    /// Non-zero iff `word` contains a zero byte (Mycroft's bit trick).
    #[inline(always)]
    fn has_zero_byte(word: u64) -> u64 {
        word.wrapping_sub(ONES) & !word & HIGH
    }
    if text.len() < 8 {
        return text.iter().any(|b| *b == b'\r' || *b == b'\n');
    }
    let mut chunks = text.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        let cr = has_zero_byte(word ^ (ONES * u64::from(b'\r')));
        let lf = has_zero_byte(word ^ (ONES * u64::from(b'\n')));
        if cr | lf != 0 {
            return true;
        }
    }
    chunks.remainder().iter().any(|b| *b == b'\r' || *b == b'\n')
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

/// The longest `Display` an `f64` produces, in bytes. Rust's `{}` prints
/// the shortest round-trip digits (≤ 17) positioned without an exponent,
/// so the extremes are a 309-digit integer (`f64::MAX`) and `0.` followed
/// by 307 zeros and 17 digits (a negative near `f64::MIN_POSITIVE`:
/// `-1.1125369292536007e-308` is 327 bytes); `inf`/`NaN` are short. The
/// bound is enumerated by `f64_display_never_exceeds_fmtbuf` over every
/// binary exponent and its neighbours. Before batch 12 of the 2026-08-30
/// review the buffer was 40 bytes and a RESP3 `JSON.NUMINCRBY` reply of
/// `1e300` (301 digits) killed the cell — Theme 4's shape: a release
/// `expect("f64 display fits 40 bytes")` justified by a claim about the
/// caller that client-supplied numbers falsified.
const F64_DISPLAY_MAX: usize = 336;

/// `fmt::Write` sink for double formatting: a stack buffer sized to the
/// worst case above, so `format` is total for every `f64`.
struct FmtBuf {
    buf: [u8; F64_DISPLAY_MAX],
    len: usize,
}

impl Default for FmtBuf {
    fn default() -> FmtBuf {
        FmtBuf { buf: [0; F64_DISPLAY_MAX], len: 0 }
    }
}

impl FmtBuf {
    fn format(&mut self, args: core::fmt::Arguments<'_>) -> &str {
        use core::fmt::Write;
        self.len = 0;
        // Total by construction: `F64_DISPLAY_MAX` covers every `f64`'s
        // `Display` (the sweep test is the proof) — an `Err` here would
        // be a change to `impl Display for f64`, not an input.
        self.write_fmt(args).expect("f64 display fits F64_DISPLAY_MAX bytes");
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

    /// The bound behind `F64_DISPLAY_MAX`: every binary exponent (both
    /// signs, plus the neighbouring representable values) and the named
    /// extremes. A future `impl Display for f64` that prints longer
    /// trips this before it trips `FmtBuf::format`'s expect.
    #[test]
    fn f64_display_never_exceeds_fmtbuf() {
        let mut longest = (0usize, 0.0f64);
        let mut consider = |v: f64| {
            let len = format!("{v}").len();
            if len > longest.0 {
                longest = (len, v);
            }
        };
        for exp in -1074i32..=1023 {
            // Exact powers of two by bit pattern (`powi` underflows the
            // deep subnormals to zero).
            let bits = if exp < -1022 { 1u64 << (exp + 1074) } else { ((exp + 1023) as u64) << 52 };
            let v = f64::from_bits(bits);
            for w in [v, f64::from_bits(bits + 1), f64::from_bits(bits - 1)] {
                consider(w);
                consider(-w);
            }
        }
        for v in [
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            5e-324,
            -5e-324,
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            consider(v);
        }
        assert!(
            longest.0 <= F64_DISPLAY_MAX,
            "f64 Display reaches {} bytes for {:e}; F64_DISPLAY_MAX is {}",
            longest.0,
            longest.1,
            F64_DISPLAY_MAX
        );
        // The bound is tight to within the margin: the sweep finds the
        // 327-byte extreme the constant was derived from.
        assert_eq!(longest.0, 327, "the extreme moved: {:e}", longest.1);
    }

    /// Batch 12 of the 2026-08-30 review: before the fix this panicked
    /// (`f64 display fits 40 bytes`) — a `JSON.NUMINCRBY` on a RESP3
    /// connection with `1e300` in the document killed the cell.
    #[test]
    fn double_is_total_at_the_extremes() {
        let plain_1e300 = format!("{}", 1e300);
        assert_eq!(plain_1e300.len(), 301);
        assert_eq!(
            render(Protocol::Resp3, |w| w.double(1e300)),
            format!(",{plain_1e300}\r\n").as_bytes()
        );
        assert_eq!(
            render(Protocol::Resp2, |w| w.double(1e300)),
            format!("$301\r\n{plain_1e300}\r\n").as_bytes()
        );
        for v in [f64::MAX, f64::MIN, 5e-324, -f64::MIN_POSITIVE, f64::INFINITY, f64::NAN] {
            let text = format!("{v}");
            assert_eq!(render(Protocol::Resp3, |w| w.double(v)), format!(",{text}\r\n").as_bytes());
        }
        // The ordinary values keep their bytes.
        assert_eq!(render(Protocol::Resp3, |w| w.double(1.75)), b",1.75\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.double(-2.5)), b"$4\r\n-2.5\r\n");
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
    fn bulk_patched_matches_bulk_byte_for_byte() {
        for payload in [&b""[..], b"x", b"hello", &[b'y'; 12_345_678]] {
            let plain = render(Protocol::Resp2, |w| w.bulk(payload));
            let patched = render(Protocol::Resp2, |w| {
                w.bulk_patched(|out| out.extend_from_slice(payload));
            });
            assert_eq!(plain, patched, "len {}", payload.len());
        }
        // Mid-stream: earlier and later replies stay untouched.
        let out = render(Protocol::Resp2, |w| {
            w.int(1);
            w.bulk_patched(|out| out.extend_from_slice(b"abc"));
            w.simple("OK");
        });
        assert_eq!(out, b":1\r\n$3\r\nabc\r\n+OK\r\n");
    }

    /// C6 (review 2026-08-30): a line-framed reply must never gain a frame
    /// boundary from its payload. The pinned bytes are what
    /// **redis-server 8.0.5** answers for the same message text — its
    /// `addReplyErrorFormatInternal` runs `sdstrim(s, "\r\n")` then
    /// `sdsmapchars(s, "\r\n", "  ", 2)`, and both halves are observable
    /// (`EXPIRE k 100 <opt>` puts client bytes at the very end of the
    /// message, `unknown command` puts them in the middle).
    #[test]
    fn error_line_is_sanitized_like_the_redis_oracle() {
        // Interior CR/LF -> one space each (oracle: `EXPIRE k 100 B\r\nAD`).
        assert_eq!(
            render(Protocol::Resp2, |w| w.error("ERR Unsupported option B\r\nAD")),
            b"-ERR Unsupported option B  AD\r\n"
        );
        // Trailing run trimmed, not mapped (oracle: `... BAD\r\n` -> `BAD`).
        assert_eq!(
            render(Protocol::Resp2, |w| w.error("ERR Unsupported option BAD\r\n")),
            b"-ERR Unsupported option BAD\r\n"
        );
        assert_eq!(
            render(Protocol::Resp2, |w| w.error("ERR Unsupported option BAD\n")),
            b"-ERR Unsupported option BAD\r\n"
        );
        // An all-CR/LF tail leaves the message empty behind it.
        assert_eq!(
            render(Protocol::Resp2, |w| w.error("ERR Unsupported option \r\n")),
            b"-ERR Unsupported option \r\n"
        );
        // Lone CR and lone LF map too — RESP framing accepts a bare LF.
        assert_eq!(render(Protocol::Resp2, |w| w.error("ERR a\nb")), b"-ERR a b\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.error("ERR a\rb")), b"-ERR a b\r\n");
        // Simple strings are line-framed too, and get the same treatment.
        assert_eq!(render(Protocol::Resp2, |w| w.simple("O\r\nK")), b"+O  K\r\n");
        // Clean text is byte-identical to before the sanitizer existed.
        assert_eq!(render(Protocol::Resp2, |w| w.error("ERR boom")), b"-ERR boom\r\n");
        assert_eq!(render(Protocol::Resp2, |w| w.simple("OK")), b"+OK\r\n");
    }

    /// The attacker's half of C6: the reply forgery is a *framing* claim, so
    /// assert framing, not the text. Every byte, at every position of a
    /// short body, through both line-framed writers: exactly one CRLF, at
    /// the end.
    #[test]
    fn no_byte_in_a_line_reply_can_open_a_second_frame() {
        for byte in 0u8..=255 {
            for position in 0..3usize {
                let mut body = *b"abc";
                body[position] = byte;
                // Only valid UTF-8 can reach the `&str` writers; the raw
                // form takes the rest (`error_bytes`, the argv path).
                let cases: [Vec<u8>; 2] = [body.to_vec(), {
                    let mut v = b"ERR ".to_vec();
                    v.extend_from_slice(&body);
                    v.extend_from_slice(b" tail");
                    v
                }];
                for case in cases {
                    let out = render(Protocol::Resp2, |w| w.error_bytes(&case));
                    assert!(out.ends_with(b"\r\n"), "byte {byte:#04x} pos {position}: {out:?}");
                    let body = &out[1..out.len() - 2];
                    assert!(
                        !body.contains(&b'\r') && !body.contains(&b'\n'),
                        "byte {byte:#04x} pos {position} left a frame boundary: {out:?}"
                    );
                    if let Ok(text) = std::str::from_utf8(&case) {
                        assert_eq!(render(Protocol::Resp2, |w| w.error(text)), out);
                        let simple = render(Protocol::Resp2, |w| w.simple(text));
                        assert_eq!(&simple[1..], &out[1..], "simple/error must agree");
                    }
                }
            }
        }
    }

    /// C9 (review 2026-08-30, F-L12-03/F-L15-01): the length header must
    /// be correct for ANY payload the builder produces. The old code
    /// reserved 8 digits sized to the 16 MiB−1 *document* cap, but a
    /// serialized reply is not bounded by the document cap (multi-path
    /// `JSON.GET`, `INDENT`/`NEWLINE` repetition, `\u00xx` escape
    /// amplification: 6 × 16 MiB−1 ≈ 100.66 MB already crosses it), and
    /// `MAX_DIGITS - text.len()` underflowed at len ≥ 100,000,000 —
    /// `debug_assert` in this profile, wrapped `copy_within` panic and a
    /// whole-node `exit(101)` in release.
    #[test]
    fn bulk_patched_widens_the_header_past_the_reserve() {
        // One past the 8-digit reserve: 100,000,000 bytes → 9 digits.
        let payload_len = 100_000_000usize;
        let out = render(Protocol::Resp2, |w| {
            w.bulk_patched(|out| {
                let at = out.len();
                out.resize(at + payload_len, b'x');
            });
        });
        let header = b"$100000000\r\n";
        assert_eq!(&out[..header.len()], header);
        assert_eq!(out.len(), header.len() + payload_len + 2);
        assert!(out.ends_with(b"\r\n"));
        assert!(out[header.len()..header.len() + payload_len].iter().all(|b| *b == b'x'));
        // Mid-stream: an earlier reply is untouched and the next reply
        // lands after the widened frame.
        let out = render(Protocol::Resp2, |w| {
            w.int(7);
            w.bulk_patched(|out| {
                let at = out.len();
                out.resize(at + payload_len, b'y');
            });
            w.simple("OK");
        });
        assert_eq!(&out[..4], b":7\r\n");
        assert_eq!(&out[4..4 + header.len()], header);
        assert_eq!(&out[out.len() - 5..], b"+OK\r\n");
    }

    /// ADR-0099 D2: a failing builder must leave the buffer exactly as it
    /// was before the frame opened — earlier replies intact, no partial
    /// `$`-header — so the caller's error reply lands where the bulk
    /// would have.
    #[test]
    fn try_bulk_patched_rolls_back_the_whole_frame_on_error() {
        let out = render(Protocol::Resp2, |w| {
            w.int(1);
            let result = w.try_bulk_patched(|out| {
                out.extend_from_slice(b"partial payload the budget refused");
                Err::<(), &str>("too large")
            });
            assert_eq!(result, Err("too large"));
            w.error("ERR reply too large");
        });
        assert_eq!(out, b":1\r\n-ERR reply too large\r\n");
        // The Ok arm is byte-identical to the infallible method.
        let out = render(Protocol::Resp2, |w| {
            w.try_bulk_patched(|out| {
                out.extend_from_slice(b"abc");
                Ok::<(), &str>(())
            })
            .expect("infallible builder");
        });
        assert_eq!(out, b"$3\r\nabc\r\n");
    }

    /// SWAR kernel against a scalar oracle, every length across the
    /// eight-byte step and every position — the word path is invisible to
    /// the byte-sweep test above, which only builds short bodies.
    #[test]
    fn contains_line_break_matches_the_scalar_oracle() {
        fn scalar(text: &[u8]) -> bool {
            text.iter().any(|b| *b == b'\r' || *b == b'\n')
        }
        for len in 0..=24usize {
            let clean = vec![b'x'; len];
            assert!(!contains_line_break(&clean), "len {len}");
            for position in 0..len {
                for byte in [b'\r', b'\n', b'\t', 0x00, 0x0c, 0x8d, 0xff] {
                    let mut body = clean.clone();
                    body[position] = byte;
                    assert_eq!(
                        contains_line_break(&body),
                        scalar(&body),
                        "len {len} position {position} byte {byte:#04x}"
                    );
                }
            }
        }
    }

    /// Length-prefixed replies are *not* sanitized — CR/LF in a bulk body is
    /// legal wire content and truncating it would corrupt values. This is
    /// the negative half of the C6 contract (assert the space you forbid).
    #[test]
    fn length_prefixed_replies_still_carry_crlf_verbatim() {
        assert_eq!(render(Protocol::Resp2, |w| w.bulk(b"a\r\nb")), b"$4\r\na\r\nb\r\n");
        assert_eq!(
            render(Protocol::Resp3, |w| w.verbatim(b"txt", b"a\r\nb")),
            b"=8\r\ntxt:a\r\nb\r\n"
        );
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
