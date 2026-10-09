//! M3-S11/S12 reply-shape and edge-matrix suite (ADR-0041 D6–D9): every
//! `JSON.*` reply byte pinned through the real `execute` path — the same
//! parser/registry/handlers production runs, minus the reactor. The S21
//! redis-stack corpus byte-diffs both protocols and admits only explicit,
//! checked deviations (L8).

#![cfg(feature = "doc")]

use inf_doc::CanonicalDoc;
use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StoreConfig};
use inf_wire::Protocol;

struct Db {
    ks: Keyspace,
    cx: ConnCx,
    clock: u64,
}

impl Db {
    fn new() -> Db {
        Db::with_config(StoreConfig::default())
    }

    fn with_config(cfg: StoreConfig) -> Db {
        Db {
            ks: Keyspace::new(cfg),
            cx: ConnCx::try_default().expect("fixture cache allocation"),
            clock: 0,
        }
    }

    fn run(&mut self, argv: &[&[u8]]) -> Vec<u8> {
        self.clock += 1;
        let mut out = Vec::new();
        execute_slices(argv, &mut self.ks, &mut self.cx, Nanos(self.clock), &mut out);
        out
    }

    fn run_str(&mut self, argv: &[&str]) -> Vec<u8> {
        let owned: Vec<&[u8]> = argv.iter().map(|s| s.as_bytes()).collect();
        self.run(&owned)
    }

    /// Runs `argv` into a send buffer that already holds `earlier`, an
    /// earlier pipelined reply, and returns the command's own reply. The
    /// earlier bytes must survive: a reply's account and its rollback start
    /// at its own mark, never at the buffer's start.
    fn run_behind(&mut self, earlier: &[u8], argv: &[&str]) -> Vec<u8> {
        self.clock += 1;
        let owned: Vec<&[u8]> = argv.iter().map(|s| s.as_bytes()).collect();
        let mut out = earlier.to_vec();
        execute_slices(&owned, &mut self.ks, &mut self.cx, Nanos(self.clock), &mut out);
        assert!(out.starts_with(earlier), "{argv:?} changed an earlier pipelined reply");
        out.split_off(earlier.len())
    }
}

fn assert_reply(db: &mut Db, argv: &[&str], expected: &str) {
    let got = db.run_str(argv);
    assert_eq!(String::from_utf8_lossy(&got), expected, "reply mismatch for {argv:?}",);
}

fn bulk(text: &str) -> String {
    format!("${}\r\n{text}\r\n", text.len())
}

// ---- JSON.SET / JSON.GET -----------------------------------------------------

