//! Command metadata + perfect-hash dispatch (M0-S12, schema frozen at M0
//! exit): name, arity, flags, and key spec — the slot router reads keys from
//! here, and M1+ (ACL, cluster, Lua) extend the registry without touching
//! dispatch internals.
//!
//! Dispatch is a compile-time perfect hash: case-fold the name, one
//! multiply-mix hash, one table probe, then a compare of the length and of
//! the bytes with only the letters' case ignored — no branching scans, no
//! allocation, and only a registry name resolves. The table is built in
//! `const` context and the build **fails to compile** on any bucket
//! collision, so adding a command can never silently degrade lookup into a
//! collision chain.
//!
//! A name packs into **two** little-endian u64 words (≤ 16 bytes —
//! `INCRBYFLOAT`/`PEXPIRETIME` are 11), and the table holds `BUCKETS`
//! entries. The lookup's shape: two register loads, two multiplies, one
//! probe, then the length and the care-masked bytes.

use crate::parser::ArgvRef;

/// Every command in the M0+M1 surface (milestones M0-S15, M1-E1).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum CommandId {
    Ping,
    Echo,
    Hello,
    Quit,
    Get,
    Set,
    Setnx,
    Setex,
    Psetex,
    Getset,
    Getdel,
    Del,
    Exists,
    Type,
    Incr,
    Decr,
    IncrBy,
    DecrBy,
    Append,
    Strlen,
    Expire,
    Pexpire,
    Ttl,
    Pttl,
    Persist,
    Info,
    Command,
    // ---- M1-S01 · string family ----
    Mget,
    Mset,
    Msetnx,
    Getrange,
    Setrange,
    Getex,
    IncrByFloat,
    Substr,
    // ---- M1-S02 · key management ----
    Rename,
    Renamenx,
    Copy,
    Touch,
    Unlink,
    Dbsize,
    Keys,
    Randomkey,
    Scan,
    Flushdb,
    Flushall,
    Object,
    Debug,
    // ---- M1-S03 · expiry completion + server introspection ----
    Expireat,
    Pexpireat,
    Expiretime,
    Pexpiretime,
    Select,
    Config,
    Client,
    Lolwut,
    // ---- M1-E5 · pub/sub ----
    Subscribe,
    Unsubscribe,
    Psubscribe,
    Punsubscribe,
    Publish,
    Pubsub,
    /// `INF.NS CREATE/LIST/INFO/DROP` — namespace registry v1 (M1-S08), the
    /// identity seam M2 durability classes attach to. An `INF.*` extension,
    /// not a Redis command.
    InfNs,
    /// `INF.CKPT [CELL k] [WAIT]` — the checkpoint operator surface
    /// (M2-S20): requests ride the control-plane epoch board; cells run
    /// checkpoints in their own MAINTAIN slices (L1); `WAIT` returns only
    /// after the new MANIFEST is durable. An `INF.*` extension.
    InfCkpt,
    /// `BGSAVE [SCHEDULE]` — maps onto `INF.CKPT` (all cells, no wait);
    /// no fork, per-cell timing (deviations documented, L8).
    Bgsave,
    /// `LASTSAVE` — unix seconds of the newest durable MANIFEST
    /// publication across cells (0 before the first; deviation noted).
    Lastsave,
    /// Internal cross-cell program op: atomically read value+TTL and delete
    /// at the owning cell. The IF form removes only matching snapshot bytes
    /// and expiry (ADR-0110). Listed in COMMAND as an INF.* extension.
    InfTake,
    /// Internal cross-cell read: value+TTL, or value+absolute expiry with
    /// ABS (the RENAME/RENAMENX/COPY snapshot, ADR-0110).
    InfPeek,
    /// Internal cross-cell put: `INF.PUT key value deadline [NX]` — the
    /// RENAME/RENAMENX destination leg. Admitted as RENAME is (WRITE, no
    /// DENYOOM); the arena's own refusal still applies (ADR-0110 A3).
    InfPut,
    // ---- M3-S11/S12 · `JSON.*` document family (ADR-0041) ----
    JsonSet,
    JsonGet,
    JsonMget,
    JsonDel,
    /// Alias of `JSON.DEL` (RedisJSON heritage).
    JsonForget,
    JsonType,
    JsonNumIncrBy,
    JsonNumMultBy,
    JsonStrAppend,
    JsonStrLen,
    JsonToggle,
    JsonClear,
    // ---- M3-S13/S14 · array + object ops, MERGE (ADR-0042) ----
    JsonArrAppend,
    JsonArrInsert,
    JsonArrIndex,
    JsonArrLen,
    JsonArrPop,
    JsonArrTrim,
    JsonObjKeys,
    JsonObjLen,
    JsonMerge,
    /// `JSON.DEBUG MEMORY key` — attributed document bytes (M3-S19,
    /// intentionally partial vs RedisJSON allocator-specific numbers).
    JsonDebug,
}

/// Command behavior flags (wire-independent bitset).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct CmdFlags(u8);

impl CmdFlags {
    pub const READONLY: CmdFlags = CmdFlags(1);
    pub const WRITE: CmdFlags = CmdFlags(1 << 1);
    pub const ADMIN: CmdFlags = CmdFlags(1 << 2);
    /// Constant-ish time; never blocks or suspends locally.
    pub const FAST: CmdFlags = CmdFlags(1 << 3);
    /// Rejected when used memory exceeds `maxmemory` and eviction cannot
    /// free below it (Redis `DENYOOM` — the M1-S07 OOM-honesty gate enters
    /// through this flag, never per-handler checks).
    pub const DENYOOM: CmdFlags = CmdFlags(1 << 4);
    /// Allowed while the node is loading (M2-S15 `-LOADING` gate).
    /// Membership mirrors Redis 8.0.5 *observed* behavior, not its docs —
    /// oracle capture `.artifacts/m2/loading-redis-capture-20260703/`:
    /// ECHO/SELECT/HELLO/pubsub/INFO/CONFIG/CLIENT/COMMAND/DEBUG/QUIT pass;
    /// **PING does not** (it answers `-LOADING` there, so it does here).
    pub const LOADING: CmdFlags = CmdFlags(1 << 5);
    /// A fabric-program primitive (ADR-0115; Redis 8's internal-command
    /// contract): unknown to every client connection, hidden from
    /// `COMMAND`, executed only by a plane program — locally, or on a
    /// fabric `Apply` carrying the program mark.
    pub const INTERNAL: CmdFlags = CmdFlags(1 << 6);

