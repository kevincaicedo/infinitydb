//! Namespace registry **v2** (M1-S08 shape, activated by M2-S08 / ADR-0015):
//! the identity seam where durability classes and M5 topics attach. v2
//! accepts `durable` — a durable namespace carries an [`FsyncClass`]
//! (defaulting to `everysec`) and must not carry an eviction policy
//! (durable namespaces do not evict in M2 — ADR-0015 D5). `topic` still
//! returns the documented not-yet-supported error (honesty over silence,
//! L8).
//!
//! The 16 default namespaces (`db0`..`db15`, Redis `SELECT 0..15`) are
//! implicit in [`Keyspace`](crate::Keyspace) and share the server-level
//! eviction config (Redis instance-wide `maxmemory` semantics). Named
//! entries created here carry their own policy/budget and, since M2, their
//! own [`NsId`]: log records name namespaces by id, so ids are allocated
//! once by the caller (a node-level counter starting at
//! [`FIRST_NAMED_NS_ID`]) and never reused (ADR-0015 D2). Registries
//! replicate per cell via the `INF.NS` scatter program (L1: no shared
//! registry, every cell owns its copy).

use inf_log::{FsyncClass, NsId};

use crate::evict::EvictionPolicy;

/// First id available to named namespaces. Ids `0..16` are the implicit
/// default namespaces (`db0`..`db15`, always `memory`) and never appear in
/// the registry or the catalog (ADR-0015 D2).
pub const FIRST_NAMED_NS_ID: u32 = 16;

/// Durability class of a namespace (§4.2). `Memory` and `Durable` are valid
/// since M2-S08; `Topic` arrives with M5.
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

/// One named-namespace registry entry (the §3.2 freeze: id/name, mode,
/// fsync class, eviction policy, memory budget).
///
/// Named-namespace ids start at [`FIRST_NAMED_NS_ID`] (16); ids `0..16` are
/// the implicit defaults (`db0`..`db15`) and are never registered here
/// (ADR-0015 D2 — ids are allocated by the caller, once, and never reused).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NsSpec {
    /// Node-unique id, ≥ [`FIRST_NAMED_NS_ID`]. Log records name namespaces
    /// by this id, so it is persisted in the catalog and never reused.
    pub id: NsId,
    pub name: Vec<u8>,
    pub mode: NsMode,
    /// Durability class. Only durable namespaces carry one; a durable spec
    /// created without it is stored with the documented default
    /// (`Everysec`), so registered durable entries always have `Some`.
    pub fsync: Option<FsyncClass>,
    /// `None` inherits the server `maxmemory-policy`. Must be `None` on
    /// durable namespaces — they do not evict in M2 (ADR-0015 D5).
    pub policy: Option<EvictionPolicy>,
    /// Node-wide budget in bytes; `None` inherits the server `maxmemory`.
    pub maxmemory: Option<u64>,
}

/// Typed registry failures (the command layer maps these to reply strings).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NsError {
    Exists,
    Unknown,
    /// `topic` before M5 (documented not-yet-supported).
    ModeNotSupported(NsMode),
    /// Default namespaces (`db0`..`db15`) cannot be created or dropped.
    DefaultImmutable,
    InvalidName,
    /// `FSYNC` given on a non-durable mode.
    FsyncRequiresDurable,
    /// `EVICTION` given on a durable namespace (durable namespaces do not
    /// evict in M2 — ADR-0015 D5).
    EvictionNotAllowedDurable,
}

/// Valid namespace names: 1..=128 bytes of `[a-zA-Z0-9_.-]`, not colliding
/// with the reserved default names.
pub fn valid_ns_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

pub(crate) fn is_default_name(name: &[u8]) -> bool {
    let Some(rest) = name.strip_prefix(b"db") else { return false };
    !rest.is_empty()
        && rest.len() <= 2
        && rest.iter().all(u8::is_ascii_digit)
        && core::str::from_utf8(rest).is_ok_and(|n| n.parse::<u8>().is_ok_and(|n| n < 16))
}

/// Per-cell registry of named namespaces (insertion-ordered).
#[derive(Default, Debug)]
pub struct NsRegistry {
    named: Vec<NsSpec>,
}

impl NsRegistry {
    /// Registers `spec`. A durable spec with `fsync: None` is stored with
    /// the documented default (`Everysec`).
    ///
    /// # Errors
    /// - `InvalidName` / `DefaultImmutable` for bad or reserved names;
    /// - `ModeNotSupported(Topic)` until M5;
    /// - `FsyncRequiresDurable` when `fsync` is set on a non-durable mode;
    /// - `EvictionNotAllowedDurable` when `policy` is set on a durable
    ///   namespace (ADR-0015 D5);
    /// - `Exists` for a duplicate name — or a duplicate id, which is a
    ///   caller bug (ids are allocated once, never reused) and additionally
    ///   trips a debug assertion.
    pub fn create(&mut self, spec: NsSpec) -> Result<(), NsError> {
        if !valid_ns_name(&spec.name) {
            return Err(NsError::InvalidName);
        }
        if is_default_name(&spec.name) {
            return Err(NsError::DefaultImmutable);
        }
        if spec.mode == NsMode::Topic {
            return Err(NsError::ModeNotSupported(NsMode::Topic));
        }
        if spec.fsync.is_some() && spec.mode != NsMode::Durable {
            return Err(NsError::FsyncRequiresDurable);
        }
        if spec.policy.is_some() && spec.mode == NsMode::Durable {
            return Err(NsError::EvictionNotAllowedDurable);
        }
        if self.get(&spec.name).is_some() {
            return Err(NsError::Exists);
        }
        if self.get_by_id(spec.id).is_some() {
            debug_assert!(false, "namespace ids are allocated once and never reused");
            return Err(NsError::Exists);
        }
        debug_assert!(spec.id.0 >= FIRST_NAMED_NS_ID, "ids 0..16 are the implicit defaults");
        let mut spec = spec;
        if spec.mode == NsMode::Durable && spec.fsync.is_none() {
            spec.fsync = Some(FsyncClass::Everysec);
        }
        self.named.push(spec);
        Ok(())
    }

