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
    /// One command producing N frames on the connection (pub/sub
    /// confirmations per channel, self-delivery push + reply): the
    /// concatenation of all N is compared byte-exact (M1-S12).
    Frames(usize),
    /// Both replies are RESP integers within `0` ± tolerance of each other.
    IntWithin(i64),
    /// Replies differ by design; both must frame-parse.
    SkipDiff(&'static str),
}

impl Check {
    /// Whether this case byte-compares against the oracle (feeds the
    /// declared-`full` enforcement in the generated matrix — M1-S13).
    pub fn compared(self) -> bool {
        !matches!(self, Check::SkipDiff(_))
    }
}

/// One scripted command.
pub struct Case {
    pub argv: &'static [&'static str],
    pub check: Check,
}

const fn c(argv: &'static [&'static str]) -> Case {
    Case { argv, check: Check::ByteExact }
}

const fn frames(argv: &'static [&'static str], n: usize) -> Case {
    Case { argv, check: Check::Frames(n) }
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
    skip(
        &["INFO"],
        "section payloads differ (InfinityDB identity/tripwires); shape client-parseable",
    ),
    skip(&["COMMAND"], "registry is the M0+M1 surface, not the full Redis set"),
    skip(&["COMMAND", "COUNT"], "registry size differs by design"),
    // ================= M1-E1 surface (M1-S01/S02/S03) =================
    // --- MGET / MSET / MSETNX ---
    c(&["MSET", "m1", "v1", "m2", "v2"]),
    c(&["MGET", "m1", "nope", "m2"]),
    c(&["MGET", "m1"]),
    c(&["MSET", "m1", "v1", "m2"]),
    c(&["MSET"]),
    c(&["MSETNX", "m1", "x", "mfresh", "y"]),
    c(&["GET", "mfresh"]),
    c(&["MSETNX", "mn1", "a", "mn2", "b"]),
    c(&["MGET", "mn1", "mn2"]),
    c(&["MSETNX", "mn3", "c", "mn3", "d"]),
    c(&["GET", "mn3"]),
    // --- GETRANGE / SUBSTR / SETRANGE ---
    c(&["SET", "gr", "Hello World"]),
    c(&["GETRANGE", "gr", "0", "4"]),
    c(&["GETRANGE", "gr", "-5", "-1"]),
    c(&["GETRANGE", "gr", "0", "-1"]),
    c(&["GETRANGE", "gr", "6", "3"]),
    c(&["GETRANGE", "gr", "99", "120"]),
    c(&["GETRANGE", "gr", "-100", "2"]),
    c(&["GETRANGE", "missing", "0", "-1"]),
    c(&["GETRANGE", "gr", "abc", "1"]),
    c(&["SUBSTR", "gr", "0", "4"]),
    c(&["SETRANGE", "sr", "5", "world"]),
    c(&["GET", "sr"]),
    c(&["SETRANGE", "sr", "0", "Hello"]),
    c(&["GET", "sr"]),
    c(&["SETRANGE", "sr", "-1", "x"]),
    c(&["SETRANGE", "srempty", "0", ""]),
    c(&["EXISTS", "srempty"]),
    c(&["STRLEN", "sr"]),
    // --- GETEX ---
    c(&["SET", "gx", "v"]),
    c(&["GETEX", "gx"]),
    c(&["TTL", "gx"]),
    c(&["GETEX", "gx", "EX", "100"]),
    c(&["TTL", "gx"]),
    c(&["GETEX", "gx", "PERSIST"]),
    c(&["TTL", "gx"]),
    c(&["GETEX", "gx", "EXAT", "1"]),
    c(&["EXISTS", "gx"]),
    c(&["GETEX", "missing", "EX", "100"]),
    c(&["SET", "gx2", "v"]),
    c(&["GETEX", "gx2", "EX", "0"]),
    c(&["GETEX", "gx2", "EX", "100", "PERSIST"]),
    c(&["GETEX", "gx2", "BOGUS"]),
    // --- INCRBYFLOAT ---
    c(&["SET", "fl", "10.5"]),
    c(&["INCRBYFLOAT", "fl", "0.1"]),
    c(&["INCRBYFLOAT", "fl", "-5"]),
    c(&["INCRBYFLOAT", "newfl", "3.5"]),
    c(&["SET", "fle", "5.0e3"]),
    c(&["INCRBYFLOAT", "fle", "200"]),
    c(&["SET", "flbad", "abc"]),
    c(&["INCRBYFLOAT", "flbad", "1"]),
    c(&["INCRBYFLOAT", "fl", "notafloat"]),
    // --- OBJECT ---
    c(&["SET", "oint", "123"]),
    c(&["OBJECT", "ENCODING", "oint"]),
    c(&["OBJECT", "REFCOUNT", "oint"]),
    c(&["SET", "oemb", "short string"]),
    c(&["OBJECT", "ENCODING", "oemb"]),
    c(&["OBJECT", "REFCOUNT", "oemb"]),
    c(&["SET", "oraw", "0123456789012345678901234567890123456789012345"]),
    c(&["OBJECT", "ENCODING", "oraw"]),
    c(&["APPEND", "oapp", "12"]),
    c(&["OBJECT", "ENCODING", "oapp"]),
    skip(
        &["OBJECT", "IDLETIME", "oint"],
        "no LRU clock until the eviction engine (M1-E3); honest 0",
    ),
    c(&["OBJECT", "FREQ", "oint"]),
    c(&["OBJECT", "ENCODING", "missing"]),
    c(&["OBJECT", "BOGUS", "oint"]),
    // --- RENAME / RENAMENX / COPY (TTL moves with the record) ---
    c(&["SET", "r1", "payload", "EX", "100"]),
    c(&["RENAME", "r1", "r2"]),
    c(&["EXISTS", "r1"]),
    c(&["GET", "r2"]),
    Case { argv: &["TTL", "r2"], check: Check::IntWithin(1) },
    c(&["RENAME", "missing", "x"]),
    c(&["SET", "rnx1", "a"]),
    c(&["SET", "rnx2", "b"]),
    c(&["RENAMENX", "rnx1", "rnx2"]),
    c(&["RENAMENX", "rnx1", "rnxfresh"]),
    c(&["GET", "rnxfresh"]),
    c(&["RENAMENX", "missing", "x"]),
    c(&["SET", "cp1", "tocopy", "EX", "100"]),
    c(&["COPY", "cp1", "cp2"]),
    c(&["GET", "cp2"]),
    Case { argv: &["TTL", "cp2"], check: Check::IntWithin(1) },
    c(&["COPY", "cp1", "cp2"]),
    c(&["COPY", "cp1", "cp2", "REPLACE"]),
    c(&["COPY", "missing", "x"]),
    c(&["COPY", "cp1", "cp1"]),
    c(&["COPY", "cp1", "cp3", "DB", "0"]),
    // Cross-db COPY (M1-E4 namespaces v1): real now, byte-exact.
    c(&["COPY", "cp1", "cp4", "DB", "3"]),
    c(&["COPY", "cp1", "cp4", "DB", "3"]),
    c(&["COPY", "cp1", "cp4", "DB", "3", "REPLACE"]),
    c(&["COPY", "cp1", "cp1", "DB", "3"]),
    c(&["COPY", "cp1", "cp4", "DB", "99"]),
    c(&["COPY", "cp1", "cp5", "BOGUS"]),
    // --- TOUCH / UNLINK ---
    c(&["TOUCH", "cp1", "missing", "cp2"]),
    c(&["UNLINK", "cp2", "cp3", "missing"]),
    c(&["EXISTS", "cp2"]),
    // --- EXPIREAT / PEXPIREAT / EXPIRETIME / PEXPIRETIME (wall clock) ---
    c(&["SET", "ea", "v"]),
    c(&["EXPIREAT", "ea", "2208988800"]),
    c(&["EXPIRETIME", "ea"]),
    c(&["PEXPIRETIME", "ea"]),
    Case { argv: &["TTL", "ea"], check: Check::IntWithin(2) },
    c(&["EXPIREAT", "ea", "2208988801", "GT"]),
    c(&["EXPIRETIME", "ea"]),
    c(&["EXPIREAT", "ea", "1", "GT"]),
    c(&["EXPIREAT", "ea", "2208988800", "NX"]),
    c(&["SET", "eap", "v"]),
    c(&["EXPIREAT", "eap", "1"]),
    c(&["EXISTS", "eap"]),
    c(&["SET", "pea", "v"]),
    c(&["PEXPIREAT", "pea", "2208988800000"]),
    c(&["PEXPIRETIME", "pea"]),
    c(&["EXPIRETIME", "missing"]),
    c(&["SET", "noex2", "v"]),
    c(&["EXPIRETIME", "noex2"]),
    c(&["PEXPIRETIME", "noex2"]),
    c(&["EXPIREAT", "ea", "notanint"]),
    // --- SET EXAT / PXAT ---
    c(&["SET", "sx", "v", "EXAT", "2208988800"]),
    c(&["EXPIRETIME", "sx"]),
    c(&["SET", "px", "v", "PXAT", "2208988800000"]),
    c(&["PEXPIRETIME", "px"]),
    c(&["SET", "sxp", "v", "EXAT", "1"]),
    c(&["GET", "sxp"]),
    c(&["SET", "sx", "v", "EXAT", "notanint"]),
    c(&["SET", "sx", "v", "EX", "10", "EXAT", "2208988800"]),
    // --- KEYS / SCAN / DBSIZE / RANDOMKEY ---
    c(&["KEYS", "gr"]),
    c(&["KEYS", "rnxfre*"]),
    c(&["KEYS", "no-such-prefix:*"]),
    skip(
        &["KEYS", "m*"],
        "result ordering differs (home-group vs dict order); set equality via DBSIZE",
    ),
    skip(&["SCAN", "0"], "cursor values are engine-internal; guarantee proptested in inf-store"),
    skip(&["SCAN", "0", "MATCH", "m*", "COUNT", "100"], "cursor values engine-internal"),
    c(&["SCAN", "notacursor"]),
    c(&["SCAN", "0", "COUNT", "0"]),
    c(&["DBSIZE"]),
    skip(&["RANDOMKEY"], "two-level random (cell, then key) — documented deviation"),
    // --- SELECT + database isolation (M1-E4 namespaces v1) ---
    c(&["SELECT", "0"]),
    c(&["SELECT", "17"]),
    c(&["SELECT", "notanint"]),
    c(&["SELECT", "3"]),
    c(&["EXISTS", "cp1"]),
    c(&["GET", "cp4"]),
    c(&["SET", "nsk", "three"]),
    c(&["GET", "nsk"]),
    c(&["DBSIZE"]),
    c(&["SELECT", "0"]),
    c(&["GET", "nsk"]),
    c(&["SELECT", "3"]),
    c(&["FLUSHDB"]),
    c(&["DBSIZE"]),
    c(&["SELECT", "0"]),
    c(&["DBSIZE"]),
    // --- CONFIG ---
    c(&["CONFIG", "GET", "maxmemory"]),
    c(&["CONFIG", "SET", "maxmemory", "100mb"]),
    c(&["CONFIG", "GET", "maxmemory"]),
    c(&["CONFIG", "SET", "maxmemory", "0"]),
    c(&["CONFIG", "GET", "maxmemory-policy"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "allkeys-lfu"]),
    c(&["CONFIG", "GET", "maxmemory-policy"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "noeviction"]),
    c(&["CONFIG", "GET", "databases"]),
    skip(&["CONFIG", "GET", "maxmemory*"], "InfinityDB returns the typed M1 key subset"),
    skip(&["CONFIG", "SET", "maxmemory-policy", "bogus"], "error detail text differs; both reject"),
    skip(&["CONFIG", "SET", "nonexistent-param", "1"], "error text shape differs slightly"),
    c(&["CONFIG", "REWRITE"]),
    // --- CLIENT ---
    skip(&["CLIENT", "ID"], "connection ids are engine-internal counters"),
    c(&["CLIENT", "GETNAME"]),
    c(&["CLIENT", "SETNAME", "compat-suite"]),
    c(&["CLIENT", "GETNAME"]),
    c(&["CLIENT", "SETNAME", "has space"]),
    skip(&["CLIENT", "LIST"], "addr/fd/timing fields differ; field vocabulary matches"),
    skip(&["CLIENT", "INFO"], "addr/fd/timing fields differ; field vocabulary matches"),
    c(&["CLIENT", "KILL", "ID", "99999"]),
    // --- DEBUG subset / LOLWUT ---
    skip(
        &["DEBUG", "JMAP"],
        "removed in Redis 8; InfinityDB accepts it as a no-op (M1-S03 surface)",
    ),
    c(&["DEBUG", "SLEEP", "0"]),
    c(&["DEBUG", "SET-ACTIVE-EXPIRE", "1"]),
    skip(
        &["DEBUG", "OBJECT", "oint"],
        "value-address/serialized-length fields are engine-internal",
    ),
    c(&["DEBUG", "OBJECT", "missing"]),
    skip(&["LOLWUT"], "version art differs by design"),
    // --- COMMAND introspection (M1 additions) ---
    c(&["COMMAND", "GETKEYS", "GET", "k"]),
    c(&["COMMAND", "GETKEYS", "MSET", "k1", "v1", "k2", "v2"]),
    c(&["COMMAND", "GETKEYS", "PING"]),
    skip(
        &["COMMAND", "INFO", "get"],
        "flags/acl detail differs; arity+keyspec verified in inf-wire",
    ),
    skip(&["COMMAND", "DOCS", "get"], "docs payload not implemented (honest empty map)"),
    // ================= M1-E5 · pub/sub (M1-S10) =================
    // The script has run in RESP3 since the introspection HELLO 3 — drop
    // back to RESP2 so subscriber-mode restriction is actually exercised.
    skip(&["HELLO", "2"], "identity fields differ; proto switch verified locally"),
    // --- RESP2 subscriber mode: restriction + frame shapes ---
    c(&["PUBLISH", "nosubs", "hello"]),
    c(&["SUBSCRIBE", "news"]),
    c(&["GET", "k1"]), // restricted-context error, byte-exact
    c(&["SET", "k1", "v"]),
    c(&["HELLO", "3"]), // not exempt from the RESP2 subscriber restriction
    c(&["PUBSUB", "CHANNELS"]),
    c(&["PING"]),
    c(&["PING", "in-sub-mode"]),
    c(&["SUBSCRIBE", "news"]), // re-subscribe: frame emitted, count unchanged
    frames(&["SUBSCRIBE", "sports", "weather"], 2),
    c(&["UNSUBSCRIBE", "sports"]),
    frames(&["UNSUBSCRIBE", "news", "weather"], 2),
    c(&["UNSUBSCRIBE"]),          // nothing left: single nil frame
    c(&["UNSUBSCRIBE", "ghost"]), // never subscribed: frame, count unchanged
    c(&["PSUBSCRIBE", "news.*"]),
    c(&["PUNSUBSCRIBE", "news.*"]),
    c(&["PUNSUBSCRIBE"]),
    c(&["SUBSCRIBE"]), // arity errors
    c(&["PUBLISH", "lonely"]),
    c(&["PUBSUB"]),
    // --- RESP3: no restriction, push frames, self-delivery ---
    skip(&["HELLO", "3"], "identity fields differ; proto switch verified locally"),
    c(&["SUBSCRIBE", "alpha"]),
    c(&["PUBSUB", "CHANNELS"]),
    c(&["PUBSUB", "CHANNELS", "al*"]),
    c(&["PUBSUB", "CHANNELS", "none*"]),
    c(&["PUBSUB", "NUMSUB", "alpha", "ghost"]),
    c(&["PUBSUB", "NUMPAT"]),
    frames(&["PUBLISH", "alpha", "selfmsg"], 2), // push frame precedes :1
    c(&["PSUBSCRIBE", "al*"]),
    c(&["PUBSUB", "NUMPAT"]),
    frames(&["PUBLISH", "alpha", "both"], 3), // message, pmessage, :2
    c(&["SET", "ps:probe", "v"]),             // RESP3 subscribers keep the full surface
    c(&["GET", "ps:probe"]),
    c(&["UNSUBSCRIBE", "alpha"]),
    c(&["PUNSUBSCRIBE", "al*"]),
    skip(
        &["PUBSUB", "SHARDCHANNELS"],
        "sharded pub/sub (SSUBSCRIBE family) is the recorded M3 cut line",
    ),
    skip(&["HELLO", "2"], "identity fields differ; proto switch verified locally"),
    c(&["PING"]),
    c(&["DEL", "ps:probe"]),
    // --- client-output-buffer-limit (M1-S11) ---
    c(&["CONFIG", "GET", "client-output-buffer-limit"]),
    c(&["CONFIG", "SET", "client-output-buffer-limit", "pubsub 256kb 64kb 5"]),
    c(&["CONFIG", "GET", "client-output-buffer-limit"]),
    c(&[
        "CONFIG",
        "SET",
        "client-output-buffer-limit",
        "normal 0 0 0 slave 268435456 67108864 60 pubsub 33554432 8388608 60",
    ]),
    // --- FLUSHDB / FLUSHALL (terminal: wipes the scripted state) ---
    c(&["FLUSHDB", "BOGUS"]),
    c(&["FLUSHALL"]),
    c(&["DBSIZE"]),
    c(&["KEYS", "*"]),
    c(&["RANDOMKEY"]),
    c(&["MGET", "m1", "m2"]),
    c(&["FLUSHDB", "ASYNC"]),
    c(&["FLUSHDB", "SYNC"]),
    // --- OOM honesty (M1-S07): maxmemory below the baseline floor makes
    // pressure unfreeable on BOTH engines, so every DENYOOM verdict and
    // error byte is comparable per policy; reads and freeing writes stay
    // allowed. allkeys-* policies evict whatever exists on both sides
    // before the verdict (state ends empty either way).
    c(&["SET", "oomprobe", "v"]),
    c(&["CONFIG", "SET", "maxmemory", "1"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "noeviction"]),
    c(&["SET", "oomk", "v"]),
    c(&["INCR", "oomctr"]),
    c(&["APPEND", "oomk", "x"]),
    c(&["GET", "oomprobe"]),
    c(&["DEL", "missing"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "volatile-lru"]),
    c(&["SET", "oomk", "v"]),
    c(&["GET", "oomprobe"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "volatile-random"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "volatile-ttl"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "volatile-lfu"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "allkeys-lru"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "allkeys-random"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "allkeys-lfu"]),
    c(&["SET", "oomk", "v"]),
    c(&["CONFIG", "SET", "maxmemory", "0"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "noeviction"]),
    c(&["SET", "oomk", "recovered"]),
    c(&["GET", "oomk"]),
    // --- OBJECT FREQ under an LFU policy (M1-S06) ---
    c(&["CONFIG", "SET", "maxmemory-policy", "allkeys-lfu"]),
    skip(
        &["OBJECT", "FREQ", "oomk"],
        "popularity scale differs: CMS Morris estimate vs Redis log counter",
    ),
    c(&["OBJECT", "FREQ", "missing"]),
    c(&["CONFIG", "SET", "maxmemory-policy", "noeviction"]),
    c(&["OBJECT", "FREQ", "oomk"]),
    // --- INF.NS (M1-S08, InfinityDB extension — unknown to the oracle) ---
    skip(&["INF.NS", "CREATE", "cache", "EVICTION", "allkeys-lfu"], "InfinityDB extension"),
    skip(
        &["INF.NS", "CREATE", "ledger", "MODE", "durable"],
        "InfinityDB extension; durable mode honestly rejected until M2",
    ),
    skip(&["INF.NS", "LIST"], "InfinityDB extension"),
    skip(&["INF.NS", "INFO", "cache"], "InfinityDB extension"),
    skip(&["INF.NS", "DROP", "cache"], "InfinityDB extension"),
    // --- terminal cleanup ---
    c(&["FLUSHALL"]),
];
