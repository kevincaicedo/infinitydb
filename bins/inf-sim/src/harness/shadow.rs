//! F-L19-04 (review 2026-08-30): an **independent** model for the apply
//! oracle. The replay model is the product's own `Keyspace` behind the
//! product's own `execute_slices`, so a defect in either is computed on
//! both sides and byte-equal by construction. This model shares nothing
//! with them: a map per scope, Redis's documented semantics for the
//! commands the memory mixes drive, InfinityDB's declared record bounds
//! (`interfaces-m0.md` §6b: a write naming a key over 255 B or a value
//! over 16 MiB − 1 answers the typed error, a read or delete treats such
//! a key as absent; ADR-0098: an over-bound pair refuses the whole
//! command).
//! A command outside its vocabulary answers `None` and is counted, never
//! silently passed. Teeth: `scripts/sim-canaries.sh` plants
//! `--cfg inf_canary_reply_lie` in the product's `GET` and this model must
//! turn `m0-smoke` red where the replay stays green.

use std::collections::BTreeMap;

use inf_foundation::time::Nanos;
use inf_server::ExecScope;

/// InfinityDB's declared record bounds (record v0).
const MAX_KEY_LEN: usize = 255;
const MAX_VAL_LEN: usize = (1 << 24) - 1;
/// Redis's string ceiling (`proto-max-bulk-len` default, 512 MiB).
const MAX_STRING_LEN: u64 = 512 << 20;
const RECORD_BOUNDS: &str = "ERR key or value exceeds InfinityDB M0 record bounds";
const NOT_INT: &str = "ERR value is not an integer or out of range";

struct Entry {
    value: Vec<u8>,
    /// Unix-free millisecond deadline on the sim clock; the key is live
    /// while `now_ms <= deadline` (Redis: expired iff `now > when`).
    deadline_ms: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ScopeKey {
    Db(u16),
    Ns(u32),
}

#[derive(Default)]
pub struct Shadow {
    scopes: BTreeMap<ScopeKey, BTreeMap<Vec<u8>, Entry>>,
    /// Apply events this model answered (a run with zero proves nothing).
    pub checked: u64,
    /// Apply events outside the vocabulary, by command name.
    pub unmodeled: BTreeMap<String, u64>,
}

fn now_ms(now: Nanos) -> u64 {
    now.0 / 1_000_000
}

fn simple(s: &str) -> Vec<u8> {
    format!("+{s}\r\n").into_bytes()
}

fn error(s: &str) -> Vec<u8> {
    format!("-{s}\r\n").into_bytes()
}

fn int(n: i64) -> Vec<u8> {
    format!(":{n}\r\n").into_bytes()
}

fn bulk(v: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", v.len()).into_bytes();
    out.extend_from_slice(v);
    out.extend_from_slice(b"\r\n");
    out
}

fn null() -> Vec<u8> {
    b"$-1\r\n".to_vec()
}

/// Redis `string2ll`: an optional `-`, then `0` alone or a digit run
/// without a leading zero, fitting an i64.
fn parse_i64(s: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(s).ok()?;
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    if text == "-0" {
        return None;
    }
    text.parse().ok()
}

fn scope_key(scope: ExecScope) -> Option<ScopeKey> {
    match scope {
        ExecScope::Db(db) => Some(ScopeKey::Db(db)),
        ExecScope::Ns(ns) => Some(ScopeKey::Ns(ns.0)),
        ExecScope::Unavailable => None,
    }
}

impl Shadow {
    /// Replays one apply event; `None` = outside the vocabulary.
    pub fn apply(&mut self, scope: ExecScope, argv: &[&[u8]], now: Nanos) -> Option<Vec<u8>> {
        let key = scope_key(scope)?;
        let name = String::from_utf8_lossy(argv[0]).to_ascii_uppercase();
        let now = now_ms(now);
        let store = self.scopes.entry(key).or_default();
        let reply = match (name.as_str(), argv.len()) {
            ("PING", 1) => simple("PONG"),
            ("PING", 2) => bulk(argv[1]),
            ("GET", 2) => Store(store, now).get(argv[1]),
            ("SET", 3) => Store(store, now).set(argv[1], argv[2]),
            ("INCR", 2) => Store(store, now).incr(argv[1]),
            ("DEL", n) if n >= 2 => Store(store, now).del(&argv[1..]),
            ("EXISTS", n) if n >= 2 => Store(store, now).exists(&argv[1..]),
            ("APPEND", 3) => Store(store, now).append(argv[1], argv[2]),
            ("STRLEN", 2) => Store(store, now).strlen(argv[1]),
            ("TYPE", 2) => Store(store, now).type_of(argv[1]),
            ("EXPIRE", 3) => Store(store, now).expire(argv[1], argv[2], 1000),
            ("PEXPIRE", 3) => Store(store, now).expire(argv[1], argv[2], 1),
            ("TTL", 2) => Store(store, now).ttl(argv[1], 1000),
            ("PTTL", 2) => Store(store, now).ttl(argv[1], 1),
            ("MSET", n) if n >= 3 && n % 2 == 1 => Store(store, now).mset(&argv[1..], false),
            ("MSETNX", n) if n >= 3 && n % 2 == 1 => Store(store, now).mset(&argv[1..], true),
            ("GETRANGE", 4) => Store(store, now).getrange(argv[1], argv[2], argv[3]),
            ("SETRANGE", 4) => Store(store, now).setrange(argv[1], argv[2], argv[3]),
            _ => {
                *self.unmodeled.entry(name).or_insert(0) += 1;
                return None;
            }
        };
        self.checked += 1;
        Some(reply)
    }

