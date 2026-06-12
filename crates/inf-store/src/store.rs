//! `CellStore` (M0-S15 substrate): the single-threaded, cell-local string
//! engine — records in the [`Arena`](inf_alloc::Arena), addresses in the
//! [`Index`](crate::index::Index), expiry strictly on read (no wheel at M0).
//!
//! Every operation takes `now: Nanos` from the caller — time is injected
//! (L7), so the store is deterministic and DST-able. Memory accounting is
//! byte-exact by construction (L5): the arena tracks live/slack/resident,
//! the index reports its table bytes, and [`MemoryReport`] exposes the
//! frozen attribution domains.
//!
//! Deviation from the freeze sketch (recorded in the ledger): mutating ops
//! return `Result<_, OpError>` where the sketch had bare values — arena
//! budget exhaustion (`OpError::OutOfMemory`) is a real outcome the command
//! layer must surface as an error reply, and panicking on allocation
//! pressure is forbidden by the engineering rules.

use inf_alloc::{Arena, ArenaAddr, ArenaConfig};
use inf_foundation::hash64;
use inf_foundation::time::Nanos;

use crate::index::Index;
use crate::record::{HEADER_LEN, MAX_KEY_LEN, MAX_VAL_LEN, RecordSpec, RecordView, TypeTag};

/// Stable hash seed: deterministic across runs and cells (L7; DST oracles
/// rely on reproducible placement).
const HASH_SEED: u64 = 0x1AF1_D8A5_0DB5_EED1;

/// Configuration for [`CellStore::new`].
#[derive(Copy, Clone, Debug, Default)]
pub struct StoreConfig {
    /// Record arena settings (chunk size, resident budget).
    pub arena: ArenaConfig,
    /// Index pre-sizing (entries before the first rehash).
    pub initial_keys: usize,
}

/// Typed operation failure surfaced to the command layer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum OpError {
    /// Value is not a 64-bit integer string (INCR family).
    NotInt,
    /// Integer overflow/underflow (INCR family).
    Overflow,
    /// Arena budget exhausted — backpressure, not a panic.
    OutOfMemory,
    /// Key or value exceeds the v0 record bounds.
    TooLarge,
}

/// `SET` condition (NX/XX shapes).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum SetCond {
    #[default]
    Always,
    /// Apply only if absent (`SETNX`, `SET .. NX`).
    IfAbsent,
    /// Apply only if present (`SET .. XX`).
    IfPresent,
}

/// `SET` expiry behavior.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum SetExpire {
    /// Drop any existing TTL (plain `SET` semantics).
    #[default]
    Clear,
    /// Keep the existing TTL (`SET .. KEEPTTL`).
    Keep,
    /// Absolute deadline (`SET .. EX/PX/EXAT/PXAT`).
    At(Nanos),
}

/// Options for [`CellStore::set`].
#[derive(Copy, Clone, Debug, Default)]
pub struct SetOptions {
    pub cond: SetCond,
    pub expire: SetExpire,
    /// Return the previous value (`SET .. GET`, `GETSET`).
    pub get_old: bool,
}

/// Result of [`CellStore::set`]. `old` is populated only when
/// `SetOptions::get_old` requested it (it costs a copy).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetOutcome {
    Applied { old: Option<Vec<u8>> },
    Skipped { old: Option<Vec<u8>> },
}

/// `EXPIRE` condition flags (NX/XX/GT/LT).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ExpireCond {
    #[default]
    Always,
    /// `NX`: only when no expiry exists.
    IfNoExpiry,
    /// `XX`: only when an expiry exists.
    IfHasExpiry,
    /// `GT`: only when new > current (no expiry counts as infinite).
    IfGreater,
    /// `LT`: only when new < current.
    IfLess,
}

/// `TTL`/`PTTL` answer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ttl {
    Missing,
    NoExpiry,
    Ms(u64),
}

/// Frozen memory attribution domains (tripwire names, M0 §3.2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MemoryReport {
    pub records_live_bytes: u64,
    pub records_slack_bytes: u64,
    pub records_resident_bytes: u64,
    pub index_bytes: u64,
    pub live_records: u64,
}

/// One cell's keyspace slice. Single-threaded by construction (owns a
/// `!Send` arena); all time is injected.
pub struct CellStore {
    arena: Arena,
    index: Index,
}

