//! The M0-S15 command × edge-case matrix. Each case is one command sent to
//! both engines in script order (state accumulates within a script).
//!
//! `Check` declares how the two replies are compared:
//! - `ByteExact` — the default and the point of the harness.
//! - `IntWithin(ms)` — timing-dependent integer replies (live `PTTL`).
//! - `SkipDiff` — documented deviations (`HELLO`/`INFO`/`COMMAND` payloads,
//!   record-format bounds). The candidate must still produce a *parseable*
//!   reply; the deviation list is the L8 honesty ledger, not an excuse pile.

/// Comparison mode for one case.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Check {
    ByteExact,
    /// Both replies are RESP integers within `0` ± tolerance of each other.
    IntWithin(i64),
    /// Replies differ by design; both must frame-parse.
    SkipDiff(&'static str),
}

/// One scripted command.
pub struct Case {
    pub argv: &'static [&'static str],
    pub check: Check,
}

const fn c(argv: &'static [&'static str]) -> Case {
    Case { argv, check: Check::ByteExact }
}

const fn skip(argv: &'static [&'static str], why: &'static str) -> Case {
    Case { argv, check: Check::SkipDiff(why) }
}

/// The v0 script. Order matters: later cases read state earlier ones wrote.
pub static MATRIX: &[Case] = &[
    // --- PING / ECHO ---
    c(&["PING"]),
    c(&["PING", "hello world"]),
    c(&["PING", "a", "b"]),
    c(&["ECHO", "payload"]),
    c(&["ECHO"]),
    c(&["ECHO", "a", "b"]),
    c(&["ping"]), // case-insensitive dispatch
    // --- GET / SET basics ---
    c(&["GET", "missing"]),
    c(&["GET"]),
    c(&["SET", "k1", "v1"]),
    c(&["GET", "k1"]),
    c(&["SET", "k1"]),
    c(&["SET", "k1", "v2"]),
    c(&["GET", "k1"]),
    c(&["SET", "", "empty-key"]),
    c(&["GET", ""]),
    c(&["SET", "bin", "a\r\nb\x00c"]),
    c(&["GET", "bin"]),
    // --- SET NX/XX/GET/KEEPTTL ---
    c(&["SET", "k1", "nx-loses", "NX"]),
    c(&["GET", "k1"]),
    c(&["SET", "fresh", "nx-wins", "NX"]),
    c(&["GET", "fresh"]),
    c(&["SET", "fresh", "xx-wins", "XX"]),
    c(&["SET", "ghost", "xx-loses", "XX"]),
    c(&["GET", "ghost"]),
    c(&["SET", "k1", "v3", "GET"]),
    c(&["SET", "ghost2", "v", "GET"]),
    c(&["SET", "k1", "nx-get", "NX", "GET"]),
    c(&["SET", "k1", "v4", "EX", "100"]),
    c(&["TTL", "k1"]),
    c(&["SET", "k1", "v5", "KEEPTTL"]),
    c(&["TTL", "k1"]),
    c(&["SET", "k1", "v6"]),
    c(&["TTL", "k1"]),
    c(&["SET", "k1", "v", "EX", "0"]),
    c(&["SET", "k1", "v", "EX", "-5"]),
    c(&["SET", "k1", "v", "PX", "notanint"]),
    c(&["SET", "k1", "v", "EX", "10", "KEEPTTL"]),
    c(&["SET", "k1", "v", "NX", "XX"]),
    c(&["SET", "k1", "v", "BOGUSOPT"]),
    // --- SETNX / SETEX / PSETEX ---
    c(&["SETNX", "k1", "loses"]),
    c(&["SETNX", "newnx", "wins"]),
    c(&["SETEX", "se", "100", "v"]),
    c(&["TTL", "se"]),
    c(&["SETEX", "se", "0", "v"]),
    c(&["SETEX", "se", "-1", "v"]),
    c(&["SETEX", "se", "nope", "v"]),
    c(&["PSETEX", "pse", "100000", "v"]),
    c(&["PSETEX", "pse", "0", "v"]),
    // --- GETSET / GETDEL ---
    c(&["GETSET", "k1", "swapped"]),
    c(&["GETSET", "brandnew", "v"]),
    c(&["GETDEL", "brandnew"]),
    c(&["GETDEL", "brandnew"]),
    c(&["GET", "brandnew"]),
    // --- DEL / EXISTS ---
    c(&["SET", "d1", "x"]),
    c(&["SET", "d2", "x"]),
    c(&["DEL", "d1", "d2", "d3"]),
    c(&["DEL", "d1"]),
    c(&["SET", "e1", "x"]),
    c(&["EXISTS", "e1", "e1", "nope", "e1"]),
    c(&["EXISTS", "nope"]),
    // --- TYPE ---
    c(&["TYPE", "e1"]),
    c(&["TYPE", "missingtype"]),
    // --- INCR family ---
    c(&["INCR", "ctr"]),
    c(&["INCR", "ctr"]),
    c(&["DECR", "ctr"]),
    c(&["INCRBY", "ctr", "41"]),
    c(&["DECRBY", "ctr", "2"]),
    c(&["INCRBY", "ctr", "-40"]),
    c(&["INCRBY", "ctr", "notanint"]),
    c(&["SET", "notnum", "abc"]),
    c(&["INCR", "notnum"]),
    c(&["SET", "padded", "007"]),
    c(&["INCR", "padded"]),
    c(&["SET", "big", "9223372036854775807"]),
    c(&["INCR", "big"]),
    c(&["SET", "small", "-9223372036854775808"]),
    c(&["DECR", "small"]),
    c(&["DECRBY", "ctr", "-9223372036854775808"]),
    c(&["SET", "negzero", "-0"]),
    c(&["INCR", "negzero"]),
    // --- APPEND / STRLEN ---
    c(&["APPEND", "app", "hello"]),
    c(&["APPEND", "app", " world"]),
    c(&["GET", "app"]),
    c(&["STRLEN", "app"]),
    c(&["STRLEN", "missinglen"]),
    // --- EXPIRE / PEXPIRE / TTL / PTTL / PERSIST ---
    c(&["SET", "ex1", "v"]),
    c(&["EXPIRE", "ex1", "100"]),
    c(&["TTL", "ex1"]),
    Case { argv: &["PTTL", "ex1"], check: Check::IntWithin(100) },
    c(&["EXPIRE", "missing", "100"]),
    c(&["TTL", "missing"]),
    c(&["PTTL", "missing"]),
    c(&["SET", "noex", "v"]),
    c(&["TTL", "noex"]),
    c(&["PTTL", "noex"]),
    c(&["EXPIRE", "noex", "50", "NX"]),
    c(&["EXPIRE", "noex", "100", "NX"]),
    c(&["EXPIRE", "noex", "100", "XX"]),
    c(&["EXPIRE", "noex", "50", "GT"]),
    c(&["EXPIRE", "noex", "200", "GT"]),
    c(&["EXPIRE", "noex", "300", "LT"]),
    c(&["EXPIRE", "noex", "100", "LT"]),
    c(&["SET", "fresh2", "v"]),
    c(&["EXPIRE", "fresh2", "100", "GT"]),
    c(&["TTL", "fresh2"]),
    c(&["EXPIRE", "fresh2", "100", "LT"]),
    c(&["TTL", "fresh2"]),
    c(&["SET", "fresh3", "v"]),
    c(&["EXPIRE", "fresh3", "100", "XX", "LT"]),
    c(&["TTL", "fresh3"]),
    c(&["EXPIRE", "fresh3", "100", "NX", "GT"]),
    c(&["EXPIRE", "fresh3", "100", "GT", "LT"]),
    c(&["EXPIRE", "fresh3", "100", "WAT"]),
    c(&["EXPIRE", "fresh3", "notanint"]),
    c(&["SET", "doomed", "v"]),
    c(&["EXPIRE", "doomed", "-1"]),
    c(&["EXISTS", "doomed"]),
    c(&["SET", "pdoomed", "v"]),
    c(&["PEXPIRE", "pdoomed", "0"]),
    c(&["EXISTS", "pdoomed"]),
    c(&["SET", "per", "v", "EX", "100"]),
    c(&["PERSIST", "per"]),
    c(&["TTL", "per"]),
    c(&["PERSIST", "per"]),
    c(&["PERSIST", "missing"]),
    // --- arity / unknown command shapes ---
    c(&["STRLEN"]),
    c(&["STRLEN", "a", "b"]),
    c(&["INCR"]),
    c(&["TTL"]),
    c(&["NOSUCHCOMMAND"]),
    c(&["NOSUCHCOMMAND", "arg1", "arg2"]),
    // --- introspection (documented deviations) ---
    skip(&["HELLO"], "identity fields differ by design (L8: server/version)"),
    skip(&["HELLO", "3"], "identity fields differ; proto switch verified locally"),
    skip(&["HELLO", "9"], "NOPROTO error text verified in unit tests"),
    skip(&["INFO"], "minimal sections at M0; allowlisted by the AC"),
    skip(&["COMMAND"], "registry is the M0 surface (26), not the full Redis set"),
    skip(&["COMMAND", "COUNT"], "registry size differs by design"),
];
