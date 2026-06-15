//! Command metadata + perfect-hash dispatch (M0-S12, schema frozen at M0
//! exit): name, arity, flags, and key spec — the slot router reads keys from
//! here, and M1+ (ACL, cluster, Lua) extend the registry without touching
//! dispatch internals.
//!
//! Dispatch is a compile-time perfect hash: case-fold the name, one
//! multiply-mix hash, one table probe, one short verify-compare — no
//! branching scans, no allocation. The table is built in `const` context and
//! the build **fails to compile** on any bucket collision, so adding a
//! command can never silently degrade lookup into a collision chain.
//!
//! M1 surface growth (M1-E1): names now pack into **two** folded u64 words
//! (≤ 16 bytes — `INCRBYFLOAT`/`PEXPIRETIME` are 11) and the table holds 256
//! buckets. The lookup cost is unchanged in shape: two register loads, two
//! multiplies, one xor, one probe, one two-word compare.

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
    /// Internal cross-cell program op: atomically read value+TTL and delete
    /// at the owning cell (the RENAME/MOVE fabric-program primitive). Not a
    /// Redis command; listed in `COMMAND` output as an `INF.*` extension.
    InfTake,
    /// Internal cross-cell program op: atomically read value+TTL (COPY).
    InfPeek,
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
    pub keys: KeySpec,
}

const RO_FAST: CmdFlags = CmdFlags::READONLY.union(CmdFlags::FAST);
const W_FAST: CmdFlags = CmdFlags::WRITE.union(CmdFlags::FAST);
/// DENYOOM membership mirrors Redis 8 per command (oracle-pinned by the
/// M1-S07 compat cases) — writes that free or only re-time memory (DEL,
/// EXPIRE, PERSIST, GETDEL, GETEX, RENAME, FLUSH*) stay allowed under OOM.
const W_OOM: CmdFlags = CmdFlags::WRITE.union(CmdFlags::DENYOOM);
const W_FAST_OOM: CmdFlags = W_FAST.union(CmdFlags::DENYOOM);

/// One registry row (the array below stays readable at 58 entries).
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
pub static COMMANDS: [CommandMeta; 65] = [
    cmd(CommandId::Ping, "PING", -1, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Echo, "ECHO", 2, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Hello, "HELLO", -1, CmdFlags::FAST, KeySpec::NONE),
    // QUIT: the server replies +OK and closes the connection after flushing
    // (handled in the plane, which owns the connection lifecycle).
    cmd(CommandId::Quit, "QUIT", 1, CmdFlags::FAST, KeySpec::NONE),
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
    cmd(CommandId::Info, "INFO", -1, CmdFlags::ADMIN, KeySpec::NONE),
    cmd(CommandId::Command, "COMMAND", -1, CmdFlags::ADMIN, KeySpec::NONE),
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
    cmd(CommandId::Debug, "DEBUG", -2, CmdFlags::ADMIN, KeySpec::NONE),
    // ---- M1-S03 · expiry completion + server introspection ----
    cmd(CommandId::Expireat, "EXPIREAT", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Pexpireat, "PEXPIREAT", -3, W_FAST, KeySpec::ONE),
    cmd(CommandId::Expiretime, "EXPIRETIME", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Pexpiretime, "PEXPIRETIME", 2, RO_FAST, KeySpec::ONE),
    cmd(CommandId::Select, "SELECT", 2, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Config, "CONFIG", -2, CmdFlags::ADMIN, KeySpec::NONE),
    cmd(CommandId::Client, "CLIENT", -2, CmdFlags::ADMIN, KeySpec::NONE),
    cmd(CommandId::Lolwut, "LOLWUT", -1, CmdFlags::READONLY, KeySpec::NONE),
    // ---- M1-E5 · pub/sub (channels are not keys: no slot routing, no
    // key specs — ownership is the plane's slot(channel) mapping) ----
    cmd(CommandId::Subscribe, "SUBSCRIBE", -2, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Unsubscribe, "UNSUBSCRIBE", -1, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Psubscribe, "PSUBSCRIBE", -2, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Punsubscribe, "PUNSUBSCRIBE", -1, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Publish, "PUBLISH", 3, CmdFlags::FAST, KeySpec::NONE),
    cmd(CommandId::Pubsub, "PUBSUB", -2, CmdFlags::READONLY, KeySpec::NONE),
    // ---- M1-E4 · namespaces v1 ----
    cmd(CommandId::InfNs, "INF.NS", -2, CmdFlags::ADMIN, KeySpec::NONE),
    // ---- internal fabric-program ops (INF.* extension namespace) ----
    cmd(CommandId::InfTake, "INF.TAKE", 2, W_FAST, KeySpec::ONE),
    cmd(CommandId::InfPeek, "INF.PEEK", 2, RO_FAST, KeySpec::ONE),
];