#[test]
fn set_root_and_get_shapes() {
    let mut db = Db::new();
    assert_reply(&mut db, &["JSON.SET", "k", "$", r#"{"a":1,"b":"x"}"#], "+OK\r\n");
    // Default path is legacy root: the value itself, unwrapped.
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":1,"b":"x"}"#));
    // `$` mode wraps the match set.
    assert_reply(&mut db, &["JSON.GET", "k", "$"], &bulk(r#"[{"a":1,"b":"x"}]"#));
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[1]"));
    assert_reply(&mut db, &["JSON.GET", "k", ".a"], &bulk("1"));
    // `$` mode with no matches answers the empty array, never an error.
    assert_reply(&mut db, &["JSON.GET", "k", "$.missing"], &bulk("[]"));
    // Legacy with no matches errors.
    assert_reply(
        &mut db,
        &["JSON.GET", "k", ".missing"],
        "-ERR Path '.missing' does not exist\r\n",
    );
    // Missing key is null in both modes.
    assert_reply(&mut db, &["JSON.GET", "nope"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.GET", "nope", "$"], "$-1\r\n");
}

#[test]
fn set_nx_xx_follow_path_existence() {
    let mut db = Db::new();
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}", "XX"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}", "NX"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}", "NX"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$.a", "1", "XX"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$.a", "1", "NX"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$.a", "2", "NX"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.SET", "k", "$.a", "2", "XX"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[2]"));
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}", "nope"], "-ERR syntax error\r\n");
}

#[test]
fn set_parent_creation_rules() {
    let mut db = Db::new();
    // Non-root path on a missing key.
    assert_reply(
        &mut db,
        &["JSON.SET", "k", "$.a.b", "1"],
        "-ERR new objects must be created at the root\r\n",
    );
    assert_reply(&mut db, &["JSON.SET", "k", "$", r#"{"a":{}}"#], "+OK\r\n");
    // Final child under an existing parent creates.
    assert_reply(&mut db, &["JSON.SET", "k", "$.a.b", "1"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":{"b":1}}"#));
    // Deep missing intermediate parents never create.
    assert_reply(
        &mut db,
        &["JSON.SET", "k", "$.x.y.z", "1"],
        "-ERR Path '$.x.y.z' does not exist\r\n",
    );
    // Multi-parent set with existing matches replaces those matches
    // only (ADR-0041 D6: creation is the zero-match arm — the parent
    // lacking `k` stays untouched when a sibling match exists).
    assert_reply(&mut db, &["JSON.SET", "m", "$", r#"{"a":{"k":0},"b":{"a":{}}}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.SET", "m", "$..a.k", "7"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "m"], &bulk(r#"{"a":{"k":7},"b":{"a":{}}}"#));
    // With no existing match, every eligible parent object gains the key.
    assert_reply(&mut db, &["JSON.SET", "m2", "$", r#"{"a":{},"b":{"a":{}}}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.SET", "m2", "$..a.k", "7"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "m2"], &bulk(r#"{"a":{"k":7},"b":{"a":{"k":7}}}"#));
}

#[test]
fn get_formatting_options_match_serde_pretty() {
    let mut db = Db::new();
    assert_reply(&mut db, &["JSON.SET", "k", "$", r#"{"a":[1,2],"b":"x"}"#], "+OK\r\n");
    let pretty = "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": \"x\"\n}";
    assert_reply(
        &mut db,
        &["JSON.GET", "k", "INDENT", "  ", "NEWLINE", "\n", "SPACE", " ", "."],
        &bulk(pretty),
    );
    // Multi-path replies key an object by the path strings as given;
    // the wrapper indents like any container.
    assert_reply(&mut db, &["JSON.GET", "k", "$.a", ".b"], &bulk(r#"{"$.a":[[1,2]],".b":"x"}"#));
    // A legacy member with no match fails the whole command.
    assert_reply(&mut db, &["JSON.GET", "k", "$.a", ".zz"], "-ERR Path '.zz' does not exist\r\n");
}

#[test]
fn mget_local_answers_per_key_elements() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "a", "$", r#"{"n":1}"#]);
    db.run_str(&["JSON.SET", "b", "$", r#"{"n":2}"#]);
    db.run_str(&["SET", "s", "plain"]);
    let expected = format!("*4\r\n{}{}$-1\r\n$-1\r\n", bulk("[1]"), bulk("[2]"));
    assert_reply(&mut db, &["JSON.MGET", "a", "b", "s", "nope", "$.n"], &expected);
    // Legacy path: first match, nil for keys where the path misses.
    let expected = format!("*2\r\n{}{}", bulk("1"), bulk("2"));
    assert_reply(&mut db, &["JSON.MGET", "a", "b", ".n"], &expected);
}

// ---- JSON.DEL / JSON.FORGET / JSON.TYPE ---------------------------------------

#[test]
fn del_root_and_paths() {
    let mut db = Db::new();
    assert_reply(&mut db, &["JSON.DEL", "nope"], ":0\r\n");
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":1,"b":[10,20]}"#]);
    assert_reply(&mut db, &["JSON.DEL", "k", "$.b[0]"], ":1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":1,"b":[20]}"#));
    assert_reply(&mut db, &["JSON.DEL", "k", "$.missing"], ":0\r\n");
    // FORGET aliases DEL; root deletion removes the key itself.
    assert_reply(&mut db, &["JSON.FORGET", "k"], ":1\r\n");
    assert_reply(&mut db, &["EXISTS", "k"], ":0\r\n");
    // Overlapping matches count against the pre-state set (§3.4 R5).
    db.run_str(&["JSON.SET", "o", "$", r#"{"a":{"a":1},"x":{"a":2}}"#]);
    assert_reply(&mut db, &["JSON.DEL", "o", "$..a"], ":3\r\n");
    assert_reply(&mut db, &["JSON.GET", "o"], &bulk(r#"{"x":{}}"#));
}

#[test]
fn type_names_and_shapes() {
    let mut db = Db::new();
    db.run_str(&[
        "JSON.SET",
        "k",
        "$",
        r#"{"i":1,"f":1.5,"s":"x","b":true,"n":null,"o":{},"a":[]}"#,
    ]);
    assert_reply(&mut db, &["JSON.TYPE", "k"], "$6\r\nobject\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "k", "$.i"], "*1\r\n$7\r\ninteger\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "k", "$.f"], "*1\r\n$6\r\nnumber\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "k", ".s"], "$6\r\nstring\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "k", "$.missing"], "*0\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "k", ".missing"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.TYPE", "nope"], "$-1\r\n");
    // The generic TYPE answers the S21 oracle-verified module type name.
    assert_reply(&mut db, &["TYPE", "k"], "+ReJSON-RL\r\n");
}

// ---- generic-command × JsonDoc interaction matrix (M3-S11) --------------------

#[test]
fn wrongtype_both_directions() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "doc", "$", r#"{"a":1}"#]);
    db.run_str(&["SET", "str", "v"]);
    const WRONGTYPE: &str =
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    // String commands against a document key.
    for cmd in [
        vec!["GET", "doc"],
        vec!["STRLEN", "doc"],
        vec!["GETRANGE", "doc", "0", "1"],
        vec!["SUBSTR", "doc", "0", "1"],
        vec!["APPEND", "doc", "x"],
        vec!["INCR", "doc"],
        vec!["INCRBYFLOAT", "doc", "1.5"],
        vec!["SETRANGE", "doc", "0", "x"],
        vec!["GETSET", "doc", "x"],
        vec!["GETDEL", "doc"],
        vec!["GETEX", "doc"],
    ] {
        let argv: Vec<&str> = cmd.clone();
        let got = db.run_str(&argv);
        assert_eq!(String::from_utf8_lossy(&got), WRONGTYPE, "for {cmd:?}");
    }
    // JSON commands against a string key.
    for cmd in [
        vec!["JSON.GET", "str"],
        vec!["JSON.SET", "str", "$", "1"],
        vec!["JSON.SET", "str", "$.a", "1"],
        vec!["JSON.DEL", "str"],
        vec!["JSON.DEL", "str", "$.a"],
        vec!["JSON.TYPE", "str"],
        vec!["JSON.NUMINCRBY", "str", "$.a", "1"],
        vec!["JSON.STRAPPEND", "str", "\"x\""],
        vec!["JSON.STRLEN", "str"],
        vec!["JSON.TOGGLE", "str", "$.a"],
        vec!["JSON.CLEAR", "str"],
    ] {
        let argv: Vec<&str> = cmd.clone();
        let got = db.run_str(&argv);
        assert_eq!(String::from_utf8_lossy(&got), WRONGTYPE, "for {cmd:?}");
    }
    // MGET answers nil for the document position, never an error.
    let expected = format!("*2\r\n{}$-1\r\n", bulk("v"));
    assert_reply(&mut db, &["MGET", "str", "doc"], &expected);
}

#[test]
fn generic_lifecycle_commands_treat_documents_as_records() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "doc", "$", r#"{"a":1}"#]);
    assert_reply(&mut db, &["EXISTS", "doc"], ":1\r\n");
    assert_reply(&mut db, &["EXPIRE", "doc", "100"], ":1\r\n");
    assert_reply(&mut db, &["PERSIST", "doc"], ":1\r\n");
    assert_reply(&mut db, &["RENAME", "doc", "doc2"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "doc2"], &bulk(r#"{"a":1}"#));
    assert_reply(&mut db, &["COPY", "doc2", "doc3"], ":1\r\n");
    assert_reply(&mut db, &["JSON.GET", "doc3"], &bulk(r#"{"a":1}"#));
    // Plain SET is a universal overwrite (ADR-0037 D6) — legal over docs.
    assert_reply(&mut db, &["SET", "doc2", "now-a-string"], "+OK\r\n");
    assert_reply(&mut db, &["GET", "doc2"], &bulk("now-a-string"));
    assert_reply(&mut db, &["DEL", "doc3"], ":1\r\n");
}

#[test]
fn debug_memory_reports_exact_attributed_bytes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "doc", "$", r#"{"pad":"xxxxxxxx"}"#]);
    let expected = db
        .ks
        .db_mut(0)
        .json_memory_usage(b"doc", Nanos(db.clock))
        .expect("document")
        .expect("present");
    assert_reply(&mut db, &["JSON.DEBUG", "MEMORY", "doc"], &format!(":{expected}\r\n"));
    assert_reply(&mut db, &["JSON.DEBUG", "MEMORY", "missing"], "$-1\r\n");
    db.run_str(&["SET", "plain", "value"]);
    assert_reply(
        &mut db,
        &["JSON.DEBUG", "MEMORY", "plain"],
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
    assert_reply(
        &mut db,
        &["JSON.DEBUG", "OTHER", "doc"],
        "-ERR unknown JSON.DEBUG subcommand\r\n",
    );
}

// ---- scalar mutations (M3-S12) ------------------------------------------------

#[test]
fn numincrby_shapes_and_semantics() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":1,"b":{"a":2.5},"s":{"a":"x"}}"#]);
    // `$` mode: JSON-text array with nulls for skipped matches.
    assert_reply(&mut db, &["JSON.NUMINCRBY", "k", "$..a", "1"], &bulk("[2,3.5,null]"));
    // Legacy: the last applied match's value.
    assert_reply(&mut db, &["JSON.NUMINCRBY", "k", "..a", "1"], &bulk("4.5"));
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[3]"));
    // Errors.
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "nope", "$.a", "1"],
        "-ERR could not perform this operation on a key that doesn't exist\r\n",
    );
    assert_reply(&mut db, &["JSON.NUMINCRBY", "k", "$.a", "x"], "-ERR value is not a number\r\n");
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "k", ".zz", "1"],
        "-ERR Path '.zz' does not exist\r\n",
    );
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "k", ".s.a", "1"],
        "-ERR Path '.s.a' does not contain a number\r\n",
    );
}

#[test]
fn numincrby_overflow_is_atomic() {
    let mut db = Db::new();
    let doc = format!(r#"[{},1]"#, i64::MAX);
    db.run_str(&["JSON.SET", "k", "$", &doc]);
    let before = db.run_str(&["JSON.GET", "k"]);
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "k", "$[*]", "1"],
        "-ERR arithmetic overflows a 64-bit integer\r\n",
    );
    // R4: the failed command mutated nothing.
    let after = db.run_str(&["JSON.GET", "k"]);
    assert_eq!(before, after);
}

#[test]
fn nummultby_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[3,4.0]}"#]);
    assert_reply(&mut db, &["JSON.NUMMULTBY", "k", "$.a[*]", "2"], &bulk("[6,8.0]"));
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":[6,8.0]}"#));
    assert_reply(
        &mut db,
        &["JSON.NUMMULTBY", "k", "$.a[1]", "1e308"],
        "-ERR result is not a number\r\n",
    );
}

#[test]
fn strappend_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"s":"hi","n":1}"#]);
    // `$` mode: RESP array, null for non-strings.
    assert_reply(&mut db, &["JSON.STRAPPEND", "k", "$.*", "\" there\""], "*2\r\n:8\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.s"], &bulk(r#"["hi there"]"#));
    // Legacy: last applied length.
    assert_reply(&mut db, &["JSON.STRAPPEND", "k", ".s", "\"!\""], ":9\r\n");
    // The implicit-root quirk: no path ⇒ legacy root.
    db.run_str(&["JSON.SET", "r", "$", "\"ab\""]);
    assert_reply(&mut db, &["JSON.STRAPPEND", "r", "\"c\""], ":3\r\n");
    // Operand must be a JSON string.
    assert_reply(&mut db, &["JSON.STRAPPEND", "k", ".s", "1"], "-ERR value is not a string\r\n");
    // Legacy on a non-string match.
    assert_reply(
        &mut db,
        &["JSON.STRAPPEND", "k", ".n", "\"x\""],
        "-ERR Path '.n' does not contain a string\r\n",
    );
}

#[test]
fn strlen_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"s":"hello","n":1}"#]);
    assert_reply(&mut db, &["JSON.STRLEN", "k", "$.*"], "*2\r\n:5\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.STRLEN", "k", ".s"], ":5\r\n");
    assert_reply(&mut db, &["JSON.STRLEN", "nope"], "$-1\r\n");
    assert_reply(
        &mut db,
        &["JSON.STRLEN", "k", ".n"],
        "-ERR Path '.n' does not contain a string\r\n",
    );
}

#[test]
fn toggle_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":true,"b":false,"n":1}"#]);
    assert_reply(&mut db, &["JSON.TOGGLE", "k", "$.*"], "*3\r\n:0\r\n:1\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":false,"b":true,"n":1}"#));
    // Legacy: the last applied match's new value as text.
    assert_reply(&mut db, &["JSON.TOGGLE", "k", ".a"], &bulk("true"));
    assert_reply(
        &mut db,
        &["JSON.TOGGLE", "k", ".n"],
        "-ERR Path '.n' does not contain a boolean\r\n",
    );
}

#[test]
fn clear_counts_and_defaults() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[],"b":[1],"c":0,"d":1.5,"e":"s"}"#]);
    // Already-clear values skip (uncounted, unwritten — ADR-0041 D8).
    assert_reply(&mut db, &["JSON.CLEAR", "k", "$.*"], ":2\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":[],"b":[],"c":0,"d":0,"e":"s"}"#));
    // Default path is the root: one container cleared.
    assert_reply(&mut db, &["JSON.CLEAR", "k"], ":1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk("{}"));
    assert_reply(
        &mut db,
        &["JSON.CLEAR", "nope"],
        "-ERR could not perform this operation on a key that doesn't exist\r\n",
    );
}

// ---- limits, guards, protocol variants ----------------------------------------

#[test]
fn configured_limits_carry_their_pinned_phrasing() {
    let mut db = Db::with_config(StoreConfig {
        doc_max_bytes: 64,
        doc_max_path_matches: 2,
        ..StoreConfig::default()
    });
    let big = format!(r#"{{"s":"{}"}}"#, "x".repeat(128));
    assert_reply(&mut db, &["JSON.SET", "k", "$", &big], "-ERR document too large\r\n");
    db.run_str(&["JSON.SET", "k", "$", r#"{"s":"xxx"}"#]);
    // Post-edit growth trips the same cap through the apply engine.
    let tail = format!("\"{}\"", "y".repeat(80));
    assert_reply(&mut db, &["JSON.STRAPPEND", "k", ".s", &tail], "-ERR document too large\r\n");
    // The match-set cap is a declared product limit (ADR-0040 D6).
    db.run_str(&["JSON.SET", "m", "$", r#"[1,2,3]"#]);
    assert_reply(&mut db, &["JSON.GET", "m", "$[*]"], "-ERR path matched too many values\r\n");
}

/// Asserts `reply` is exactly one well-formed RESP bulk string and
/// returns its payload length — the frame-validity claim of C9: whatever
/// the payload size, the length header must be right and the frame whole.
fn assert_single_bulk(reply: &[u8]) -> usize {
    assert_eq!(reply.first(), Some(&b'$'), "not a bulk reply: {:?}", &reply[..reply.len().min(64)]);
    let header_end = reply.windows(2).position(|w| w == b"\r\n").expect("bulk header CRLF");
    let len: usize = std::str::from_utf8(&reply[1..header_end])
        .expect("ASCII length digits")
        .parse()
        .expect("decimal length");
    assert_eq!(
        reply.len(),
        header_end + 2 + len + 2,
        "frame length disagrees with its header (header says {len})"
    );
    assert!(reply.ends_with(b"\r\n"), "unterminated bulk frame");
    len
}

/// C9 falsifier 1 (review 2026-08-30, F-L12-03): the exact L12 shape — a
/// sub-kilobyte document and a client-supplied `NEWLINE` separator push
/// the serialized reply past the writer's 8-digit header reserve.
/// Pre-fix this panicked (`debug_assert` here, wrapped `copy_within` →
/// cell panic → node `exit(101)` in release). The reply is legitimate
/// (under the doc-max-reply-bytes budget), so the fixed writer must
/// answer a single well-formed bulk frame.
#[test]
fn get_reply_crossing_the_header_reserve_is_a_well_formed_bulk() {
    let mut db = Db::new();
    let doc = format!("[{}]", (0..200).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    assert_reply(&mut db, &["JSON.SET", "d", "$", &doc], "+OK\r\n");
    let newline = "x".repeat(524_288);
    let reply = db.run_str(&["JSON.GET", "d", "NEWLINE", &newline, "$"]);
    let len = assert_single_bulk(&reply);
    // ≈ 202 separators × 512 KiB: 9 length digits, past the old reserve.
    assert!(len >= 100_000_000, "shape regressed below the reserve boundary: {len}");
}

/// C9 falsifier 2 (F-L15-01): multi-path repeat amplification — nine `$`
/// paths over a ~16 MB document is a ~145 MB serialized reply, over the
/// default doc-max-reply-bytes budget. Pre-fix this panicked mid-build;
/// the fix must refuse with the pinned error instead of building it.
#[test]
fn get_reply_over_the_budget_answers_a_typed_error() {
    let mut db = Db::new();
    let doc = format!(r#"{{"s":"{}"}}"#, "x".repeat(16_000_000));
    assert_reply(&mut db, &["JSON.SET", "d", "$", &doc], "+OK\r\n");
    let reply = db.run_str(&["JSON.GET", "d", "$", "$", "$", "$", "$", "$", "$", "$", "$"]);
    assert_eq!(String::from_utf8_lossy(&reply), "-ERR reply too large\r\n");
}

/// ADR-0099 D3/D4 at a lowered budget: all three amplification reaches
/// from F-L15-01 refuse with the pinned phrasing, an in-budget reply is
/// untouched, and the per-element commands answer the error as the
/// element (array header already committed — the EXEC precedent).
#[test]
fn reply_budget_refuses_every_amplification_reach() {
    let mut db =
        Db::with_config(StoreConfig { doc_max_reply_bytes: 64 << 10, ..StoreConfig::default() });
    // ~40 KiB document: under the budget once, over it twice.
    let doc = format!(r#"{{"s":"{}"}}"#, "x".repeat(40_000));
    assert_reply(&mut db, &["JSON.SET", "k", "$", &doc], "+OK\r\n");
    // In-budget single path serializes normally.
    let reply = db.run_str(&["JSON.GET", "k", "$"]);
    assert_single_bulk(&reply);
    // Reach 1 — multi-path repeat.
    assert_reply(&mut db, &["JSON.GET", "k", "$", "$"], "-ERR reply too large\r\n");
    // Reach 2 — `$..*` match amplification: every level of a 10-deep
    // nest re-serializes its whole subtree, ≈ 10 × the document, from a
    // document that serves a single path in-budget.
    let deep = format!("{}\"{}\"{}", "[".repeat(10), "d".repeat(40_000), "]".repeat(10));
    assert_reply(&mut db, &["JSON.SET", "deep", "$", &deep], "+OK\r\n");
    let reply = db.run_str(&["JSON.GET", "deep", "$"]);
    assert_single_bulk(&reply);
    assert_reply(&mut db, &["JSON.GET", "deep", "$..*"], "-ERR reply too large\r\n");
    // Reach 3 — client-supplied separator bytes.
    let newline = "n".repeat(30_000);
    db.run_str(&["JSON.SET", "a", "$", "[1,2,3]"]);
    assert_reply(&mut db, &["JSON.GET", "a", "NEWLINE", &newline, "$"], "-ERR reply too large\r\n");
    // MGET: the healthy key's element survives beside the refused one.
    let reply = db.run_str(&["JSON.MGET", "a", "deep", "$..*"]);
    let text = String::from_utf8_lossy(&reply);
    assert!(text.starts_with("*2\r\n$"), "healthy element first: {text:?}");
    assert!(text.ends_with("-ERR reply too large\r\n"), "refused element: {text:?}");
    // ARRPOP: a popped element over the budget refuses the whole reply,
    // which is built before the pop commits, so nothing is popped
    // (ADR-0099 A1).
    let big_elem = format!(r#"["{}"]"#, "y".repeat(70_000));
    db.run_str(&["JSON.SET", "p", "$", &big_elem]);
    assert_reply(&mut db, &["JSON.ARRPOP", "p", "$"], "-ERR reply too large\r\n");
    assert_reply(&mut db, &["JSON.ARRLEN", "p", "$"], "*1\r\n:1\r\n");
}

const REFUSED: &str = "-ERR reply too large\r\n";

/// A printable head of a reply or argument: the amplified replies below are
/// hundreds of kilobytes, and a mismatch message needs only their shape.
fn head(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(48)];
    format!("{} B {:?}", bytes.len(), String::from_utf8_lossy(shown))
}

/// `assert_reply` for large replies: a mismatch prints lengths and heads.
fn assert_reply_head(db: &mut Db, argv: &[&str], expected: &str) {
    let got = db.run_str(argv);
    let args: Vec<String> = argv.iter().map(|a| head(a.as_bytes())).collect();
    assert!(
        got == expected.as_bytes(),
        "reply mismatch for {args:?}: got {}, want {}",
        head(&got),
        head(expected.as_bytes())
    );
}

fn db_with_reply_budget(budget: usize) -> Db {
    Db::with_config(StoreConfig { doc_max_reply_bytes: budget, ..StoreConfig::default() })
}

/// `[[["y"×40000]]]`: `$[0][0]` is one array holding one 40,002-byte
/// serialized string. Its `ARRPOP` reply is 4 + (1 + 5 + 2 + 40,002 + 2)
/// = 40,016 B.
fn pop_fixture(db: &mut Db) -> String {
    let element = format!("\"{}\"", "y".repeat(40_000));
    assert_reply_head(db, &["JSON.SET", "p", "$", &format!("[[[{element}]]]")], "+OK\r\n");
    element
}

/// `[{k0000…k4999}]`: one key array is `*5000\r\n` + 5,000 × `$5\r\nkNNNN\r\n`
/// = 55,007 B.
fn keys_fixture(db: &mut Db) {
    let members: Vec<String> = (0..5_000).map(|i| format!("\"k{i:04}\":0")).collect();
    let doc = format!("[{{{}}}]", members.join(","));
    assert_reply_head(db, &["JSON.SET", "o", "$", &doc], "+OK\r\n");
}

/// ADR-0099 A1: `JSON.ARRPOP` builds its whole reply from the frozen
/// pre-image before the pop commits, under one account. Duplicate union
/// members give four raw matches of one 40 KB element: 4 + 4 × 40,012 =
/// 160,052 B against a 64 KiB budget. The command answers the error and
/// pops nothing; one raw match (40,016 B) is served and pops.
#[test]
fn arrpop_over_the_reply_budget_refuses_before_the_pop() {
    let mut db = db_with_reply_budget(64 << 10);
    let element = pop_fixture(&mut db);
    assert_reply_head(&mut db, &["JSON.ARRPOP", "p", "$[0,0][0,0]"], REFUSED);
    assert_reply_head(&mut db, &["JSON.ARRLEN", "p", "$[0][0]"], "*1\r\n:1\r\n");
    let served = format!("*1\r\n{}", bulk(&element));
    assert_reply_head(&mut db, &["JSON.ARRPOP", "p", "$[0][0]"], &served);
    assert_reply_head(&mut db, &["JSON.ARRLEN", "p", "$[0][0]"], "*1\r\n:0\r\n");
}

/// ADR-0099 A1: `JSON.OBJKEYS` writes every key of every raw match under
/// the reply account. Eight raw matches of 5,000 keys are 4 + 8 × 55,007 =
/// 440,060 B against a 64 KiB budget; one match (55,011 B) is served.
#[test]
fn objkeys_over_the_reply_budget_refuses() {
    let mut db = db_with_reply_budget(64 << 10);
    keys_fixture(&mut db);
    assert_reply_head(&mut db, &["JSON.OBJKEYS", "o", "$[0,0,0,0,0,0,0,0]"], REFUSED);
    let reply = db.run_str(&["JSON.OBJKEYS", "o", "$[0]"]);
    assert_eq!(reply.len(), 55_011, "one key array: {}", head(&reply));
    assert!(reply.starts_with(b"*1\r\n*5000\r\n$5\r\nk0000\r\n"), "{}", head(&reply));
}

/// ADR-0099 A1's charged predicate is exact on whole-reply wire bytes: a
/// reply is served at budget = its length and refused one byte below, for
/// keys (`*2` of two 55,007 B key arrays = 110,018 B) and for a document
/// bulk (`ARRPOP`'s 40,016 B), whose refusal leaves `ARRLEN` unchanged.
#[test]
fn reply_budget_boundary_is_exact_for_keys_and_documents() {
    for (budget, served) in [(110_018, true), (110_017, false)] {
        let mut db = db_with_reply_budget(budget);
        keys_fixture(&mut db);
        let reply = db.run_str(&["JSON.OBJKEYS", "o", "$[0,0]"]);
        if served {
            assert_eq!(reply.len(), 110_018, "served at {budget}: {}", head(&reply));
            assert!(reply.starts_with(b"*2\r\n*5000\r\n"), "{}", head(&reply));
        } else {
            assert!(reply == REFUSED.as_bytes(), "refused at {budget}: {}", head(&reply));
        }
    }
    for (budget, served) in [(40_016, true), (40_015, false)] {
        let mut db = db_with_reply_budget(budget);
        let element = pop_fixture(&mut db);
        let want = if served { format!("*1\r\n{}", bulk(&element)) } else { REFUSED.to_owned() };
        assert_reply_head(&mut db, &["JSON.ARRPOP", "p", "$[0][0]"], &want);
        let len = if served { "*1\r\n:0\r\n" } else { "*1\r\n:1\r\n" };
        assert_reply_head(&mut db, &["JSON.ARRLEN", "p", "$[0][0]"], len);
    }
}

#[test]
fn filter_rejection_names_the_plan() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", "[1]"]);
    assert_reply(
        &mut db,
        &["JSON.GET", "k", "$[?(@>1)]"],
        "-ERR filter expressions are not supported (planned for M4.5)\r\n",
    );
}

#[test]
fn durable_namespace_json_surface_is_reachable_after_s17() {
    let mut db = Db::new();
    let spec = NsSpec {
        id: inf_store::NsId(inf_store::FIRST_NAMED_NS_ID),
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: None,
    };
    db.ks.ns_create(spec).expect("namespace registers");
    db.cx.ns = inf_server::ConnNamespace::Named(inf_store::NsId(inf_store::FIRST_NAMED_NS_ID));
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk("{}"));
    // Memory-class connections are untouched.
    db.cx.ns = inf_server::ConnNamespace::Default;
    assert_reply(&mut db, &["JSON.SET", "k", "$", "{}"], "+OK\r\n");
}

#[test]
fn resp3_nulls_ride_the_protocol() {
    let mut db = Db::new();
    db.cx.proto = Protocol::Resp3;
    db.run_str(&["JSON.SET", "k", "$", r#"{"s":"x","n":1}"#]);
    assert_reply(&mut db, &["JSON.GET", "nope"], "_\r\n");
    assert_reply(&mut db, &["JSON.STRLEN", "k", "$.*"], "*2\r\n:1\r\n_\r\n");
}

#[test]
fn program_cache_serves_repeat_paths() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":1}"#]);
    let (h0, m0) = {
        let cache = db.cx.node.path_cache.borrow();
        (cache.hits(), cache.misses())
    };
    db.run_str(&["JSON.GET", "k", "$.a"]);
    db.run_str(&["JSON.GET", "k", "$.a"]);
    db.run_str(&["JSON.GET", "k", "$.a"]);
    let cache = db.cx.node.path_cache.borrow();
    assert_eq!(cache.misses(), m0 + 1, "first $.a lookup compiles");
    assert_eq!(cache.hits(), h0 + 2, "repeats hit");
    assert!(cache.bytes() > 0);
}

// ---- array ops (M3-S13, ADR-0042) ----------------------------------------------

#[test]
fn arrappend_shapes_and_the_three_argument_quirk() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[1],"n":1}"#]);
    // `$` mode: new length per match, null for non-arrays.
    assert_reply(&mut db, &["JSON.ARRAPPEND", "k", "$.*", "2", "3"], "*2\r\n:3\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[[1,2,3]]"));
    // Legacy: last applied match's new length.
    assert_reply(&mut db, &["JSON.ARRAPPEND", "k", ".a", "4"], ":4\r\n");
    // Three arguments ⇒ legacy root + one value (ADR-0042 D7).
    db.run_str(&["JSON.SET", "r", "$", "[1]"]);
    assert_reply(&mut db, &["JSON.ARRAPPEND", "r", "9"], ":2\r\n");
    assert_reply(&mut db, &["JSON.GET", "r"], &bulk("[1,9]"));
    // Legacy on a non-array match.
    assert_reply(
        &mut db,
        &["JSON.ARRAPPEND", "k", ".n", "1"],
        "-ERR Path '.n' does not contain an array\r\n",
    );
    assert_reply(
        &mut db,
        &["JSON.ARRAPPEND", "k", ".zz", "1"],
        "-ERR Path '.zz' does not exist\r\n",
    );
    assert_reply(
        &mut db,
        &["JSON.ARRAPPEND", "nope", "$.a", "1"],
        "-ERR could not perform this operation on a key that doesn't exist\r\n",
    );
    // Operands are full JSON values.
    assert_reply(
        &mut db,
        &["JSON.ARRAPPEND", "k", "$.a", "{bad"],
        "-ERR invalid JSON: unexpected character 'b' at offset 1\r\n",
    );
}

/// ADR-0169 D3: a 128-level `ARRAPPEND` element would nest its operand's
/// wrapper 129, which the delta decoder refuses, so the command answers the
/// nesting error while its operands parse — ahead of the missing-key error,
/// as RedisJSON orders its own parse refusal (ADR-0042 A1).
#[test]
fn arrappend_element_past_the_depth_bound_refuses_before_lookup() {
    let mut db = Db::new();
    let element = format!("{}0{}", "[".repeat(128), "]".repeat(128));
    assert_reply(
        &mut db,
        &["JSON.ARRAPPEND", "missing", "$", &element],
        "-ERR document nesting too deep\r\n",
    );
}

/// ADR-0169 D2/D5: a `STRAPPEND` that grows a body one byte past the
/// stored-document bound (16,777,192 body bytes) refuses in the apply plan
/// with the size text, and the document is unchanged.
#[test]
fn strappend_past_the_stored_document_bound_refuses() {
    const STORED_BODY_MAX: usize = (1 << 24) - 1 - 15 - 8;
    let mut db = Db::new();
    // `{"s":…}`: object header 4, key 2, str24 header 4.
    let big = format!(r#"{{"s":"{}"}}"#, "x".repeat(STORED_BODY_MAX - 10 - 99));
    assert_reply(&mut db, &["JSON.SET", "k", "$", &big], "+OK\r\n");
    let before = db.run_str(&["JSON.STRLEN", "k", "$.s"]);
    let tail = format!("\"{}\"", "y".repeat(100));
    assert_reply(&mut db, &["JSON.STRAPPEND", "k", "$.s", &tail], "-ERR document too large\r\n");
    assert_eq!(db.run_str(&["JSON.STRLEN", "k", "$.s"]), before, "the refusal changed nothing");
}

#[test]
fn arrinsert_shapes_and_atomic_out_of_bounds() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[1,2,3],"b":[1]}"#]);
    assert_reply(&mut db, &["JSON.ARRINSERT", "k", "$.a", "1", "9"], "*1\r\n:4\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[[1,9,2,3]]"));
    // Match 2 of 2 is out of bounds: the whole command aborts (§3.4 R4).
    let before = db.run_str(&["JSON.GET", "k"]);
    assert_reply(
        &mut db,
        &["JSON.ARRINSERT", "k", "$.*", "3", "0"],
        "-ERR index out of bounds\r\n",
    );
    let after = db.run_str(&["JSON.GET", "k"]);
    assert_eq!(before, after, "an aborted multi-match insert mutates nothing");
    assert_reply(
        &mut db,
        &["JSON.ARRINSERT", "k", "$.a", "x", "0"],
        "-ERR value is not an integer or out of range\r\n",
    );
}

#[test]
fn arrindex_ranges_and_scalar_needles() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[1,"x",true,null,1.0],"n":3}"#]);
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "\"x\""], "*1\r\n:1\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "true"], "*1\r\n:2\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "null"], "*1\r\n:3\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "9"], "*1\r\n:-1\r\n");
    // Mixed-width numbers compare numerically (`1.0` finds the fixint 1).
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "1.0"], "*1\r\n:0\r\n");
    // Range: [start, stop) with stop 0 = end; negatives resolve; clamped.
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "1", "1"], "*1\r\n:4\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "\"x\"", "0", "1"], "*1\r\n:-1\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "null", "-2", "0"], "*1\r\n:3\r\n");
    // Non-arrays answer null per match in `$` mode; error in legacy.
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.n", "1"], "*1\r\n$-1\r\n");
    assert_reply(
        &mut db,
        &["JSON.ARRINDEX", "k", ".n", "1"],
        "-ERR Path '.n' does not contain an array\r\n",
    );
    // Container needles are rejected (ADR-0042 D3).
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "[1]"], "-ERR value is not a scalar\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "k", "$.a", "{}"], "-ERR value is not a scalar\r\n");
    assert_reply(&mut db, &["JSON.ARRINDEX", "nope", "$.a", "1"], "$-1\r\n");
}

