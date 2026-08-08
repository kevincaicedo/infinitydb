//! Namespace-catalog encoding **v2** (M2-S08/ADR-0015 D3 shape; M4-S19/
//! ADR-0062 D6 adds the tier block): the bytes that ride `inf-log`'s
//! `META` envelope. The envelope (write/fsync/rename swap, CRC, version
//! tag) is `inf-log`'s protocol and treats this payload as opaque;
//! `inf-store` owns only the encoding and still never opens a file.
//!
//! ## Wire format v2 (all integers little-endian)
//!
//! ```text
//! catalog := version: u8 (= 2)
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
//!            tier:      u8         # 0 = absent, 1 = tier block follows
//!            [tier_block]          # only when tier = 1 (46 bytes)
//!            name_len:  u16
//!            name:      name_len bytes
//! tier_block := mem_budget: u64 · disk_budget: u64 ·
//!               mutable_permille: u16 · maintain_slice: u64 ·
//!               cold_read_qd: u16 · compaction_dead_ratio: u8 ·
//!               compaction_slice: u64 · blob_threshold: u32 ·
//!               tier_io_mode: u8 (0 = buffered, 1 = direct) ·
//!               tail_stall_timeout_ms: u32
//! ```
//!
//! **v1 decodes forever** (ADR-0062 D6): v1 is v2 without the tier byte,
//! and v0.3.0-alpha shipped v1 catalogs — refusing them would turn an
//! upgrade into data loss. The writer always emits v2; versions above 2
//! fail-stop exactly as before.
//!
//! Decode is **fail-stop** for callers (§8.4 honesty): unknown version,
//! truncation, trailing bytes, invalid mode/fsync/policy/tier bytes,
//! invalid or reserved names, reserved ids, and duplicate ids are all
//! typed errors — nothing is skipped or repaired. Cross-field rules are
//! enforced too: encode always writes the *resolved* fsync class for
//! durable entries (`None` normalizes to `everysec`, the registry
//! default), so decode requires durable ⇒ fsync ≠ 0 and non-durable ⇒
//! fsync = 0; durable ⇒ policy = 0 (ADR-0015 D5); tier present ⇒ durable
//! (ADR-0062 D1), and a decoded tier block runs the same range gauntlet
//! the command parser does. `maxmemory = u64::MAX` is the reserved
//! inherit sentinel — a literal budget of `u64::MAX` is not
//! representable (it would be meaningless).

use core::fmt;

use inf_log::fs::TierIoMode;
use inf_log::{FsyncClass, NsId};

use crate::evict::EvictionPolicy;
use crate::ns::{FIRST_NAMED_NS_ID, NsMode, NsSpec, TierSpec, is_default_name, valid_ns_name};

const CATALOG_VERSION: u8 = 2;
/// The last version whose payloads this decoder still accepts (v1 = the
/// v0.3.0-alpha on-disk catalogs — tier-absent by construction).
const CATALOG_VERSION_V1: u8 = 1;
/// Fixed tier-block size (ADR-0062 D6).
const TIER_BLOCK_BYTES: usize = 46;

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
    /// Tier presence byte outside `0..=1` (M4-S19, ADR-0062 D6).
    InvalidTierByte(u8),
    /// Tier block violates a cross-field or range rule; the reason names
    /// it (the same gauntlet the command parser runs — one vocabulary).
    InvalidTierConfig(&'static str),
    /// A tiered entry carries `MAXMEMORY` (M4-S27, ADR-0068 D1): one
    /// budget authority per namespace — the parser refuses it, so a
    /// catalog holding it is corrupt or foreign.
    TierOwnsBudget(u32),
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
            CatalogError::InvalidTierByte(b) => write!(f, "invalid tier presence byte {b}"),
            CatalogError::InvalidTierConfig(reason) => write!(f, "invalid tier config: {reason}"),
            CatalogError::TierOwnsBudget(id) => {
                write!(
                    f,
                    "tiered namespace id {id} carries MAXMEMORY (MEM-BUDGET is its one budget authority)"
                )
            }
        }
    }
}

impl std::error::Error for CatalogError {}

