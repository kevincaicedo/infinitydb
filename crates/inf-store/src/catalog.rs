//! Namespace-catalog encoding **v1** (M2-S08, ADR-0015 D3): the bytes that
//! ride `inf-log`'s `META` envelope. The envelope (write/fsync/rename swap,
//! CRC, version tag) is `inf-log`'s protocol and treats this payload as
//! opaque; `inf-store` owns only the encoding and still never opens a file.
//!
//! ## Wire format v1 (all integers little-endian)
//!
//! ```text
//! catalog := version: u8 (= 1)
//!            next_id: u32          # node id counter; named ids start at 16
//!            count:   u32          # entry count
//!            entry*count
//! entry   := id:        u32        # ≥ 16 (0..16 are the implicit defaults)
//!            mode:      u8         # 0 = memory, 1 = durable, 2 = topic
//!            fsync:     u8         # 0 = none, 1 = everysec, 2 = always
//!            policy:    u8         # 0 = inherit, else EvictionPolicy
//!                                  # discriminant + 1 (1 = noeviction ..
//!                                  # 8 = volatile-lfu)
//!            maxmemory: u64        # u64::MAX = inherit (`None`)
//!            name_len:  u16
//!            name:      name_len bytes
//! ```
//!
//! Decode is **fail-stop** for callers (§8.4 honesty): unknown version,
//! truncation, trailing bytes, invalid mode/fsync/policy bytes, invalid or
//! reserved names, reserved ids, and duplicate ids are all typed errors —
//! nothing is skipped or repaired. Cross-field rules are enforced too:
//! encode always writes the *resolved* fsync class for durable entries
//! (`None` normalizes to `everysec`, the registry default), so decode
//! requires durable ⇒ fsync ≠ 0 and non-durable ⇒ fsync = 0; durable ⇒
//! policy = 0 (durable namespaces do not evict in M2 — ADR-0015 D5).
//! `maxmemory = u64::MAX` is the reserved inherit sentinel — a literal
//! budget of `u64::MAX` is not representable (it would be meaningless).

use core::fmt;

use inf_log::{FsyncClass, NsId};

use crate::evict::EvictionPolicy;
use crate::ns::{FIRST_NAMED_NS_ID, NsMode, NsSpec, is_default_name, valid_ns_name};

const CATALOG_VERSION: u8 = 1;

/// The node's namespace catalog: the id counter plus every named entry.
/// Boot seeds each cell's registry from it before cells replay or serve
/// (ADR-0015 D3); DDL persists it through the `META` swap.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NsCatalog {
    /// Next id the node-level allocator hands out (≥ [`FIRST_NAMED_NS_ID`]).
    pub next_id: u32,
    pub entries: Vec<NsSpec>,
}