    #[inline]
    pub fn contains(self, other: CmdFlags) -> bool {
        self.0 & other.0 == other.0
    }

    const fn union(self, other: CmdFlags) -> CmdFlags {
        CmdFlags(self.0 | other.0)
    }
}

impl core::ops::BitOr for CmdFlags {
    type Output = CmdFlags;
    fn bitor(self, rhs: CmdFlags) -> CmdFlags {
        self.union(rhs)
    }
}

/// Where a command's keys sit in its argv (Redis `first/last/step`
/// convention). `first == 0` ⇒ the command has no keys. `last < 0` counts
/// from the end (`-1` = final argument — variadic key lists).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct KeySpec {
    pub first: u8,
    pub last: i8,
    pub step: u8,
}

impl KeySpec {
    pub const NONE: KeySpec = KeySpec { first: 0, last: 0, step: 0 };
    /// Single key at argv[1] — the dominant shape.
    pub const ONE: KeySpec = KeySpec { first: 1, last: 1, step: 1 };
    /// Keys from argv[1] through the final argument (DEL, EXISTS, MGET).
    pub const ALL_TRAILING: KeySpec = KeySpec { first: 1, last: -1, step: 1 };
    /// Keys from argv[1] through the second-to-last argument
    /// (JSON.MGET: `key [key …] path`).
    pub const ALL_BUT_LAST: KeySpec = KeySpec { first: 1, last: -2, step: 1 };
    /// Two keys at argv[1..=2] (RENAME, COPY).
    pub const TWO: KeySpec = KeySpec { first: 1, last: 2, step: 1 };
    /// Key/value pairs from argv[1] (MSET, MSETNX): every second argument.
    pub const PAIRS: KeySpec = KeySpec { first: 1, last: -1, step: 2 };
    /// Single key at argv[2] — subcommand shapes (OBJECT ENCODING key).
    pub const SECOND: KeySpec = KeySpec { first: 2, last: 2, step: 1 };
}

/// Frozen command metadata schema (milestone §3.2).
#[derive(Debug)]
pub struct CommandMeta {
    pub id: CommandId,
    pub name: &'static str,
    /// Redis convention: positive = exact argv length (including the command
    /// name); negative = at least `|arity|`.
    pub arity: i8,
    pub flags: CmdFlags,
    /// The static registry row (the shape `COMMAND INFO` renders). Routing
    /// reads [`key_spec`], which overrides this row for the
    /// subcommand-scoped commands (ADR-0104).
    pub keys: KeySpec,
}

const RO_FAST: CmdFlags = CmdFlags::READONLY.union(CmdFlags::FAST);
const W_FAST: CmdFlags = CmdFlags::WRITE.union(CmdFlags::FAST);
/// DENYOOM membership mirrors Redis 8 per command (oracle-pinned by the
/// M1-S07 compat cases) — writes that free or only re-time memory (DEL,
/// EXPIRE, PERSIST, GETDEL, GETEX, RENAME, FLUSH*) stay allowed under OOM.
const W_OOM: CmdFlags = CmdFlags::WRITE.union(CmdFlags::DENYOOM);
/// Loading-allowed membership mirrors observed Redis 8.0.5 (M2-S15 capture
/// artifact) — notably PING is NOT loading-allowed there.
const LOADING_FAST: CmdFlags = CmdFlags::FAST.union(CmdFlags::LOADING);
const LOADING_ADMIN: CmdFlags = CmdFlags::ADMIN.union(CmdFlags::LOADING);
const LOADING_RO_FAST: CmdFlags = CmdFlags::READONLY.union(CmdFlags::FAST).union(CmdFlags::LOADING);
const W_FAST_OOM: CmdFlags = W_FAST.union(CmdFlags::DENYOOM);
const W_FAST_INTERNAL: CmdFlags = W_FAST.union(CmdFlags::INTERNAL);
const RO_FAST_INTERNAL: CmdFlags = RO_FAST.union(CmdFlags::INTERNAL);
const W_INTERNAL: CmdFlags = CmdFlags::WRITE.union(CmdFlags::INTERNAL);

/// One registry row (the array below stays readable at 91 entries).
const fn cmd(
    id: CommandId,
    name: &'static str,
    arity: i8,
    flags: CmdFlags,
    keys: KeySpec,
) -> CommandMeta {
    CommandMeta { id, name, arity, flags, keys }
}