#[test]
fn arrlen_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[1,2,3],"n":1}"#]);
    assert_reply(&mut db, &["JSON.ARRLEN", "k", "$.*"], "*2\r\n:3\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.ARRLEN", "k", ".a"], ":3\r\n");
    assert_reply(&mut db, &["JSON.ARRLEN", "nope"], "$-1\r\n");
    assert_reply(
        &mut db,
        &["JSON.ARRLEN", "k", ".n"],
        "-ERR Path '.n' does not contain an array\r\n",
    );
    assert_reply(&mut db, &["JSON.ARRLEN", "k", ".zz"], "-ERR Path '.zz' does not exist\r\n");
}

#[test]
fn arrpop_shapes_serialize_popped_elements() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[1,{"x":2},3],"e":[],"n":1}"#]);
    // `$` mode: popped element per match; null for empty arrays and
    // non-arrays alike.
    let expected = format!("*3\r\n{}$-1\r\n$-1\r\n", bulk("3"));
    assert_reply(&mut db, &["JSON.ARRPOP", "k", "$.*"], &expected);
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk(r#"[[1,{"x":2}]]"#));
    // Explicit index; structured elements serialize as JSON text.
    assert_reply(&mut db, &["JSON.ARRPOP", "k", "$.a", "0"], &format!("*1\r\n{}", bulk("1")));
    // Legacy: the last array match's popped element.
    assert_reply(&mut db, &["JSON.ARRPOP", "k", ".a"], &bulk(r#"{"x":2}"#));
    // Legacy on an empty array answers null, not an error.
    assert_reply(&mut db, &["JSON.ARRPOP", "k", ".e"], "$-1\r\n");
    assert_reply(
        &mut db,
        &["JSON.ARRPOP", "k", ".n"],
        "-ERR Path '.n' does not contain an array\r\n",
    );
    assert_reply(
        &mut db,
        &["JSON.ARRPOP", "nope"],
        "-ERR could not perform this operation on a key that doesn't exist\r\n",
    );
}

#[test]
fn arrtrim_clamps_and_reports_lengths() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"a":[0,1,2,3,4],"n":1}"#]);
    assert_reply(&mut db, &["JSON.ARRTRIM", "k", "$.*", "1", "3"], "*2\r\n:3\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[[1,2,3]]"));
    // Out-of-range never errors: start past the end empties.
    assert_reply(&mut db, &["JSON.ARRTRIM", "k", "$.a", "9", "9"], "*1\r\n:0\r\n");
    assert_reply(&mut db, &["JSON.GET", "k", "$.a"], &bulk("[[]]"));
    assert_reply(
        &mut db,
        &["JSON.ARRTRIM", "k", ".n", "0", "1"],
        "-ERR Path '.n' does not contain an array\r\n",
    );
}