/// Why a catalog payload failed to decode. Every variant means the `META`
/// payload is corrupt, foreign, or newer than this binary — callers must
/// fail-stop, never guess.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CatalogError {
    /// Version byte outside the known vocabulary.
    UnknownVersion(u8),
    /// Payload ends before its declared extent.
    Truncated,
    /// Bytes remain after the declared entries.
    TrailingBytes,
    /// Mode byte outside `0..=2`.
    InvalidMode(u8),
    /// Fsync byte outside `0..=2`, zero on a durable entry, or non-zero on
    /// a non-durable entry.
    InvalidFsync(u8),
    /// Policy byte outside `0..=8`, or non-zero on a durable entry.
    InvalidPolicy(u8),
    /// Name fails [`valid_ns_name`] or collides with a default name.
    InvalidName,
    /// Entry id below [`FIRST_NAMED_NS_ID`] (defaults are implicit).
    ReservedId(u32),
    /// Two entries claim the same id (ids are never reused).
    DuplicateId(u32),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::UnknownVersion(v) => write!(f, "unknown catalog version {v}"),
            CatalogError::Truncated => write!(f, "catalog payload truncated"),
            CatalogError::TrailingBytes => write!(f, "trailing bytes after catalog entries"),
            CatalogError::InvalidMode(b) => write!(f, "invalid namespace mode byte {b}"),
            CatalogError::InvalidFsync(b) => write!(f, "invalid fsync byte {b}"),
            CatalogError::InvalidPolicy(b) => write!(f, "invalid eviction policy byte {b}"),
            CatalogError::InvalidName => write!(f, "invalid namespace name"),
            CatalogError::ReservedId(id) => write!(f, "namespace id {id} is reserved"),
            CatalogError::DuplicateId(id) => write!(f, "duplicate namespace id {id}"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl NsCatalog {
    /// Encodes the catalog into the v1 payload (see module docs). Durable
    /// entries always encode a resolved fsync class (`None` → `everysec`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let payload: usize = self.entries.iter().map(|e| 16 + e.name.len()).sum();
        let mut out = Vec::with_capacity(9 + payload);
        out.push(CATALOG_VERSION);
        out.extend_from_slice(&self.next_id.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            debug_assert!(valid_ns_name(&e.name), "catalog entries carry validated names");
            out.extend_from_slice(&e.id.0.to_le_bytes());
            out.push(mode_to_byte(e.mode));
            out.push(fsync_to_byte(e));
            out.push(policy_to_byte(e.policy));
            out.extend_from_slice(&e.maxmemory.unwrap_or(u64::MAX).to_le_bytes());
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&e.name);
        }
        out
    }

    /// Decodes a v1 payload, enforcing every format and registry rule (see
    /// module docs).
    ///
    /// # Errors
    /// A [`CatalogError`] naming the violated rule; callers fail-stop.
    pub fn decode(buf: &[u8]) -> Result<NsCatalog, CatalogError> {
        let mut r = Cursor { buf, at: 0 };
        let version = r.u8()?;
        if version != CATALOG_VERSION {
            return Err(CatalogError::UnknownVersion(version));
        }
        let next_id = r.u32_le()?;
        let count = r.u32_le()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let entry = decode_entry(&mut r)?;
            if entries.iter().any(|e: &NsSpec| e.id == entry.id) {
                return Err(CatalogError::DuplicateId(entry.id.0));
            }
            entries.push(entry);
        }
        if r.at != buf.len() {
            return Err(CatalogError::TrailingBytes);
        }
        Ok(NsCatalog { next_id, entries })
    }
}

fn decode_entry(r: &mut Cursor<'_>) -> Result<NsSpec, CatalogError> {
    let id = r.u32_le()?;
    if id < FIRST_NAMED_NS_ID {
        return Err(CatalogError::ReservedId(id));
    }
    let mode = mode_from_byte(r.u8()?)?;
    let fsync = fsync_from_byte(r.u8()?, mode)?;
    let policy = policy_from_byte(r.u8()?, mode)?;
    let maxmemory = match r.u64_le()? {
        u64::MAX => None,
        v => Some(v),
    };
    let name_len = r.u16_le()?;
    let name = r.bytes(usize::from(name_len))?.to_vec();
    if !valid_ns_name(&name) || is_default_name(&name) {
        return Err(CatalogError::InvalidName);
    }
    Ok(NsSpec { id: NsId(id), name, mode, fsync, policy, maxmemory })
}

fn mode_to_byte(mode: NsMode) -> u8 {
    match mode {
        NsMode::Memory => 0,
        NsMode::Durable => 1,
        NsMode::Topic => 2,
    }
}

fn mode_from_byte(b: u8) -> Result<NsMode, CatalogError> {
    Ok(match b {
        0 => NsMode::Memory,
        1 => NsMode::Durable,
        2 => NsMode::Topic,
        _ => return Err(CatalogError::InvalidMode(b)),
    })
}

/// Durable entries encode their resolved class (`None` → `everysec`, the
/// registry default); every other mode encodes 0 — one value, one encoding.
fn fsync_to_byte(spec: &NsSpec) -> u8 {
    if spec.mode != NsMode::Durable {
        return 0;
    }
    match spec.fsync {
        None | Some(FsyncClass::Everysec) => 1,
        Some(FsyncClass::Always) => 2,
    }
}

fn fsync_from_byte(b: u8, mode: NsMode) -> Result<Option<FsyncClass>, CatalogError> {
    let fsync = match b {
        0 => None,
        1 => Some(FsyncClass::Everysec),
        2 => Some(FsyncClass::Always),
        _ => return Err(CatalogError::InvalidFsync(b)),
    };
    // Encode resolves durable classes, so durable ⇒ fsync ≠ 0 here; a
    // non-durable entry carrying one violates the registry rules.
    if (mode == NsMode::Durable) != fsync.is_some() {
        return Err(CatalogError::InvalidFsync(b));
    }
    Ok(fsync)
}