// ---- Compile-time perfect hash ---------------------------------------------

const BUCKET_BITS: u32 = 8;
const BUCKETS: usize = 1 << BUCKET_BITS;
/// Longest command name in the registry (verified in `build_table`).
const MAX_NAME_LEN: usize = 16;
/// Multiply-mix constants found offline over the packed name word pairs; the
/// const builder below proves them collision-free at compile time, so a new
/// command that breaks them fails the build (re-search the constants then —
/// `(w0·M1 ^ w1·M2) >> 56` over random odd pairs; the M1-E5 pub/sub growth
/// to 64 names re-searched in ~1k attempts).
const HASH_MULTIPLIER_LO: u64 = 0x1FC5_3112_C1E2_07B5;
const HASH_MULTIPLIER_HI: u64 = 0xF76D_1FD1_8160_AEBB;
/// Word-wide ASCII case fold (`a-z` → `A-Z`); zero padding stays zero.
/// Non-letters map somewhere harmless — the verify word-compare against the
/// canonical name rejects any non-command byte sequence.
const FOLD_MASK: u64 = 0xDFDF_DFDF_DFDF_DFDF;

/// Packs a name (≤ 16 bytes) into two folded little-endian words; the length
/// mixes into the high word. Distinct names ⇒ distinct word pairs, so a
/// collision-free multiplier pair always exists.
#[inline(always)]
const fn pack_folded(name: &[u8]) -> (u64, u64) {
    let mut w0 = 0u64;
    let mut w1 = 0u64;
    let mut i = 0;
    while i < name.len() {
        if i < 8 {
            w0 |= (name[i] as u64) << (i * 8);
        } else {
            w1 |= (name[i] as u64) << ((i - 8) * 8);
        }
        i += 1;
    }
    (w0 & FOLD_MASK, (w1 & FOLD_MASK) ^ name.len() as u64)
}

#[inline(always)]
const fn hash_words(w0: u64, w1: u64) -> usize {
    ((w0.wrapping_mul(HASH_MULTIPLIER_LO) ^ w1.wrapping_mul(HASH_MULTIPLIER_HI))
        >> (64 - BUCKET_BITS)) as usize
}

/// `bucket → (canonical packed words, command index + 1)` (index 0 = empty).
/// The packed words are precomputed so the lookup verify is two u64 compares
/// against the probed entry — no per-lookup repack of the canonical name.
/// Collisions fail the build.
static TABLE: [(u64, u64, u8); BUCKETS] = build_table();

const fn build_table() -> [(u64, u64, u8); BUCKETS] {
    let mut table = [(0u64, 0u64, 0u8); BUCKETS];
    let mut i = 0;
    while i < COMMANDS.len() {
        let name = COMMANDS[i].name.as_bytes();
        assert!(name.len() <= MAX_NAME_LEN, "command name exceeds MAX_NAME_LEN");
        let (w0, w1) = pack_folded(name);
        let bucket = hash_words(w0, w1);
        assert!(table[bucket].2 == 0, "perfect-hash collision — re-search the multipliers");
        table[bucket] = (w0, w1, (i + 1) as u8);
        i += 1;
    }
    table
}

/// Case-insensitive O(1) command lookup: pack+fold (one ≤16-byte copy), two
/// multiplies, one probe, one two-word compare. `None` for anything not in
/// the registry.
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
    let w0 = u64::from_le_bytes(padded[..8].try_into().expect("8 bytes")) & FOLD_MASK;
    let w1 =
        (u64::from_le_bytes(padded[8..].try_into().expect("8 bytes")) & FOLD_MASK) ^ len as u64;
    let (c0, c1, slot) = TABLE[hash_words(w0, w1)];
    // Two precomputed word compares verify name AND length (length is mixed
    // into the high packed word; empty buckets hold (0, 0), which no name
    // packs to because the length mix is non-zero).
    if w0 != c0 || w1 != c1 || slot == 0 {
        return None;
    }
    Some(&COMMANDS[(slot - 1) as usize])
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

/// Extracts the key slices of `argv` per `meta.keys`. Robust against
/// malformed arity (an argv shorter than the spec yields fewer keys — arity
/// validation rejects the command separately).
pub fn extract_keys<'v, 'a>(meta: &CommandMeta, argv: &'v ArgvRef<'a>) -> KeyIter<'v, 'a> {
    let spec = meta.keys;
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
        ] {
            assert!(lookup(probe).is_none(), "{probe:?} must not resolve");
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