/// The registry. M1+ append here (and only here) — the hash table below is
/// derived mechanically at compile time.
pub static COMMANDS: [CommandMeta; 91] = [
    cmd(CommandId::Ping, "PING", -1, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Echo, "ECHO", 2, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Hello, "HELLO", -1, LOADING_FAST, KeySpec::NONE),
    // QUIT: the server replies +OK and closes the connection after flushing
    // (handled in the plane, which owns the connection lifecycle).
    cmd(CommandId::Quit, "QUIT", 1, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Get, "GET", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Set, "SET", -3, W_OOM, KeySpec::ONE),
    cmd(CommandId::Setnx, "SETNX", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Setex, "SETEX", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::Psetex, "PSETEX", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::Getset, "GETSET", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Getdel, "GETDEL", 2, W_FAST, KeySpec::ONE),
    cmd(CommandId::Del, "DEL", -2, CmdFlags::WRITE, KeySpec::ALL_TRAILING),
    cmd(CommandId::Exists, "EXISTS", -2, RO_FAST, KeySpec::ALL_TRAILING),
    cmd(CommandId::Type, "TYPE", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Incr, "INCR", 2, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Decr, "DECR", 2, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::IncrBy, "INCRBY", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::DecrBy, "DECRBY", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Append, "APPEND", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Strlen, "STRLEN", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Expire, "EXPIRE", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Pexpire, "PEXPIRE", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Ttl, "TTL", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Pttl, "PTTL", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Persist, "PERSIST", 2, W_FAST, KeySpec::ONE),
    cmd(CommandId::Info, "INFO", -1, LOADING_ADMIN, KeySpec::NONE),
    cmd(CommandId::Command, "COMMAND", -1, LOADING_ADMIN, KeySpec::NONE),
    // ---- M1-S01 · string family ----
    cmd(CommandId::Mget, "MGET", -2, RO_FAST, KeySpec::ALL_TRAILING),
    cmd(CommandId::Mset, "MSET", -3, W_OOM, KeySpec::PAIRS),
    cmd(CommandId::Msetnx, "MSETNX", -3, W_OOM, KeySpec::PAIRS),
    cmd(CommandId::Getrange, "GETRANGE", 4, CmdFlags::READONLY, KeySpec::ONE),
    cmd(CommandId::Setrange, "SETRANGE", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::Getex, "GETEX", -2, W_FAST, KeySpec::ONE),
    cmd(CommandId::IncrByFloat, "INCRBYFLOAT", 3, W_FAST_OOM, KeySpec::ONE),
    cmd(CommandId::Substr, "SUBSTR", 4, CmdFlags::READONLY, KeySpec::ONE),
    // ---- M1-S02 · key management ----
    cmd(CommandId::Rename, "RENAME", 3, CmdFlags::WRITE, KeySpec::TWO),
    cmd(CommandId::Renamenx, "RENAMENX", 3, W_FAST, KeySpec::TWO),
    cmd(CommandId::Copy, "COPY", -3, W_OOM, KeySpec::TWO),
    cmd(CommandId::Touch, "TOUCH", -2, RO_FAST, KeySpec::ALL_TRAILING),
    cmd(CommandId::Unlink, "UNLINK", -2, W_FAST, KeySpec::ALL_TRAILING),
    cmd(CommandId::Dbsize, "DBSIZE", 1, RO_FAST, KeySpec::NONE),
    cmd(CommandId::Keys, "KEYS", 2, CmdFlags::READONLY, KeySpec::NONE),
    cmd(CommandId::Randomkey, "RANDOMKEY", 1, CmdFlags::READONLY, KeySpec::NONE),
    cmd(CommandId::Scan, "SCAN", -2, CmdFlags::READONLY, KeySpec::NONE),
    cmd(CommandId::Flushdb, "FLUSHDB", -1, CmdFlags::WRITE, KeySpec::NONE),
    cmd(CommandId::Flushall, "FLUSHALL", -1, CmdFlags::WRITE, KeySpec::NONE),
    cmd(CommandId::Object, "OBJECT", -2, CmdFlags::READONLY, KeySpec::SECOND),
    cmd(CommandId::Debug, "DEBUG", -2, LOADING_ADMIN, KeySpec::NONE),
    // ---- M1-S03 · expiry completion + server introspection ----
    cmd(CommandId::Expireat, "EXPIREAT", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Pexpireat, "PEXPIREAT", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Expiretime, "EXPIRETIME", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Pexpiretime, "PEXPIRETIME", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Select, "SELECT", 2, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Config, "CONFIG", -2, LOADING_ADMIN, KeySpec::NONE),
    cmd(CommandId::Client, "CLIENT", -2, LOADING_ADMIN, KeySpec::NONE),
    cmd(CommandId::Lolwut, "LOLWUT", -1, CmdFlags::READONLY, KeySpec::NONE),
    // ---- M1-E5 · pub/sub (channels are not keys: no slot routing, no
    // key specs — ownership is the plane's slot(channel) mapping) ----
    cmd(CommandId::Subscribe, "SUBSCRIBE", -2, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Unsubscribe, "UNSUBSCRIBE", -1, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Psubscribe, "PSUBSCRIBE", -2, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Punsubscribe, "PUNSUBSCRIBE", -1, LOADING_FAST, KeySpec::NONE),
    cmd(CommandId::Publish, "PUBLISH", 3, LOADING_FAST, KeySpec::NONE),
    cmd(
        CommandId::Pubsub,
        "PUBSUB",
        -2,
        CmdFlags::READONLY.union(CmdFlags::LOADING),
        KeySpec::NONE,
    ),
    // ---- M1-E4 · namespaces v1 ----
    cmd(CommandId::InfNs, "INF.NS", -2, CmdFlags::ADMIN, KeySpec::NONE),
    // ---- M2-E8 · checkpoint operator surface (M2-S20, ADR-0021 D6) ----
    cmd(CommandId::InfCkpt, "INF.CKPT", -1, CmdFlags::ADMIN, KeySpec::NONE),
    cmd(CommandId::Bgsave, "BGSAVE", -1, CmdFlags::ADMIN, KeySpec::NONE),
    // LASTSAVE loading membership is docs-derived (loading+stale in the
    // Redis command table), not capture-verified like the M2-S15 set —
    // recorded deviation.
    cmd(CommandId::Lastsave, "LASTSAVE", 1, LOADING_RO_FAST, KeySpec::NONE),
    // ---- internal fabric-program ops (INF.* extension namespace) ----
    cmd(CommandId::InfTake, "INF.TAKE", -2, W_FAST_INTERNAL, KeySpec::ONE),
    cmd(CommandId::InfPeek, "INF.PEEK", -2, RO_FAST_INTERNAL, KeySpec::ONE),
    cmd(CommandId::InfPut, "INF.PUT", -4, W_INTERNAL, KeySpec::ONE),
    // ---- M3-S11/S12 · `JSON.*` document family (ADR-0041 D6–D9).
    // DENYOOM membership mirrors the RedisJSON module declarations:
    // memory-growing writes deny under OOM; DEL/FORGET/CLEAR free.
    cmd(CommandId::JsonSet, "JSON.SET", -4, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonGet, "JSON.GET", -2, CmdFlags::READONLY, KeySpec::ONE),
    cmd(CommandId::JsonMget, "JSON.MGET", -3, CmdFlags::READONLY, KeySpec::ALL_BUT_LAST),
    cmd(CommandId::JsonDel, "JSON.DEL", -2, CmdFlags::WRITE, KeySpec::ONE),
    cmd(CommandId::JsonForget, "JSON.FORGET", -2, CmdFlags::WRITE, KeySpec::ONE),
    cmd(CommandId::JsonType, "JSON.TYPE", -2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::JsonNumIncrBy, "JSON.NUMINCRBY", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonNumMultBy, "JSON.NUMMULTBY", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonStrAppend, "JSON.STRAPPEND", -3, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonStrLen, "JSON.STRLEN", -2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::JsonToggle, "JSON.TOGGLE", -2, W_FAST, KeySpec::ONE),
    cmd(CommandId::JsonClear, "JSON.CLEAR", -2, CmdFlags::WRITE, KeySpec::ONE),
    // ---- M3-S13/S14 · array + object ops, MERGE (ADR-0042 D7). Same
    // DENYOOM rule: growth denies under OOM; POP/TRIM free. ARRAPPEND
    // keeps the RedisJSON optional-path quirk (argv == 3 ⇒ legacy root),
    // so its arity floor is 3 like STRAPPEND.
    cmd(CommandId::JsonArrAppend, "JSON.ARRAPPEND", -3, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonArrInsert, "JSON.ARRINSERT", -5, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonArrIndex, "JSON.ARRINDEX", -4, CmdFlags::READONLY, KeySpec::ONE),
    cmd(CommandId::JsonArrLen, "JSON.ARRLEN", -2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::JsonArrPop, "JSON.ARRPOP", -2, CmdFlags::WRITE, KeySpec::ONE),
    cmd(CommandId::JsonArrTrim, "JSON.ARRTRIM", 5, CmdFlags::WRITE, KeySpec::ONE),
    cmd(CommandId::JsonObjKeys, "JSON.OBJKEYS", -2, CmdFlags::READONLY, KeySpec::ONE),
    cmd(CommandId::JsonObjLen, "JSON.OBJLEN", -2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::JsonMerge, "JSON.MERGE", 4, W_OOM, KeySpec::ONE),
    cmd(CommandId::JsonDebug, "JSON.DEBUG", 3, RO_FAST, KeySpec::SECOND),
];

