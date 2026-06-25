//! Namespace registry **v2** (M1-S08/M2-S08, master plan §4.2): the identity seam
//! where M2 durability classes and M5 topics attach. Public activation covers
//! `memory` and M2 `durable` namespaces; `topic` remains reserved for M5.
//!
//! The 16 default namespaces (`db0`..`db15`, Redis `SELECT 0..15`) are
//! implicit in [`Keyspace`](crate::Keyspace) and share the server-level
//! eviction config (Redis instance-wide `maxmemory` semantics). Named
//! entries created here carry their own policy/budget; they become
//! *addressable* keyspaces when M2 adds namespace selection — recorded
//! limitation: in M1 they are registry + config state, not key storage.
//! Registries replicate per cell via the `INF.NS` scatter program (L1: no
//! shared registry, every cell owns its copy).

use inf_simd::crc32c;

use crate::evict::EvictionPolicy;

const CATALOG_MAGIC: &[u8; 8] = b"INFNSCAT";
const CATALOG_VERSION_V1: u16 = 1;
const CATALOG_VERSION: u16 = 2;
const CATALOG_HEADER_V1_LEN: usize = 8 + 2 + 4;
const CATALOG_HEADER_LEN: usize = 8 + 2 + 4 + 4;
const CATALOG_CRC_LEN: usize = 4;
const CATALOG_ENTRY_FIXED_MAX_LEN: usize = 4 + 1 + 1 + 1 + 1 + 1 + 8;
const FIRST_NAMED_NS_ID: u32 = 16;
const EXHAUSTED_NAMED_NS_ID: u32 = u32::MAX;

/// Maximum valid namespace name length in the namespace catalog.
pub const MAX_NS_NAME_LEN: usize = 128;

/// Hard bound for named namespaces per node. This keeps catalog decode,
/// duplicate checks, scatter DDL, and INFO/LIST work bounded.
pub const MAX_NAMED_NAMESPACES: usize = 1024;

/// Maximum encoded byte length for a valid namespace catalog v2 image.
pub const MAX_NAMESPACE_CATALOG_BYTES: usize = CATALOG_HEADER_LEN
    + CATALOG_CRC_LEN
    + MAX_NAMED_NAMESPACES * (MAX_NS_NAME_LEN + CATALOG_ENTRY_FIXED_MAX_LEN);

/// Store-owned namespace id. `0..15` are the Redis default databases; named
/// namespace ids start at 16 and are never reused by `NsRegistry`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NsId(u32);

impl NsId {
    #[inline]
    pub const fn new(raw: u32) -> NsId {
        NsId(raw)
    }

    #[inline]
    pub const fn first_named() -> NsId {
        NsId(FIRST_NAMED_NS_ID)
    }

    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn is_named(self) -> bool {
        self.0 >= FIRST_NAMED_NS_ID && self.0 < EXHAUSTED_NAMED_NS_ID
    }

    #[inline]
    fn next_after_alloc(self) -> Option<NsId> {
        (self.0 < EXHAUSTED_NAMED_NS_ID).then_some(NsId(self.0 + 1))
    }
}

/// Durability class of a namespace (§4.2).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum NsMode {
    #[default]
    Memory,
    Durable,
    Topic,
}

impl NsMode {
    pub fn parse(text: &str) -> Option<NsMode> {
        Some(match text {
            "memory" => NsMode::Memory,
            "durable" => NsMode::Durable,
            "topic" => NsMode::Topic,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            NsMode::Memory => "memory",
            NsMode::Durable => "durable",
            NsMode::Topic => "topic",
        }
    }
}

/// Fsync/loss-window policy for a durable namespace (§8.2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NsFsyncPolicy {
    Always,
    Everysec,
}

impl NsFsyncPolicy {
    pub fn parse(text: &str) -> Option<NsFsyncPolicy> {
        Some(match text {
            "always" => NsFsyncPolicy::Always,
            "everysec" => NsFsyncPolicy::Everysec,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            NsFsyncPolicy::Always => "always",
            NsFsyncPolicy::Everysec => "everysec",
        }
    }
}

/// One named-namespace registry entry (the §3.2 freeze: id/name, mode,
/// durable fsync policy, eviction policy, memory budget).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NsSpec {
    /// Stable id serialized in log records and never reused by `NsRegistry`.
    pub id: NsId,
    pub name: Vec<u8>,
    pub mode: NsMode,
    /// Required for `durable`; invalid for `memory` and `topic`.
    pub fsync: Option<NsFsyncPolicy>,
    /// `None` inherits the server `maxmemory-policy`.
    pub policy: Option<EvictionPolicy>,
    /// Node-wide budget in bytes; `None` inherits the server `maxmemory`.
    pub maxmemory: Option<u64>,
}

/// Public namespace creation request. The registry assigns the stable id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NsCreateSpec {
    pub name: Vec<u8>,
    pub mode: NsMode,
    /// Required for `durable`; invalid for `memory` and `topic`.
    pub fsync: Option<NsFsyncPolicy>,
    /// `None` inherits the server `maxmemory-policy`.
    pub policy: Option<EvictionPolicy>,
    /// Node-wide budget in bytes; `None` inherits the server `maxmemory`.
    pub maxmemory: Option<u64>,
}

