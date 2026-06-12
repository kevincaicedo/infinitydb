//! `CellStore` (M0-S15 substrate, extended by M1-E1/E2): the
//! single-threaded, cell-local string engine — records in the
//! [`Arena`](inf_alloc::Arena), addresses in the
//! [`Index`](crate::index::Index), expiry lazy on read **plus** the M1
//! hierarchical [`TtlWheel`](crate::wheel) driven by budgeted
//! [`expire_tick`](CellStore::expire_tick) MAINTAIN slices.
//!
//! Every operation takes `now: Nanos` from the caller — time is injected
//! (L7), so the store is deterministic and DST-able. Memory accounting is
//! byte-exact by construction (L5): the arena tracks live/slack/resident,
//! the index reports its table bytes, the wheel reports pool + slot bytes,
//! and [`MemoryReport`] exposes the frozen attribution domains.
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
use crate::record::{
    HEADER_LEN, MAX_EXPIRE_MS, MAX_KEY_LEN, MAX_VAL_LEN, RecordSpec, RecordView, TypeTag,
};
use crate::wheel::{ArmOutcome, TtlWheel};

pub use crate::wheel::ExpiryBudget;

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
    /// Value is not a parseable float (INCRBYFLOAT).
    NotFloat,
    /// Float op produced NaN/Infinity (INCRBYFLOAT).
    NanOrInf,
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

/// TTL side effect of `GETEX` (M1-S01).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum TtlUpdate {
    /// Plain `GETEX` — read only.
    #[default]
    Keep,
    /// `GETEX .. PERSIST`.
    Persist,
    /// `GETEX .. EX/PX/EXAT/PXAT` absolute deadline.
    At(Nanos),
}

/// `OBJECT ENCODING` answer for string records (M1-S02). Derived from the
/// value plus the record's raw flag — matching Redis's `int`/`embstr`/`raw`
/// classification (embstr threshold 44, byte-surgery forces raw).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Encoding {
    Int,
    Embstr,
    Raw,
}

impl Encoding {
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Int => "int",
            Encoding::Embstr => "embstr",
            Encoding::Raw => "raw",
        }
    }
}

/// `COPY` outcome (M1-S02).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CopyResult {
    Copied,
    SourceMissing,
    DestinationExists,
}

/// One [`CellStore::expire_tick`] slice result (M1-S05). `lag_ms` is the
/// `expiry_debt` backlog metric: how far the wheel cursor still trails `now`
/// after the slice (0 = caught up); the MAINTAIN caller escalates budgets on
/// it while foreground latency stays protected.
#[derive(Copy, Clone, Default, Debug)]
pub struct ExpiryStats {
    /// Records actually reaped by this slice.
    pub reaped: u64,
    /// Wheel entries that no longer matched a live expired record.
    pub stale: u64,
    /// Cursor work performed (ms steps + fast-forward jumps).
    pub steps: u32,
    /// Backlog: milliseconds the wheel still trails `now` (0 = caught up).
    pub lag_ms: u64,
    /// Live wheel entries after the slice.
    pub armed: u64,
}

/// Always-on store counters (feeds `INFO stats`/`keyspace` and the M1
/// expiry oracles). Plain fields — the store is single-threaded (L1).
#[derive(Copy, Clone, Default, Debug)]
pub struct StoreStats {
    pub keyspace_hits: u64,
    pub keyspace_misses: u64,
    /// Reaped by expire-on-read.
    pub expired_lazy: u64,
    /// Reaped by wheel slices.
    pub expired_active: u64,
    /// Live records currently carrying a TTL (`INFO keyspace` `expires=`).
    pub ttl_live: u64,
    /// Wheel entries that fired without a matching expired record.
    pub wheel_stale: u64,
    /// TTL writes that could not arm the wheel (pool cap) — lazy-only keys.
    pub wheel_fallback: u64,
}

/// Frozen memory attribution domains (tripwire names, M0 §3.2; `wheel_bytes`
/// joins in M1-S04 — the ≤ 16 B/TTL'd-key budget is verified against it).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MemoryReport {
    pub records_live_bytes: u64,
    pub records_slack_bytes: u64,
    pub records_resident_bytes: u64,
    pub index_bytes: u64,
    pub wheel_bytes: u64,
    pub live_records: u64,
}