// ---- Compile-time perfect hash ---------------------------------------------

const BUCKET_BITS: u32 = 9;
const BUCKETS: usize = 1 << BUCKET_BITS;
/// Longest command name in the registry (verified in `build_table`).
const MAX_NAME_LEN: usize = 16;
/// Multiply-mix constants found offline over the registry's hash words: a
/// name's bucket is `(w0·M1 ^ w1·M2) >> (64 − BUCKET_BITS)`, and the const
/// builder below proves the pair collision-free at compile time, so a new
/// command that collides fails the build. Then re-search the pair over odd
/// constants from a seeded generator, so the search repeats; when a search
/// is exhausted, raise `BUCKET_BITS` (ADR-0042 D7), which doubles the table
/// (`BUCKETS` entries of one `Bucket` each) and keeps one probe per lookup.
const HASH_MULTIPLIER_LO: u64 = 0x4C19_0974_F5FB_1CA9;
const HASH_MULTIPLIER_HI: u64 = 0x0AD3_0175_CBB3_C0C5;
/// The hash's case fold: bit 5 cleared in every byte (`a-z` → `A-Z`), so
/// every letter case of a name probes one bucket; zero padding stays zero.
/// The fold is lossy by design (`.` and 0x0E fold equal, a space folds to
/// zero): it only chooses the bucket, and the verify in [`lookup`] is exact.
const FOLD_MASK: u64 = 0xDFDF_DFDF_DFDF_DFDF;

/// Packs a name (≤ 16 bytes) into two little-endian words, zero-padded and
/// not folded: the byte order `lookup` loads its input in.
#[inline(always)]
const fn pack_raw(name: &[u8]) -> [u64; 2] {
    let mut words = [0u64; 2];
    let mut i = 0;
    while i < name.len() {
        words[i / 8] |= (name[i] as u64) << ((i % 8) * 8);
        i += 1;
    }
    words
}

/// The verify's mask over a name's raw words: every bit of every byte but
/// bit 5 of a byte that is an ASCII letter, where its two cases differ. A
/// non-letter byte and the zero padding are compared whole.
#[inline(always)]
const fn care_mask(name: &[u8]) -> [u64; 2] {
    let mut care = [!0u64; 2];
    let mut i = 0;
    while i < name.len() {
        if name[i].is_ascii_alphabetic() {
            care[i / 8] &= !(0x20u64 << ((i % 8) * 8));
        }
        i += 1;
    }
    care
}

/// The hash's input: the raw words with bit 5 cleared in every byte and the
/// length XORed into the high word. Build and probe both reach the hash
/// through this one function, so every letter case of a name lands in its
/// bucket.
#[inline(always)]
const fn fold_for_hash(raw: [u64; 2], len: usize) -> [u64; 2] {
    [raw[0] & FOLD_MASK, (raw[1] & FOLD_MASK) ^ len as u64]
}