impl CellStore {
    pub fn new(cfg: StoreConfig) -> CellStore {
        CellStore {
            arena: Arena::new(cfg.arena),
            index: Index::with_capacity(cfg.initial_keys.max(64)),
        }
    }

    /// Stable key hash — also what the batch pipeline computes up front.
    #[inline]
    pub fn hash_key(key: &[u8]) -> u64 {
        hash64(key, HASH_SEED)
    }

    /// Prefetch the index probe path for a pre-hashed key (PARSE→hash→
    /// prefetch→EXECUTE pipeline, L3/L4).
    #[inline]
    pub fn prefetch(&self, key_hash: u64) {
        self.index.prefetch(key_hash);
    }

    /// Live key count (post-expiry keys may still be counted until read).
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Byte-exact attribution snapshot (L5).
    pub fn report(&self) -> MemoryReport {
        let arena = self.arena.report();
        MemoryReport {
            records_live_bytes: arena.live_bytes,
            records_slack_bytes: arena.slack_bytes,
            records_resident_bytes: arena.resident_bytes,
            index_bytes: self.index.memory_bytes() as u64,
            live_records: arena.live_allocs,
        }
    }

    // ---- reads (expire-on-read makes them `&mut`) ----

    /// `GET`.
    pub fn get(&mut self, key: &[u8], now: Nanos) -> Option<&[u8]> {
        self.get_with_hash(key, Self::hash_key(key), now)
    }

    /// `GET` with a precomputed hash — the batch pipeline path: EXECUTE
    /// hashes and [`prefetch`](Self::prefetch)es a whole parse batch first,
    /// then executes with the hashes it already has (L3/L4).
    pub fn get_with_hash(&mut self, key: &[u8], hash: u64, now: Nanos) -> Option<&[u8]> {
        debug_assert_eq!(hash, Self::hash_key(key));
        let (addr, len) = self.resolve_hashed(key, hash, now)?;
        Some(RecordView::new(self.arena.bytes(addr, len)).value())
    }

    /// Diagnostics: index groups visited to terminate a probe for `key`
    /// (probe-length histogram artifact, M0-S14 AC).
    pub fn probe_groups(&self, key: &[u8]) -> usize {
        let arena = &self.arena;
        self.index.probe_groups(Self::hash_key(key), |addr| record_at(arena, addr).key() == key)
    }

    /// Batched `GET` — the full §7.3 pipeline. Per 32-key chunk:
    /// 1. hash every key and prefetch its probe lines (ctrl + slots);
    /// 2. probe to the first 22-bit-fingerprint candidate (no record touch)
    ///    and prefetch the candidate's record lines;
    /// 3. verify keys and read values — by now both the probe and record
    ///    lines are (likely) in cache, so misses overlapped across the
    ///    whole chunk instead of serializing per key.
    ///
    /// Fingerprint collisions (≈2⁻²² per probe) and expired records fall
    /// back to the exact per-key path; results arrive via `out(i, value)`.
    pub fn get_many(
        &mut self,
        keys: &[&[u8]],
        now: Nanos,
        mut out: impl FnMut(usize, Option<&[u8]>),
    ) {
        const CHUNK: usize = 32;
        let mut hashes = [0u64; CHUNK];
        let mut candidates: [Option<ArenaAddr>; CHUNK] = [None; CHUNK];
        for (chunk_at, chunk) in keys.chunks(CHUNK).enumerate() {
            let base = chunk_at * CHUNK;
            for (i, key) in chunk.iter().enumerate() {
                hashes[i] = Self::hash_key(key);
                self.index.prefetch(hashes[i]);
            }
            for (i, _) in chunk.iter().enumerate() {
                // Unverified probe: first fingerprint match, zero record reads.
                candidates[i] = self.index.find(hashes[i], |_| true);
                if let Some(addr) = candidates[i] {
                    let head = self.arena.bytes(addr, HEADER_LEN).as_ptr();
                    inf_simd::prefetch_read(head);
                    inf_simd::prefetch_read(head.wrapping_add(64));
                }
            }
            // A reap (expired record freed mid-chunk) invalidates any later
            // candidate holding the same address (duplicate keys in one
            // batch) — those redo the exact path instead of reading a freed
            // slot.
            let mut redo = [false; CHUNK];
            for i in 0..chunk.len() {
                let key = chunk[i];
                let exact_path = match candidates[i] {
                    None => {
                        out(base + i, None);
                        continue;
                    }
                    Some(_) if redo[i] => true,
                    Some(addr) => {
                        let view = record_at(&self.arena, addr);
                        if view.key() != key {
                            true // fingerprint collision (≈2⁻²²)
                        } else if view.is_expired(now) {
                            let len = view.encoded_len();
                            self.index.remove(hashes[i], addr);
                            self.arena.free(addr, len);
                            mark_stale(&mut redo, &candidates, i, addr);
                            out(base + i, None);
                            continue;
                        } else {
                            out(base + i, Some(view.value()));
                            continue;
                        }
                    }
                };
                debug_assert!(exact_path);
                match self.resolve_hashed(key, hashes[i], now) {
                    Some((addr, len)) => {
                        let view = RecordView::new(self.arena.bytes(addr, len));
                        out(base + i, Some(view.value()));
                    }
                    None => {
                        // The exact path may itself have reaped an expired
                        // record; invalidate matching later candidates.
                        if let Some(addr) = candidates[i] {
                            mark_stale(&mut redo, &candidates, i, addr);
                        }
                        out(base + i, None);
                    }
                }
            }
        }

        fn mark_stale(
            redo: &mut [bool],
            candidates: &[Option<ArenaAddr>],
            after: usize,
            addr: ArenaAddr,
        ) {
            for j in after + 1..candidates.len() {
                if candidates[j] == Some(addr) {
                    redo[j] = true;
                }
            }
        }
    }