// ---- object ops + MERGE (M3-S14, ADR-0042) --------------------------------------

#[test]
fn objkeys_and_objlen_shapes() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"o":{"b":1,"a":2},"n":1}"#]);
    // Keys in insertion order — never sorted (ADR-0036).
    assert_reply(&mut db, &["JSON.OBJKEYS", "k", "$.o"], "*1\r\n*2\r\n$1\r\nb\r\n$1\r\na\r\n");
    assert_reply(
        &mut db,
        &["JSON.OBJKEYS", "k", "$.*"],
        "*2\r\n*2\r\n$1\r\nb\r\n$1\r\na\r\n$-1\r\n",
    );
    assert_reply(&mut db, &["JSON.OBJKEYS", "k", ".o"], "*2\r\n$1\r\nb\r\n$1\r\na\r\n");
    assert_reply(&mut db, &["JSON.OBJKEYS", "k"], "*2\r\n$1\r\no\r\n$1\r\nn\r\n");
    assert_reply(
        &mut db,
        &["JSON.OBJKEYS", "k", ".n"],
        "-ERR Path '.n' does not contain an object\r\n",
    );
    assert_reply(&mut db, &["JSON.OBJKEYS", "nope"], "$-1\r\n");
    assert_reply(&mut db, &["JSON.OBJLEN", "k", "$.*"], "*2\r\n:2\r\n$-1\r\n");
    assert_reply(&mut db, &["JSON.OBJLEN", "k", ".o"], ":2\r\n");
    assert_reply(&mut db, &["JSON.OBJLEN", "nope"], "$-1\r\n");
    assert_reply(
        &mut db,
        &["JSON.OBJLEN", "k", ".n"],
        "-ERR Path '.n' does not contain an object\r\n",
    );
}

