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

use crate::parser::ArgvRef;

/// Every command in the M0 surface (milestone M0-S15).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum CommandId {
    Ping,
    Echo,
    Hello,
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
    /// Single key at argv[1] — the dominant M0 shape.
    pub const ONE: KeySpec = KeySpec { first: 1, last: 1, step: 1 };
    /// Keys from argv[1] through the final argument (DEL, EXISTS).
    pub const ALL_TRAILING: KeySpec = KeySpec { first: 1, last: -1, step: 1 };
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

/// The registry. M1+ append here (and only here) — the hash table below is
/// derived mechanically at compile time.
pub static COMMANDS: [CommandMeta; 26] = [
    CommandMeta {
        id: CommandId::Ping,
        name: "PING",
        arity: -1,
        flags: CmdFlags::FAST,
        keys: KeySpec::NONE,
    },
    CommandMeta {
        id: CommandId::Echo,
        name: "ECHO",
        arity: 2,
        flags: CmdFlags::FAST,
        keys: KeySpec::NONE,
    },
    CommandMeta {
        id: CommandId::Hello,
        name: "HELLO",
        arity: -1,
        flags: CmdFlags::FAST,
        keys: KeySpec::NONE,
    },
    CommandMeta { id: CommandId::Get, name: "GET", arity: 2, flags: RO_FAST, keys: KeySpec::ONE },
    CommandMeta {
        id: CommandId::Set,
        name: "SET",
        arity: -3,
        flags: CmdFlags::WRITE,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Setnx,
        name: "SETNX",
        arity: 3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Setex,
        name: "SETEX",
        arity: 4,
        flags: CmdFlags::WRITE,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Psetex,
        name: "PSETEX",
        arity: 4,
        flags: CmdFlags::WRITE,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Getset,
        name: "GETSET",
        arity: 3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Getdel,
        name: "GETDEL",
        arity: 2,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Del,
        name: "DEL",
        arity: -2,
        flags: CmdFlags::WRITE,
        keys: KeySpec::ALL_TRAILING,
    },
    CommandMeta {
        id: CommandId::Exists,
        name: "EXISTS",
        arity: -2,
        flags: RO_FAST,
        keys: KeySpec::ALL_TRAILING,
    },
    CommandMeta { id: CommandId::Type, name: "TYPE", arity: 2, flags: RO_FAST, keys: KeySpec::ONE },
    CommandMeta { id: CommandId::Incr, name: "INCR", arity: 2, flags: W_FAST, keys: KeySpec::ONE },
    CommandMeta { id: CommandId::Decr, name: "DECR", arity: 2, flags: W_FAST, keys: KeySpec::ONE },
    CommandMeta {
        id: CommandId::IncrBy,
        name: "INCRBY",
        arity: 3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::DecrBy,
        name: "DECRBY",
        arity: 3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Append,
        name: "APPEND",
        arity: 3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Strlen,
        name: "STRLEN",
        arity: 2,
        flags: RO_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Expire,
        name: "EXPIRE",
        arity: -3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Pexpire,
        name: "PEXPIRE",
        arity: -3,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta { id: CommandId::Ttl, name: "TTL", arity: 2, flags: RO_FAST, keys: KeySpec::ONE },
    CommandMeta { id: CommandId::Pttl, name: "PTTL", arity: 2, flags: RO_FAST, keys: KeySpec::ONE },
    CommandMeta {
        id: CommandId::Persist,
        name: "PERSIST",
        arity: 2,
        flags: W_FAST,
        keys: KeySpec::ONE,
    },
    CommandMeta {
        id: CommandId::Info,
        name: "INFO",
        arity: -1,
        flags: CmdFlags::ADMIN,
        keys: KeySpec::NONE,
    },
    CommandMeta {
        id: CommandId::Command,
        name: "COMMAND",
        arity: -1,
        flags: CmdFlags::ADMIN,
        keys: KeySpec::NONE,
    },
];

// ---- Compile-time perfect hash ---------------------------------------------

const BUCKET_BITS: u32 = 6;
const BUCKETS: usize = 1 << BUCKET_BITS;
/// Longest command name in the registry (verified in `build_table`).
const MAX_NAME_LEN: usize = 7;
/// Multiply-shift constant found offline over the packed name words; the
/// const builder below proves it collision-free at compile time, so a new
/// command that breaks it fails the build (re-search the constant then).
const HASH_MULTIPLIER: u64 = 0x99C9_4309_570D_C195;
/// Word-wide ASCII case fold (`a-z` → `A-Z`); zero padding stays zero.
/// Non-letters map somewhere harmless — the verify word-compare against the
/// canonical name rejects any non-command byte sequence.
const FOLD_MASK: u64 = 0xDFDF_DFDF_DFDF_DFDF;

/// Packs a name (≤ 8 bytes) into a folded little-endian word mixed with its
/// length. Feature-complete by construction: distinct names ⇒ distinct
/// words, so a collision-free multiplier always exists.
#[inline(always)]
const fn pack_folded(name: &[u8]) -> u64 {
    let mut word = 0u64;
    let mut i = 0;
    while i < name.len() {
        word |= (name[i] as u64) << (i * 8);
        i += 1;
    }
    (word & FOLD_MASK) ^ name.len() as u64
}

#[inline(always)]
const fn hash_folded(name: &[u8]) -> usize {
    (pack_folded(name).wrapping_mul(HASH_MULTIPLIER) >> (64 - BUCKET_BITS)) as usize
}

/// `bucket → (canonical packed word, command index + 1)` (index 0 = empty).
/// The packed word is precomputed so the lookup verify is a single u64
/// compare against the probed entry — no per-lookup repack of the canonical
/// name. Collisions fail the build.
static TABLE: [(u64, u8); BUCKETS] = build_table();

const fn build_table() -> [(u64, u8); BUCKETS] {
    let mut table = [(0u64, 0u8); BUCKETS];
    let mut i = 0;
    while i < COMMANDS.len() {
        let name = COMMANDS[i].name.as_bytes();
        assert!(name.len() <= MAX_NAME_LEN, "command name exceeds MAX_NAME_LEN");
        let word = pack_folded(name);
        let bucket = hash_folded(name);
        assert!(table[bucket].1 == 0, "perfect-hash collision — adjust hash_folded mixers");
        table[bucket] = (word, (i + 1) as u8);
        i += 1;
    }
    table
}

/// Case-insensitive O(1) command lookup: pack+fold (one ≤8-byte copy), one
/// multiply, one probe, one word compare. `None` for anything not in the M0
/// surface.
#[inline]
pub fn lookup(name: &[u8]) -> Option<&'static CommandMeta> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return None;
    }
    let mut padded = [0u8; 8];
    padded[..name.len()].copy_from_slice(name);
    let word = (u64::from_le_bytes(padded) & FOLD_MASK) ^ name.len() as u64;
    let (canonical, slot) =
        TABLE[(word.wrapping_mul(HASH_MULTIPLIER) >> (64 - BUCKET_BITS)) as usize];
    // One precomputed word compare verifies name AND length (length is mixed
    // into the packed word; empty buckets hold word 0, which no name packs
    // to because the length mix is non-zero).
    if word != canonical || slot == 0 {
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
        for probe in
            [&b"GETX"[..], b"GE", b"", b"PINGG", b"@ET", b"\x00\x00\x00", b"SETEXX", b"XCOMMAND"]
        {
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
}