    /// `EXISTS` (single key).
    pub fn exists(&mut self, key: &[u8], now: Nanos) -> bool {
        self.resolve(key, now).is_some()
    }

    /// `STRLEN`. Missing keys are length 0 (Redis semantics).
    pub fn strlen(&mut self, key: &[u8], now: Nanos) -> u64 {
        match self.resolve(key, now) {
            Some((addr, len)) => RecordView::new(self.arena.bytes(addr, len)).vlen() as u64,
            None => 0,
        }
    }

    /// `TYPE`.
    pub fn type_of(&mut self, key: &[u8], now: Nanos) -> Option<TypeTag> {
        let (addr, len) = self.resolve(key, now)?;
        Some(RecordView::new(self.arena.bytes(addr, len)).type_tag())
    }

    /// `TTL`/`PTTL` in milliseconds.
    pub fn ttl(&mut self, key: &[u8], now: Nanos) -> Ttl {
        match self.resolve(key, now) {
            None => Ttl::Missing,
            Some((addr, len)) => {
                match RecordView::new(self.arena.bytes(addr, len)).expire_at_ms() {
                    None => Ttl::NoExpiry,
                    Some(at) => Ttl::Ms(at.saturating_sub(now.0 / 1_000_000)),
                }
            }
        }
    }

    // ---- writes ----

    /// `SET` family. See [`SetOptions`]/[`SetOutcome`].
    pub fn set(
        &mut self,
        key: &[u8],
        value: &[u8],
        opts: SetOptions,
        now: Nanos,
    ) -> Result<SetOutcome, OpError> {
        check_bounds(key, value)?;
        let existing = self.resolve(key, now);
        let old_view = existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let old_value = if opts.get_old { old_view.map(|v| v.value().to_vec()) } else { None };
        let applies = match opts.cond {
            SetCond::Always => true,
            SetCond::IfAbsent => existing.is_none(),
            SetCond::IfPresent => existing.is_some(),
        };
        if !applies {
            return Ok(SetOutcome::Skipped { old: old_value });
        }
        let (version, expire_at_ms) = match old_view {
            Some(view) => (
                view.version().wrapping_add(1),
                match opts.expire {
                    SetExpire::Clear => None,
                    SetExpire::Keep => view.expire_at_ms(),
                    SetExpire::At(at) => Some(at.0 / 1_000_000),
                },
            ),
            None => (
                1,
                match opts.expire {
                    SetExpire::At(at) => Some(at.0 / 1_000_000),
                    _ => None,
                },
            ),
        };
        let spec = RecordSpec { key, value, version, expire_at_ms };
        self.write_record(key, existing, spec)?;
        Ok(SetOutcome::Applied { old: old_value })
    }