impl NsCreateSpec {
    fn into_spec(self, id: NsId) -> NsSpec {
        NsSpec {
            id,
            name: self.name,
            mode: self.mode,
            fsync: self.fsync,
            policy: self.policy,
            maxmemory: self.maxmemory,
        }
    }
}

/// Validated namespace-catalog snapshot. It carries the monotonic next-id
/// watermark so `DROP` followed by restart cannot reuse a log-visible id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NsCatalog {
    next_id: NsId,
    specs: Vec<NsSpec>,
}

impl NsCatalog {
    pub fn empty() -> NsCatalog {
        NsCatalog { next_id: NsId::first_named(), specs: Vec::new() }
    }

    pub fn new(next_id: NsId, specs: Vec<NsSpec>) -> Result<NsCatalog, NsCatalogError> {
        validate_catalog_snapshot(next_id, &specs)?;
        Ok(NsCatalog { next_id, specs })
    }

    fn from_validated(next_id: NsId, specs: Vec<NsSpec>) -> NsCatalog {
        debug_assert!(validate_catalog_snapshot(next_id, &specs).is_ok());
        NsCatalog { next_id, specs }
    }

    #[cfg(test)]
    pub(crate) fn from_parts_unchecked_for_test(next_id: NsId, specs: Vec<NsSpec>) -> NsCatalog {
        NsCatalog { next_id, specs }
    }

    #[inline]
    pub fn next_id(&self) -> NsId {
        self.next_id
    }

    #[inline]
    pub fn specs(&self) -> &[NsSpec] {
        &self.specs
    }

    pub fn into_specs(self) -> Vec<NsSpec> {
        self.specs
    }
}

/// Typed registry failures (the command layer maps these to reply strings).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NsError {
    Exists,
    Unknown,
    /// Namespace mode not yet supported by the public create/drop path.
    ModeNotSupported(NsMode),
    /// `MODE durable` must name the §8.2 loss-window policy explicitly.
    FsyncRequired,
    /// `FSYNC` belongs only to `MODE durable`.
    FsyncNotAllowed(NsMode),
    /// Named namespace count is bounded for catalog decode and scatter DDL.
    TooManyNamespaces,
    /// The monotonic named id space is exhausted.
    NamespaceIdsExhausted,
    /// Default namespaces (`db0`..`db15`) cannot be created or dropped.
    DefaultImmutable,
    InvalidName,
}

/// Namespace catalog byte-format errors (M2-S08, ADR-0027).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NsCatalogError {
    BadMagic,
    UnsupportedVersion(u16),
    Truncated,
    TrailingBytes { at: usize, len: usize },
    CrcMismatch { stored: u32, computed: u32 },
    TooManyEntries { count: u32 },
    InvalidName { index: u32 },
    InvalidShape { index: u32, error: NsError },
    DuplicateName { index: u32 },
    DuplicateId { index: u32, id: u32 },
    InvalidId { index: u32, id: u32 },
    InvalidNextId { next_id: u32 },
    IdBeyondNext { index: u32, id: u32, next_id: u32 },
    InvalidMode { index: u32, code: u8 },
    InvalidFsync { index: u32, code: u8 },
    InvalidEviction { index: u32, code: u8 },
    InvalidMaxmemoryFlag { index: u32, code: u8 },
}

/// Valid namespace names: 1..=128 bytes of `[a-zA-Z0-9_.-]`, not colliding
/// with the reserved default names.
pub fn valid_ns_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NS_NAME_LEN
        && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn is_default_name(name: &[u8]) -> bool {
    let Some(rest) = name.strip_prefix(b"db") else { return false };
    !rest.is_empty()
        && rest.len() <= 2
        && rest.iter().all(u8::is_ascii_digit)
        && core::str::from_utf8(rest).is_ok_and(|n| n.parse::<u8>().is_ok_and(|n| n < 16))
}

/// Per-cell registry of named namespaces (insertion-ordered).
#[derive(Debug)]
pub struct NsRegistry {
    named: Vec<NsSpec>,
    next_id: NsId,
}

impl Default for NsRegistry {
    fn default() -> NsRegistry {
        NsRegistry { named: Vec::new(), next_id: NsId::first_named() }
    }
}

impl NsRegistry {
    /// Build a registry from a validated on-disk namespace catalog.
    ///
    /// This is a recovery-only path: it accepts validated catalog specs
    /// without applying public DDL side effects.
    pub fn from_recovered_catalog(catalog: NsCatalog) -> Result<NsRegistry, NsCatalogError> {
        validate_catalog_snapshot(catalog.next_id, &catalog.specs)?;
        Ok(NsRegistry { named: catalog.specs, next_id: catalog.next_id })
    }

    /// Atomically replace this registry with recovered catalog state.
    ///
    /// Validation happens before assignment, so a corrupt catalog cannot
    /// partially overwrite the live boot state.
    pub fn replace_with_recovered_catalog(
        &mut self,
        catalog: NsCatalog,
    ) -> Result<(), NsCatalogError> {
        let recovered = NsRegistry::from_recovered_catalog(catalog)?;
        debug_assert!(recovered.named.len() <= MAX_NAMED_NAMESPACES);
        *self = recovered;
        Ok(())
    }

