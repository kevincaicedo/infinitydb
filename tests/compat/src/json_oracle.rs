//! M3-S21 RedisJSON differential corpus and deviation policy.
//!
//! Raw RESP bytes govern. JSON semantic equality is reported only as a
//! diagnostic; it never makes a byte mismatch pass. Every accepted mismatch
//! names one stable case + protocol and carries the justification rendered
//! into `docs/compat-matrix.md`.

use crate::resp::frame_len;

pub const REDIS_STACK_IMAGE: &str = "redis/redis-stack-server:7.4.0-v8";
pub const REDIS_STACK_DIGEST: &str =
    "sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a";
pub const REDISJSON_MODULE_VERSION: &str = "20809";

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Protocol {
    Resp2,
    Resp3,
}

impl Protocol {
    pub const ALL: [Protocol; 2] = [Protocol::Resp2, Protocol::Resp3];

    pub fn name(self) -> &'static str {
        match self {
            Protocol::Resp2 => "RESP2",
            Protocol::Resp3 => "RESP3",
        }
    }
}

pub struct JsonCase {
    pub id: &'static str,
    pub argv: &'static [&'static str],
    /// This reply may contain JSON text in RESP bulk payloads. Used only for
    /// the independent semantic diagnostic on a byte mismatch.
    pub json_reply: bool,
    pub source: &'static str,
}

const fn c(
    id: &'static str,
    argv: &'static [&'static str],
    json_reply: bool,
    source: &'static str,
) -> JsonCase {
    JsonCase { id, argv, json_reply, source }
}

