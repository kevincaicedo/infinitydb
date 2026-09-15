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
    pub fn parse(input: &str) -> Result<Json, String> {
        let mut p = Parser { bytes: input.as_bytes(), pos: 0 };
        let value = p.value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(value)
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

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
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
    fn value(&mut self) -> Result<Json, String> {
        let mut stack: Vec<Open> = Vec::new();
        loop {
            self.skip_ws();
            let mut value = match self.bytes.get(self.pos) {
                Some(b'{') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.bytes.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        Json::Obj(Vec::new())
                    } else {
                        let key = self.key()?;
                        self.open(&mut stack, Open::Obj(Vec::new(), key))?;
                        continue;
                    }
                }
                Some(b'[') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.bytes.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        Json::Arr(Vec::new())
                    } else {
                        self.open(&mut stack, Open::Arr(Vec::new()))?;
                        continue;
                    }
                }
                Some(b'"') => Json::Str(self.string()?),
                Some(b't') => self.literal("true", Json::Bool(true))?,
                Some(b'f') => self.literal("false", Json::Bool(false))?,
                Some(b'n') => self.literal("null", Json::Null)?,
                Some(&b) if b == b'-' || b.is_ascii_digit() => self.number()?,
                Some(&b) => return Err(format!("unexpected byte `{}` at {}", b as char, self.pos)),
                None => return Err("unexpected end of input".into()),
            };
            // Deliver the value into the innermost open container, closing
            // every container it completes.
            loop {
                let Some(top) = stack.last_mut() else { return Ok(value) };
                self.skip_ws();
                let next = self.bytes.get(self.pos).copied();
                self.pos += usize::from(matches!(next, Some(b',' | b']' | b'}')));
                match top {
                    Open::Arr(items) => {
                        items.push(value);
                        match next {
                            Some(b',') => break,
                            Some(b']') => {}
                            _ => return Err(format!("expected ',' or ']' at {}", self.pos)),
                        }
                    }
                    Open::Obj(fields, key) => {
                        fields.push((std::mem::take(key), value));
                        match next {
                            Some(b',') => {
                                *key = self.key()?;
                                break;
                            }
                            Some(b'}') => {}
                            _ => return Err(format!("expected ',' or '}}' at {}", self.pos)),
                        }
                    }
                }
                value = match stack.pop().expect("checked non-empty") {
                    Open::Arr(items) => Json::Arr(items),
                    Open::Obj(fields, _) => Json::Obj(fields),
                };
            }
        }
    }

    fn open(&self, stack: &mut Vec<Open>, container: Open) -> Result<(), String> {
        if stack.len() == MAX_DEPTH {
            return Err(format!("nesting deeper than {MAX_DEPTH} at byte {}", self.pos));
        }
        stack.push(container);
        Ok(())
    }

    /// An object key and its `:`.
    fn key(&mut self) -> Result<String, String> {
        self.skip_ws();
        if self.bytes.get(self.pos) != Some(&b'"') {
            return Err(format!("expected object key at {}", self.pos));
        }
        let key = self.string()?;
        self.skip_ws();
        if self.bytes.get(self.pos) != Some(&b':') {
            return Err(format!("expected ':' after key `{key}` at {}", self.pos));
        }
        self.pos += 1;
        Ok(key)
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(val)
        } else {
            Err(format!("invalid literal at {}", self.pos))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err("unterminated string".into());
            };
            self.pos += 1;
            match b {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let Some(&esc) = self.bytes.get(self.pos) else {
                        return Err("unterminated escape".into());
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => {
                            let hex =
                                self.bytes.get(self.pos..self.pos + 4).ok_or("short \\u escape")?;
                            let digits = core::str::from_utf8(hex).map_err(|_| "bad \\u escape")?;
                            let code =
                                u32::from_str_radix(digits, 16).map_err(|_| "bad \\u hex")?;
                            self.pos += 4;
                            let ch = char::from_u32(code).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => return Err(format!("bad escape `\\{}`", other as char)),
                    }
                }
                _ => out.push(b),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = core::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "bad number")?;
        text.parse::<f64>().map(Json::Num).map_err(|_| format!("invalid number `{text}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(err.contains("nesting"), "{err}");
        let deep = "{\"k\":".repeat(200_000);
        let err = Json::parse(&deep).expect_err("typed");
        assert!(err.contains("nesting"), "{err}");
        // Exactly at the cap: legal; one past: refused.
        let at_cap = format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(Json::parse(&at_cap).is_ok());
        let past = format!("{}1{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(Json::parse(&past).expect_err("typed").contains("nesting"));
    }
}
