//! Minimal, zero-dependency JSON reader — just enough to navigate
//! `memtier_benchmark --json-out-file` output by path.
//!
//! It parses into an owned tree and exposes object navigation plus `f64`
//! extraction. It is deliberately *not* a serializer and not a hot-path
//! parser: the tool reads one small file per benchmark row. Navigating by
//! path matters because memtier repeats the `p50.00/p99.00/p99.90` keys both
//! at `ALL STATS / Totals / Percentile Latencies` (the run aggregate this tool
//! wants) and inside every per-second `Time-Serie` bucket — a `grep` for
//! `p99.90` would grab the wrong one.

use std::io::Read;

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ITEMS: usize = 256 * 1024;

#[derive(Debug)]
pub enum Error {
    ByteLimit,
    DepthLimit,
    ItemLimit,
    Syntax(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ByteLimit => output.write_str("JSON byte limit exceeded"),
            Self::DepthLimit => output.write_str("JSON nesting limit exceeded"),
            Self::ItemLimit => output.write_str("JSON item limit exceeded"),
            Self::Syntax(reason) => output.write_str(reason),
            Self::Io(error) => write!(output, "read JSON: {error}"),
        }
    }
}

impl From<&str> for Error {
    fn from(reason: &str) -> Self {
        Self::Syntax(reason.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Parse a complete JSON document. Returns an error (never panics) on any
    /// malformed input or trailing data.
    pub fn parse(input: &str) -> Result<Json, Error> {
        if input.len() > MAX_BYTES {
            return Err(Error::ByteLimit);
        }
        let mut p = Parser { bytes: input.as_bytes(), pos: 0, items: 0 };
        let value = p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(Error::Syntax(format!("trailing data at byte {}", p.pos)));
        }
        Ok(value)
    }

    /// Admit bytes while reading, including files that grow after opening.
    pub fn read(reader: impl Read) -> Result<Json, Error> {
        let mut bytes = Vec::new();
        reader.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(Error::Io)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::ByteLimit);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "invalid JSON UTF-8")?;
        Self::parse(text)
    }

    /// Navigate nested objects by key, e.g.
    /// `get(&["ALL STATS", "Totals", "Ops/sec"])`.
    pub fn get(&self, path: &[&str]) -> Option<&Json> {
        let mut cur = self;
        for key in path {
            let Json::Obj(fields) = cur else { return None };
            cur = fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
        }
        Some(cur)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// Numeric field at `path`, or an error naming the path that was missing.
    pub fn num_at(&self, path: &[&str]) -> Result<f64, String> {
        self.get(path)
            .and_then(Json::as_f64)
            .ok_or_else(|| format!("memtier json: missing numeric field `{}`", path.join(" / ")))
    }
}

/// The deepest nesting the reader accepts (F-L18-07, review of
/// 2026-08-30): the parser is iterative with an explicit stack and this
/// explicit depth limit. memtier's output nests about six deep.
pub const MAX_DEPTH: usize = 32;

/// One container still collecting on the explicit stack; an object also
/// carries the key its next value belongs to.
enum Open {
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>, String),
}

enum Start {
    Value(Json),
    Container(Open),
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    items: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Parses one value. Iterative: the open containers live on an
    /// explicit stack bounded by [`MAX_DEPTH`] (F-L18-07).
    fn value(&mut self) -> Result<Json, Error> {
        let mut stack = Vec::with_capacity(MAX_DEPTH);
        loop {
            let mut value = match self.start(stack.len())? {
                Start::Container(container) => {
                    stack.push(container);
                    continue;
                }
                Start::Value(value) => value,
            };
            loop {
                let Some(container) = stack.last_mut() else { return Ok(value) };
                if !self.accept(container, value)? {
                    break;
                }
                value = match stack.pop().expect("container exists") {
                    Open::Arr(items) => Json::Arr(items),
                    Open::Obj(fields, _) => Json::Obj(fields),
                };
            }
        }
    }

    fn start(&mut self, depth: usize) -> Result<Start, Error> {
        self.skip_ws();
        self.count_item()?;
        let value = match self.bytes.get(self.pos) {
            Some(tag @ (b'[' | b'{')) => {
                Self::check_depth(depth)?;
                let object = *tag == b'{';
                self.pos += 1;
                self.skip_ws();
                if self.take(if object { b'}' } else { b']' }) {
                    if object { Json::Obj(Vec::new()) } else { Json::Arr(Vec::new()) }
                } else {
                    let container = if object {
                        Open::Obj(Vec::new(), self.key()?)
                    } else {
                        Open::Arr(Vec::new())
                    };
                    return Ok(Start::Container(container));
                }
            }
            Some(b'"') => Json::Str(self.string()?),
            Some(b't') => self.literal("true", Json::Bool(true))?,
            Some(b'f') => self.literal("false", Json::Bool(false))?,
            Some(b'n') => self.literal("null", Json::Null)?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ => return Err(self.error("expected value")),
        };
        Ok(Start::Value(value))
    }