/// Ordered, stateful script re-derived from the local S11-S15 command
/// matrices. The final `fuzz-*` cases are minimized parser/delta edge shapes,
/// not copied upstream tests.
pub static JSON_CASES: &[JsonCase] = &[
    c("s15-debug-missing", &["JSON.DEBUG", "MEMORY", "nope"], false, "S15"),
    c(
        "s15-set-root",
        &[
            "JSON.SET",
            "k",
            "$",
            r#"{"a":[1,2],"s":"x","n":5,"b":true,"o":{"k":1},"e":[],"f":1.5,"nl":null}"#,
        ],
        false,
        "S11",
    ),
    c("s11-generic-type-json", &["TYPE", "k"], false, "S11"),
    c("s15-set-modern-xx", &["JSON.SET", "k", "$.a", "[1,2]", "XX"], false, "S11"),
    c("s15-set-modern-miss", &["JSON.SET", "k", "$.zz", "1", "XX"], false, "S11"),
    c("s15-get-modern", &["JSON.GET", "k", "$.a"], true, "S11"),
    c("s15-get-legacy", &["JSON.GET", "k", ".a"], true, "S11"),
    c("s15-get-missing", &["JSON.GET", "nope"], true, "S11"),
    c("s15-mget", &["JSON.MGET", "k", "nope", "$.s"], true, "S11"),
    c("s15-type-modern", &["JSON.TYPE", "k", "$.n"], false, "S11"),
    c("s15-type-legacy-miss", &["JSON.TYPE", "k", ".missing"], false, "S11"),
    c("s15-numincr-modern", &["JSON.NUMINCRBY", "k", "$.n", "1"], true, "S12"),
    c("s15-nummult-modern", &["JSON.NUMMULTBY", "k", "$.n", "2"], true, "S12"),
    c("s15-numincr-legacy", &["JSON.NUMINCRBY", "k", ".n", "0"], true, "S12"),
    c("s15-strappend-modern", &["JSON.STRAPPEND", "k", "$.s", r#""y""#], false, "S12"),
    c("s15-strappend-skip", &["JSON.STRAPPEND", "k", "$.*", r#""!""#], false, "S12"),
    c("s15-strlen-modern", &["JSON.STRLEN", "k", "$.s"], false, "S12"),
    c("s15-strlen-skip", &["JSON.STRLEN", "k", "$.n"], false, "S12"),
    c("s15-strlen-legacy", &["JSON.STRLEN", "k", ".s"], false, "S12"),
    c("s15-toggle-modern", &["JSON.TOGGLE", "k", "$.b"], false, "S12"),
    c("s15-toggle-legacy", &["JSON.TOGGLE", "k", ".b"], true, "S12"),
    c("s15-clear", &["JSON.CLEAR", "k", "$.o"], false, "S12"),
    c("s15-arrappend-modern", &["JSON.ARRAPPEND", "k", "$.a", "3"], false, "S13"),
    c("s15-arrappend-skip", &["JSON.ARRAPPEND", "k", "$.*", "9"], false, "S13"),
    c("s15-arrinsert-modern", &["JSON.ARRINSERT", "k", "$.a", "0", "0"], false, "S13"),
    c("s15-arrinsert-legacy", &["JSON.ARRINSERT", "k", ".a", "0", "-1"], false, "S13"),
    c("s15-arrindex-modern", &["JSON.ARRINDEX", "k", "$.a", "2"], false, "S13"),
    c("s15-arrindex-skip", &["JSON.ARRINDEX", "k", "$.s", "2"], false, "S13"),
    c("s15-arrindex-legacy", &["JSON.ARRINDEX", "k", ".a", "2", "0", "0"], false, "S13"),
    c("s15-arrlen-modern", &["JSON.ARRLEN", "k", "$.a"], false, "S13"),
    c("s15-arrlen-skip", &["JSON.ARRLEN", "k", "$.n"], false, "S13"),
    c("s15-arrlen-legacy", &["JSON.ARRLEN", "k", ".a"], false, "S13"),
    c("s15-arrpop-modern", &["JSON.ARRPOP", "k", "$.a"], true, "S13"),
    c("s15-arrpop-skip", &["JSON.ARRPOP", "k", "$.n"], false, "S13"),
    c("s15-arrpop-legacy", &["JSON.ARRPOP", "k", ".a", "0"], true, "S13"),
    c("s15-arrtrim-modern", &["JSON.ARRTRIM", "k", "$.a", "0", "1"], false, "S13"),
    c("s15-arrtrim-legacy", &["JSON.ARRTRIM", "k", ".a", "0", "0"], false, "S13"),
    c("s15-objkeys-modern", &["JSON.OBJKEYS", "k", "$.o"], false, "S14"),
    c("s15-objkeys-root", &["JSON.OBJKEYS", "k"], false, "S14"),
    c("s15-objlen-modern", &["JSON.OBJLEN", "k", "$.o"], false, "S14"),
    c("s15-objlen-legacy", &["JSON.OBJLEN", "k", ".o"], false, "S14"),
    c("s15-merge", &["JSON.MERGE", "k", "$.o", r#"{"z":1}"#], false, "S14"),
    c("s15-objlen-after-merge", &["JSON.OBJLEN", "k", ".o"], false, "S14"),
    c(
        "edge-merge-null-set",
        &["JSON.SET", "merge:null", "$", r#"{"a":[1,2],"o":{"x":1}}"#],
        false,
        "S14",
    ),
    c("edge-merge-null-member", &["JSON.MERGE", "merge:null", "$.o.x", "null"], false, "S14"),
    c("edge-merge-null-array", &["JSON.MERGE", "merge:null", "$.a[0]", "null"], false, "S14"),
    c("edge-merge-null-get", &["JSON.GET", "merge:null"], true, "S14"),
    c("edge-merge-null-root", &["JSON.MERGE", "merge:null", "$", "null"], false, "S14"),
    c("edge-merge-null-root-get", &["JSON.GET", "merge:null"], true, "S14"),
    c(
        "edge-merge-overlap-set",
        &["JSON.SET", "merge:overlap", "$", r#"{"o":{"b":{"k":1}}}"#],
        false,
        "S14",
    ),
    c(
        "edge-merge-overlap",
        &["JSON.MERGE", "merge:overlap", "$..*", r#"{"b":{"k":1}}"#],
        false,
        "S14",
    ),
    c("edge-merge-overlap-get", &["JSON.GET", "merge:overlap"], true, "S14"),
    c("edge-trim-overlap-set", &["JSON.SET", "trim:overlap", "$", "[[null,[null]]]"], false, "S13"),
    c("edge-trim-overlap", &["JSON.ARRTRIM", "trim:overlap", "$..*", "1", "1"], false, "S13"),
    c("edge-trim-overlap-get", &["JSON.GET", "trim:overlap"], true, "S13"),
    c("s15-del-path", &["JSON.DEL", "k", "$.nl"], false, "S11"),
    c("s15-forget-path", &["JSON.FORGET", "k", "$.f"], false, "S11"),
    c("s15-del-root", &["JSON.DEL", "k"], false, "S11"),
    c("s15-get-after-del", &["JSON.GET", "k"], true, "S11"),
    c("edge-set-pretty", &["JSON.SET", "pretty", "$", r#"{"a":[1,2],"b":"x"}"#], false, "S11"),
    c(
        "edge-get-pretty",
        &["JSON.GET", "pretty", "INDENT", "  ", "NEWLINE", "\n", "SPACE", " ", "."],
        true,
        "S11",
    ),
    c("edge-set-atomic", &["JSON.SET", "atomic", "$", r#"{"a":[1,2,3],"b":[1]}"#], false, "S13"),
    c("edge-arrinsert-abort", &["JSON.ARRINSERT", "atomic", "$.*", "3", "0"], false, "S13"),
    c("edge-get-after-abort", &["JSON.GET", "atomic"], true, "S13"),
    c(
        "edge-set-overlap",
        &["JSON.SET", "overlap", "$", r#"{"a":{"a":1},"x":{"a":2}}"#],
        false,
        "S12",
    ),
    c("edge-del-overlap", &["JSON.DEL", "overlap", "$..a"], false, "S12"),
    c("edge-get-overlap", &["JSON.GET", "overlap"], true, "S12"),
    c("edge-set-string", &["SET", "plain", "v"], false, "S11"),
    c("edge-wrongtype-get", &["JSON.GET", "plain"], false, "S11"),
    c("edge-wrongtype-arrlen", &["JSON.ARRLEN", "plain"], false, "S13"),
    c("edge-debug-memory", &["JSON.DEBUG", "MEMORY", "pretty"], false, "S15"),
    c("fuzz-dup-set", &["JSON.SET", "fuzz:dup", "$", r#"{"k":1,"k":2}"#], false, "fuzz-min"),
    c("fuzz-dup-get", &["JSON.GET", "fuzz:dup"], true, "fuzz-min"),
    c("fuzz-escape-set", &["JSON.SET", "fuzz:escape", "$", r#""2 1\u0061""#], false, "fuzz-min"),
    c("fuzz-escape-get", &["JSON.GET", "fuzz:escape"], true, "fuzz-min"),
    c("fuzz-empty-array-set", &["JSON.SET", "fuzz:empty", "$", r#"["",""]"#], false, "fuzz-min"),
    c("fuzz-empty-array-get", &["JSON.GET", "fuzz:empty"], true, "fuzz-min"),
    c(
        "fuzz-nested-set",
        &["JSON.SET", "fuzz:nested", "$", r#"{"N":{"-":{"N":{}}}}"#],
        false,
        "fuzz-min",
    ),
    c("fuzz-nested-get", &["JSON.GET", "fuzz:nested"], true, "fuzz-min"),
    c("fuzz-exp-set", &["JSON.SET", "fuzz:exp", "$", "[3e72,3e73]"], false, "fuzz-min"),
    c("fuzz-exp-get", &["JSON.GET", "fuzz:exp"], true, "fuzz-min"),
    c("edge-negative-zero-set", &["JSON.SET", "edge:-0", "$", "-0"], false, "S05"),
    c("edge-negative-zero-get", &["JSON.GET", "edge:-0"], true, "S05"),
    c("edge-invalid-json", &["JSON.SET", "edge:bad", "$", "{bad"], false, "S05"),
];

pub struct Deviation {
    pub case_id: &'static str,
    pub protocol: Protocol,
    /// Command that owns the semantic difference. This may differ from the
    /// observation command when a follow-up read exposes an earlier write's
    /// state divergence.
    pub command: &'static str,
    pub justification: &'static str,
}

const fn d(
    case_id: &'static str,
    protocol: Protocol,
    command: &'static str,
    justification: &'static str,
) -> Deviation {
    Deviation { case_id, protocol, command, justification }
}

/// The only accepted RedisJSON byte deviations. Entries are populated from
/// observed, understood differences; the runtime test rejects stale entries.
pub static DEVIATIONS: &[Deviation] = &[
    d(
        "s15-debug-missing",
        Protocol::Resp2,
        "JSON.DEBUG",
        "missing document: InfinityDB returns null; RedisJSON returns integer 0",
    ),
    d(
        "edge-get-after-abort",
        Protocol::Resp2,
        "JSON.ARRINSERT",
        "InfinityDB validates the full match set before commit; RedisJSON mutates an earlier match before a later index error",
    ),
    d(
        "edge-del-overlap",
        Protocol::Resp2,
        "JSON.DEL",
        "recursive overlap: InfinityDB reports three raw matches; RedisJSON reports two removals; post-state is identical",
    ),
    d(
        "edge-wrongtype-get",
        Protocol::Resp2,
        "JSON.GET",
        "InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text",
    ),
    d(
        "edge-wrongtype-arrlen",
        Protocol::Resp2,
        "JSON.ARRLEN",
        "InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text",
    ),
    d(
        "edge-debug-memory",
        Protocol::Resp2,
        "JSON.DEBUG",
        "InfinityDB reports canonical document attribution; RedisJSON reports module allocator bytes",
    ),
    d(
        "fuzz-exp-get",
        Protocol::Resp2,
        "JSON.GET",
        "large-exponent f64 parsing and canonical text differ from RedisJSON rounding",
    ),
    d(
        "edge-invalid-json",
        Protocol::Resp2,
        "JSON.SET",
        "both reject the malformed input at the first member; parser-specific error text differs",
    ),
    d(
        "edge-merge-overlap-get",
        Protocol::Resp2,
        "JSON.MERGE",
        "InfinityDB computes retaining overlaps from one snapshot and lets a changed ancestor supersede descendants; RedisJSON cascades descendant results",
    ),
    d(
        "edge-trim-overlap",
        Protocol::Resp2,
        "JSON.ARRTRIM",
        "overlapping mixed-type matches reach the same post-state; RedisJSON returns a path error while InfinityDB reports per-match length/null results",
    ),
    d(
        "s15-debug-missing",
        Protocol::Resp3,
        "JSON.DEBUG",
        "missing document: InfinityDB returns null; RedisJSON returns integer 0",
    ),
    d(
        "edge-get-after-abort",
        Protocol::Resp3,
        "JSON.ARRINSERT",
        "InfinityDB validates the full match set before commit; RedisJSON mutates an earlier match before a later index error",
    ),
    d(
        "edge-del-overlap",
        Protocol::Resp3,
        "JSON.DEL",
        "recursive overlap: InfinityDB reports three raw matches; RedisJSON reports two removals; post-state is identical",
    ),
    d(
        "edge-wrongtype-get",
        Protocol::Resp3,
        "JSON.GET",
        "InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text",
    ),
    d(
        "edge-wrongtype-arrlen",
        Protocol::Resp3,
        "JSON.ARRLEN",
        "InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text",
    ),
    d(
        "edge-debug-memory",
        Protocol::Resp3,
        "JSON.DEBUG",
        "InfinityDB reports canonical document attribution; RedisJSON reports module allocator bytes",
    ),
    d(
        "fuzz-exp-get",
        Protocol::Resp3,
        "JSON.GET",
        "large-exponent f64 parsing and canonical text differ from RedisJSON rounding",
    ),
    d(
        "edge-invalid-json",
        Protocol::Resp3,
        "JSON.SET",
        "both reject the malformed input at the first member; parser-specific error text differs",
    ),
    d(
        "edge-merge-overlap-get",
        Protocol::Resp3,
        "JSON.MERGE",
        "InfinityDB computes retaining overlaps from one snapshot and lets a changed ancestor supersede descendants; RedisJSON cascades descendant results",
    ),
    d(
        "edge-trim-overlap",
        Protocol::Resp3,
        "JSON.ARRTRIM",
        "overlapping mixed-type matches reach the same post-state; RedisJSON returns a path error while InfinityDB reports per-match length/null results",
    ),
];

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Comparison {
    Exact,
    Allowed { deviation_index: usize, semantic_equal: bool },
}

pub fn compare(
    case: &JsonCase,
    protocol: Protocol,
    oracle: &[u8],
    candidate: &[u8],
    deviations: &[Deviation],
) -> Result<Comparison, String> {
    if oracle == candidate {
        return Ok(Comparison::Exact);
    }
    let semantic_equal = case.json_reply
        && semantic_json_values(oracle).zip(semantic_json_values(candidate)).is_some_and(
            |(left, right)| {
                left.len() == right.len()
                    && left.iter().zip(&right).all(|(left, right)| json_semantic_eq(left, right))
            },
        );
    if let Some((deviation_index, _)) = deviations
        .iter()
        .enumerate()
        .find(|(_, deviation)| deviation.case_id == case.id && deviation.protocol == protocol)
    {
        return Ok(Comparison::Allowed { deviation_index, semantic_equal });
    }
    Err(format!(
        "{} {} {}: unallowlisted byte mismatch (semantic_equal={semantic_equal})\n  oracle    {:?}\n  candidate {:?}",
        protocol.name(),
        case.id,
        case.argv[0],
        String::from_utf8_lossy(oracle),
        String::from_utf8_lossy(candidate),
    ))
}

fn semantic_json_values(reply: &[u8]) -> Option<Vec<serde_json::Value>> {
    frame_len(reply).ok().flatten().filter(|length| *length == reply.len())?;
    let mut values = Vec::new();
    collect_bulk_json(reply, 0, &mut values).ok()?;
    (!values.is_empty()).then_some(values)
}

fn json_semantic_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;

    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => {
            let integer = |number: &serde_json::Number| {
                number.as_i64().map(i128::from).or_else(|| number.as_u64().map(i128::from))
            };
            match (integer(left), integer(right)) {
                (Some(left), Some(right)) => left == right,
                _ => left.as_f64().zip(right.as_f64()).is_some_and(|(left, right)| left == right),
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| json_semantic_eq(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right.get(key).is_some_and(|right| json_semantic_eq(left, right))
                })
        }
        _ => false,
    }
}

fn collect_bulk_json(
    reply: &[u8],
    at: usize,
    values: &mut Vec<serde_json::Value>,
) -> Result<usize, ()> {
    let tag = *reply.get(at).ok_or(())?;
    match tag {
        b'$' | b'=' => {
            let (head_end, len) = header(reply, at)?;
            if len < 0 {
                return Ok(head_end);
            }
            let end = head_end + len as usize;
            let payload = reply.get(head_end..end).ok_or(())?;
            if let Ok(value) = serde_json::from_slice(payload) {
                values.push(value);
            }
            Ok(end + 2)
        }
        b'*' | b'~' | b'>' => aggregate(reply, at, 1, values),
        b'%' | b'|' => aggregate(reply, at, 2, values),
        b'+' | b'-' | b':' | b'#' | b',' | b'(' | b'_' => line_end(reply, at + 1),
        _ => Err(()),
    }
}

fn aggregate(
    reply: &[u8],
    at: usize,
    width: usize,
    values: &mut Vec<serde_json::Value>,
) -> Result<usize, ()> {
    let (mut cursor, count) = header(reply, at)?;
    if count < 0 {
        return Ok(cursor);
    }
    for _ in 0..count as usize * width {
        cursor = collect_bulk_json(reply, cursor, values)?;
    }
    Ok(cursor)
}

fn header(reply: &[u8], at: usize) -> Result<(usize, i64), ()> {
    let end = line_end(reply, at + 1)?;
    let value =
        std::str::from_utf8(&reply[at + 1..end - 2]).map_err(|_| ())?.parse().map_err(|_| ())?;
    Ok((end, value))
}

fn line_end(reply: &[u8], from: usize) -> Result<usize, ()> {
    let offset =
        reply.get(from..).ok_or(())?.windows(2).position(|bytes| bytes == b"\r\n").ok_or(())?;
    Ok(from + offset + 2)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn an_unallowlisted_deviation_fails() {
        let case = c("canary", &["JSON.GET", "k"], true, "canary");
        assert!(compare(&case, Protocol::Resp2, b"$1\r\n1\r\n", b"$3\r\n1.0\r\n", &[]).is_err());
    }

    #[test]
    fn semantic_equality_is_diagnostic_only() {
        let case = c("canary", &["JSON.GET", "k"], true, "canary");
        let deviation = Deviation {
            case_id: "canary",
            protocol: Protocol::Resp2,
            command: "JSON.GET",
            justification: "test-only",
        };
        assert_eq!(
            compare(&case, Protocol::Resp2, b"$1\r\n1\r\n", b"$3\r\n1.0\r\n", &[deviation]),
            Ok(Comparison::Allowed { deviation_index: 0, semantic_equal: true })
        );
    }

    #[test]
    fn case_and_allowlist_keys_are_unique_and_total() {
        let mut ids = BTreeSet::new();
        for case in JSON_CASES {
            assert!(ids.insert(case.id), "duplicate JSON case id {}", case.id);
            assert!(!case.argv.is_empty() && !case.source.is_empty());
        }
        let mut entries = BTreeSet::new();
        for deviation in DEVIATIONS {
            assert!(entries.insert((deviation.case_id, deviation.protocol.name())));
            JSON_CASES.iter().find(|case| case.id == deviation.case_id).expect("case");
            assert!(
                JSON_CASES.iter().any(|case| case.argv[0] == deviation.command),
                "uncovered owner command {}",
                deviation.command
            );
            assert!(!deviation.justification.is_empty());
        }
    }
}