    /// A `FLUSHDB` the node acknowledged on `scope`.
    pub fn flush_db(&mut self, scope: ExecScope) {
        if let Some(key) = scope_key(scope) {
            self.scopes.remove(&key);
        }
    }

    /// A `FLUSHALL` the node acknowledged: every db.
    pub fn flush_all(&mut self) {
        self.scopes.clear();
    }
}

/// One scope's map at one instant.
struct Store<'a>(&'a mut BTreeMap<Vec<u8>, Entry>, u64);

impl Store<'_> {
    /// The live entry, or `None`: absent, expired at this instant, or a
    /// key past the record bound (which no write can have stored).
    fn live(&mut self, key: &[u8]) -> Option<&mut Entry> {
        if key.len() > MAX_KEY_LEN {
            return None;
        }
        let now = self.1;
        if self.0.get(key).is_some_and(|e| e.deadline_ms.is_some_and(|d| now > d)) {
            self.0.remove(key);
        }
        self.0.get_mut(key)
    }

    fn get(&mut self, key: &[u8]) -> Vec<u8> {
        match self.live(key) {
            Some(e) => bulk(&e.value),
            None => null(),
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Vec<u8> {
        if key.len() > MAX_KEY_LEN || value.len() > MAX_VAL_LEN {
            return error(RECORD_BOUNDS);
        }
        self.0.insert(key.to_vec(), Entry { value: value.to_vec(), deadline_ms: None });
        simple("OK")
    }

    fn incr(&mut self, key: &[u8]) -> Vec<u8> {
        if key.len() > MAX_KEY_LEN {
            return error(RECORD_BOUNDS);
        }
        let current = match self.live(key) {
            Some(e) => match parse_i64(&e.value) {
                Some(n) => n,
                None => return error(NOT_INT),
            },
            None => 0,
        };
        let Some(next) = current.checked_add(1) else {
            return error("ERR increment or decrement would overflow");
        };
        let text = next.to_string().into_bytes();
        match self.live(key) {
            Some(e) => e.value = text,
            None => {
                self.0.insert(key.to_vec(), Entry { value: text, deadline_ms: None });
            }
        }
        int(next)
    }

    fn del(&mut self, keys: &[&[u8]]) -> Vec<u8> {
        let mut n = 0;
        for key in keys {
            if self.live(key).is_some() {
                self.0.remove(*key);
                n += 1;
            }
        }
        int(n)
    }

    fn exists(&mut self, keys: &[&[u8]]) -> Vec<u8> {
        let n = keys.iter().filter(|k| self.live(k).is_some()).count();
        int(n as i64)
    }

    fn append(&mut self, key: &[u8], tail: &[u8]) -> Vec<u8> {
        if key.len() > MAX_KEY_LEN {
            return error(RECORD_BOUNDS);
        }
        let new_len = match self.live(key) {
            Some(e) => e.value.len() + tail.len(),
            None => tail.len(),
        };
        if new_len as u64 > MAX_STRING_LEN {
            return error("ERR string exceeds maximum allowed size (proto-max-bulk-len)");
        }
        if new_len > MAX_VAL_LEN {
            return error(RECORD_BOUNDS);
        }
        match self.live(key) {
            Some(e) => e.value.extend_from_slice(tail),
            None => {
                self.0.insert(key.to_vec(), Entry { value: tail.to_vec(), deadline_ms: None });
            }
        }
        int(new_len as i64)
    }

    fn strlen(&mut self, key: &[u8]) -> Vec<u8> {
        int(self.live(key).map_or(0, |e| e.value.len() as i64))
    }

    fn type_of(&mut self, key: &[u8]) -> Vec<u8> {
        simple(if self.live(key).is_some() { "string" } else { "none" })
    }

    /// `EXPIRE`/`PEXPIRE` without options (Redis 7+: a past or zero
    /// deadline deletes the key and answers 1).
    fn expire(&mut self, key: &[u8], arg: &[u8], unit_ms: i64) -> Vec<u8> {
        if key.len() > MAX_KEY_LEN {
            return error(RECORD_BOUNDS);
        }
        let Some(n) = parse_i64(arg) else {
            return error(NOT_INT);
        };
        let overflow = || {
            error(&format!(
                "ERR invalid expire time in '{}' command",
                if unit_ms == 1000 { "expire" } else { "pexpire" }
            ))
        };
        let Some(relative) = n.checked_mul(unit_ms) else {
            return overflow();
        };
        let Some(when) = relative.checked_add(self.1 as i64) else {
            return overflow();
        };
        if self.live(key).is_none() {
            return int(0);
        }
        if when <= self.1 as i64 {
            self.0.remove(key);
        } else if let Some(e) = self.live(key) {
            e.deadline_ms = Some(when as u64);
        }
        int(1)
    }

    fn ttl(&mut self, key: &[u8], unit_ms: u64) -> Vec<u8> {
        let now = self.1;
        match self.live(key) {
            None => int(-2),
            Some(Entry { deadline_ms: None, .. }) => int(-1),
            Some(Entry { deadline_ms: Some(d), .. }) => {
                let left = d.saturating_sub(now);
                int(if unit_ms == 1 { left } else { (left + 500) / 1000 } as i64)
            }
        }
    }

    /// `MSET` (`+OK`) / `MSETNX` (`:1`, or `:0` when any key is live);
    /// an over-bound pair anywhere refuses the whole command (ADR-0098).
    fn mset(&mut self, pairs: &[&[u8]], only_if_absent: bool) -> Vec<u8> {
        if pairs.chunks(2).any(|p| p[0].len() > MAX_KEY_LEN || p[1].len() > MAX_VAL_LEN) {
            return error(RECORD_BOUNDS);
        }
        if only_if_absent && pairs.chunks(2).any(|p| self.live(p[0]).is_some()) {
            return int(0);
        }
        for p in pairs.chunks(2) {
            self.0.insert(p[0].to_vec(), Entry { value: p[1].to_vec(), deadline_ms: None });
        }
        if only_if_absent { int(1) } else { simple("OK") }
    }

    /// Redis `getrangeCommand`: negative offsets count from the end, the
    /// range clamps into the string, an inverted or empty range is empty.
    fn getrange(&mut self, key: &[u8], start: &[u8], end: &[u8]) -> Vec<u8> {
        let (Some(mut start), Some(mut end)) = (parse_i64(start), parse_i64(end)) else {
            return error(NOT_INT);
        };
        let value: &[u8] = match self.live(key) {
            Some(e) => &e.value,
            None => &[],
        };
        let len = value.len() as i64;
        if start < 0 && end < 0 && start > end {
            return bulk(b"");
        }
        if start < 0 {
            start += len;
        }
        if end < 0 {
            end += len;
        }
        start = start.max(0);
        end = end.max(0);
        if end >= len {
            end = len - 1;
        }
        if len == 0 || start > end {
            return bulk(b"");
        }
        bulk(&value[start as usize..=end as usize])
    }

    /// Redis `setrangeCommand`: zero-padded to `offset`, a missing key
    /// with an empty patch stays missing, the result is bounded.
    fn setrange(&mut self, key: &[u8], offset: &[u8], patch: &[u8]) -> Vec<u8> {
        if key.len() > MAX_KEY_LEN {
            return error(RECORD_BOUNDS);
        }
        let Some(offset) = parse_i64(offset) else {
            return error(NOT_INT);
        };
        if offset < 0 {
            return error("ERR offset is out of range");
        }
        let offset = offset as u64;
        let existing = self.live(key).map(|e| e.value.len());
        if patch.is_empty() {
            return int(existing.unwrap_or(0) as i64);
        }
        let new_len = (offset + patch.len() as u64).max(existing.unwrap_or(0) as u64);
        if new_len > MAX_STRING_LEN {
            return error("ERR string exceeds maximum allowed size (proto-max-bulk-len)");
        }
        if new_len as usize > MAX_VAL_LEN {
            return error(RECORD_BOUNDS);
        }
        let entry = match self.live(key) {
            Some(e) => e,
            None => {
                self.0.entry(key.to_vec()).or_insert(Entry { value: Vec::new(), deadline_ms: None })
            }
        };
        if entry.value.len() < new_len as usize {
            entry.value.resize(new_len as usize, 0);
        }
        entry.value[offset as usize..offset as usize + patch.len()].copy_from_slice(patch);
        int(new_len as i64)
    }
}