    pub fn create(&mut self, spec: NsCreateSpec) -> Result<NsId, NsError> {
        validate_create_spec_shape(&spec)?;
        if spec.mode == NsMode::Topic {
            return Err(NsError::ModeNotSupported(spec.mode));
        }
        if self.named.len() >= MAX_NAMED_NAMESPACES {
            return Err(NsError::TooManyNamespaces);
        }
        if self.get(&spec.name).is_some() {
            return Err(NsError::Exists);
        }
        let id = self.next_id;
        let Some(next_id) = id.next_after_alloc() else {
            return Err(NsError::NamespaceIdsExhausted);
        };
        self.named.push(spec.into_spec(id));
        self.next_id = next_id;
        Ok(id)
    }

    pub fn drop_ns(&mut self, name: &[u8]) -> Result<(), NsError> {
        if is_default_name(name) {
            return Err(NsError::DefaultImmutable);
        }
        let at = self.named.iter().position(|s| s.name == name).ok_or(NsError::Unknown)?;
        let mode = self.named[at].mode;
        if mode != NsMode::Memory {
            return Err(NsError::ModeNotSupported(mode));
        }
        self.named.remove(at);
        Ok(())
    }

    pub fn get(&self, name: &[u8]) -> Option<&NsSpec> {
        self.named.iter().find(|s| s.name == name)
    }