impl NsCatalog {
    /// Encodes the catalog into the v2 payload (see module docs). Durable
    /// entries always encode a resolved fsync class (`None` → `everysec`).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let payload: usize = self
            .entries
            .iter()
            .map(|e| 17 + if e.tier.is_some() { TIER_BLOCK_BYTES } else { 0 } + e.name.len())
            .sum();
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
            match &e.tier {
                None => out.push(0),
                Some(tier) => {
                    debug_assert!(tier.validate().is_ok(), "registered tier specs are validated");
                    out.push(1);
                    encode_tier_block(tier, &mut out);
                }
            }
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&e.name);
        }
        out
    }

    /// Decodes a v1 or v2 payload, enforcing every format and registry
    /// rule (see module docs). v1 — the v0.3.0-alpha on-disk format — is
    /// v2 without the tier byte and decodes with every entry tier-absent
    /// (ADR-0062 D6: a shipped catalog never becomes unreadable).
    ///
    /// # Errors
    /// A [`CatalogError`] naming the violated rule; callers fail-stop.
    pub fn decode(buf: &[u8]) -> Result<NsCatalog, CatalogError> {
        let mut r = Cursor { buf, at: 0 };
        let version = r.u8()?;
        if version != CATALOG_VERSION && version != CATALOG_VERSION_V1 {
            return Err(CatalogError::UnknownVersion(version));
        }
        let next_id = r.u32_le()?;
        let count = r.u32_le()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let entry = decode_entry(&mut r, version)?;
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

fn decode_entry(r: &mut Cursor<'_>, version: u8) -> Result<NsSpec, CatalogError> {
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
    let tier = if version >= CATALOG_VERSION {
        match r.u8()? {
            0 => None,
            1 => Some(decode_tier_block(r, mode)?),
            b => return Err(CatalogError::InvalidTierByte(b)),
        }
    } else {
        None
    };
    if tier.is_some() && maxmemory.is_some() {
        // The same rule the command parser enforces (ADR-0068 D1): a
        // tiered namespace's budget authority is MEM-BUDGET alone.
        return Err(CatalogError::TierOwnsBudget(id));
    }
    let name_len = r.u16_le()?;
    let name = r.bytes(usize::from(name_len))?.to_vec();
    if !valid_ns_name(&name) || is_default_name(&name) {
        return Err(CatalogError::InvalidName);
    }
    Ok(NsSpec { id: NsId(id), name, mode, fsync, policy, maxmemory, tier })
}

fn encode_tier_block(tier: &TierSpec, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&tier.mem_budget_bytes.to_le_bytes());
    out.extend_from_slice(&tier.disk_budget_bytes.to_le_bytes());
    // Permille ≤ 999 by validation — the u16 narrowing is exact.
    out.extend_from_slice(&(tier.mutable_permille as u16).to_le_bytes());
    out.extend_from_slice(&tier.maintain_slice_bytes.to_le_bytes());
    out.extend_from_slice(&tier.cold_read_qd.to_le_bytes());
    out.push(tier.compaction_dead_ratio_pct);
    out.extend_from_slice(&tier.compaction_slice_bytes.to_le_bytes());
    out.extend_from_slice(&tier.blob_threshold_bytes.to_le_bytes());
    out.push(match tier.tier_io_mode {
        TierIoMode::Buffered => 0,
        TierIoMode::Direct => 1,
    });
    out.extend_from_slice(&tier.tail_stall_timeout_ms.to_le_bytes());
    debug_assert_eq!(out.len() - start, TIER_BLOCK_BYTES, "the D6 block size is the contract");
}