    pub fn drop_ns(&mut self, name: &[u8]) -> Result<(), NsError> {
        if is_default_name(name) {
            return Err(NsError::DefaultImmutable);
        }
        let at = self.named.iter().position(|s| s.name == name).ok_or(NsError::Unknown)?;
        self.named.remove(at);
        Ok(())
    }

    pub fn get(&self, name: &[u8]) -> Option<&NsSpec> {
        self.named.iter().find(|s| s.name == name)
    }

    /// Lookup by id — the replay/selection path (records name namespaces by
    /// id). Linear scan: registries hold few entries.
    pub fn get_by_id(&self, id: NsId) -> Option<&NsSpec> {
        self.named.iter().find(|s| s.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &NsSpec> {
        self.named.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u32, name: &[u8], mode: NsMode) -> NsSpec {
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
    fn create_list_drop_roundtrip() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"cache", NsMode::Memory)).expect("create");
        assert_eq!(reg.create(spec(17, b"cache", NsMode::Memory)), Err(NsError::Exists));
        assert_eq!(reg.iter().count(), 1);
        assert!(reg.get(b"cache").is_some());
        reg.drop_ns(b"cache").expect("drop");
        assert_eq!(reg.drop_ns(b"cache"), Err(NsError::Unknown));
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn durable_is_accepted_and_topic_is_honestly_rejected() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"ledger", NsMode::Durable)).expect("durable is real since M2-S08");
        assert_eq!(
            reg.create(spec(17, b"events", NsMode::Topic)),
            Err(NsError::ModeNotSupported(NsMode::Topic))
        );
        assert_eq!(reg.iter().count(), 1, "rejected modes must not register");
    }

    #[test]
    fn durable_without_fsync_stores_the_everysec_default() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"ledger", NsMode::Durable)).expect("create");
        assert_eq!(reg.get(b"ledger").expect("registered").fsync, Some(FsyncClass::Everysec));
        // An explicit class is kept as given.
        let explicit =
            NsSpec { fsync: Some(FsyncClass::Always), ..spec(17, b"audit", NsMode::Durable) };
        reg.create(explicit).expect("create");
        assert_eq!(reg.get(b"audit").expect("registered").fsync, Some(FsyncClass::Always));
    }

    #[test]
    fn fsync_on_non_durable_is_rejected() {
        let mut reg = NsRegistry::default();
        let bad = NsSpec { fsync: Some(FsyncClass::Always), ..spec(16, b"cache", NsMode::Memory) };
        assert_eq!(reg.create(bad), Err(NsError::FsyncRequiresDurable));
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn eviction_on_durable_is_rejected() {
        let mut reg = NsRegistry::default();
        let bad = NsSpec {
            policy: Some(EvictionPolicy::AllKeysLru),
            ..spec(16, b"ledger", NsMode::Durable)
        };
        assert_eq!(reg.create(bad), Err(NsError::EvictionNotAllowedDurable));
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn get_by_id_finds_registered_entries() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"cache", NsMode::Memory)).expect("create");
        reg.create(spec(17, b"ledger", NsMode::Durable)).expect("create");
        assert_eq!(reg.get_by_id(NsId(17)).map(|s| s.name.as_slice()), Some(&b"ledger"[..]));
        assert_eq!(reg.get_by_id(NsId(18)), None);
        assert_eq!(reg.get_by_id(NsId(0)), None, "defaults are implicit, never registered");
    }

    // Only debug builds assert here (release answers `Exists`), so the
    // should-panic expectation holds only under debug_assertions.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "never reused")]
    fn duplicate_id_is_a_caller_bug() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"cache", NsMode::Memory)).expect("create");
        // Release builds answer `Exists`; debug builds assert (ids are
        // allocated by the caller and must never repeat).
        let _ = reg.create(spec(16, b"other", NsMode::Memory));
    }

    #[test]
    fn default_names_are_reserved() {
        let mut reg = NsRegistry::default();
        assert_eq!(reg.create(spec(16, b"db0", NsMode::Memory)), Err(NsError::DefaultImmutable));
        assert_eq!(reg.create(spec(17, b"db15", NsMode::Memory)), Err(NsError::DefaultImmutable));
        // Not defaults: out-of-range index, non-numeric suffix.
        reg.create(spec(16, b"db16", NsMode::Memory)).expect("db16 is a plain name");
        reg.create(spec(17, b"dbx", NsMode::Memory)).expect("dbx is a plain name");
        assert_eq!(reg.drop_ns(b"db3"), Err(NsError::DefaultImmutable));
    }

    #[test]
    fn name_validation() {
        assert!(valid_ns_name(b"cart-sessions_v2.prod"));
        assert!(!valid_ns_name(b""));
        assert!(!valid_ns_name(b"has space"));
        assert!(!valid_ns_name(&[b'x'; 129]));
    }
}