/// A name's hash words. The registry's names pack to distinct pairs, which
/// a collision-free multiplier pair needs and `build_table` checks; other
/// inputs may share a name's pair, and the verify refuses them.
#[inline(always)]
const fn pack_folded(name: &[u8]) -> [u64; 2] {
    fold_for_hash(pack_raw(name), name.len())
}

#[inline(always)]
const fn hash_words(words: [u64; 2]) -> usize {
    ((words[0].wrapping_mul(HASH_MULTIPLIER_LO) ^ words[1].wrapping_mul(HASH_MULTIPLIER_HI))
        >> (64 - BUCKET_BITS)) as usize
}

/// One bucket of the table: a registry name's raw words, its care mask and
/// its length, precomputed so the verify reads the probed entry alone, and
/// its row. An empty bucket has length 0, which no probed input has, and
/// no row.
#[derive(Clone, Copy)]
struct Bucket {
    name: [u64; 2],
    care: [u64; 2],
    len: u8,
    row: Option<&'static CommandMeta>,
}

impl Bucket {
    /// The verify: `Some(row)` exactly when the input of `len` bytes, whose
    /// zero-padded little-endian words are `raw`, is this bucket's name in
    /// some letter case. The length first, then the bytes: where the name
    /// has a letter, the input's byte may differ from it in the case bit
    /// alone; every other byte, the zero padding included, must be equal.
    /// An empty bucket has length 0, which no probed input has.
    #[inline(always)]
    fn resolve(&self, raw: [u64; 2], len: usize) -> Option<&'static CommandMeta> {
        let differ =
            ((raw[0] ^ self.name[0]) & self.care[0]) | ((raw[1] ^ self.name[1]) & self.care[1]);
        if len != usize::from(self.len) || differ != 0 {
            return None;
        }
        self.row
    }
}

/// `bucket → Bucket`, built at compile time. Collisions fail the build.
static TABLE: [Bucket; BUCKETS] = build_table();

const fn build_table() -> [Bucket; BUCKETS] {
    const EMPTY: Bucket = Bucket { name: [0; 2], care: [0; 2], len: 0, row: None };
    let mut table = [EMPTY; BUCKETS];
    let mut i = 0;
    while i < COMMANDS.len() {
        let name = COMMANDS[i].name.as_bytes();
        assert!(name.len() <= MAX_NAME_LEN, "command name exceeds MAX_NAME_LEN");
        let bucket = hash_words(pack_folded(name));
        assert!(table[bucket].row.is_none(), "perfect-hash collision — re-search the multipliers");
        table[bucket] = Bucket {
            name: pack_raw(name),
            care: care_mask(name),
            // Exact: the assert above bounds the length by MAX_NAME_LEN.
            len: name.len() as u8,
            row: Some(&COMMANDS[i]),
        };
        i += 1;
    }
    table
}

/// Case-insensitive O(1) command lookup: `Some(row)` exactly when `name`
/// equals a registry name under ASCII case folding of letters — the same
/// length, and byte for byte equal with `a`–`z` read as `A`–`Z`. `None` for
/// anything not in the registry. One ≤ 16-byte copy, two multiplies, one
/// probe, then the length and the care-masked bytes.
#[inline]
pub fn lookup(name: &[u8]) -> Option<&'static CommandMeta> {
    let len = name.len();
    if len == 0 || len > MAX_NAME_LEN {
        return None;
    }
    let mut padded = [0u8; 16];
    // Fixed-length arms: each lowers to plain register loads. A
    // runtime-length `copy_from_slice` lowered to a memcpy call that
    // dominated the whole probe (measured on Raptor Lake, see the wire
    // bench artifact).
    match len {
        1 => padded[..1].copy_from_slice(name),
        2 => padded[..2].copy_from_slice(name),
        3 => padded[..3].copy_from_slice(name),
        4 => padded[..4].copy_from_slice(name),
        5 => padded[..5].copy_from_slice(name),
        6 => padded[..6].copy_from_slice(name),
        7 => padded[..7].copy_from_slice(name),
        8 => padded[..8].copy_from_slice(name),
        9 => padded[..9].copy_from_slice(name),
        10 => padded[..10].copy_from_slice(name),
        11 => padded[..11].copy_from_slice(name),
        12 => padded[..12].copy_from_slice(name),
        13 => padded[..13].copy_from_slice(name),
        14 => padded[..14].copy_from_slice(name),
        15 => padded[..15].copy_from_slice(name),
        _ => padded.copy_from_slice(name),
    }
    let raw = [
        u64::from_le_bytes(padded[..8].try_into().expect("8 bytes")),
        u64::from_le_bytes(padded[8..].try_into().expect("8 bytes")),
    ];
    // Nothing else that shares a name's hash words — a longer spelling whose
    // added bytes fold away, a non-letter one bit 5 off — resolves.
    TABLE[hash_words(fold_for_hash(raw, len))].resolve(raw, len)
}

// ---- Key extraction ----------------------------------------------------------

/// Iterator over the key arguments of one parsed command, driven purely by
/// the [`KeySpec`] — never per-command ad-hoc parsing.
#[derive(Debug)]
pub struct KeyIter<'v, 'a> {
    argv: &'v ArgvRef<'a>,
    next: usize,
    last: usize,
    step: usize,
}

impl<'a> Iterator for KeyIter<'_, 'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.step == 0 || self.next > self.last || self.next >= self.argv.len() {
            return None;
        }
        let key = self.argv.arg(self.next);
        self.next += self.step;
        Some(key)
    }
}

