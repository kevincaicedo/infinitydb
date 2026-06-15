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

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(&b) if b == b'-' || b.is_ascii_digit() => self.number(),
            Some(&b) => Err(format!("unexpected byte `{}` at {}", b as char, self.pos)),
            None => Err("unexpected end of input".into()),
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(val)
        } else {
            Err(format!("invalid literal at {}", self.pos))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.pos += 1; // consume '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
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
            let val = self.value()?;
            fields.push((key, val));
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err(format!("expected ',' or '}}' at {}", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at {}", self.pos)),
            }
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
}