    /// `DEL` (single key). True if the key existed.
    pub fn del(&mut self, key: &[u8], now: Nanos) -> bool {
        match self.resolve(key, now) {
            Some((addr, len)) => {
                self.index.remove(Self::hash_key(key), addr);
                self.arena.free(addr, len);
                true
            }
            None => false,
        }
    }

    /// `GETDEL`: the value, removing the key.
    pub fn getdel(&mut self, key: &[u8], now: Nanos) -> Option<Vec<u8>> {
        let (addr, len) = self.resolve(key, now)?;
        let value = RecordView::new(self.arena.bytes(addr, len)).value().to_vec();
        self.index.remove(Self::hash_key(key), addr);
        self.arena.free(addr, len);
        Some(value)
    }

    /// `INCR`/`DECR`/`INCRBY`/`DECRBY` (delta may be negative).
    pub fn incr_by(&mut self, key: &[u8], delta: i64, now: Nanos) -> Result<i64, OpError> {
        let existing = self.resolve(key, now);
        let (current, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                (parse_int(view.value())?, view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (0, 1, None),
        };
        let next = current.checked_add(delta).ok_or(OpError::Overflow)?;
        let mut buf = [0u8; 20];
        let value = fmt_i64(&mut buf, next);
        let spec = RecordSpec { key, value, version, expire_at_ms };
        self.write_record(key, existing, spec)?;
        Ok(next)
    }

    /// `APPEND`: new value length.
    pub fn append(&mut self, key: &[u8], tail: &[u8], now: Nanos) -> Result<u64, OpError> {
        let existing = self.resolve(key, now);
        let (mut value, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                (view.value().to_vec(), view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (Vec::new(), 1, None),
        };
        value.extend_from_slice(tail);
        check_bounds(key, &value)?;
        let new_len = value.len() as u64;
        let spec = RecordSpec { key, value: &value, version, expire_at_ms };
        self.write_record(key, existing, spec)?;
        Ok(new_len)
    }

    /// `EXPIRE`/`PEXPIRE`/`PERSIST` (`at: None` removes the TTL). True if
    /// the deadline was applied/removed.
    pub fn expire(&mut self, key: &[u8], at: Option<Nanos>, cond: ExpireCond, now: Nanos) -> bool {
        let Some((addr, len)) = self.resolve(key, now) else { return false };
        let view = RecordView::new(self.arena.bytes(addr, len));
        let current = view.expire_at_ms();
        let new_ms = at.map(|n| n.0 / 1_000_000);
        let applies = match cond {
            ExpireCond::Always => true,
            ExpireCond::IfNoExpiry => current.is_none(),
            ExpireCond::IfHasExpiry => current.is_some(),
            // GT/LT: a missing current TTL counts as infinite (Redis rules):
            // GT never beats infinity; LT always does.
            ExpireCond::IfGreater => match (new_ms, current) {
                (Some(new), Some(cur)) => new > cur,
                (Some(_), None) => false,
                (None, _) => false, // PERSIST with GT/LT is a command error upstream
            },
            ExpireCond::IfLess => match (new_ms, current) {
                (Some(new), Some(cur)) => new < cur,
                (Some(_), None) => true,
                (None, _) => false,
            },
        };
        if !applies || (at.is_none() && current.is_none()) {
            return false;
        }
        // EXPIRE with a deadline at/before `now` deletes the key (Redis
        // semantics) and still reports success.
        if let Some(ms) = new_ms
            && ms <= now.0 / 1_000_000
        {
            self.index.remove(Self::hash_key(key), addr);
            self.arena.free(addr, len);
            return true;
        }
        // Rewrite with the new TTL-extension state. The ±5-byte extension
        // may cross a size class, so the record borrow must end before the
        // write: copy out (TTL changes are rare; M1 can specialize the
        // same-class in-place path).
        let key_owned = view.key().to_vec();
        let value_owned = view.value().to_vec();
        let version = view.version().wrapping_add(1);
        let spec =
            RecordSpec { key: &key_owned, value: &value_owned, version, expire_at_ms: new_ms };
        self.write_record_at(key, Some((addr, len)), spec).is_ok()
    }

    // ---- internals ----

    /// Index lookup + expire-on-read: returns the live record's address and
    /// encoded length, reaping it if its deadline passed.
    fn resolve(&mut self, key: &[u8], now: Nanos) -> Option<(ArenaAddr, usize)> {
        self.resolve_hashed(key, Self::hash_key(key), now)
    }

    fn resolve_hashed(&mut self, key: &[u8], hash: u64, now: Nanos) -> Option<(ArenaAddr, usize)> {
        let arena = &self.arena;
        let addr = self.index.find(hash, |addr| record_at(arena, addr).key() == key)?;
        let view = record_at(arena, addr);
        let len = view.encoded_len();
        if view.is_expired(now) {
            self.index.remove(hash, addr);
            self.arena.free(addr, len);
            return None;
        }
        Some((addr, len))
    }

    fn write_record(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        self.write_record_at(key, existing, spec)
    }

    /// Writes `spec`, reusing `existing`'s slot when the size class allows,
    /// else alloc-copy-free with an index address swap.
    fn write_record_at(
        &mut self,
        key: &[u8],
        existing: Option<(ArenaAddr, usize)>,
        spec: RecordSpec<'_>,
    ) -> Result<(), OpError> {
        let new_len = spec.encoded_len();
        let hash = Self::hash_key(key);
        match existing {
            Some((addr, old_len)) if self.arena.resize_in_place(addr, old_len, new_len) => {
                spec.write(self.arena.bytes_mut(addr, new_len));
                Ok(())
            }
            Some((addr, old_len)) => {
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.replace(hash, addr, new_addr);
                self.arena.free(addr, old_len);
                Ok(())
            }
            None => {
                if self.index.needs_grow() {
                    let arena = &self.arena;
                    self.index.grow(|addr| Self::hash_key(record_at(arena, addr).key()));
                }
                let new_addr = self.arena.alloc(new_len).ok_or(OpError::OutOfMemory)?;
                spec.write(self.arena.bytes_mut(new_addr, new_len));
                self.index.insert(hash, new_addr);
                Ok(())
            }
        }
    }
}

/// Reads the record at `addr`: header first (fixed 9 bytes) to learn the
/// full encoded length, then the complete slice.
#[inline]
fn record_at(arena: &Arena, addr: ArenaAddr) -> RecordView<'_> {
    let head = arena.bytes(addr, HEADER_LEN);
    let full_len = crate::record::encoded_len_from_header(head);
    RecordView::new(arena.bytes(addr, full_len))
}

#[inline]
fn check_bounds(key: &[u8], value: &[u8]) -> Result<(), OpError> {
    if key.len() > MAX_KEY_LEN || value.len() > MAX_VAL_LEN {
        return Err(OpError::TooLarge);
    }
    Ok(())
}

/// Strict Redis `string2ll` semantics: optional sign, no leading zeros, no
/// `-0` (oracle-pinned vs Redis 8.0.5 by the compat harness), i64 range
/// (overflow-on-parse is `NotInt`, matching "not an integer or out of
/// range").
fn parse_int(bytes: &[u8]) -> Result<i64, OpError> {
    if bytes.is_empty() || bytes.len() > 21 {
        return Err(OpError::NotInt);
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(OpError::NotInt);
    }
    if digits[0] == b'0' && (digits.len() > 1 || neg) {
        return Err(OpError::NotInt);
    }
    let mut acc: i64 = 0;
    for &d in digits {
        acc = acc
            .checked_mul(10)
            .and_then(|a| {
                let v = i64::from(d - b'0');
                if neg { a.checked_sub(v) } else { a.checked_add(v) }
            })
            .ok_or(OpError::NotInt)?;
    }
    Ok(acc)
}

/// Formats an i64 into a stack buffer (no allocation on the INCR path).
fn fmt_i64(buf: &mut [u8; 20], v: i64) -> &[u8] {
    let mut at = buf.len();
    let neg = v < 0;
    // Work in negative space: i64::MIN has no positive counterpart.
    let mut n = if neg { v } else { -v };
    loop {
        at -= 1;
        buf[at] = b'0' + (-(n % 10)) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if neg {
        at -= 1;
        buf[at] = b'-';
    }
    &buf[at..]
}