/// The key spec the router reads for one parsed command: the registry row,
/// or the subcommand-scoped row for the commands whose key position
/// depends on argv[1] (ADR-0104). `subcommand` is argv[1] when present.
///
/// Exactly one command has a scoped row today. `DEBUG` copies Redis's
/// keyless table row, but Redis never routes; here `DEBUG OBJECT <key>`
/// addresses argv[2] like its sibling `OBJECT ENCODING <key>`, while
/// `SLEEP <secs>` / `JMAP` / `SET-ACTIVE-EXPIRE <0|1>` address nothing —
/// so `DEBUG SLEEP 0.5` must never route on the bytes `"0.5"`. Every
/// consumer of key positions (`extract_keys`, the plane's owner pass,
/// `COMMAND GETKEYS`) reads this function, never `meta.keys` directly:
/// review of 2026-08-30, L12-01 was two notions of a command's keys
/// disagreeing about one argv shape (the last member of C1's class).
#[inline]
#[allow(clippy::wildcard_enum_match_arm, reason = "ADR-0143: column key_spec")]
pub fn key_spec(meta: &CommandMeta, subcommand: Option<&[u8]>) -> KeySpec {
    match meta.id {
        CommandId::Debug => match subcommand {
            Some(sub) if sub.eq_ignore_ascii_case(b"OBJECT") => KeySpec::SECOND,
            _ => KeySpec::NONE,
        },
        _ => meta.keys,
    }
}

/// What a command addresses in the connection's selected keyspace (the
/// numbered database or the named namespace an `INF.NS USE` bound). The
/// plane's namespace-bound dispatch reads this to decide which commands
/// the namespace's engine arm must see and which execute the same
/// whatever the connection selected (ADR-0108; review of 2026-08-30,
/// batch-8 residual: a tiered-bound connection answered `PING`, `ECHO`,
/// `CLIENT`, `INFO`, … with "not supported on tiered namespaces" because
/// every command the string family did not name fell into the tiered
/// arm's refusal — a limitation recorded where a class was missing).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KeyspaceScope {
    /// Nothing in the keyspace: connection or node state (`PING`, `HELLO`,
    /// `INFO`, `CLIENT`, `CONFIG GET`, `INF.NS LIST`, pub/sub, …). Such a
    /// command executes identically on every binding.
    None,
    /// The keys its [`key_spec`] names (`GET`, `MSET`, `DEBUG OBJECT`).
    Keys,
    /// The whole selected keyspace without naming a key (`DBSIZE`, `KEYS`,
    /// `SCAN`, `RANDOMKEY`, `FLUSHDB`, `FLUSHALL`): the scatter programs.
    Whole,
}

/// The [`KeyspaceScope`] of one parsed command. `subcommand` is argv[1]
/// when present (the subcommand-scoped rows read it — ADR-0104). Total
/// over the registry: a row is `Whole` by membership in the fixed scatter
/// set, `Keys` when its scoped key spec names a position, `None`
/// otherwise — so a new registry row lands in a class by construction,
/// and the plane cannot leave it to a catch-all refusal.
#[inline]
pub fn keyspace_scope(meta: &CommandMeta, subcommand: Option<&[u8]>) -> KeyspaceScope {
    if matches!(
        meta.id,
        CommandId::Dbsize
            | CommandId::Keys
            | CommandId::Scan
            | CommandId::Randomkey
            | CommandId::Flushdb
            | CommandId::Flushall
    ) {
        return KeyspaceScope::Whole;
    }
    if key_spec(meta, subcommand).first == 0 { KeyspaceScope::None } else { KeyspaceScope::Keys }
}

/// Extracts the key slices of `argv` per [`key_spec`]. Robust against
/// malformed arity (an argv shorter than the spec yields fewer keys — arity
/// validation rejects the command separately).
pub fn extract_keys<'v, 'a>(meta: &CommandMeta, argv: &'v ArgvRef<'a>) -> KeyIter<'v, 'a> {
    let spec = key_spec(meta, (argv.len() > 1).then(|| argv.arg(1)));
    if spec.first == 0 || argv.is_empty() {
        return KeyIter { argv, next: 1, last: 0, step: 0 };
    }
    let last = if spec.last >= 0 {
        spec.last as usize
    } else {
        // Counting from the end: -1 = final argument.
        argv.len().saturating_sub(spec.last.unsigned_abs() as usize)
    };
    KeyIter { argv, next: usize::from(spec.first), last, step: usize::from(spec.step) }
}

/// Arity check per the Redis convention. `argc` includes the command name.
#[inline]
pub fn arity_ok(meta: &CommandMeta, argc: usize) -> bool {
    if meta.arity >= 0 {
        argc == meta.arity as usize
    } else {
        argc >= meta.arity.unsigned_abs() as usize
    }
}