#[test]
fn merge_shapes_create_update_and_delete() {
    let mut db = Db::new();
    // Missing key + root path creates (nulls stripped through object
    // chains — MergePatch against absent, ADR-0042 D6).
    assert_reply(&mut db, &["JSON.MERGE", "k", "$", r#"{"a":1,"gone":null}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":1}"#));
    // Recursive merge: replace, append, delete-by-null.
    assert_reply(&mut db, &["JSON.MERGE", "k", "$", r#"{"a":{"x":1},"b":2}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":{"x":1},"b":2}"#));
    assert_reply(&mut db, &["JSON.MERGE", "k", "$.a", r#"{"y":2,"x":null}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":{"y":2},"b":2}"#));
    // A path-targeted null is a literal replacement; only null members
    // inside an object patch delete members (RFC 7386, S21 oracle pin).
    assert_reply(&mut db, &["JSON.MERGE", "k", "$.b", "null"], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":{"y":2},"b":null}"#));
    // No matches + final child name: the SET parent-creation rule.
    assert_reply(&mut db, &["JSON.MERGE", "k", "$.c", r#"{"n":null,"m":3}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.GET", "k"], &bulk(r#"{"a":{"y":2},"b":null,"c":{"m":3}}"#));
    // Missing key + non-root path: the root-creation error.
    assert_reply(
        &mut db,
        &["JSON.MERGE", "nope", "$.a", "1"],
        "-ERR new objects must be created at the root\r\n",
    );
    // Deep missing parents never create.
    assert_reply(
        &mut db,
        &["JSON.MERGE", "k", "$.x.y.z", "1"],
        "-ERR Path '$.x.y.z' does not exist\r\n",
    );
    // A no-op merge is still +OK.
    assert_reply(&mut db, &["JSON.MERGE", "k", "$", "{}"], "+OK\r\n");
    // WRONGTYPE against a string key, both path shapes.
    db.run_str(&["SET", "str", "v"]);
    const WRONGTYPE: &str =
        "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    let got = db.run_str(&["JSON.MERGE", "str", "$", "{}"]);
    assert_eq!(String::from_utf8_lossy(&got), WRONGTYPE);
    let got = db.run_str(&["JSON.MERGE", "str", "$.a", "{}"]);
    assert_eq!(String::from_utf8_lossy(&got), WRONGTYPE);
}

#[test]
fn strlen_legacy_missing_path_answers_the_path_error() {
    // Regression: the legacy zero-match arm previously projected match 0
    // before checking emptiness — a reachable panic, fixed in the shared
    // `int_read` skeleton.
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "k", "$", r#"{"s":"x"}"#]);
    assert_reply(&mut db, &["JSON.STRLEN", "k", ".zz"], "-ERR Path '.zz' does not exist\r\n");
}

#[test]
fn new_write_commands_run_in_durable_namespaces_after_s17() {
    let mut db = Db::new();
    let spec = NsSpec {
        id: inf_store::NsId(inf_store::FIRST_NAMED_NS_ID),
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: None,
    };
    db.ks.ns_create(spec).expect("namespace registers");
    db.cx.ns = inf_server::ConnNamespace::Named(inf_store::NsId(inf_store::FIRST_NAMED_NS_ID));
    assert_reply(&mut db, &["JSON.MERGE", "k", "$", r#"{"a":[1]}"#], "+OK\r\n");
    assert_reply(&mut db, &["JSON.ARRAPPEND", "k", "$.a", "2"], "*1\r\n:2\r\n");
    assert_reply(&mut db, &["JSON.ARRPOP", "k", "$.a"], "*1\r\n$1\r\n2\r\n");
    assert_reply(&mut db, &["JSON.ARRLEN", "k", "$.a"], "*1\r\n:1\r\n");
}

// ---- RESP2 × RESP3 reply corpus (M3-S15, ADR-0042 D9) ---------------------------

/// One ordered corpus script covering every `JSON.*` command in both
/// path modes, including the null/skip arms. Executed twice — RESP2 and
/// RESP3. Its expectation starts with protocol-level null substitution,
/// then applies the five RedisJSON-native TYPE/number frames discovered by
/// the pinned S21 oracle. A future shape divergence fails here loudly.
fn reply_corpus() -> Vec<(Vec<&'static str>, String)> {
    let doc = r#"{"a":[1,2],"s":"x","n":5,"b":true,"o":{"k":1},"e":[],"f":1.5,"nl":null}"#;
    vec![
        (vec!["JSON.DEBUG", "MEMORY", "nope"], "$-1\r\n".into()),
        (vec!["JSON.SET", "k", "$", doc], "+OK\r\n".into()),
        (vec!["JSON.SET", "k", "$.a", "[1,2]", "XX"], "+OK\r\n".into()),
        (vec!["JSON.SET", "k", "$.zz", "1", "XX"], "$-1\r\n".into()),
        (vec!["JSON.GET", "k", "$.a"], bulk("[[1,2]]")),
        (vec!["JSON.GET", "k", ".a"], bulk("[1,2]")),
        (vec!["JSON.GET", "nope"], "$-1\r\n".into()),
        (vec!["JSON.MGET", "k", "nope", "$.s"], format!("*2\r\n{}$-1\r\n", bulk(r#"["x"]"#))),
        (vec!["JSON.TYPE", "k", "$.n"], "*1\r\n$7\r\ninteger\r\n".into()),
        (vec!["JSON.TYPE", "k", ".missing"], "$-1\r\n".into()),
        (vec!["JSON.NUMINCRBY", "k", "$.n", "1"], bulk("[6]")),
        (vec!["JSON.NUMMULTBY", "k", "$.n", "2"], bulk("[12]")),
        (vec!["JSON.NUMINCRBY", "k", ".n", "0"], bulk("12")),
        (vec!["JSON.STRAPPEND", "k", "$.s", "\"y\""], "*1\r\n:2\r\n".into()),
        (
            vec!["JSON.STRAPPEND", "k", "$.*", "\"!\""],
            "*8\r\n$-1\r\n:3\r\n$-1\r\n$-1\r\n$-1\r\n$-1\
             \r\n$-1\r\n$-1\r\n"
                .into(),
        ),
        (vec!["JSON.STRLEN", "k", "$.s"], "*1\r\n:3\r\n".into()),
        (vec!["JSON.STRLEN", "k", "$.n"], "*1\r\n$-1\r\n".into()),
        (vec!["JSON.STRLEN", "k", ".s"], ":3\r\n".into()),
        (vec!["JSON.TOGGLE", "k", "$.b"], "*1\r\n:0\r\n".into()),
        (vec!["JSON.TOGGLE", "k", ".b"], bulk("true")),
        (vec!["JSON.CLEAR", "k", "$.o"], ":1\r\n".into()),
        (vec!["JSON.ARRAPPEND", "k", "$.a", "3"], "*1\r\n:3\r\n".into()),
        (
            vec!["JSON.ARRAPPEND", "k", "$.*", "9"],
            "*8\r\n:4\r\n$-1\r\n$-1\r\n$-1\r\n$-1\r\n:1\r\n$\
             -1\r\n$-1\r\n"
                .into(),
        ),
        (vec!["JSON.ARRINSERT", "k", "$.a", "0", "0"], "*1\r\n:5\r\n".into()),
        (vec!["JSON.ARRINSERT", "k", ".a", "0", "-1"], ":6\r\n".into()),
        (vec!["JSON.ARRINDEX", "k", "$.a", "2"], "*1\r\n:3\r\n".into()),
        (vec!["JSON.ARRINDEX", "k", "$.s", "2"], "*1\r\n$-1\r\n".into()),
        (vec!["JSON.ARRINDEX", "k", ".a", "2", "0", "0"], ":3\r\n".into()),
        (vec!["JSON.ARRLEN", "k", "$.a"], "*1\r\n:6\r\n".into()),
        (vec!["JSON.ARRLEN", "k", "$.n"], "*1\r\n$-1\r\n".into()),
        (vec!["JSON.ARRLEN", "k", ".a"], ":6\r\n".into()),
        (vec!["JSON.ARRPOP", "k", "$.a"], format!("*1\r\n{}", bulk("9"))),
        (vec!["JSON.ARRPOP", "k", "$.n"], "*1\r\n$-1\r\n".into()),
        (vec!["JSON.ARRPOP", "k", ".a", "0"], bulk("-1")),
        (vec!["JSON.ARRTRIM", "k", "$.a", "0", "1"], "*1\r\n:2\r\n".into()),
        (vec!["JSON.ARRTRIM", "k", ".a", "0", "0"], ":1\r\n".into()),
        (vec!["JSON.OBJKEYS", "k", "$.o"], "*1\r\n*0\r\n".into()),
        (
            vec!["JSON.OBJKEYS", "k"],
            "*8\r\n$1\r\na\r\n$1\r\ns\r\n$1\r\nn\r\n$1\r\nb\r\n$1\r\no\r\n\
             $1\r\ne\r\n$1\r\nf\r\n$2\r\nnl\r\n"
                .into(),
        ),
        (vec!["JSON.OBJLEN", "k", "$.o"], "*1\r\n:0\r\n".into()),
        (vec!["JSON.OBJLEN", "k", ".o"], ":0\r\n".into()),
        (vec!["JSON.MERGE", "k", "$.o", r#"{"z":1}"#], "+OK\r\n".into()),
        (vec!["JSON.OBJLEN", "k", ".o"], ":1\r\n".into()),
        (vec!["JSON.DEL", "k", "$.nl"], ":1\r\n".into()),
        (vec!["JSON.FORGET", "k", "$.f"], ":1\r\n".into()),
        (vec!["JSON.DEL", "k"], ":1\r\n".into()),
        (vec!["JSON.GET", "k"], "$-1\r\n".into()),
    ]
}

#[test]
fn reply_corpus_is_pinned_under_resp2() {
    let mut db = Db::new();
    for (argv, expected) in reply_corpus() {
        let got = db.run_str(&argv);
        assert_eq!(String::from_utf8_lossy(&got), expected, "RESP2 mismatch for {argv:?}");
    }
}

#[test]
fn reply_corpus_is_pinned_under_resp3_with_protocol_nulls() {
    let mut db = Db::new();
    db.cx.proto = Protocol::Resp3;
    for (argv, expected) in reply_corpus() {
        let expected = match argv.as_slice() {
            ["JSON.TYPE", "k", "$.n"] => "*1\r\n*1\r\n$7\r\ninteger\r\n".into(),
            ["JSON.TYPE", "k", ".missing"] => "*1\r\n_\r\n".into(),
            ["JSON.NUMINCRBY", "k", "$.n", "1"] => "*1\r\n:6\r\n".into(),
            ["JSON.NUMMULTBY", "k", "$.n", "2"] => "*1\r\n:12\r\n".into(),
            ["JSON.NUMINCRBY", "k", ".n", "0"] => "*1\r\n:12\r\n".into(),
            _ => expected.replace("$-1\r\n", "_\r\n"),
        };
        let got = db.run_str(&argv);
        assert_eq!(String::from_utf8_lossy(&got), expected, "RESP3 mismatch for {argv:?}");
    }
}

// ---- batch 12 of the 2026-08-30 review: RESP3 doubles are total -------------

/// Before the fix `RespWriter::double` formatted through a 40-byte stack
/// buffer with `expect("f64 display fits 40 bytes")`; a document number of
/// magnitude ≥ 1e40 (or below 1e-40) reached it through every numeric
/// reply on a RESP3 connection and killed the cell. Rust's `Display`
/// prints `1e300` as 301 digits; the reply now carries them.
/// Review C10: a path index beyond the `u32` width used to wrap onto a
/// real element — `JSON.GET` answered element 0 for `[4294967296]`, and
/// `JSON.SET` / `JSON.DEL` overwrote or deleted it with a success reply,
/// while `[99]` was correctly refused. Every command must now treat the
/// index as the out-of-range one it is, on every evaluation lane.
#[test]
fn path_index_beyond_the_u32_width_selects_nothing_on_every_command() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "d", "$", r#"{"a":[10,20,30,40,50],"b":[true]}"#]);
    let intact = bulk("[[10,20,30,40,50]]");
    // Read lane, both modes, simple and general programs.
    for index in ["4294967296", "4294967297", "8589934592", "4294967295", "9223372036854775807"] {
        let dollar = format!("$.a[{index}]");
        assert_reply(&mut db, &["JSON.GET", "d", &dollar], &bulk("[]"));
        let legacy = format!(".a[{index}]");
        assert_reply(
            &mut db,
            &["JSON.GET", "d", &legacy],
            &format!("-ERR Path '{legacy}' does not exist\r\n"),
        );
        let union = format!("$.a[{index},1]");
        assert_reply(&mut db, &["JSON.GET", "d", &union], &bulk("[20]"));
        let descend = format!("$..[{index}]");
        assert_reply(&mut db, &["JSON.GET", "d", &descend], &bulk("[]"));
    }
    // Controls: the in-range neighbours and an ordinary out-of-range index.
    assert_reply(&mut db, &["JSON.GET", "d", "$.a[4]"], &bulk("[50]"));
    assert_reply(&mut db, &["JSON.GET", "d", "$.a[5]"], &bulk("[]"));
    assert_reply(&mut db, &["JSON.GET", "d", "$.a[99]"], &bulk("[]"));
    // Mutation lane: the review's exact rows.
    assert_reply(
        &mut db,
        &["JSON.SET", "d", "$.a[4294967296]", "999"],
        "-ERR Path '$.a[4294967296]' does not exist\r\n",
    );
    assert_reply(&mut db, &["JSON.GET", "d", "$.a"], &intact);
    assert_reply(&mut db, &["JSON.DEL", "d", "$.a[4294967296]"], ":0\r\n");
    assert_reply(&mut db, &["JSON.GET", "d", "$.a"], &intact);
    // The numeric op on both of its lanes: the in-place probe (simple
    // path, same-width result) and the canonical apply lane (a union is
    // never simple), plus the width-changing operand on the simple path.
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", "$.a[4294967296]", "1"], &bulk("[]"));
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", "$.a[4294967296]", "1000"], &bulk("[]"));
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "d", "$.a[4294967296,4294967296]", "1000"],
        &bulk("[]"),
    );
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "d", ".a[4294967296]", "1"],
        "-ERR Path '.a[4294967296]' does not exist\r\n",
    );
    assert_reply(&mut db, &["JSON.TOGGLE", "d", "$.b[4294967296]"], "*0\r\n");
    assert_reply(&mut db, &["JSON.GET", "d"], &bulk(r#"{"a":[10,20,30,40,50],"b":[true]}"#));
}

/// Review C11: a slice step near `i64::MAX` overflowed the cursor and
/// killed the cell on one `JSON.GET`. Every step magnitude is a bounded
/// walk now, on the read lane and through the mutation planner.
#[test]
fn giant_slice_steps_answer_bounded_selections() {
    let mut db = Db::new();
    db.run_str(&["JSON.SET", "d", "$", r#"{"a":[10,20,30,40,50]}"#]);
    let cases = [
        ("$.a[0:5:1]", "[10,20,30,40,50]"),
        ("$.a[1::4294967296]", "[20]"),
        ("$.a[1::9223372036854775807]", "[20]"),
        ("$.a[2:100:9223372036854775806]", "[30]"),
        ("$.a[-1::9223372036854775807]", "[50]"),
        ("$.a[::-9223372036854775808]", "[50]"),
        ("$.a[3:0:-9223372036854775808]", "[40]"),
        ("$.a[-9223372036854775808:9223372036854775807]", "[10,20,30,40,50]"),
        ("$.a[9223372036854775807:-9223372036854775808:-1]", "[50,40,30,20,10]"),
    ];
    for (path, want) in cases {
        assert_reply(&mut db, &["JSON.GET", "d", path], &bulk(want));
    }
    assert_reply(
        &mut db,
        &["JSON.NUMINCRBY", "d", "$.a[1::9223372036854775807]", "1"],
        &bulk("[21]"),
    );
    assert_reply(&mut db, &["JSON.DEL", "d", "$.a[-1::9223372036854775807]"], ":1\r\n");
    assert_reply(&mut db, &["JSON.GET", "d", "$.a"], &bulk("[[10,21,30,40]]"));
}

#[test]
fn resp3_numeric_replies_survive_extreme_doubles() {
    let mut db = Db::new();
    db.cx.proto = Protocol::Resp3;
    db.run_str(&[
        "JSON.SET",
        "d",
        "$",
        r#"{"big":1e300,"tiny":5e-324,"neg":-2.2250738585072014e-308}"#,
    ]);
    let big = format!("{}", 1e300);
    assert_eq!(big.len(), 301);
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", "$.big", "0"], &format!("*1\r\n,{big}\r\n"));
    let tiny = format!("{}", 5e-324);
    assert_reply(&mut db, &["JSON.NUMMULTBY", "d", "$.tiny", "1"], &format!("*1\r\n,{tiny}\r\n"));
    let neg = format!("{}", -2.2250738585072014e-308);
    assert_eq!(neg.len(), 327, "the longest f64 Display");
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", "$.neg", "0"], &format!("*1\r\n,{neg}\r\n"));
    // Legacy (non-`$`) path replies ride the same writer.
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", ".big", "0"], &format!("*1\r\n,{big}\r\n"));
    // RESP2 stays the JSON number text (`serialize_number_text`, bounded,
    // exponent form with Redis's sign).
    db.cx.proto = Protocol::Resp2;
    assert_reply(&mut db, &["JSON.NUMINCRBY", "d", "$.big", "0"], "$8\r\n[1e+300]\r\n");
}

// ---- ADR-0099 A1: every JSON reply is charged, refused before its effect -------

use inf_server::limits::FixedShape;

/// How a fixture's reply settles (ADR-0099 A1's settlement table).
#[derive(Copy, Clone, Debug)]
enum Settlement {
    /// Exact: the final reply's bytes > budget.
    Charged,
    /// budget < M(shape): the reservation precedes the effect.
    Fixed(FixedShape),
    /// The general path behind a fixed reservation: budget < M(shape) or
    /// the final reply's bytes > budget.
    FixedThenCharged(FixedShape),
    /// `JSON.MGET`: each element against its own account.
    PerElement,
}

/// The fixture's arm, and the witness that proves the arm ran.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Arm {
    /// No arm choice beyond the path mode.
    Plain,
    /// The in-place scalar lane: `json_scalar_patches_in_place` moves by
    /// one on a served leg.
    Fast,
    /// The general path of a scalar command: the witness does not move.
    General,
    /// A root `SET`/`MERGE`/`DEL`/`FORGET`: the key's existence flips.
    Root,
    /// A path `SET`/`MERGE`/`DEL`/`FORGET`: the key stays present.
    Path,
}

struct Fixture {
    row: &'static str,
    arm: Arm,
    settlement: Settlement,
    /// `Some(true)` for a `$` path, `Some(false)` for a legacy one; the
    /// compiler selects `$` mode by a leading `$`, so the path text at
    /// `path_at` must agree.
    dollar: Option<bool>,
    path_at: usize,
    /// Documents written through the store API, never the reply path.
    docs: &'static [(&'static str, &'static str)],
    argv: &'static [&'static str],
    resp2: &'static str,
    resp3: &'static str,
}

const D: &str = r#"{"a":1,"s":"x","f":true,"arr":[1,2],"o":{"x":0}}"#;

const fn fx(
    row: &'static str,
    arm: Arm,
    settlement: Settlement,
    docs: &'static [(&'static str, &'static str)],
    argv: &'static [&'static str],
    resp2: &'static str,
    resp3: &'static str,
) -> Fixture {
    Fixture { row, arm, settlement, dollar: None, path_at: 2, docs, argv, resp2, resp3 }
}

const fn dollar(mut f: Fixture, at: usize, dollar: bool) -> Fixture {
    f.dollar = Some(dollar);
    f.path_at = at;
    f
}

/// Matches of the RESP3 double fixture: enough 7 B `,0.75\r\n` frames that
/// the general path's reply, not the fixed floor, sets arm (c)'s boundary.
const FLOAT_LEAVES: usize = 80;

/// `[{"x":0.5}, …]` with [`FLOAT_LEAVES`] members and its `$..x` += 0.25
/// replies, built from the reply-shape contract: RESP2 one bulk of
/// `[0.75,…]` (409 B), RESP3 `*80` and a native double per match (565 B).
fn float_leaves() -> (&'static [(&'static str, &'static str)], &'static str, &'static str) {
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let doc = format!("[{}]", vec![r#"{"x":0.5}"#; FLOAT_LEAVES].join(","));
    let text = format!("[{}]", vec!["0.75"; FLOAT_LEAVES].join(","));
    let resp2 = format!("${}\r\n{text}\r\n", text.len());
    let resp3 = format!("*{FLOAT_LEAVES}\r\n{}", ",0.75\r\n".repeat(FLOAT_LEAVES));
    let floor = inf_server::limits::JSON_FIXED_REPLY_BYTES_MAX;
    assert!(resp2.len() > floor, "the RESP2 reply passes the fixed floor");
    assert!(resp3.len() > floor, "the RESP3 reply passes the fixed floor");
    let docs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([("m", leak(doc))]));
    (docs, leak(resp2), leak(resp3))
}

/// One or more fixtures per registry row. The pinned bytes are written by
/// hand from the reply-shape contract, not produced by the server.
fn reply_fixtures() -> Vec<Fixture> {
    use Arm::{Fast, General, Path, Plain, Root};
    use FixedShape::{Count, Number, Status, Toggle};
    use Settlement::{Charged, Fixed, FixedThenCharged, PerElement};
    let ok = "+OK\r\n";
    let (float_docs, float_resp2, float_resp3) = float_leaves();
    vec![
        dollar(
            fx("JSON.SET", Root, Fixed(Status), &[], &["JSON.SET", "k", "$", "[1]"], ok, ok),
            2,
            true,
        ),
        dollar(
            fx("JSON.SET", Path, Charged, &[("k", D)], &["JSON.SET", "k", "$.a", "2"], ok, ok),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.GET",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.GET", "k", "$.a"],
                "$3\r\n[1]\r\n",
                "$3\r\n[1]\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.GET",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.GET", "k", ".s"],
                "$3\r\n\"x\"\r\n",
                "$3\r\n\"x\"\r\n",
            ),
            2,
            false,
        ),
        dollar(
            fx(
                "JSON.GET",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.GET", "k", "$.a", ".s"],
                "$20\r\n{\"$.a\":[1],\".s\":\"x\"}\r\n",
                "$20\r\n{\"$.a\":[1],\".s\":\"x\"}\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.MGET",
                Plain,
                PerElement,
                &[("k1", r#"{"a":1}"#), ("k2", r#"{"a":22}"#)],
                &["JSON.MGET", "k1", "k2", "$.a"],
                "*2\r\n$3\r\n[1]\r\n$4\r\n[22]\r\n",
                "*2\r\n$3\r\n[1]\r\n$4\r\n[22]\r\n",
            ),
            3,
            true,
        ),
        dollar(
            fx(
                "JSON.DEL",
                Root,
                Fixed(Count),
                &[("k", D)],
                &["JSON.DEL", "k", "$"],
                ":1\r\n",
                ":1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.DEL",
                Path,
                Charged,
                &[("k", D)],
                &["JSON.DEL", "k", "$.a"],
                ":1\r\n",
                ":1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.FORGET",
                Root,
                Fixed(Count),
                &[("k", D)],
                &["JSON.FORGET", "k", "."],
                ":1\r\n",
                ":1\r\n",
            ),
            2,
            false,
        ),
        dollar(
            fx(
                "JSON.FORGET",
                Path,
                Charged,
                &[("k", D)],
                &["JSON.FORGET", "k", ".a"],
                ":1\r\n",
                ":1\r\n",
            ),
            2,
            false,
        ),
        // The uncharged-bulk canary's red: this whole RESP2 reply is one bulk.
        dollar(
            fx(
                "JSON.TYPE",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.TYPE", "k", ".a"],
                "$7\r\ninteger\r\n",
                "*1\r\n$7\r\ninteger\r\n",
            ),
            2,
            false,
        ),
        dollar(
            fx(
                "JSON.TYPE",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.TYPE", "k", "$.s"],
                "*1\r\n$6\r\nstring\r\n",
                "*1\r\n*1\r\n$6\r\nstring\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.NUMINCRBY",
                Fast,
                Fixed(Number),
                &[("k", D)],
                &["JSON.NUMINCRBY", "k", "$.a", "1"],
                "$3\r\n[2]\r\n",
                "*1\r\n:2\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.NUMINCRBY",
                Fast,
                Fixed(Number),
                &[("k", D)],
                &["JSON.NUMINCRBY", "k", ".a", "2"],
                "$1\r\n3\r\n",
                "*1\r\n:3\r\n",
            ),
            2,
            false,
        ),
        // The tape stores -32..=127 as a one-byte fixint and 128 as a tag
        // and a two-byte varint, so 127 + 1 changes the encoded width: the
        // in-place probe answers `Unsupported` and the general path serves
        // 11 B under the 348 B floor.
        dollar(
            fx(
                "JSON.NUMINCRBY",
                General,
                FixedThenCharged(Number),
                &[("g", r#"{"a":127}"#)],
                &["JSON.NUMINCRBY", "g", "$.a", "1"],
                "$5\r\n[128]\r\n",
                "*1\r\n:128\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.NUMMULTBY",
                Fast,
                Fixed(Number),
                &[("k", D)],
                &["JSON.NUMMULTBY", "k", "$.a", "3"],
                "$3\r\n[3]\r\n",
                "*1\r\n:3\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.NUMMULTBY",
                General,
                FixedThenCharged(Number),
                &[("g", r#"{"a":127}"#)],
                &["JSON.NUMMULTBY", "g", "$.a", "2"],
                "$5\r\n[254]\r\n",
                "*1\r\n:254\r\n",
            ),
            2,
            true,
        ),
        // RESP3 answers each match as a native double. Over the 348 B
        // floor, so arm (c) is decided by the charged `,0.75\r\n` frames.
        dollar(
            fx(
                "JSON.NUMINCRBY",
                General,
                FixedThenCharged(Number),
                float_docs,
                &["JSON.NUMINCRBY", "m", "$..x", "0.25"],
                float_resp2,
                float_resp3,
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.STRAPPEND",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.STRAPPEND", "k", "$.s", "\"b\""],
                "*1\r\n:2\r\n",
                "*1\r\n:2\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.STRLEN",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.STRLEN", "k", "$.s"],
                "*1\r\n:1\r\n",
                "*1\r\n:1\r\n",
            ),
            2,
            true,
        ),
        // A two-digit integer as the crossing frame: the decimal-width
        // count past its first digit.
        dollar(
            fx(
                "JSON.STRLEN",
                Plain,
                Charged,
                &[("k", r#"{"s":"abcdefghijkl"}"#)],
                &["JSON.STRLEN", "k", "$.s"],
                "*1\r\n:12\r\n",
                "*1\r\n:12\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.TOGGLE",
                Fast,
                Fixed(Toggle),
                &[("k", D)],
                &["JSON.TOGGLE", "k", "$.f"],
                "*1\r\n:0\r\n",
                "*1\r\n:0\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.TOGGLE",
                Fast,
                Fixed(Toggle),
                &[("k", D)],
                &["JSON.TOGGLE", "k", ".f"],
                "$5\r\nfalse\r\n",
                "$5\r\nfalse\r\n",
            ),
            2,
            false,
        ),
        // A descendant path is not a simple path: the general path serves
        // 8 B under an 11 B floor.
        dollar(
            fx(
                "JSON.TOGGLE",
                General,
                FixedThenCharged(Toggle),
                &[("k", D)],
                &["JSON.TOGGLE", "k", "$..f"],
                "*1\r\n:0\r\n",
                "*1\r\n:0\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.CLEAR",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.CLEAR", "k", "$.arr"],
                ":1\r\n",
                ":1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRAPPEND",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRAPPEND", "k", "$.arr", "3"],
                "*1\r\n:3\r\n",
                "*1\r\n:3\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRINSERT",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRINSERT", "k", "$.arr", "0", "0"],
                "*1\r\n:3\r\n",
                "*1\r\n:3\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRINDEX",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRINDEX", "k", "$.arr", "2"],
                "*1\r\n:1\r\n",
                "*1\r\n:1\r\n",
            ),
            2,
            true,
        ),
        // `charge` adds a frame's prediction to the bytes already written,
        // so only the crossing frame's predictor decides arm (c): here the
        // signed integer's, `:-1` (a miss) as the final frame.
        dollar(
            fx(
                "JSON.ARRINDEX",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRINDEX", "k", "$.arr", "9"],
                "*1\r\n:-1\r\n",
                "*1\r\n:-1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRLEN",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRLEN", "k", "$.arr"],
                "*1\r\n:2\r\n",
                "*1\r\n:2\r\n",
            ),
            2,
            true,
        ),
        // No match: the array header is the whole reply, so its predictor
        // writes the crossing frame.
        dollar(
            fx(
                "JSON.ARRLEN",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRLEN", "k", "$.zz"],
                "*0\r\n",
                "*0\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRPOP",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRPOP", "k", "$.arr"],
                "*1\r\n$1\r\n2\r\n",
                "*1\r\n$1\r\n2\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.ARRPOP",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRPOP", "k", ".arr"],
                "$1\r\n2\r\n",
                "$1\r\n2\r\n",
            ),
            2,
            false,
        ),
        dollar(
            fx(
                "JSON.ARRTRIM",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.ARRTRIM", "k", "$.arr", "0", "0"],
                "*1\r\n:1\r\n",
                "*1\r\n:1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.OBJKEYS",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.OBJKEYS", "k", "$.o"],
                "*1\r\n*1\r\n$1\r\nx\r\n",
                "*1\r\n*1\r\n$1\r\nx\r\n",
            ),
            2,
            true,
        ),
        // The last frame is a bulk with a two-digit length header.
        dollar(
            fx(
                "JSON.OBJKEYS",
                Plain,
                Charged,
                &[("k", r#"{"o":{"long_key_0":1}}"#)],
                &["JSON.OBJKEYS", "k", "$.o"],
                "*1\r\n*1\r\n$10\r\nlong_key_0\r\n",
                "*1\r\n*1\r\n$10\r\nlong_key_0\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.OBJKEYS",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.OBJKEYS", "k", ".o"],
                "*1\r\n$1\r\nx\r\n",
                "*1\r\n$1\r\nx\r\n",
            ),
            2,
            false,
        ),
        dollar(
            fx(
                "JSON.OBJLEN",
                Plain,
                Charged,
                &[("k", D)],
                &["JSON.OBJLEN", "k", "$.o"],
                "*1\r\n:1\r\n",
                "*1\r\n:1\r\n",
            ),
            2,
            true,
        ),
        dollar(
            fx("JSON.MERGE", Path, Charged, &[("k", D)], &["JSON.MERGE", "k", "$.a", "5"], ok, ok),
            2,
            true,
        ),
        dollar(
            fx(
                "JSON.MERGE",
                Root,
                Fixed(Status),
                &[],
                &["JSON.MERGE", "k", "$", r#"{"a":1}"#],
                ok,
                ok,
            ),
            2,
            true,
        ),
        fx("JSON.DEBUG", Plain, Charged, &[], &["JSON.DEBUG", "MEMORY", "k"], "$-1\r\n", "_\r\n"),
    ]
}

/// What one leg observed: the reply, the watched keys' canonical bytes
/// (`None` = absent) before and after, and the in-place witness's move.
struct Leg {
    reply: Vec<u8>,
    before: Vec<Option<Vec<u8>>>,
    after: Vec<Option<Vec<u8>>>,
    in_place: u64,
}

fn store_doc(db: &mut Db, key: &str, json: &str) {
    let idoc = inf_doc::JsonParser::new().parse(json.as_bytes()).expect("fixture JSON");
    let opts = inf_store::JsonSetOptions {
        cond: inf_store::SetCond::Always,
        expire: inf_store::SetExpire::Keep,
    };
    let outcome = db.ks.db_mut(0).json_set(
        key.as_bytes(),
        &CanonicalDoc::validate(&idoc).expect("canonical fixture"),
        opts,
        Nanos(db.clock),
    );
    assert_eq!(outcome, Ok(inf_store::JsonSetOutcome::Applied), "fixture write {key}");
}

/// The keys a leg watches: every fixture document and the command's key.
fn watched(f: &Fixture) -> Vec<&'static str> {
    let mut keys: Vec<&str> = f.docs.iter().map(|(key, _)| *key).collect();
    keys.push(f.argv[1]);
    keys
}

fn state(db: &mut Db, f: &Fixture) -> Vec<Option<Vec<u8>>> {
    let now = Nanos(db.clock);
    let store = db.ks.db_mut(0);
    watched(f)
        .iter()
        .map(|key| store.json_freeze(key.as_bytes(), now).expect("watched keys hold documents"))
        .collect()
}

/// The reply already in the send buffer when a leg's command runs.
const PIPELINED: &[u8] = b"+PONG\r\n";

/// One leg on a fresh keyspace at `budget` reply bytes, answered behind
/// [`PIPELINED`]: a budget counted from the buffer's start, or a refusal
/// that truncates to it, fails every arm of every row.
fn leg(f: &Fixture, proto: Protocol, budget: usize) -> Leg {
    let mut db = db_with_reply_budget(budget);
    db.cx.proto = proto;
    for (key, json) in f.docs {
        store_doc(&mut db, key, json);
    }
    let before = state(&mut db, f);
    let in_place_before = db.ks.stats().json_scalar_patches_in_place;
    let reply = db.run_behind(PIPELINED, f.argv);
    let in_place = db.ks.stats().json_scalar_patches_in_place - in_place_before;
    let after = state(&mut db, f);
    Leg { reply, before, after, in_place }
}

/// The RESP elements of an `*N` reply (bulk, null or error lines): the
/// per-element oracle compares element by element.
fn elements(reply: &[u8]) -> Vec<&[u8]> {
    let line_end =
        |from: usize| from + reply[from..].windows(2).position(|w| w == b"\r\n").expect("CRLF") + 2;
    let mut at = line_end(0);
    let mut out = Vec::new();
    while at < reply.len() {
        let header_end = line_end(at);
        let end = match reply[at] {
            b'$' if reply[at + 1] != b'-' => {
                let digits = std::str::from_utf8(&reply[at + 1..header_end - 2]).expect("digits");
                header_end + digits.parse::<usize>().expect("length") + 2
            }
            _ => header_end,
        };
        out.push(&reply[at..end]);
        at = end;
    }
    out
}

fn per_element_refusal(served: &[u8], budget: usize) -> Vec<u8> {
    let count = elements(served).len();
    let mut want = format!("*{count}\r\n").into_bytes();
    for element in elements(served) {
        let refused = element.len() > budget;
        want.extend_from_slice(if refused { REFUSED.as_bytes() } else { element });
    }
    want
}

/// The fixture's declared arm, checked on a served leg — an arm whose
/// witness did not move is VACUOUS, and VACUOUS is red.
fn assert_engaged(f: &Fixture, leg: &Leg, what: &str) {
    let expected_in_place = u64::from(f.arm == Arm::Fast);
    assert_eq!(leg.in_place, expected_in_place, "VACUOUS {what}: {:?} lane witness", f.arm);
    let key_at = leg.before.len() - 1;
    let (present_before, present_after) =
        (leg.before[key_at].is_some(), leg.after[key_at].is_some());
    match f.arm {
        Arm::Root => {
            assert_ne!(present_before, present_after, "VACUOUS {what}: EXISTS did not flip")
        }
        Arm::Path => assert!(present_before && present_after, "VACUOUS {what}: key not kept"),
        Arm::Plain | Arm::Fast | Arm::General => {}
    }
    let write =
        inf_wire::lookup(f.row.as_bytes()).expect("row").flags.contains(inf_wire::CmdFlags::WRITE);
    assert_eq!(
        leg.after != leg.before,
        write,
        "{what}: a write row changes state, a read row does not"
    );
}

fn assert_refused(leg: &Leg, want: &[u8], what: &str) {
    assert!(leg.reply == want, "{what}: got {}, want {}", head(&leg.reply), head(want));
    assert!(leg.after == leg.before, "{what}: state changed under a refusal");
    assert_eq!(leg.in_place, 0, "{what}: an effect ran before its reservation");
}

fn check_fixture(f: &Fixture, proto: Protocol) {
    let pinned = match proto {
        Protocol::Resp2 => f.resp2,
        Protocol::Resp3 => f.resp3,
    };
    let what = |arm: &str| format!("{} {:?} {:?} {:?}: {arm}", f.row, f.argv, proto, f.arm);
    // (a) the default budget: the pinned reply, and the fixture's arm ran.
    let served = leg(f, proto, StoreConfig::default().doc_max_reply_bytes);
    assert!(served.reply == pinned.as_bytes(), "{}: got {}", what("(a)"), head(&served.reply));
    assert_engaged(f, &served, &what("(a)"));
    let length = served.reply.len();
    // (b) a zero budget refuses before any effect.
    let zero = match f.settlement {
        Settlement::PerElement => per_element_refusal(&served.reply, 0),
        Settlement::Charged | Settlement::Fixed(_) | Settlement::FixedThenCharged(_) => {
            REFUSED.as_bytes().to_vec()
        }
    };
    assert_refused(&leg(f, proto, 0), &zero, &what("(b) budget 0"));
    // (c) the boundary pair B, B − 1.
    let boundary = match f.settlement {
        Settlement::Charged => length,
        Settlement::Fixed(shape) => {
            assert!(length <= shape.reply_bytes_max(), "{}: L over M", what("(c)"));
            shape.reply_bytes_max()
        }
        Settlement::FixedThenCharged(shape) => length.max(shape.reply_bytes_max()),
        Settlement::PerElement => elements(&served.reply).iter().map(|e| e.len()).max().unwrap(),
    };
    let at = leg(f, proto, boundary);
    let at_what = what(&format!("(c) served at B = {boundary}"));
    assert!(at.reply == served.reply, "{at_what}: got {}", head(&at.reply));
    assert!(at.after == served.after, "{at_what}: post-state differs from (a)");
    assert_engaged(f, &at, &at_what);
    let below = leg(f, proto, boundary - 1);
    let below_want = match f.settlement {
        Settlement::PerElement => per_element_refusal(&served.reply, boundary - 1),
        Settlement::Charged | Settlement::Fixed(_) | Settlement::FixedThenCharged(_) => {
            REFUSED.as_bytes().to_vec()
        }
    };
    assert_refused(&below, &below_want, &what(&format!("(c) refused at B - 1 = {}", boundary - 1)));
}

/// ADR-0099 A1's oracle: every `JSON.*` registry row has a fixture, and
/// each fixture, under RESP2 and RESP3 on a fresh keyspace per leg, is
/// (a) served its pinned bytes at the default budget with its arm's
/// witness moving, (b) refused at budget 0 with no state change and no
/// in-place effect, and (c) served at its class's boundary B and refused
/// at B − 1. It judges by pinned wire bytes, canonical document bytes,
/// key existence and `inf_server::limits` only.
#[test]
fn every_json_reply_is_charged_and_refused_before_its_effect() {
    let fixtures = reply_fixtures();
    let rows: Vec<&str> =
        inf_wire::COMMANDS.iter().map(|m| m.name).filter(|n| n.starts_with("JSON.")).collect();
    for fixture in &fixtures {
        assert!(rows.contains(&fixture.row), "{} is not a registry row", fixture.row);
        assert_eq!(fixture.argv[0], fixture.row, "a fixture runs its own row");
        if let Some(dollar) = fixture.dollar {
            let text = fixture.argv[fixture.path_at];
            assert_eq!(text.starts_with('$'), dollar, "{} {text}: declared mode", fixture.row);
        }
    }
    for row in &rows {
        let own: Vec<&Fixture> = fixtures.iter().filter(|f| f.row == *row).collect();
        assert!(!own.is_empty(), "registry row {row} has no fixture");
        for fixture in own {
            for proto in [Protocol::Resp2, Protocol::Resp3] {
                check_fixture(fixture, proto);
            }
        }
    }
}
