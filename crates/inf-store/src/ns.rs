//! Namespace registry **v1** (M1-S08, master plan §4.2): the identity seam
//! where M2 durability classes and M5 topics attach. v1 is `memory`-mode
//! only — the mode enum already carries `durable`/`topic` so the ABI does
//! not break when they arrive, and creating one returns the documented
//! not-yet-supported error (honesty over silence, L8).
//!
//! The 16 default namespaces (`db0`..`db15`, Redis `SELECT 0..15`) are
//! implicit in [`Keyspace`](crate::Keyspace) and share the server-level
//! eviction config (Redis instance-wide `maxmemory` semantics). Named
//! entries created here carry their own policy/budget; they become
//! *addressable* keyspaces when M2 adds namespace selection — recorded
//! limitation: in M1 they are registry + config state, not key storage.
//! Registries replicate per cell via the `INF.NS` scatter program (L1: no
//! shared registry, every cell owns its copy).

use crate::evict::EvictionPolicy;

/// Durability class of a namespace (§4.2). Only `Memory` is valid until M2.
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
/// eviction policy, memory budget).
#[derive(Clone, Debug)]
pub struct NsSpec {
    pub name: Vec<u8>,
    pub mode: NsMode,
    /// `None` inherits the server `maxmemory-policy`.
    pub policy: Option<EvictionPolicy>,
    /// Node-wide budget in bytes; `None` inherits the server `maxmemory`.
    pub maxmemory: Option<u64>,
}

/// Typed registry failures (the command layer maps these to reply strings).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NsError {
    Exists,
    Unknown,
    /// `durable`/`topic` before M2/M5 (documented not-yet-supported).
    ModeNotSupported(NsMode),
    /// Default namespaces (`db0`..`db15`) cannot be created or dropped.
    DefaultImmutable,
    InvalidName,
}

/// Valid namespace names: 1..=128 bytes of `[a-zA-Z0-9_.-]`, not colliding
/// with the reserved default names.
pub fn valid_ns_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 128
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
#[derive(Default, Debug)]
pub struct NsRegistry {
    named: Vec<NsSpec>,
}

impl NsRegistry {
    pub fn create(&mut self, spec: NsSpec) -> Result<(), NsError> {
        if !valid_ns_name(&spec.name) {
            return Err(NsError::InvalidName);
        }
        if is_default_name(&spec.name) {
            return Err(NsError::DefaultImmutable);
        }
        if spec.mode != NsMode::Memory {
            return Err(NsError::ModeNotSupported(spec.mode));
        }
        if self.get(&spec.name).is_some() {
            return Err(NsError::Exists);
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

    pub fn iter(&self) -> impl Iterator<Item = &NsSpec> {
        self.named.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &[u8], mode: NsMode) -> NsSpec {
        NsSpec { name: name.to_vec(), mode, policy: None, maxmemory: None }
    }

    #[test]
    fn create_list_drop_roundtrip() {
        let mut reg = NsRegistry::default();
        reg.create(spec(b"cache", NsMode::Memory)).expect("create");
        assert_eq!(reg.create(spec(b"cache", NsMode::Memory)), Err(NsError::Exists));
        assert_eq!(reg.iter().count(), 1);
        assert!(reg.get(b"cache").is_some());
        reg.drop_ns(b"cache").expect("drop");
        assert_eq!(reg.drop_ns(b"cache"), Err(NsError::Unknown));
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn durable_and_topic_are_honestly_rejected() {
        let mut reg = NsRegistry::default();
        assert_eq!(
            reg.create(spec(b"ledger", NsMode::Durable)),
            Err(NsError::ModeNotSupported(NsMode::Durable))
        );
        assert_eq!(
            reg.create(spec(b"events", NsMode::Topic)),
            Err(NsError::ModeNotSupported(NsMode::Topic))
        );
        assert_eq!(reg.iter().count(), 0, "rejected modes must not register");
    }

    #[test]
    fn default_names_are_reserved() {
        let mut reg = NsRegistry::default();
        assert_eq!(reg.create(spec(b"db0", NsMode::Memory)), Err(NsError::DefaultImmutable));
        assert_eq!(reg.create(spec(b"db15", NsMode::Memory)), Err(NsError::DefaultImmutable));
        // Not defaults: out-of-range index, non-numeric suffix.
        reg.create(spec(b"db16", NsMode::Memory)).expect("db16 is a plain name");
        reg.create(spec(b"dbx", NsMode::Memory)).expect("dbx is a plain name");
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