/// One cell's keyspace slice. Single-threaded by construction (owns a
/// `!Send` arena); all time is injected.
pub struct CellStore {
    arena: Arena,
    index: Index,
    wheel: TtlWheel,
    stats: StoreStats,
    cfg: StoreConfig,
}

impl CellStore {
    pub fn new(cfg: StoreConfig) -> CellStore {
        CellStore {
            arena: Arena::new(cfg.arena),
            index: Index::with_capacity(cfg.initial_keys.max(64)),
            // Cursor 0: the first tick fast-forwards to `now` (empty wheel).
            wheel: TtlWheel::new(0),
            stats: StoreStats::default(),
            cfg,
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

    /// Live key count (post-expiry keys may still be counted until read or
    /// wheel-reaped) — `DBSIZE`.
    #[inline]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Always-on counters snapshot.
    #[inline]
    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// `CONFIG RESETSTAT`: zero the lifetime counters; the live-state census
    /// (`ttl_live`) is structural and survives.
    pub fn reset_stats(&mut self) {
        let ttl_live = self.stats.ttl_live;
        self.stats = StoreStats { ttl_live, ..StoreStats::default() };
    }

    /// Byte-exact attribution snapshot (L5).
    pub fn report(&self) -> MemoryReport {
        let arena = self.arena.report();
        MemoryReport {
            records_live_bytes: arena.live_bytes,
            records_slack_bytes: arena.slack_bytes,
            records_resident_bytes: arena.resident_bytes,
            index_bytes: self.index.memory_bytes() as u64,
            wheel_bytes: (self.wheel.pool_bytes() + self.wheel.table_bytes()) as u64,
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
        let Some((addr, len)) = self.resolve_hashed(key, hash, now) else {
            self.stats.keyspace_misses += 1;
            return None;
        };
        self.stats.keyspace_hits += 1;
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
                        self.stats.keyspace_misses += 1;
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
                            self.note_reap_lazy();
                            mark_stale(&mut redo, &candidates, i, addr);
                            self.stats.keyspace_misses += 1;
                            out(base + i, None);
                            continue;
                        } else {
                            self.stats.keyspace_hits += 1;
                            out(base + i, Some(view.value()));
                            continue;
                        }
                    }
                };
                debug_assert!(exact_path);
                match self.resolve_hashed(key, hashes[i], now) {
                    Some((addr, len)) => {
                        let view = RecordView::new(self.arena.bytes(addr, len));
                        self.stats.keyspace_hits += 1;
                        out(base + i, Some(view.value()));
                    }
                    None => {
                        // The exact path may itself have reaped an expired
                        // record; invalidate matching later candidates.
                        if let Some(addr) = candidates[i] {
                            mark_stale(&mut redo, &candidates, i, addr);
                        }
                        self.stats.keyspace_misses += 1;
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

    /// Absolute expiry deadline in clock milliseconds (`EXPIRETIME`).
    pub fn expire_at(&mut self, key: &[u8], now: Nanos) -> Ttl {
        match self.resolve(key, now) {
            None => Ttl::Missing,
            Some((addr, len)) => {
                match RecordView::new(self.arena.bytes(addr, len)).expire_at_ms() {
                    None => Ttl::NoExpiry,
                    Some(at) => Ttl::Ms(at),
                }
            }
        }
    }

    /// `GETRANGE`/`SUBSTR` — Redis index semantics (negatives count from the
    /// end, ranges clamp, inverted ranges are empty).
    pub fn get_range(&mut self, key: &[u8], start: i64, end: i64, now: Nanos) -> &[u8] {
        let Some((addr, len)) = self.resolve(key, now) else {
            self.stats.keyspace_misses += 1;
            return b"";
        };
        self.stats.keyspace_hits += 1;
        let value = RecordView::new(self.arena.bytes(addr, len)).value();
        let n = value.len() as i64;
        let from = if start < 0 { (n + start).max(0) } else { start };
        let to = if end < 0 { (n + end).max(0) } else { end }.min(n - 1);
        if n == 0 || from > to || from >= n {
            return b"";
        }
        &value[from as usize..=to as usize]
    }

    /// `OBJECT ENCODING` view: the encoding plus the parsed integer when
    /// int-encoded (drives Redis's shared-integer REFCOUNT answer).
    pub fn object_encoding(&mut self, key: &[u8], now: Nanos) -> Option<(Encoding, Option<i64>)> {
        let (addr, len) = self.resolve(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        if !view.is_raw()
            && let Ok(v) = parse_int(view.value())
        {
            return Some((Encoding::Int, Some(v)));
        }
        if view.is_raw() || view.vlen() > 44 {
            Some((Encoding::Raw, None))
        } else {
            Some((Encoding::Embstr, None))
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
        let old_deadline = old_view.and_then(|v| v.expire_at_ms());
        let old_value = if opts.get_old { old_view.map(|v| v.value().to_vec()) } else { None };
        let applies = match opts.cond {
            SetCond::Always => true,
            SetCond::IfAbsent => existing.is_none(),
            SetCond::IfPresent => existing.is_some(),
        };
        if !applies {
            return Ok(SetOutcome::Skipped { old: old_value });
        }
        let version = old_view.map_or(1, |v| v.version().wrapping_add(1));
        let expire_at_ms = match opts.expire {
            SetExpire::Clear => None,
            SetExpire::Keep => old_deadline,
            SetExpire::At(at) => Some((at.0 / 1_000_000).min(MAX_EXPIRE_MS)),
        };
        let spec = RecordSpec { key, value, version, expire_at_ms, raw: false };
        self.write_record(key, existing, spec)?;
        self.note_ttl(old_deadline.is_some(), expire_at_ms.is_some());
        if let Some(ms) = expire_at_ms
            && old_deadline != Some(ms)
        {
            self.arm_wheel(Self::hash_key(key), ms);
        }
        Ok(SetOutcome::Applied { old: old_value })
    }

    /// `DEL` (single key). True if the key existed.
    pub fn del(&mut self, key: &[u8], now: Nanos) -> bool {
        match self.resolve(key, now) {
            Some((addr, len)) => {
                let had_ttl = RecordView::new(self.arena.bytes(addr, len)).expire_at_ms().is_some();
                self.index.remove(Self::hash_key(key), addr);
                self.arena.free(addr, len);
                self.note_ttl(had_ttl, false);
                true
            }
            None => false,
        }
    }

    /// `GETDEL`: the value, removing the key.
    pub fn getdel(&mut self, key: &[u8], now: Nanos) -> Option<Vec<u8>> {
        let (addr, len) = self.resolve(key, now)?;
        let view = RecordView::new(self.arena.bytes(addr, len));
        let value = view.value().to_vec();
        let had_ttl = view.expire_at_ms().is_some();
        self.index.remove(Self::hash_key(key), addr);
        self.arena.free(addr, len);
        self.note_ttl(had_ttl, false);
        Some(value)
    }

    /// `GETEX`: the value, with an optional TTL side effect (M1-S01). A
    /// past deadline deletes the key after the read (Redis semantics).
    pub fn get_ex(&mut self, key: &[u8], update: TtlUpdate, now: Nanos) -> Option<Vec<u8>> {
        let value = {
            let Some((addr, len)) = self.resolve(key, now) else {
                self.stats.keyspace_misses += 1;
                return None;
            };
            self.stats.keyspace_hits += 1;
            RecordView::new(self.arena.bytes(addr, len)).value().to_vec()
        };
        match update {
            TtlUpdate::Keep => {}
            TtlUpdate::Persist => {
                self.expire(key, None, ExpireCond::Always, now);
            }
            TtlUpdate::At(at) => {
                self.expire(key, Some(at), ExpireCond::Always, now);
            }
        }
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
        let spec = RecordSpec { key, value, version, expire_at_ms, raw: false };
        self.write_record(key, existing, spec)?;
        Ok(next)
    }

    /// `INCRBYFLOAT`: the formatted new value (what the record now holds).
    pub fn incr_by_float(
        &mut self,
        key: &[u8],
        delta: f64,
        now: Nanos,
    ) -> Result<Vec<u8>, OpError> {
        let existing = self.resolve(key, now);
        let (current, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                (parse_float(view.value())?, view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (0.0, 1, None),
        };
        let next = current + delta;
        if !next.is_finite() {
            return Err(OpError::NanOrInf);
        }
        // Shortest round-trip formatting; integers print without a decimal
        // point — the Redis `%.17Lg` + zero-strip shape for f64 range.
        // (Recorded deviation: Redis computes in 80-bit long double, so
        // extreme-precision tails can differ — compat-matrix entry.)
        let text = format!("{next}").into_bytes();
        let spec = RecordSpec { key, value: &text, version, expire_at_ms, raw: false };
        self.write_record(key, existing, spec)?;
        Ok(text)
    }

    /// `APPEND`: new value length. Appending to an EXISTING record marks it
    /// raw; a fresh key stays encodable (Redis `tryObjectEncoding` shape,
    /// oracle-pinned).
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
        let raw = existing.is_some();
        let spec = RecordSpec { key, value: &value, version, expire_at_ms, raw };
        self.write_record(key, existing, spec)?;
        Ok(new_len)
    }

    /// `SETRANGE`: patch `patch` at `offset`, zero-padding any gap; returns
    /// the new length. Empty patches never create or mutate (Redis).
    pub fn set_range(
        &mut self,
        key: &[u8],
        offset: usize,
        patch: &[u8],
        now: Nanos,
    ) -> Result<u64, OpError> {
        let existing = self.resolve(key, now);
        if patch.is_empty() {
            return Ok(match existing {
                Some((addr, len)) => RecordView::new(self.arena.bytes(addr, len)).vlen() as u64,
                None => 0,
            });
        }
        let end = offset.checked_add(patch.len()).ok_or(OpError::TooLarge)?;
        if key.len() > MAX_KEY_LEN || end > MAX_VAL_LEN {
            return Err(OpError::TooLarge);
        }
        let (mut value, version, expire_at_ms) = match existing {
            Some((addr, len)) => {
                let view = RecordView::new(self.arena.bytes(addr, len));
                (view.value().to_vec(), view.version().wrapping_add(1), view.expire_at_ms())
            }
            None => (Vec::new(), 1, None),
        };
        if value.len() < end {
            value.resize(end, 0);
        }
        value[offset..end].copy_from_slice(patch);
        let new_len = value.len() as u64;
        let spec = RecordSpec { key, value: &value, version, expire_at_ms, raw: true };
        self.write_record(key, existing, spec)?;
        Ok(new_len)
    }

    /// `RENAME` (single cell): value, TTL, and encoding move; the source is
    /// removed. `Ok(false)` = source missing. The destination write happens
    /// first so OOM leaves the source intact.
    pub fn rename(&mut self, src: &[u8], dst: &[u8], now: Nanos) -> Result<bool, OpError> {
        if src == dst {
            return Ok(self.exists(src, now));
        }
        let Some((src_addr, src_len)) = self.resolve(src, now) else {
            return Ok(false);
        };
        let view = RecordView::new(self.arena.bytes(src_addr, src_len));
        let value = view.value().to_vec();
        let deadline = view.expire_at_ms();
        let raw = view.is_raw();
        check_bounds(dst, &value)?;
        let dst_existing = self.resolve(dst, now);
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        let spec = RecordSpec { key: dst, value: &value, version, expire_at_ms: deadline, raw };
        self.write_record(dst, dst_existing, spec)?;
        self.note_ttl(dst_had_ttl, deadline.is_some());
        // Source removal: the dst write never moves the src record.
        let src_had_ttl = deadline.is_some();
        self.index.remove(Self::hash_key(src), src_addr);
        self.arena.free(src_addr, src_len);
        self.note_ttl(src_had_ttl, false);
        if let Some(ms) = deadline {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(true)
    }

    /// `COPY` (single cell): duplicates value, TTL, and encoding.
    pub fn copy(
        &mut self,
        src: &[u8],
        dst: &[u8],
        replace: bool,
        now: Nanos,
    ) -> Result<CopyResult, OpError> {
        let Some((src_addr, src_len)) = self.resolve(src, now) else {
            return Ok(CopyResult::SourceMissing);
        };
        let view = RecordView::new(self.arena.bytes(src_addr, src_len));
        let value = view.value().to_vec();
        let deadline = view.expire_at_ms();
        let raw = view.is_raw();
        check_bounds(dst, &value)?;
        let dst_existing = self.resolve(dst, now);
        if dst_existing.is_some() && !replace {
            return Ok(CopyResult::DestinationExists);
        }
        let dst_old = dst_existing.map(|(addr, len)| RecordView::new(self.arena.bytes(addr, len)));
        let version = dst_old.map_or(1, |v| v.version().wrapping_add(1));
        let dst_had_ttl = dst_old.and_then(|v| v.expire_at_ms()).is_some();
        let spec = RecordSpec { key: dst, value: &value, version, expire_at_ms: deadline, raw };
        self.write_record(dst, dst_existing, spec)?;
        self.note_ttl(dst_had_ttl, deadline.is_some());
        if let Some(ms) = deadline {
            self.arm_wheel(Self::hash_key(dst), ms);
        }
        Ok(CopyResult::Copied)
    }

    /// `EXPIRE`/`PEXPIRE`/`PERSIST` (`at: None` removes the TTL). True if
    /// the deadline was applied/removed.
    pub fn expire(&mut self, key: &[u8], at: Option<Nanos>, cond: ExpireCond, now: Nanos) -> bool {
        let Some((addr, len)) = self.resolve(key, now) else { return false };
        let view = RecordView::new(self.arena.bytes(addr, len));
        let current = view.expire_at_ms();
        let new_ms = at.map(|n| (n.0 / 1_000_000).min(MAX_EXPIRE_MS));
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
            self.note_ttl(current.is_some(), false);
            return true;
        }
        // Rewrite with the new TTL-extension state. The ±5-byte extension
        // may cross a size class, so the record borrow must end before the
        // write: copy out (TTL changes are rare; a same-class in-place
        // specialization stays reserved).
        let key_owned = view.key().to_vec();
        let value_owned = view.value().to_vec();
        let version = view.version().wrapping_add(1);
        let raw = view.is_raw();
        let spec =
            RecordSpec { key: &key_owned, value: &value_owned, version, expire_at_ms: new_ms, raw };
        if self.write_record_at(key, Some((addr, len)), spec).is_err() {
            return false;
        }
        self.note_ttl(current.is_some(), new_ms.is_some());
        if let Some(ms) = new_ms
            && current != new_ms
        {
            self.arm_wheel(Self::hash_key(key), ms);
        }
        true
    }

    // ---- keyspace iteration (M1-S02) ----

    /// `SCAN` over one cell: home-group enumeration in reverse-binary cursor
    /// order. Guarantee: every key present for the whole scan is emitted at
    /// least once, across doubling growth and tombstone-recycling rehashes
    /// (same-capacity rebuilds keep home groups fixed; doublings split a
    /// home group `g` into `{g, g + groups}` — exactly the split the
    /// reverse-binary order tolerates). Keys written or removed mid-scan may
    /// or may not appear (Redis contract). Expired records encountered are
    /// reaped, never emitted. Returns the next cursor (0 = done).
    pub fn scan(
        &mut self,
        cursor: u64,
        count: usize,
        now: Nanos,
        mut emit: impl FnMut(&[u8]),
    ) -> u64 {
        let mask = self.index.group_count() as u64 - 1;
        let mut cursor = cursor & mask;
        let mut emitted = 0usize;
        let mut batch: Vec<ArenaAddr> = Vec::new();
        loop {
            batch.clear();
            {
                let arena = &self.arena;
                self.index.scan_home_group(
                    cursor as usize,
                    |addr| Self::hash_key(record_at(arena, addr).key()),
                    |addr| batch.push(addr),
                );
            }
            for &addr in &batch {
                let view = record_at(&self.arena, addr);
                if view.is_expired(now) {
                    let (hash, len) = (Self::hash_key(view.key()), view.encoded_len());
                    self.index.remove(hash, addr);
                    self.arena.free(addr, len);
                    self.note_reap_lazy();
                } else {
                    emit(view.key());
                    emitted += 1;
                }
            }
            cursor = next_rev_cursor(cursor, mask);
            if cursor == 0 || emitted >= count {
                return cursor;
            }
        }
    }

    /// `RANDOMKEY` probe: first live key at/after a caller-rolled slot
    /// (randomness is injected — L7). Two-level random (cell, then slot) is
    /// the documented compat deviation.
    pub fn random_key(&mut self, roll: u64, now: Nanos) -> Option<Vec<u8>> {
        loop {
            let addr = self.index.live_from(roll as usize)?;
            let view = record_at(&self.arena, addr);
            if !view.is_expired(now) {
                return Some(view.key().to_vec());
            }
            let (hash, len) = (Self::hash_key(view.key()), view.encoded_len());
            self.index.remove(hash, addr);
            self.arena.free(addr, len);
            self.note_reap_lazy();
        }
    }

    /// `FLUSHDB`/`FLUSHALL` (this cell's slice): drop every record, reset
    /// the wheel, keep lifetime counters (Redis flush does not reset stats).
    pub fn flush(&mut self, now: Nanos) {
        self.arena = Arena::new(self.cfg.arena);
        self.index = Index::with_capacity(self.cfg.initial_keys.max(64));
        self.wheel = TtlWheel::new(now.0 / 1_000_000);
        self.stats.ttl_live = 0;
    }

    // ---- active expiry (M1-E2) ----

    /// One budgeted expiry MAINTAIN slice (M1-S05): advance the wheel toward
    /// `now`, validating each fired entry against the index and reaping only
    /// records genuinely expired. Stale entries (TTL changed/persisted/key
    /// gone) drop with a counter. Bounded by `budget` on both fires and
    /// cursor steps so a 1M-same-second storm cannot cliff the loop.
    pub fn expire_tick(&mut self, now: Nanos, budget: ExpiryBudget) -> ExpiryStats {
        let now_ms = now.0 / 1_000_000;
        let CellStore { arena, index, wheel, stats, .. } = self;
        let mut out = ExpiryStats::default();
        let tick = wheel.tick(now_ms, budget, |hash, _deadline| {
            // Reap any record on this hash's probe path that is genuinely
            // expired (full-hash check keeps fingerprint collisions out;
            // reaping an expired record is correct regardless of which key
            // armed the entry).
            let found = index.find(hash, |addr| {
                let view = record_at(arena, addr);
                view.is_expired(now) && hash64(view.key(), HASH_SEED) == hash
            });
            match found {
                Some(addr) => {
                    let len = record_at(arena, addr).encoded_len();
                    index.remove(hash, addr);
                    arena.free(addr, len);
                    stats.expired_active += 1;
                    stats.ttl_live = stats.ttl_live.saturating_sub(1);
                    out.reaped += 1;
                }
                None => {
                    stats.wheel_stale += 1;
                    out.stale += 1;
                }
            }
        });
        out.steps = tick.steps;
        out.lag_ms = if tick.caught_up { 0 } else { now_ms.saturating_sub(wheel_cursor(wheel)) };
        out.armed = wheel.live();
        out
    }

    // ---- internals ----

    fn arm_wheel(&mut self, hash: u64, deadline_ms: u64) {
        if self.wheel.arm(hash, deadline_ms) == ArmOutcome::PoolFull {
            self.stats.wheel_fallback += 1;
        }
    }

    /// TTL-record census transition (`INFO keyspace` `expires=`).
    #[inline]
    fn note_ttl(&mut self, old: bool, new: bool) {
        match (old, new) {
            (false, true) => self.stats.ttl_live += 1,
            (true, false) => self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1),
            _ => {}
        }
    }

    #[inline]
    fn note_reap_lazy(&mut self) {
        self.stats.expired_lazy += 1;
        self.stats.ttl_live = self.stats.ttl_live.saturating_sub(1);
    }

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
            self.note_reap_lazy();
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

/// The wheel cursor in ms (private peek for the lag metric).
#[inline]
fn wheel_cursor(wheel: &TtlWheel) -> u64 {
    wheel.cursor_ms()
}

/// Reverse-binary cursor increment (the Redis `dictScan` order) over a
/// power-of-two group space: high bits advance first, so groups split by a
/// doubling are visited adjacently and never missed.
#[inline]
fn next_rev_cursor(cursor: u64, mask: u64) -> u64 {
    let mut v = cursor | !mask;
    v = v.reverse_bits();
    v = v.wrapping_add(1);
    v.reverse_bits()
}

/// Reads the record at `addr`: header first (fixed 8 bytes) to learn the
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

/// Redis `strtold`-shape float parse: full-string consume, no surrounding
/// whitespace, NaN rejected (Infinity parses; the *result* check rejects
/// non-finite outcomes, matching Redis error split).
fn parse_float(bytes: &[u8]) -> Result<f64, OpError> {
    let s = core::str::from_utf8(bytes).map_err(|_| OpError::NotFloat)?;
    if s.is_empty() || s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) {
        return Err(OpError::NotFloat);
    }
    let v: f64 = s.parse().map_err(|_| OpError::NotFloat)?;
    if v.is_nan() {
        return Err(OpError::NotFloat);
    }
    Ok(v)
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