#[allow(clippy::disallowed_types, reason = "test-only: std containers in test code (ADR-0163 D2)")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_resolves_in_any_case() {
        for meta in &COMMANDS {
            let upper = meta.name.as_bytes();
            let lower = meta.name.to_ascii_lowercase();
            let mixed: Vec<u8> = upper
                .iter()
                .enumerate()
                .map(|(i, b)| if i % 2 == 0 { b.to_ascii_lowercase() } else { *b })
                .collect();
            for probe in [upper, lower.as_bytes(), &mixed] {
                let hit = lookup(probe)
                    .unwrap_or_else(|| panic!("{} failed to resolve from {probe:?}", meta.name));
                assert_eq!(hit.id, meta.id);
            }
        }
    }

    #[test]
    fn near_misses_do_not_resolve() {
        for probe in [
            &b"GETX"[..],
            b"GE",
            b"",
            b"PINGG",
            b"@ET",
            b"\x00\x00\x00",
            b"SETEXX",
            b"XCOMMAND",
            b"GETRANGEX",
            b"INCRBYFLOA",
            b"INCRBYFLOATT",
            b"PEXPIRETIMEX",
            b"INF.TAKEN",
            b"AAAAAAAAAAAAAAAAA", // 17 bytes — over MAX_NAME_LEN
            // Each shares a registry name's hash words, so it probes that
            // name's bucket and only the verify refuses it: added bytes that
            // fold to zero with a ninth byte that cancels the length change,
            // and a non-letter that differs from the name's byte in bit 5.
            b"PING    -",
            b"GET     *",
            b"SUBSCRIBF ",
            b"JSON\x0eGET",
            b"inf\x0ens",
            b"PING\0\0\0\0\r",
        ] {
            assert!(lookup(probe).is_none(), "{probe:?} must not resolve");
        }
    }

    /// The rule `lookup` serves, as the standard library states it: the
    /// registry row whose name equals the input under ASCII case folding.
    fn oracle(input: &[u8]) -> Option<CommandId> {
        COMMANDS.iter().find(|m| m.name.as_bytes().eq_ignore_ascii_case(input)).map(|m| m.id)
    }

    /// The spelling one byte longer than `name` that packs to the name's
    /// hash words. A name of at most 8 bytes is padded to 8 with `fill`
    /// (a space or NUL: both fold to zero) and followed by the byte that
    /// cancels the length change in the high word; a longer name changes
    /// its ninth byte by the length change and gains a trailing `fill`.
    fn length_twin(name: &[u8], fill: u8) -> Vec<u8> {
        let len = name.len();
        let mut twin = name.to_vec();
        if len <= 8 {
            twin.resize(8, fill);
            // The high word held the length alone; a space keeps bit 5 set.
            let cancel = u8::try_from(len ^ 9).expect("a short name") | (fill & 0x20);
            twin.push(cancel);
        } else {
            twin[8] ^= u8::try_from(len ^ (len + 1)).expect("a name is at most 16 bytes");
            twin.push(fill);
        }
        twin
    }

    /// `name` with each `.` replaced by 0x0E, which differs from it in bit 5
    /// alone; `None` for a name with no `.`.
    fn bit5_twin(name: &[u8]) -> Option<Vec<u8>> {
        name.contains(&b'.')
            .then(|| name.iter().map(|&b| if b == b'.' { 0x0E } else { b }).collect())
    }

    /// Upper, lower and alternating case, then each letter flipped alone.
    fn case_spellings(name: &[u8]) -> Vec<Vec<u8>> {
        let mut spellings = vec![
            name.to_vec(),
            name.to_ascii_lowercase(),
            name.iter()
                .enumerate()
                .map(|(i, b)| if i % 2 == 0 { b.to_ascii_lowercase() } else { *b })
                .collect(),
        ];
        for (i, b) in name.iter().enumerate().filter(|(_, b)| b.is_ascii_alphabetic()) {
            let mut flipped = name.to_vec();
            flipped[i] = b ^ 0x20;
            spellings.push(flipped);
        }
        spellings
    }

    /// Each position replaced by each substitute; then the name one byte
    /// shorter, and one byte longer by each fixed substitute.
    fn substitutions_and_lengths(name: &[u8]) -> Vec<Vec<u8>> {
        const FIXED: [u8; 5] = [0x00, 0x20, 0x0D, 0x0A, 0xFF];
        let mut spellings = Vec::new();
        for (i, &b) in name.iter().enumerate() {
            for substitute in [b ^ 0x20, 0x80 | b, b ^ 0x01].into_iter().chain(FIXED) {
                let mut changed = name.to_vec();
                changed[i] = substitute;
                spellings.push(changed);
            }
        }
        spellings.push(name[..name.len() - 1].to_vec());
        for extra in FIXED {
            let mut longer = name.to_vec();
            longer.push(extra);
            spellings.push(longer);
        }
        spellings
    }

    /// Every spelling class a lossy verify confuses, judged against the
    /// oracle: the case spellings resolve; the length twins (space and NUL
    /// fill), the bit-5 twins, the substitutions, the lengths one off and
    /// the bounds resolve only where the oracle says. Each twin is checked
    /// to pack to its name's hash words first, so a drifted generator reads
    /// red instead of proving nothing.
    #[test]
    fn lookup_agrees_with_case_insensitive_equality_on_every_spelling_class() {
        assert_eq!(oracle(b"ping"), Some(CommandId::Ping));
        assert_eq!(oracle(b"PING    -"), None);
        let mut spellings = Vec::new();
        for meta in &COMMANDS {
            let name = meta.name.as_bytes();
            let twins = [length_twin(name, b' '), length_twin(name, 0)];
            for twin in twins.into_iter().chain(bit5_twin(name)) {
                assert_eq!(pack_folded(&twin), pack_folded(name), "{twin:?} is no twin");
                spellings.push(twin);
            }
            spellings.extend(case_spellings(name));
            spellings.extend(substitutions_and_lengths(name));
        }
        // The bounds: empty; a name padded with spaces to the longest a name
        // may be, and one byte past it; eight spaces and the byte that folds
        // the length away, whose hash words are (0, 0).
        let mut padded = b"PING".to_vec();
        padded.resize(MAX_NAME_LEN, b' ');
        let over = [padded.as_slice(), b"P"].concat();
        spellings.extend([Vec::new(), padded, over, b"        \x09".to_vec()]);
        // The NUL extensions: each name and 1 to `MAX_NAME_LEN - len` NUL
        // bytes, which share the name's raw words.
        for meta in &COMMANDS {
            let name = meta.name.as_bytes();
            for len in name.len() + 1..=MAX_NAME_LEN {
                let mut extended = name.to_vec();
                extended.resize(len, 0);
                spellings.push(extended);
            }
        }
        let wrong: Vec<&Vec<u8>> =
            spellings.iter().filter(|s| lookup(s).map(|m| m.id) != oracle(s)).collect();
        let distinct: std::collections::HashSet<&Vec<u8>> = wrong.iter().copied().collect();
        assert!(
            wrong.is_empty(),
            "{} of {} spellings disagree with the oracle ({} distinct), first {:?}",
            wrong.len(),
            spellings.len(),
            distinct.len(),
            &wrong[..wrong.len().min(4)]
        );
    }

    /// A NUL extension of a name (the name and 1 to `MAX_NAME_LEN - len`
    /// NUL bytes) has the name's raw words, so only the length tells them
    /// apart. The length is mixed into the hash and no such input probes
    /// its name's bucket in this table, so `lookup` cannot reach the case:
    /// the probe is built here, on the name's own bucket, and the verify
    /// must refuse it.
    #[test]
    fn the_verify_refuses_a_nul_extension_on_its_names_bucket() {
        for meta in &COMMANDS {
            let name = meta.name.as_bytes();
            let bucket = &TABLE[hash_words(pack_folded(name))];
            let row = bucket.resolve(pack_raw(name), name.len()).map(|m| m.id);
            assert_eq!(row, Some(meta.id), "{} resolves on its bucket", meta.name);
            for len in name.len() + 1..=MAX_NAME_LEN {
                let extension = bucket.resolve(pack_raw(name), len);
                assert!(extension.is_none(), "{} and {} NUL bytes", meta.name, len - name.len());
            }
        }
    }

    #[test]
    fn registry_ids_are_unique_and_table_is_perfect() {
        // The const builder already fails the build on collision; assert the
        // runtime view agrees and ids are distinct.
        let mut seen = std::collections::HashSet::new();
        for meta in &COMMANDS {
            assert!(seen.insert(meta.id), "duplicate id {:?}", meta.id);
            assert_eq!(lookup(meta.name.as_bytes()).expect("resolves").id, meta.id);
        }
    }

    /// ADR-0108: every registry row has exactly one keyspace scope, the
    /// `Whole` set is the scatter set and nothing else, and `None` means
    /// "no key position" — the property the plane's namespace-bound
    /// dispatch relies on. Iterates the table so a new row cannot land
    /// unclassified (review of 2026-08-30, Theme 3: assert the class,
    /// not one command).
    #[test]
    fn every_command_has_one_keyspace_scope() {
        let whole: &[CommandId] = &[
            CommandId::Dbsize,
            CommandId::Keys,
            CommandId::Scan,
            CommandId::Randomkey,
            CommandId::Flushdb,
            CommandId::Flushall,
        ];
        let mut none = 0;
        let mut keys = 0;
        for meta in &COMMANDS {
            let scope = keyspace_scope(meta, None);
            if whole.contains(&meta.id) {
                assert_eq!(scope, KeyspaceScope::Whole, "{}", meta.name);
                assert_eq!(meta.keys, KeySpec::NONE, "{} names no key position", meta.name);
                continue;
            }
            match scope {
                KeyspaceScope::Whole => panic!("{} is not a scatter program", meta.name),
                KeyspaceScope::Keys => {
                    assert_ne!(meta.keys.first, 0, "{}", meta.name);
                    keys += 1;
                }
                KeyspaceScope::None => {
                    assert_eq!(meta.keys, KeySpec::NONE, "{}", meta.name);
                    none += 1;
                }
            }
        }
        assert_eq!(none + keys + whole.len(), COMMANDS.len());
        // The subcommand-scoped row (ADR-0104): `DEBUG OBJECT` addresses a
        // key, every other `DEBUG` form addresses nothing.
        let debug = lookup(b"DEBUG").expect("registered");
        assert_eq!(keyspace_scope(debug, Some(b"OBJECT")), KeyspaceScope::Keys);
        assert_eq!(keyspace_scope(debug, Some(b"object")), KeyspaceScope::Keys);
        assert_eq!(keyspace_scope(debug, Some(b"SLEEP")), KeyspaceScope::None);
        assert_eq!(keyspace_scope(debug, None), KeyspaceScope::None);
        // The connection-level rows the batch-8 residual named.
        for name in [&b"PING"[..], b"ECHO", b"HELLO", b"QUIT", b"CLIENT", b"INFO", b"COMMAND"] {
            let meta = lookup(name).expect("registered");
            assert_eq!(keyspace_scope(meta, None), KeyspaceScope::None, "{}", meta.name);
        }
    }

    #[test]
    fn pair_and_two_key_specs_extract_correct_keys() {
        use crate::parser::{ConnParser, Parsed, ParserLimits};
        type Case = (&'static [&'static [u8]], &'static [&'static [u8]]);
        let cases: &[Case] = &[
            (&[b"MSET", b"k1", b"v1", b"k2", b"v2"], &[b"k1", b"k2"]),
            (&[b"RENAME", b"src", b"dst"], &[b"src", b"dst"]),
            (&[b"COPY", b"src", b"dst", b"REPLACE"], &[b"src", b"dst"]),
            (&[b"OBJECT", b"ENCODING", b"key"], &[b"key"]),
            (&[b"OBJECT", b"HELP"], &[]),
            (&[b"MGET", b"a", b"b", b"c"], &[b"a", b"b", b"c"]),
        ];
        for (parts, want) in cases {
            let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
            for p in *parts {
                wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
                wire.extend_from_slice(p);
                wire.extend_from_slice(b"\r\n");
            }
            let mut parser = ConnParser::new(ParserLimits::default());
            let mut iter = parser.feed(&wire);
            let Some(Parsed::Command(argv)) = iter.next() else { panic!("one command") };
            let meta = lookup(parts[0]).expect("registered");
            let keys: Vec<&[u8]> = extract_keys(meta, &argv).collect();
            assert_eq!(&keys, want, "{}", meta.name);
        }
    }
}