/// Explicit `EvictionPolicy` ↔ byte map (0 = inherit, else discriminant + 1
/// in `evict.rs` declaration order). No transmutes — the match is the wire
/// contract and the compiler flags new variants.
fn policy_to_byte(policy: Option<EvictionPolicy>) -> u8 {
    match policy {
        None => 0,
        Some(EvictionPolicy::NoEviction) => 1,
        Some(EvictionPolicy::AllKeysLru) => 2,
        Some(EvictionPolicy::VolatileLru) => 3,
        Some(EvictionPolicy::AllKeysRandom) => 4,
        Some(EvictionPolicy::VolatileRandom) => 5,
        Some(EvictionPolicy::VolatileTtl) => 6,
        Some(EvictionPolicy::AllKeysLfu) => 7,
        Some(EvictionPolicy::VolatileLfu) => 8,
    }
}

fn policy_from_byte(b: u8, mode: NsMode) -> Result<Option<EvictionPolicy>, CatalogError> {
    let policy = match b {
        0 => None,
        1 => Some(EvictionPolicy::NoEviction),
        2 => Some(EvictionPolicy::AllKeysLru),
        3 => Some(EvictionPolicy::VolatileLru),
        4 => Some(EvictionPolicy::AllKeysRandom),
        5 => Some(EvictionPolicy::VolatileRandom),
        6 => Some(EvictionPolicy::VolatileTtl),
        7 => Some(EvictionPolicy::AllKeysLfu),
        8 => Some(EvictionPolicy::VolatileLfu),
        _ => return Err(CatalogError::InvalidPolicy(b)),
    };
    if mode == NsMode::Durable && policy.is_some() {
        return Err(CatalogError::InvalidPolicy(b));
    }
    Ok(policy)
}