    pub fn get_by_id(&self, id: NsId) -> Option<&NsSpec> {
        self.named.iter().find(|s| s.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &NsSpec> {
        self.named.iter()
    }

    /// Snapshot the named entries and the monotonic next-id watermark.
    ///
    /// This is a cold control-plane helper for namespace-catalog publish.
    /// It deliberately returns owned state so the file-I/O side never holds
    /// a borrow of the live registry across a reactor boundary.
    pub fn catalog_snapshot(&self) -> NsCatalog {
        NsCatalog::from_validated(self.next_id, self.named.clone())
    }
}

pub fn encode_namespace_catalog(
    catalog: &NsCatalog,
    out: &mut Vec<u8>,
) -> Result<(), NsCatalogError> {
    validate_catalog_snapshot(catalog.next_id, &catalog.specs)?;
    out.clear();
    out.extend_from_slice(CATALOG_MAGIC);
    put_u16(CATALOG_VERSION, out);
    put_u32(catalog.specs.len() as u32, out);
    put_u32(catalog.next_id.get(), out);
    for spec in &catalog.specs {
        put_u32(spec.id.get(), out);
        out.push(spec.name.len() as u8);
        out.extend_from_slice(&spec.name);
        out.push(mode_code(spec.mode));
        out.push(fsync_code(spec.fsync));
        out.push(eviction_code(spec.policy));
        match spec.maxmemory {
            Some(bytes) => {
                out.push(1);
                put_u64(bytes, out);
            }
            None => out.push(0),
        }
    }
    append_crc(out);
    Ok(())
}

pub fn decode_namespace_catalog(bytes: &[u8]) -> Result<NsCatalog, NsCatalogError> {
    if bytes.len() < CATALOG_HEADER_V1_LEN + CATALOG_CRC_LEN {
        return Err(NsCatalogError::Truncated);
    }
    verify_crc(bytes)?;
    let body_len = bytes.len() - CATALOG_CRC_LEN;
    let mut cursor = CatalogCursor::new(&bytes[..body_len]);
    let magic = cursor.read_bytes(CATALOG_MAGIC.len())?;
    if magic != CATALOG_MAGIC {
        return Err(NsCatalogError::BadMagic);
    }
    let version = cursor.read_u16()?;
    let count = cursor.read_u32()?;
    if count as usize > MAX_NAMED_NAMESPACES {
        return Err(NsCatalogError::TooManyEntries { count });
    }
    let next_id = match version {
        CATALOG_VERSION_V1 => NsId::new(FIRST_NAMED_NS_ID + count),
        CATALOG_VERSION => NsId::new(cursor.read_u32()?),
        _ => return Err(NsCatalogError::UnsupportedVersion(version)),
    };
    let mut specs = Vec::with_capacity(count as usize);
    for index in 0..count {
        specs.push(match version {
            CATALOG_VERSION_V1 => {
                let id = NsId::new(FIRST_NAMED_NS_ID + index);
                decode_catalog_entry_v1(&mut cursor, index, id)?
            }
            CATALOG_VERSION => decode_catalog_entry_v2(&mut cursor, index)?,
            _ => unreachable!("version checked above"),
        });
    }
    if cursor.at != body_len {
        return Err(NsCatalogError::TrailingBytes { at: cursor.at, len: body_len });
    }
    NsCatalog::new(next_id, specs)
}

fn validate_catalog_snapshot(next_id: NsId, specs: &[NsSpec]) -> Result<(), NsCatalogError> {
    if next_id.get() < FIRST_NAMED_NS_ID {
        return Err(NsCatalogError::InvalidNextId { next_id: next_id.get() });
    }
    let count = specs.len();
    if count > MAX_NAMED_NAMESPACES {
        return Err(NsCatalogError::TooManyEntries { count: count as u32 });
    }
    for (index, spec) in specs.iter().enumerate() {
        if !spec.id.is_named() {
            return Err(NsCatalogError::InvalidId { index: index as u32, id: spec.id.get() });
        }
        validate_create_spec_shape(&NsCreateSpec {
            name: spec.name.clone(),
            mode: spec.mode,
            fsync: spec.fsync,
            policy: spec.policy,
            maxmemory: spec.maxmemory,
        })
        .map_err(|error| {
            if error == NsError::InvalidName || error == NsError::DefaultImmutable {
                NsCatalogError::InvalidName { index: index as u32 }
            } else {
                NsCatalogError::InvalidShape { index: index as u32, error }
            }
        })?;
        if spec.id.get() >= next_id.get() {
            return Err(NsCatalogError::IdBeyondNext {
                index: index as u32,
                id: spec.id.get(),
                next_id: next_id.get(),
            });
        }
        if specs[..index].iter().any(|prior| prior.name == spec.name) {
            return Err(NsCatalogError::DuplicateName { index: index as u32 });
        }
        if specs[..index].iter().any(|prior| prior.id == spec.id) {
            return Err(NsCatalogError::DuplicateId { index: index as u32, id: spec.id.get() });
        }
    }
    Ok(())
}

fn validate_create_spec_shape(spec: &NsCreateSpec) -> Result<(), NsError> {
    if !valid_ns_name(&spec.name) {
        return Err(NsError::InvalidName);
    }
    if is_default_name(&spec.name) {
        return Err(NsError::DefaultImmutable);
    }
    match (spec.mode, spec.fsync) {
        (NsMode::Durable, None) => Err(NsError::FsyncRequired),
        (NsMode::Memory | NsMode::Topic, Some(_)) => Err(NsError::FsyncNotAllowed(spec.mode)),
        _ => Ok(()),
    }
}

fn decode_catalog_entry_v1(
    cursor: &mut CatalogCursor<'_>,
    index: u32,
    id: NsId,
) -> Result<NsSpec, NsCatalogError> {
    let name_len = usize::from(cursor.read_u8()?);
    let name = cursor.read_bytes(name_len)?.to_vec();
    let mode = decode_mode(index, cursor.read_u8()?)?;
    let fsync = decode_fsync(index, cursor.read_u8()?)?;
    let policy = decode_eviction(index, cursor.read_u8()?)?;
    let maxmemory = match cursor.read_u8()? {
        0 => None,
        1 => Some(cursor.read_u64()?),
        code => return Err(NsCatalogError::InvalidMaxmemoryFlag { index, code }),
    };
    Ok(NsSpec { id, name, mode, fsync, policy, maxmemory })
}

fn decode_catalog_entry_v2(
    cursor: &mut CatalogCursor<'_>,
    index: u32,
) -> Result<NsSpec, NsCatalogError> {
    let id = NsId::new(cursor.read_u32()?);
    if !id.is_named() {
        return Err(NsCatalogError::InvalidId { index, id: id.get() });
    }
    decode_catalog_entry_v1(cursor, index, id)
}

struct CatalogCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> CatalogCursor<'a> {
    fn new(bytes: &'a [u8]) -> CatalogCursor<'a> {
        CatalogCursor { bytes, at: 0 }
    }

    fn read_u8(&mut self) -> Result<u8, NsCatalogError> {
        let bytes = self.read_bytes(1)?;
        Ok(bytes[0])
    }

    fn read_u16(&mut self) -> Result<u16, NsCatalogError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, NsCatalogError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> Result<u64, NsCatalogError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], NsCatalogError> {
        let end = self.at.checked_add(len).ok_or(NsCatalogError::Truncated)?;
        if end > self.bytes.len() {
            return Err(NsCatalogError::Truncated);
        }
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }
}

fn verify_crc(bytes: &[u8]) -> Result<(), NsCatalogError> {
    let crc_at = bytes.len() - CATALOG_CRC_LEN;
    let stored = u32::from_le_bytes([
        bytes[crc_at],
        bytes[crc_at + 1],
        bytes[crc_at + 2],
        bytes[crc_at + 3],
    ]);
    let computed = crc32c(&bytes[..crc_at]);
    if stored != computed {
        return Err(NsCatalogError::CrcMismatch { stored, computed });
    }
    Ok(())
}

fn append_crc(out: &mut Vec<u8>) {
    let crc = crc32c(out);
    put_u32(crc, out);
}

fn put_u16(value: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(value: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(value: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn mode_code(mode: NsMode) -> u8 {
    match mode {
        NsMode::Memory => 0,
        NsMode::Durable => 1,
        NsMode::Topic => 2,
    }
}

fn decode_mode(index: u32, code: u8) -> Result<NsMode, NsCatalogError> {
    match code {
        0 => Ok(NsMode::Memory),
        1 => Ok(NsMode::Durable),
        2 => Ok(NsMode::Topic),
        _ => Err(NsCatalogError::InvalidMode { index, code }),
    }
}

fn fsync_code(fsync: Option<NsFsyncPolicy>) -> u8 {
    match fsync {
        None => 0,
        Some(NsFsyncPolicy::Always) => 1,
        Some(NsFsyncPolicy::Everysec) => 2,
    }
}

fn decode_fsync(index: u32, code: u8) -> Result<Option<NsFsyncPolicy>, NsCatalogError> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(NsFsyncPolicy::Always)),
        2 => Ok(Some(NsFsyncPolicy::Everysec)),
        _ => Err(NsCatalogError::InvalidFsync { index, code }),
    }
}

fn eviction_code(policy: Option<EvictionPolicy>) -> u8 {
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

fn decode_eviction(index: u32, code: u8) -> Result<Option<EvictionPolicy>, NsCatalogError> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(EvictionPolicy::NoEviction)),
        2 => Ok(Some(EvictionPolicy::AllKeysLru)),
        3 => Ok(Some(EvictionPolicy::VolatileLru)),
        4 => Ok(Some(EvictionPolicy::AllKeysRandom)),
        5 => Ok(Some(EvictionPolicy::VolatileRandom)),
        6 => Ok(Some(EvictionPolicy::VolatileTtl)),
        7 => Ok(Some(EvictionPolicy::AllKeysLfu)),
        8 => Ok(Some(EvictionPolicy::VolatileLfu)),
        _ => Err(NsCatalogError::InvalidEviction { index, code }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn create_spec(name: &[u8], mode: NsMode) -> NsCreateSpec {
        NsCreateSpec { name: name.to_vec(), mode, fsync: None, policy: None, maxmemory: None }
    }

    fn durable_create_spec(name: &[u8], fsync: NsFsyncPolicy) -> NsCreateSpec {
        NsCreateSpec { fsync: Some(fsync), ..create_spec(name, NsMode::Durable) }
    }

    fn memory_create_spec(
        name: &[u8],
        policy: Option<EvictionPolicy>,
        maxmemory: Option<u64>,
    ) -> NsCreateSpec {
        NsCreateSpec { policy, maxmemory, ..create_spec(name, NsMode::Memory) }
    }

    fn spec(id: u32, name: &[u8], mode: NsMode) -> NsSpec {
        NsSpec {
            id: NsId::new(id),
            name: name.to_vec(),
            mode,
            fsync: None,
            policy: None,
            maxmemory: None,
        }
    }

    fn durable_spec(id: u32, name: &[u8], fsync: NsFsyncPolicy) -> NsSpec {
        NsSpec { fsync: Some(fsync), ..spec(id, name, NsMode::Durable) }
    }

    fn memory_spec(
        id: u32,
        name: &[u8],
        policy: Option<EvictionPolicy>,
        maxmemory: Option<u64>,
    ) -> NsSpec {
        NsSpec { policy, maxmemory, ..spec(id, name, NsMode::Memory) }
    }

    fn catalog(specs: Vec<NsSpec>) -> NsCatalog {
        let next =
            specs.iter().map(|spec| spec.id.get()).max().map_or(FIRST_NAMED_NS_ID, |id| id + 1);
        NsCatalog::new(NsId::new(next), specs).expect("valid test catalog")
    }

    fn refresh_crc(bytes: &mut Vec<u8>) {
        bytes.truncate(bytes.len() - CATALOG_CRC_LEN);
        append_crc(bytes);
    }

    fn first_entry_mode_offset(bytes: &[u8]) -> usize {
        let name_len = usize::from(bytes[CATALOG_HEADER_LEN + 4]);
        CATALOG_HEADER_LEN + 4 + 1 + name_len
    }

    fn encode_namespace_catalog_v1_for_test(specs: &[NsSpec], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(CATALOG_MAGIC);
        put_u16(CATALOG_VERSION_V1, out);
        put_u32(specs.len() as u32, out);
        for spec in specs {
            out.push(spec.name.len() as u8);
            out.extend_from_slice(&spec.name);
            out.push(mode_code(spec.mode));
            out.push(fsync_code(spec.fsync));
            out.push(eviction_code(spec.policy));
            match spec.maxmemory {
                Some(bytes) => {
                    out.push(1);
                    put_u64(bytes, out);
                }
                None => out.push(0),
            }
        }
        append_crc(out);
    }

    #[test]
    fn catalog_max_size_constant_bounds_worst_case() {
        let mut specs = Vec::new();
        for index in 0..MAX_NAMED_NAMESPACES {
            let mut name = format!("ns{index:04}").into_bytes();
            name.resize(MAX_NS_NAME_LEN, b'x');
            specs.push(NsSpec {
                id: NsId::new(FIRST_NAMED_NS_ID + index as u32),
                name,
                mode: NsMode::Memory,
                fsync: None,
                policy: Some(EvictionPolicy::AllKeysLfu),
                maxmemory: Some(u64::MAX),
            });
        }
        let catalog = NsCatalog::new(
            NsId::new(FIRST_NAMED_NS_ID + MAX_NAMED_NAMESPACES as u32),
            specs.clone(),
        )
        .expect("catalog");

        let mut bytes = Vec::new();
        encode_namespace_catalog(&catalog, &mut bytes).unwrap();
        assert_eq!(bytes.len(), MAX_NAMESPACE_CATALOG_BYTES);
        assert_eq!(decode_namespace_catalog(&bytes).unwrap(), catalog);
    }

    #[test]
    fn create_list_drop_roundtrip() {
        let mut reg = NsRegistry::default();
        let cache_id = reg.create(create_spec(b"cache", NsMode::Memory)).expect("create");
        assert_eq!(cache_id, NsId::new(FIRST_NAMED_NS_ID));
        assert_eq!(reg.create(create_spec(b"cache", NsMode::Memory)), Err(NsError::Exists));
        assert_eq!(reg.iter().count(), 1);
        assert!(reg.get(b"cache").is_some());
        reg.drop_ns(b"cache").expect("drop");
        assert_eq!(reg.drop_ns(b"cache"), Err(NsError::Unknown));
        assert_eq!(reg.iter().count(), 0);
        let scratch_id = reg.create(create_spec(b"scratch", NsMode::Memory)).expect("create");
        assert_eq!(scratch_id, NsId::new(FIRST_NAMED_NS_ID + 1));
        let snapshot = reg.catalog_snapshot();
        assert_eq!(snapshot.next_id(), NsId::new(FIRST_NAMED_NS_ID + 2));
        assert_eq!(snapshot.specs()[0].id, scratch_id);
    }

    #[test]
    fn durable_create_succeeds_and_topic_is_honestly_rejected() {
        let mut reg = NsRegistry::default();
        let ledger = reg
            .create(durable_create_spec(b"ledger", NsFsyncPolicy::Always))
            .expect("durable always namespace is public in M2");
        let sessions = reg
            .create(durable_create_spec(b"sessions", NsFsyncPolicy::Everysec))
            .expect("durable everysec namespace is public in M2");
        assert_eq!(
            reg.create(create_spec(b"events", NsMode::Topic)),
            Err(NsError::ModeNotSupported(NsMode::Topic))
        );
        assert_eq!(ledger, NsId::new(FIRST_NAMED_NS_ID));
        assert_eq!(sessions, NsId::new(FIRST_NAMED_NS_ID + 1));
        assert_eq!(reg.iter().count(), 2, "durable modes register; topic does not");
    }

    #[test]
    fn durable_requires_explicit_fsync_policy() {
        let mut reg = NsRegistry::default();
        assert_eq!(
            reg.create(create_spec(b"ledger", NsMode::Durable)),
            Err(NsError::FsyncRequired)
        );

        let mut memory = create_spec(b"cache", NsMode::Memory);
        memory.fsync = Some(NsFsyncPolicy::Everysec);
        assert_eq!(reg.create(memory), Err(NsError::FsyncNotAllowed(NsMode::Memory)));

        let mut topic = create_spec(b"events", NsMode::Topic);
        topic.fsync = Some(NsFsyncPolicy::Always);
        assert_eq!(reg.create(topic), Err(NsError::FsyncNotAllowed(NsMode::Topic)));
    }

    #[test]
    fn registry_rejects_unbounded_namespace_growth() {
        let mut reg = NsRegistry::default();
        for i in 0..MAX_NAMED_NAMESPACES {
            let name = format!("ns{i:04}");
            reg.create(create_spec(name.as_bytes(), NsMode::Memory)).expect("within bound");
        }
        assert_eq!(
            reg.create(create_spec(b"overflow", NsMode::Memory)),
            Err(NsError::TooManyNamespaces)
        );
    }

    #[test]
    fn catalog_roundtrip_preserves_durable_policy_contract() {
        let specs = vec![
            memory_spec(16, b"cache", Some(EvictionPolicy::AllKeysLfu), Some(16 * 1024 * 1024)),
            durable_spec(17, b"ledger", NsFsyncPolicy::Always),
            durable_spec(18, b"sessions", NsFsyncPolicy::Everysec),
        ];
        let catalog = catalog(specs);
        let mut bytes = Vec::new();

        encode_namespace_catalog(&catalog, &mut bytes).expect("encode");

        assert_eq!(decode_namespace_catalog(&bytes), Ok(catalog));
    }

    #[test]
    fn recovered_catalog_installs_durable_specs_and_preserves_next_public_id() {
        let specs = vec![
            durable_spec(16, b"ledger", NsFsyncPolicy::Always),
            durable_spec(17, b"sessions", NsFsyncPolicy::Everysec),
            memory_spec(18, b"cache", Some(EvictionPolicy::AllKeysLfu), Some(4096)),
        ];
        let catalog = catalog(specs.clone());

        let reg = NsRegistry::from_recovered_catalog(catalog).expect("recover registry");

        assert_eq!(reg.iter().cloned().collect::<Vec<_>>(), specs);
        assert_eq!(reg.get(b"ledger").expect("ledger").fsync, Some(NsFsyncPolicy::Always));
        let mut public = NsRegistry::default();
        let id = public
            .create(durable_create_spec(b"public-ledger", NsFsyncPolicy::Always))
            .expect("public durable create is active");
        assert_eq!(id, NsId::first_named());
    }

    #[test]
    fn durable_and_topic_namespaces_cannot_drop_without_tombstones() {
        let specs = vec![
            durable_spec(16, b"ledger", NsFsyncPolicy::Always),
            spec(17, b"events", NsMode::Topic),
            memory_spec(18, b"cache", None, None),
        ];
        let mut reg = NsRegistry::from_recovered_catalog(catalog(specs)).expect("recover registry");

        assert_eq!(reg.drop_ns(b"ledger"), Err(NsError::ModeNotSupported(NsMode::Durable)));
        assert_eq!(reg.drop_ns(b"events"), Err(NsError::ModeNotSupported(NsMode::Topic)));
        reg.drop_ns(b"cache").expect("memory drop remains supported");
        assert!(reg.get(b"ledger").is_some());
        assert!(reg.get(b"events").is_some());
        assert!(reg.get(b"cache").is_none());
    }

    #[test]
    fn recovered_catalog_rejects_invalid_specs() {
        let duplicate =
            vec![memory_spec(16, b"cache", None, None), memory_spec(17, b"cache", None, None)];
        let err = NsRegistry::from_recovered_catalog(NsCatalog {
            next_id: NsId::new(18),
            specs: duplicate,
        })
        .expect_err("duplicate recovered name");
        assert_eq!(err, NsCatalogError::DuplicateName { index: 1 });

        let default_name = vec![memory_spec(16, b"db0", None, None)];
        let err = NsRegistry::from_recovered_catalog(NsCatalog {
            next_id: NsId::new(17),
            specs: default_name,
        })
        .expect_err("default recovered name");
        assert_eq!(err, NsCatalogError::InvalidName { index: 0 });

        let missing_fsync = vec![spec(16, b"ledger", NsMode::Durable)];
        let err = NsRegistry::from_recovered_catalog(NsCatalog {
            next_id: NsId::new(17),
            specs: missing_fsync,
        })
        .expect_err("missing durable fsync");
        assert_eq!(err, NsCatalogError::InvalidShape { index: 0, error: NsError::FsyncRequired });
    }

    #[test]
    fn replace_with_recovered_catalog_is_atomic_on_error() {
        let initial = vec![memory_spec(16, b"cache", None, Some(1024))];
        let mut reg =
            NsRegistry::from_recovered_catalog(catalog(initial.clone())).expect("initial registry");

        let bad =
            NsCatalog { next_id: NsId::new(17), specs: vec![memory_spec(16, b"db0", None, None)] };
        let err = reg.replace_with_recovered_catalog(bad).expect_err("invalid recovered catalog");

        assert_eq!(err, NsCatalogError::InvalidName { index: 0 });
        assert_eq!(reg.iter().cloned().collect::<Vec<_>>(), initial);
    }

    #[test]
    fn registry_catalog_roundtrip_uses_live_entries() {
        let mut reg = NsRegistry::default();
        reg.create(memory_create_spec(b"cache", Some(EvictionPolicy::NoEviction), None))
            .expect("create cache");
        reg.create(memory_create_spec(b"scratch", None, Some(4096))).expect("create scratch");
        let mut bytes = Vec::new();

        let live = reg.catalog_snapshot();
        encode_namespace_catalog(&live, &mut bytes).expect("encode registry catalog");
        let decoded = decode_namespace_catalog(&bytes).expect("decode registry catalog");

        assert_eq!(decoded, live);
    }

    #[test]
    fn catalog_decode_rejects_corruption_and_truncation() {
        let catalog = catalog(vec![memory_spec(16, b"cache", None, None)]);
        let mut bytes = Vec::new();
        encode_namespace_catalog(&catalog, &mut bytes).expect("encode");

        let mut corrupt = bytes.clone();
        corrupt[0] ^= 0xFF;
        assert!(matches!(
            decode_namespace_catalog(&corrupt),
            Err(NsCatalogError::CrcMismatch { .. })
        ));

        let truncated = &bytes[..CATALOG_HEADER_V1_LEN + CATALOG_CRC_LEN - 1];
        assert_eq!(decode_namespace_catalog(truncated), Err(NsCatalogError::Truncated));

        let mut bad_magic = bytes;
        bad_magic[0] ^= 0xFF;
        refresh_crc(&mut bad_magic);
        assert_eq!(decode_namespace_catalog(&bad_magic), Err(NsCatalogError::BadMagic));
    }

    #[test]
    fn catalog_decode_rejects_unknown_codes_with_valid_crc() {
        let catalog = catalog(vec![memory_spec(16, b"cache", None, None)]);
        let mut bytes = Vec::new();
        encode_namespace_catalog(&catalog, &mut bytes).expect("encode");
        let mode_at = first_entry_mode_offset(&bytes);

        let mut bad_mode = bytes.clone();
        bad_mode[mode_at] = 99;
        refresh_crc(&mut bad_mode);
        assert_eq!(
            decode_namespace_catalog(&bad_mode),
            Err(NsCatalogError::InvalidMode { index: 0, code: 99 })
        );

        let mut bad_fsync = bytes.clone();
        bad_fsync[mode_at + 1] = 99;
        refresh_crc(&mut bad_fsync);
        assert_eq!(
            decode_namespace_catalog(&bad_fsync),
            Err(NsCatalogError::InvalidFsync { index: 0, code: 99 })
        );

        let mut bad_eviction = bytes.clone();
        bad_eviction[mode_at + 2] = 99;
        refresh_crc(&mut bad_eviction);
        assert_eq!(
            decode_namespace_catalog(&bad_eviction),
            Err(NsCatalogError::InvalidEviction { index: 0, code: 99 })
        );

        let mut bad_maxmemory = bytes;
        bad_maxmemory[mode_at + 3] = 99;
        refresh_crc(&mut bad_maxmemory);
        assert_eq!(
            decode_namespace_catalog(&bad_maxmemory),
            Err(NsCatalogError::InvalidMaxmemoryFlag { index: 0, code: 99 })
        );
    }

    #[test]
    fn catalog_rejects_duplicates_and_invalid_shapes() {
        let duplicate = NsCatalog {
            next_id: NsId::new(18),
            specs: vec![
                memory_spec(16, b"cache", None, None),
                memory_spec(17, b"cache", None, None),
            ],
        };
        let mut bytes = Vec::new();
        assert_eq!(
            encode_namespace_catalog(&duplicate, &mut bytes),
            Err(NsCatalogError::DuplicateName { index: 1 })
        );

        let bad_fsync = NsCatalog {
            next_id: NsId::new(17),
            specs: vec![NsSpec {
                fsync: Some(NsFsyncPolicy::Always),
                ..memory_spec(16, b"cache", None, None)
            }],
        };
        assert_eq!(
            encode_namespace_catalog(&bad_fsync, &mut bytes),
            Err(NsCatalogError::InvalidShape {
                index: 0,
                error: NsError::FsyncNotAllowed(NsMode::Memory),
            })
        );
    }

    #[test]
    fn default_names_are_reserved() {
        let mut reg = NsRegistry::default();
        assert_eq!(reg.create(create_spec(b"db0", NsMode::Memory)), Err(NsError::DefaultImmutable));
        assert_eq!(
            reg.create(create_spec(b"db15", NsMode::Memory)),
            Err(NsError::DefaultImmutable)
        );
        // Not defaults: out-of-range index, non-numeric suffix.
        reg.create(create_spec(b"db16", NsMode::Memory)).expect("db16 is a plain name");
        reg.create(create_spec(b"dbx", NsMode::Memory)).expect("dbx is a plain name");
        assert_eq!(reg.drop_ns(b"db3"), Err(NsError::DefaultImmutable));
    }

    #[test]
    fn catalog_rejects_invalid_ids_and_next_id() {
        let duplicate_id = NsCatalog {
            next_id: NsId::new(18),
            specs: vec![
                memory_spec(16, b"cache", None, None),
                memory_spec(16, b"scratch", None, None),
            ],
        };
        assert_eq!(
            encode_namespace_catalog(&duplicate_id, &mut Vec::new()),
            Err(NsCatalogError::DuplicateId { index: 1, id: 16 })
        );

        let default_id =
            NsCatalog { next_id: NsId::new(17), specs: vec![memory_spec(0, b"cache", None, None)] };
        assert_eq!(
            encode_namespace_catalog(&default_id, &mut Vec::new()),
            Err(NsCatalogError::InvalidId { index: 0, id: 0 })
        );

        let next_too_low = NsCatalog {
            next_id: NsId::new(16),
            specs: vec![memory_spec(16, b"cache", None, None)],
        };
        assert_eq!(
            encode_namespace_catalog(&next_too_low, &mut Vec::new()),
            Err(NsCatalogError::IdBeyondNext { index: 0, id: 16, next_id: 16 })
        );
    }

    #[test]
    fn catalog_v1_decode_assigns_monotonic_ids() {
        let legacy = vec![
            memory_spec(99, b"cache", Some(EvictionPolicy::AllKeysLru), Some(2048)),
            durable_spec(100, b"ledger", NsFsyncPolicy::Always),
        ];
        let mut bytes = Vec::new();
        encode_namespace_catalog_v1_for_test(&legacy, &mut bytes);

        let decoded = decode_namespace_catalog(&bytes).expect("decode v1");

        assert_eq!(decoded.next_id(), NsId::new(18));
        assert_eq!(decoded.specs()[0].id, NsId::new(16));
        assert_eq!(decoded.specs()[0].name, b"cache");
        assert_eq!(decoded.specs()[1].id, NsId::new(17));
        assert_eq!(decoded.specs()[1].name, b"ledger");
    }

    #[test]
    fn name_validation() {
        assert!(valid_ns_name(b"cart-sessions_v2.prod"));
        assert!(!valid_ns_name(b""));
        assert!(!valid_ns_name(b"has space"));
        assert!(!valid_ns_name(&[b'x'; 129]));
    }

    proptest! {
        #[test]
        fn catalog_roundtrip_memory_specs(ids in prop::collection::vec(0u16..2048, 0..32)) {
            let mut specs = Vec::new();
            for id in ids {
                let name = format!("ns{id:04}");
                if specs.iter().any(|spec: &NsSpec| spec.name == name.as_bytes()) {
                    continue;
                }
                let policy = match id % 9 {
                    0 => None,
                    1 => Some(EvictionPolicy::NoEviction),
                    2 => Some(EvictionPolicy::AllKeysLru),
                    3 => Some(EvictionPolicy::VolatileLru),
                    4 => Some(EvictionPolicy::AllKeysRandom),
                    5 => Some(EvictionPolicy::VolatileRandom),
                    6 => Some(EvictionPolicy::VolatileTtl),
                    7 => Some(EvictionPolicy::AllKeysLfu),
                    _ => Some(EvictionPolicy::VolatileLfu),
                };
                let maxmemory = (id % 3 == 0).then_some(u64::from(id) * 1024);
                let ns_id = FIRST_NAMED_NS_ID + specs.len() as u32;
                specs.push(memory_spec(ns_id, name.as_bytes(), policy, maxmemory));
            }
            let catalog = catalog(specs);
            let mut bytes = Vec::new();
            encode_namespace_catalog(&catalog, &mut bytes).expect("encode");
            prop_assert_eq!(decode_namespace_catalog(&bytes), Ok(catalog));
        }
    }
}