    fn accept(&mut self, container: &mut Open, value: Json) -> Result<bool, Error> {
        self.skip_ws();
        match container {
            Open::Arr(items) => {
                items.push(value);
                if self.take(b']') {
                    return Ok(true);
                }
                if !self.take(b',') {
                    return Err(self.error("expected ',' or ']'"));
                }
            }
            Open::Obj(fields, key) => {
                fields.push((std::mem::take(key), value));
                if self.take(b'}') {
                    return Ok(true);
                }
                if !self.take(b',') {
                    return Err(self.error("expected ',' or '}'"));
                }
                *key = self.key()?;
            }
        }
        Ok(false)
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.pos) != Some(&byte) {
            return false;
        }
        self.pos += 1;
        true
    }

    fn error(&self, reason: &str) -> Error {
        Error::Syntax(format!("JSON {reason} at byte {}", self.pos))
    }

    fn check_depth(depth: usize) -> Result<(), Error> {
        if depth == MAX_DEPTH { Err(Error::DepthLimit) } else { Ok(()) }
    }

    fn count_item(&mut self) -> Result<(), Error> {
        if self.items == MAX_ITEMS {
            return Err(Error::ItemLimit);
        }
        self.items += 1;
        Ok(())
    }

    /// An object key and its `:`.
    fn key(&mut self) -> Result<String, Error> {
        self.count_item()?;
        self.skip_ws();
        if self.bytes.get(self.pos) != Some(&b'"') {
            return Err(Error::Syntax(format!("expected object key at {}", self.pos)));
        }
        let key = self.string()?;
        self.skip_ws();
        if self.bytes.get(self.pos) != Some(&b':') {
            return Err(Error::Syntax(format!("expected colon at {}", self.pos)));
        }
        self.pos += 1;
        Ok(key)
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, Error> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(val)
        } else {
            Err(Error::Syntax(format!("invalid literal at {}", self.pos)))
        }
    }

    fn string(&mut self) -> Result<String, Error> {
        self.pos += 1;
        let mut bytes = Vec::new();
        while let Some(&byte) = self.bytes.get(self.pos) {
            self.pos += 1;
            match byte {
                b'"' => return String::from_utf8(bytes).map_err(|_| self.error("invalid UTF-8")),
                b'\\' => self.escape(&mut bytes)?,
                0..=0x1f => return Err(self.error("unescaped control byte")),
                _ => bytes.push(byte),
            }
        }
        Err(self.error("unterminated string"))
    }

    fn escape(&mut self, bytes: &mut Vec<u8>) -> Result<(), Error> {
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Err(self.error("unterminated escape"));
        };
        self.pos += 1;
        match byte {
            b'"' | b'\\' | b'/' => bytes.push(byte),
            b'n' => bytes.push(b'\n'),
            b'r' => bytes.push(b'\r'),
            b't' => bytes.push(b'\t'),
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'u' => {
                let mut code = self.hex()?;
                if (0xd800..=0xdbff).contains(&code) {
                    if !self.take(b'\\') || !self.take(b'u') {
                        return Err(self.error("missing low surrogate"));
                    }
                    let low = self.hex()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(self.error("invalid low surrogate"));
                    }
                    code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                }
                let character = char::from_u32(code).ok_or_else(|| self.error("invalid scalar"))?;
                bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
            }
            _ => return Err(self.error("invalid escape")),
        }
        Ok(())
    }

    fn hex(&mut self) -> Result<u32, Error> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = self.bytes.get(self.pos).and_then(|&b| (b as char).to_digit(16));
            code = code * 16 + digit.ok_or_else(|| self.error("invalid Unicode escape"))?;
            self.pos += 1;
        }
        Ok(code)
    }

    fn number(&mut self) -> Result<Json, Error> {
        let begin = self.pos;
        self.take(b'-');
        if !self.take(b'0') {
            self.digits()?;
        }
        if self.take(b'.') {
            self.digits()?;
        }
        if self.take(b'e') || self.take(b'E') {
            if !self.take(b'+') {
                self.take(b'-');
            }
            self.digits()?;
        }
        let number = std::str::from_utf8(&self.bytes[begin..self.pos])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|number| number.is_finite())
            .ok_or_else(|| self.error("invalid or non-finite number"))?;
        Ok(Json::Num(number))
    }

    fn digits(&mut self) -> Result<(), Error> {
        let begin = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.pos == begin { Err(self.error("expected digit")) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_byte_and_item_limits_and_bounded_reader() {
        let document = format!("{}1", " ".repeat(MAX_BYTES - 1));
        assert_eq!(Json::parse(&document).unwrap(), Json::Num(1.0));
        assert_eq!(Json::read(document.as_bytes()).unwrap(), Json::Num(1.0));
        assert!(matches!(Json::read(std::io::repeat(b' ')), Err(Error::ByteLimit)));
        let document = format!("[{}0]", "0,".repeat(MAX_ITEMS - 2));
        assert!(Json::parse(&document).is_ok());
        let document = format!("[{}0]", "0,".repeat(MAX_ITEMS - 1));
        assert!(matches!(Json::parse(&document), Err(Error::ItemLimit)));
        // Keys also consume the item budget.
        let object = format!("{{{}}}", "\"x\":0,".repeat(MAX_ITEMS / 2));
        assert!(matches!(Json::parse(&object), Err(Error::ItemLimit)));
    }

    #[test]
    fn mixed_empty_container_depth_is_exact() {
        let prefix = "[".repeat(MAX_DEPTH - 1);
        let suffix = "]".repeat(MAX_DEPTH - 1);
        assert!(Json::parse(&format!("{prefix}{{}}{suffix}")).is_ok());
        assert!(matches!(Json::parse(&format!("[{prefix}{{}}{suffix}]")), Err(Error::DepthLimit)));
    }

    #[test]
    fn malformed_tokens_unicode_and_nonfinite_numbers_are_refused() {
        for invalid in [
            "01",
            "1.",
            "1e",
            "-.1",
            "1e999",
            "[1,]",
            "{\"k\":}",
            "\"\n\"",
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\uD800\u0000""#,
        ] {
            assert!(Json::parse(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(Json::parse(r#""\uD83D\uDE00""#).unwrap(), Json::Str("😀".into()));
        assert_eq!(Json::parse("-0.01e+2").unwrap(), Json::Num(-1.0));
    }

    #[test]
    fn empty_containers_count_toward_depth_and_documents_are_bounded() {
        let nested = format!("{}[]{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(Json::parse(&nested).is_err());
        let oversized = format!("{}1", " ".repeat(16 * 1024 * 1024));
        assert!(Json::parse(&oversized).is_err());
        let too_many = format!("[{}0]", "0,".repeat(256 * 1024));
        assert!(Json::parse(&too_many).is_err());
    }

    // A miniature of memtier's real shape: the same percentile keys appear in
    // a Time-Serie bucket AND in the run aggregate, so path navigation must
    // pick the aggregate.
    const SAMPLE: &str = r#"{
        "ALL STATS":{
            "Totals":{
                "Ops/sec": 412345.6,
                "Average Latency": 0.019,
                "Time-Serie":{ "0":{ "p50.00": 9.9, "p99.00": 9.9, "p99.90": 9.9 } },
                "Percentile Latencies":{ "p50.00": 0.023, "p99.00": 0.031, "p99.90": 0.103 }
            }
        }
    }"#;

    #[test]
    fn navigates_to_run_aggregate_not_timeserie() {
        let json = Json::parse(SAMPLE).expect("parse");
        assert_eq!(json.num_at(&["ALL STATS", "Totals", "Ops/sec"]).unwrap(), 412345.6);
        assert_eq!(
            json.num_at(&["ALL STATS", "Totals", "Percentile Latencies", "p99.90"]).unwrap(),
            0.103
        );
    }

    #[test]
    fn missing_field_is_an_error_not_a_panic() {
        let json = Json::parse(SAMPLE).unwrap();
        assert!(json.num_at(&["ALL STATS", "Totals", "Nope"]).is_err());
    }

    #[test]
    fn parses_scalars_strings_arrays_and_escapes() {
        assert_eq!(Json::parse("-12.5e1").unwrap(), Json::Num(-125.0));
        assert_eq!(Json::parse("  true ").unwrap(), Json::Bool(true));
        assert_eq!(Json::parse(r#""a\/b\n""#).unwrap(), Json::Str("a/b\n".into()));
        assert_eq!(
            Json::parse("[1, 2, [3]]").unwrap(),
            Json::Arr(vec![Json::Num(1.0), Json::Num(2.0), Json::Arr(vec![Json::Num(3.0)])])
        );
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(Json::parse("{} junk").is_err());
    }

    /// F-L18-07 (review of 2026-08-30): a document nested past the depth
    /// cap is a typed error, never a stack overflow.
    #[test]
    fn nesting_past_the_cap_is_a_typed_error() {
        let deep = "[".repeat(200_000);
        let err = Json::parse(&deep).expect_err("typed");
        assert!(err.to_string().contains("nesting"), "{err}");
        let deep = "{\"k\":".repeat(200_000);
        let err = Json::parse(&deep).expect_err("typed");
        assert!(err.to_string().contains("nesting"), "{err}");
        // Exactly at the cap: legal; one past: refused.
        let at_cap = format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(Json::parse(&at_cap).is_ok());
        let past = format!("{}1{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(Json::parse(&past).expect_err("typed").to_string().contains("nesting"));
    }
}