/// Bounds-checked little-endian reads; every short read is `Truncated`.
struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], CatalogError> {
        let end = self.at.checked_add(len).ok_or(CatalogError::Truncated)?;
        let slice = self.buf.get(self.at..end).ok_or(CatalogError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CatalogError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16_le(&mut self) -> Result<u16, CatalogError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().expect("length checked")))
    }

    fn u32_le(&mut self) -> Result<u32, CatalogError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().expect("length checked")))
    }

    fn u64_le(&mut self) -> Result<u64, CatalogError> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().expect("length checked")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, name: &[u8], mode: NsMode) -> NsSpec {
        NsSpec {
            id: NsId(id),
            name: name.to_vec(),
            mode,
            fsync: None,
            policy: None,
            maxmemory: None,
        }
    }

    #[test]
    fn empty_catalog_round_trips() {
        let cat = NsCatalog { next_id: 16, entries: Vec::new() };
        assert_eq!(NsCatalog::decode(&cat.encode()), Ok(cat));
    }

    #[test]
    fn every_mode_fsync_policy_combination_round_trips() {
        let policies = [
            None,
            Some(EvictionPolicy::NoEviction),
            Some(EvictionPolicy::AllKeysLru),
            Some(EvictionPolicy::VolatileLru),
            Some(EvictionPolicy::AllKeysRandom),
            Some(EvictionPolicy::VolatileRandom),
            Some(EvictionPolicy::VolatileTtl),
            Some(EvictionPolicy::AllKeysLfu),
            Some(EvictionPolicy::VolatileLfu),
        ];
        let mut entries = Vec::new();
        let mut id = FIRST_NAMED_NS_ID;
        // Memory: every policy value, alternating budget inherit/explicit.
        for (i, policy) in policies.iter().enumerate() {
            let mut e = entry(id, format!("mem-{i}").as_bytes(), NsMode::Memory);
            e.policy = *policy;
            e.maxmemory = if i % 2 == 0 { None } else { Some(1 << (20 + i)) };
            entries.push(e);
            id += 1;
        }
        // Durable: both classes (no policy — ADR-0015 D5).
        for (i, fsync) in [FsyncClass::Everysec, FsyncClass::Always].into_iter().enumerate() {
            let mut e = entry(id, format!("dur-{i}").as_bytes(), NsMode::Durable);
            e.fsync = Some(fsync);
            entries.push(e);
            id += 1;
        }
        // Topic: format-valid (mode byte 2); the registry rejects it at
        // seed time until M5.
        entries.push(entry(id, b"topic-0", NsMode::Topic));
        let cat = NsCatalog { next_id: id + 1, entries };
        assert_eq!(NsCatalog::decode(&cat.encode()), Ok(cat));
    }

    #[test]
    fn durable_fsync_none_encodes_as_resolved_everysec() {
        let cat = NsCatalog { next_id: 17, entries: vec![entry(16, b"ledger", NsMode::Durable)] };
        let decoded = NsCatalog::decode(&cat.encode()).expect("decode");
        assert_eq!(decoded.entries[0].fsync, Some(FsyncClass::Everysec));
    }

    #[test]
    fn truncation_at_every_prefix_length_errors() {
        let mut e = entry(16, b"ledger", NsMode::Durable);
        e.fsync = Some(FsyncClass::Always);
        let cat = NsCatalog { next_id: 17, entries: vec![e, entry(17, b"cache", NsMode::Memory)] };
        let buf = cat.encode();
        for cut in 0..buf.len() {
            assert!(NsCatalog::decode(&buf[..cut]).is_err(), "cut at {cut} must not decode");
        }
    }

    #[test]
    fn bad_version_errors() {
        let cat = NsCatalog { next_id: 16, entries: Vec::new() };
        let mut buf = cat.encode();
        buf[0] = 2;
        assert_eq!(NsCatalog::decode(&buf), Err(CatalogError::UnknownVersion(2)));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let cat = NsCatalog {
            next_id: 18,
            entries: vec![entry(16, b"one", NsMode::Memory), entry(16, b"two", NsMode::Memory)],
        };
        assert_eq!(NsCatalog::decode(&cat.encode()), Err(CatalogError::DuplicateId(16)));
    }

    #[test]
    fn reserved_ids_are_rejected() {
        let cat = NsCatalog { next_id: 16, entries: vec![entry(3, b"sneaky", NsMode::Memory)] };
        assert_eq!(NsCatalog::decode(&cat.encode()), Err(CatalogError::ReservedId(3)));
    }

    #[test]
    fn invalid_bytes_are_typed_errors() {
        let cat = NsCatalog { next_id: 17, entries: vec![entry(16, b"cache", NsMode::Memory)] };
        let buf = cat.encode();
        // Entry layout after the 9-byte header: id(4) mode(1) fsync(1)
        // policy(1) maxmemory(8) name_len(2) name.
        let (mode_at, fsync_at, policy_at) = (13, 14, 15);
        let mut bad = buf.clone();
        bad[mode_at] = 3;
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidMode(3)));
        let mut bad = buf.clone();
        bad[fsync_at] = 9;
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidFsync(9)));
        let mut bad = buf.clone();
        bad[fsync_at] = 1; // everysec on a memory entry
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidFsync(1)));
        let mut bad = buf.clone();
        bad[policy_at] = 9;
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidPolicy(9)));
        // Durable entry rules: fsync = none and policy ≠ inherit both fail.
        let mut bad = buf.clone();
        bad[mode_at] = 1; // durable, fsync byte still 0
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidFsync(0)));
        let mut bad = buf;
        bad[mode_at] = 1;
        bad[fsync_at] = 2;
        bad[policy_at] = 1;
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidPolicy(1)));
    }

    #[test]
    fn invalid_names_are_rejected() {
        let cat = NsCatalog { next_id: 17, entries: vec![entry(16, b"ok", NsMode::Memory)] };
        let mut buf = cat.encode();
        let space_at = buf.len() - 2;
        buf[space_at] = b' ';
        assert_eq!(NsCatalog::decode(&buf), Err(CatalogError::InvalidName));
        // Default-name collisions are registry rules and rejected here too.
        let dflt = NsCatalog { next_id: 17, entries: vec![entry(16, b"db0", NsMode::Memory)] };
        assert_eq!(NsCatalog::decode(&dflt.encode()), Err(CatalogError::InvalidName));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let cat = NsCatalog { next_id: 16, entries: Vec::new() };
        let mut buf = cat.encode();
        buf.push(0);
        assert_eq!(NsCatalog::decode(&buf), Err(CatalogError::TrailingBytes));
    }

    #[test]
    fn maxmemory_sentinel_is_inherit() {
        let mut e = entry(16, b"cap", NsMode::Memory);
        e.maxmemory = Some(4096);
        let cat = NsCatalog { next_id: 17, entries: vec![e] };
        let decoded = NsCatalog::decode(&cat.encode()).expect("decode");
        assert_eq!(decoded.entries[0].maxmemory, Some(4096));
        let mut e = entry(16, b"cap", NsMode::Memory);
        e.maxmemory = None;
        let cat = NsCatalog { next_id: 17, entries: vec![e] };
        let decoded = NsCatalog::decode(&cat.encode()).expect("decode");
        assert_eq!(decoded.entries[0].maxmemory, None);
    }
}