fn decode_tier_block(r: &mut Cursor<'_>, mode: NsMode) -> Result<TierSpec, CatalogError> {
    if mode != NsMode::Durable {
        return Err(CatalogError::InvalidTierConfig("tier config requires MODE durable"));
    }
    let mem_budget_bytes = r.u64_le()?;
    let disk_budget_bytes = r.u64_le()?;
    let mutable_permille = u32::from(r.u16_le()?);
    let maintain_slice_bytes = r.u64_le()?;
    let cold_read_qd = r.u16_le()?;
    let compaction_dead_ratio_pct = r.u8()?;
    let compaction_slice_bytes = r.u64_le()?;
    let blob_threshold_bytes = r.u32_le()?;
    let tier_io_mode = match r.u8()? {
        0 => TierIoMode::Buffered,
        1 => TierIoMode::Direct,
        _ => return Err(CatalogError::InvalidTierConfig("invalid tier io mode byte")),
    };
    let tail_stall_timeout_ms = r.u32_le()?;
    let tier = TierSpec {
        mem_budget_bytes,
        disk_budget_bytes,
        mutable_permille,
        maintain_slice_bytes,
        cold_read_qd,
        compaction_dead_ratio_pct,
        compaction_slice_bytes,
        blob_threshold_bytes,
        tier_io_mode,
        tail_stall_timeout_ms,
    };
    // The same range gauntlet the command parser runs (one vocabulary):
    // a persisted block that fails it is corrupt or foreign, fail-stop.
    tier.validate().map_err(CatalogError::InvalidTierConfig)?;
    Ok(tier)
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
            tier: None,
        }
    }

    /// A v1 payload for the given entries — byte-for-byte the
    /// v0.3.0-alpha encoder's output (no tier byte), kept here so the
    /// upgrade path is pinned against real v1 bytes, not against this
    /// crate's current writer.
    fn encode_v1(cat: &NsCatalog) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend_from_slice(&cat.next_id.to_le_bytes());
        out.extend_from_slice(&(cat.entries.len() as u32).to_le_bytes());
        for e in &cat.entries {
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
        buf[0] = 3;
        assert_eq!(NsCatalog::decode(&buf), Err(CatalogError::UnknownVersion(3)));
    }

    /// M4-S19 (ADR-0062 D6): a tiered entry round-trips its whole tier
    /// block — every field off its default so a swapped pair would show.
    #[test]
    fn tiered_entry_round_trips_every_field() {
        use crate::ns::TierSpec;
        let tier = TierSpec {
            mem_budget_bytes: 96 << 20,
            disk_budget_bytes: 5 << 30,
            mutable_permille: 300,
            maintain_slice_bytes: 2 << 20,
            cold_read_qd: 128,
            compaction_dead_ratio_pct: 60,
            compaction_slice_bytes: 512 << 10,
            blob_threshold_bytes: 8 << 20,
            tier_io_mode: TierIoMode::Buffered,
            tail_stall_timeout_ms: 2_500,
        };
        let mut e = entry(16, b"tiered", NsMode::Durable);
        e.fsync = Some(FsyncClass::Always);
        e.tier = Some(tier);
        let cat = NsCatalog { next_id: 17, entries: vec![e, entry(17, b"plain", NsMode::Memory)] };
        let decoded = NsCatalog::decode(&cat.encode()).expect("decode");
        assert_eq!(decoded, cat);
        assert_eq!(decoded.entries[0].tier, Some(tier));
        assert_eq!(decoded.entries[1].tier, None);
    }

    /// The v0.3.0-alpha upgrade path: a v1 payload (no tier byte)
    /// decodes forever, every entry tier-absent.
    #[test]
    fn v1_payloads_decode_with_tier_absent() {
        let mut e = entry(16, b"ledger", NsMode::Durable);
        e.fsync = Some(FsyncClass::Everysec);
        let cat = NsCatalog { next_id: 18, entries: vec![e, entry(17, b"cache", NsMode::Memory)] };
        let decoded = NsCatalog::decode(&encode_v1(&cat)).expect("v1 decodes");
        assert_eq!(decoded, cat, "v1 is v2 with every entry tier-absent");
    }

    /// The v2 strictness discipline extends over the tier block:
    /// truncation at every prefix of a tiered entry errors, and the
    /// invalid tier bytes are typed.
    #[test]
    fn tier_block_truncation_and_invalid_bytes_error() {
        use crate::ns::TierSpec;
        let mut e = entry(16, b"tiered", NsMode::Durable);
        e.fsync = Some(FsyncClass::Everysec);
        e.tier = Some(TierSpec::for_budget(64 << 20));
        let cat = NsCatalog { next_id: 17, entries: vec![e] };
        let buf = cat.encode();
        for cut in 0..buf.len() {
            assert!(NsCatalog::decode(&buf[..cut]).is_err(), "cut at {cut} must not decode");
        }
        // Entry layout after the 9-byte header: id(4) mode(1) fsync(1)
        // policy(1) maxmemory(8) tier(1) block(46) …
        let tier_at = 9 + 15;
        let mut bad = buf.clone();
        bad[tier_at] = 2;
        assert_eq!(NsCatalog::decode(&bad), Err(CatalogError::InvalidTierByte(2)));
        // Dead-ratio byte sits at block offset 8+8+2+8+2 = 28: the D2
        // clamp holds at decode too (the S16 canary trigger is
        // unrepresentable through persistence, not just through parse).
        let mut bad = buf.clone();
        bad[tier_at + 1 + 28] = 10;
        assert!(matches!(NsCatalog::decode(&bad), Err(CatalogError::InvalidTierConfig(_))));
        // io-mode byte at block offset 8+8+2+8+2+1+8+4 = 41.
        let mut bad = buf;
        bad[tier_at + 1 + 41] = 9;
        assert!(matches!(NsCatalog::decode(&bad), Err(CatalogError::InvalidTierConfig(_))));
    }

    /// Tier on a non-durable entry is a cross-field violation at decode
    /// (the D1 rule holds against hand-crafted bytes, not just the
    /// registry).
    #[test]
    fn tier_on_non_durable_entry_is_rejected() {
        use crate::ns::TierSpec;
        let mut e = entry(16, b"tiered", NsMode::Durable);
        e.fsync = Some(FsyncClass::Everysec);
        e.tier = Some(TierSpec::for_budget(64 << 20));
        let cat = NsCatalog { next_id: 17, entries: vec![e] };
        let mut buf = cat.encode();
        // Flip the mode byte to memory; zero the fsync byte to keep the
        // fsync cross-rule satisfied — the tier rule must fire.
        buf[9 + 4] = 0;
        buf[9 + 5] = 0;
        assert_eq!(
            NsCatalog::decode(&buf),
            Err(CatalogError::InvalidTierConfig("tier config requires MODE durable"))
        );
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

    /// M4-S27 (ADR-0068 D1): a tiered entry carrying `MAXMEMORY` is
    /// refused by the decoder like it is by the parser — the registry can
    /// never hold it, so bytes claiming it are corrupt or foreign.
    #[test]
    fn tiered_entry_with_maxmemory_is_rejected() {
        let bad = NsSpec {
            maxmemory: Some(1 << 20),
            fsync: Some(FsyncClass::Everysec),
            tier: Some(TierSpec::for_budget(64 << 20)),
            ..entry(16, b"hot", NsMode::Durable)
        };
        let cat = NsCatalog { next_id: 17, entries: vec![bad] };
        assert_eq!(NsCatalog::decode(&cat.encode()), Err(CatalogError::TierOwnsBudget(16)));
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
