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

use inf_log::fs::TierIoMode;
use inf_log::{FsyncClass, NsId};

use crate::demote::{DemotionConfig, MUTABLE_PERMILLE_DEFAULT};
use crate::evict::EvictionPolicy;
use crate::extents::{BLOB_THRESHOLD_DEFAULT, BlobConfig};
use crate::tiered::compact::CompactionConfig;

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

/// Per-namespace tiering configuration (M4-S19, ADR-0062 D1/D2): the
/// `INF.NS` key vocabulary as typed state. Present ⇔ the namespace is
/// durable-**tiered** — `MEM-BUDGET` is the discriminator, never a
/// fourth mode (§9: "configurations, not codepaths"). Every field is a
/// key from the ADR-0062 D2 table; [`validate`](Self::validate) is the
/// one range gauntlet both the command parser and the catalog decoder
/// stand on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TierSpec {
    /// `MEM-BUDGET`: the committed ring window this namespace may hold
    /// (ADR-0053 D1 — resident bytes, not live bytes). Required; > 0.
    pub mem_budget_bytes: u64,
    /// `DISK-BUDGET`: tier files + extents bound (ADR-0062 D5).
    /// `0` = unbounded (the honest default until S21 lands enforcement).
    pub disk_budget_bytes: u64,
    /// `MUTABLE-FRACTION` in permille (ADR-0053 D2; default 250).
    pub mutable_permille: u32,
    /// `MAINTAIN-SLICE`: the seal/flush/release quantum (ADR-0053 D3).
    /// Floors at 64 KiB — the S13 finding measured a slice near the
    /// frame size paying for most frames twice.
    pub maintain_slice_bytes: u64,
    /// `COLD-READ-QD` (ADR-0055 D2; CreateOnly — construction parameter
    /// of the cell's cold-read machinery).
    pub cold_read_qd: u16,
    /// `COMPACTION-DEAD-RATIO` percent (ADR-0059 D1). Clamped to
    /// 50..=100 at validation — the ADR-0060 D6 obligation: below 50%
    /// copy-forward moves more than it reclaims and the steady-state
    /// model breaches the 3× gate under pure churn (8.318× measured at
    /// a 10% trigger, the S16 canary).
    pub compaction_dead_ratio_pct: u8,
    /// `COMPACTION-SLICE` (ADR-0059 D6).
    pub compaction_slice_bytes: u64,
    /// `BLOB-THRESHOLD` (ADR-0061 D1). Ceilinged at 16 MiB — values
    /// above the u24 inline bound cannot be inline records, so a higher
    /// threshold would promise a routing the format cannot deliver.
    pub blob_threshold_bytes: u32,
    /// `TIER-IO-MODE` (ADR-0054; CreateOnly — per-file at open).
    pub tier_io_mode: TierIoMode,
    /// `TAIL-STALL-TIMEOUT` in milliseconds (ADR-0053 D4).
    pub tail_stall_timeout_ms: u32,
}

/// The smallest ring window a tiered namespace may reserve (ADR-0102
/// D1, the ADR-0052 D1 floor as implemented): `MEM-BUDGET +
/// MAINTAIN-SLICE` must cover four commit pages, so the ring's seal-hole
/// arithmetic and the admission window both have room to work.
pub const RING_WINDOW_MIN_BYTES: u64 = 4 * inf_alloc::REGION_PAGE_BYTES as u64;

impl TierSpec {
    /// The defaults for a given memory budget — every other key at its
    /// owning ADR's default; the blob threshold derives from the ring
    /// (ADR-0102 D2), so a budget-only spec never admits an inline
    /// record its ring cannot hold.
    #[must_use]
    pub fn for_budget(mem_budget_bytes: u64) -> TierSpec {
        TierSpec {
            mem_budget_bytes,
            disk_budget_bytes: 0,
            mutable_permille: MUTABLE_PERMILLE_DEFAULT,
            maintain_slice_bytes: 1 << 20,
            cold_read_qd: 64,
            compaction_dead_ratio_pct: 50,
            compaction_slice_bytes: 1 << 20,
            blob_threshold_bytes: BLOB_THRESHOLD_DEFAULT,
            tier_io_mode: TierIoMode::Direct,
            tail_stall_timeout_ms: 1_000,
        }
        .with_default_blob_threshold()
    }

    /// The largest `BLOB-THRESHOLD` a ring of `ring_bytes` can honour
    /// (ADR-0102 D2): every inline record — header, a 255-byte key, and
    /// a value below the threshold — must fit half the ring (ADR-0052
    /// D1's `R ≥ 2 × RECORD_INLINE_MAX`). Saturates to the u24 inline
    /// ceiling; zero for a ring below the floor (no threshold is legal
    /// there — D1 refuses first).
    #[must_use]
    pub fn blob_threshold_max(ring_bytes: u64) -> u32 {
        let overhead = (crate::record::HEADER_LEN + crate::record::MAX_KEY_LEN) as u64;
        let half = ring_bytes / 2;
        let cap = half.saturating_sub(overhead).saturating_add(1);
        u32::try_from(cap.min(u64::from(BLOB_THRESHOLD_DEFAULT))).expect("bounded above")
    }

    /// The ring-derived default threshold (ADR-0102 D2): a quarter of
    /// the ring, ceilinged at the u24 inline bound — at every legal ring
    /// (D1) this satisfies [`blob_threshold_max`](Self::blob_threshold_max)
    /// with room for the record overhead. `None` when the spec has no
    /// representable ring (the gauntlet refuses those).
    #[must_use]
    pub fn default_blob_threshold(&self) -> Option<u32> {
        let ring = self.demotion_config().ring_reserve_bytes()? as u64;
        let quarter = ring / 4;
        let derived = quarter.min(u64::from(BLOB_THRESHOLD_DEFAULT));
        Some(u32::try_from(derived).expect("bounded above"))
    }

    /// `self` with the blob threshold replaced by the ring-derived
    /// default — what a spec gets when `BLOB-THRESHOLD` was not given
    /// (the parser and [`for_budget`](Self::for_budget)). A spec with no
    /// representable ring is returned unchanged (the gauntlet owns it).
    #[must_use]
    pub fn with_default_blob_threshold(mut self) -> TierSpec {
        if let Some(threshold) = self.default_blob_threshold() {
            self.blob_threshold_bytes = threshold;
        }
        self
    }

    /// The ring this spec reserves when materialized (`next_pow2(budget
    /// + slice)`), or `None` when unrepresentable.
    #[must_use]
    pub fn ring_bytes(&self) -> Option<u64> {
        self.demotion_config().ring_reserve_bytes().map(|r| r as u64)
    }

    /// The ADR-0062 D2 range gauntlet. One place, shared by the command
    /// parser, the catalog decoder, and `INF.NS SET` — a spec that
    /// passes here is constructible everywhere downstream.
    ///
    /// # Errors
    /// A static reason naming the violated range (the command layer
    /// wraps it into the reply string).
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.mem_budget_bytes == 0 {
            return Err("MEM-BUDGET must be > 0");
        }
        let Some(ring) = self.ring_bytes() else {
            return Err("MEM-BUDGET + MAINTAIN-SLICE has no representable ring reservation");
        };
        // ADR-0102 D1 (H0): the four-page floor — before this rule the
        // constructor's release assert answered a client `MEM-BUDGET`.
        if self.mem_budget_bytes.saturating_add(self.maintain_slice_bytes) < RING_WINDOW_MIN_BYTES {
            return Err("MEM-BUDGET + MAINTAIN-SLICE must reserve at least 4mb (four commit \
                        pages — ADR-0102 D1)");
        }
        if self.disk_budget_bytes != 0 && self.disk_budget_bytes < (1 << 20) {
            return Err("DISK-BUDGET is 0 (unbounded) or >= 1mb");
        }
        if self.mutable_permille == 0 || self.mutable_permille > 999 {
            return Err("MUTABLE-FRACTION is 1..=999 permille");
        }
        if !(64 << 10..=64 << 20).contains(&self.maintain_slice_bytes) {
            return Err("MAINTAIN-SLICE is 64kb..=64mb");
        }
        if self.cold_read_qd == 0 || self.cold_read_qd > 4096 {
            return Err("COLD-READ-QD is 1..=4096");
        }
        if !(50..=100).contains(&self.compaction_dead_ratio_pct) {
            return Err("COMPACTION-DEAD-RATIO is 50..=100 (below 50 the write-amplification \
                        gate is at risk by construction — ADR-0062 D2)");
        }
        if !(64 << 10..=64 << 20).contains(&self.compaction_slice_bytes) {
            return Err("COMPACTION-SLICE is 64kb..=64mb");
        }
        if !(4 << 10..=1 << 24).contains(&self.blob_threshold_bytes) {
            return Err("BLOB-THRESHOLD is 4kb..=16mb");
        }
        // ADR-0102 D2 (F-L06-01): the largest inline record must fit
        // half the ring this spec reserves — the ADR-0052 D1 invariant
        // as a typed rule. `ring` is the smallest ring the spec can
        // ever materialize (a reboot re-derives it), so a `SET` that
        // lowers MEM-BUDGET under an explicit threshold is refused here
        // rather than accepted for one life and refused at the next boot.
        if self.blob_threshold_bytes > TierSpec::blob_threshold_max(ring) {
            return Err("BLOB-THRESHOLD exceeds the ring's inline record bound (half of \
                        next_pow2(MEM-BUDGET + MAINTAIN-SLICE), less the record overhead — \
                        ADR-0102 D2): raise MEM-BUDGET or lower BLOB-THRESHOLD");
        }
        if self.tail_stall_timeout_ms == 0 || self.tail_stall_timeout_ms > 60_000 {
            return Err("TAIL-STALL-TIMEOUT is 1..=60000 milliseconds");
        }
        Ok(())
    }

    /// The demotion configuration this spec derives (ADR-0053 shape:
    /// budget, fraction, slice).
    #[must_use]
    pub fn demotion_config(&self) -> DemotionConfig {
        DemotionConfig {
            mem_budget_bytes: self.mem_budget_bytes,
            mutable_permille: self.mutable_permille,
            slice_bytes: self.maintain_slice_bytes,
        }
    }

    /// The compaction configuration this spec derives (ADR-0059 D1/D6).
    #[must_use]
    pub fn compaction_config(&self) -> CompactionConfig {
        CompactionConfig {
            dead_ratio_pct: self.compaction_dead_ratio_pct,
            slice_bytes: self.compaction_slice_bytes,
        }
    }

    /// The blob routing configuration this spec derives (ADR-0061 D1).
    /// The per-value hard cap stays the ADR default — it bounds one
    /// value, not the namespace, and has no `INF.NS` key.
    #[must_use]
    pub fn blob_config(&self) -> BlobConfig {
        BlobConfig { threshold_bytes: self.blob_threshold_bytes, ..BlobConfig::default() }
    }
}

/// One named-namespace registry entry (the §3.2 freeze: id/name, mode,
/// fsync class, eviction policy, memory budget; M4-S19 adds the tier
/// block).
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
    /// durable namespaces — they do not evict (ADR-0015 D5, scoped to
    /// durable by ADR-0068). Enforced on memory namespaces since M4-S27.
    pub policy: Option<EvictionPolicy>,
    /// Node-wide budget in bytes; `None` inherits the server `maxmemory`
    /// (the store joins the global eviction hand). `Some` on a memory
    /// namespace is enforced per-namespace since M4-S27 (ADR-0068 D2);
    /// refused on tiered namespaces (`MEM-BUDGET` owns their memory —
    /// ADR-0062); registry-carried but unenforced on durable namespaces
    /// (reserved — a durable budget story would need refusal semantics).
    pub maxmemory: Option<u64>,
    /// Tiering configuration (M4-S19, ADR-0062 D1): `Some` ⇔ the
    /// namespace is durable-tiered. Requires `mode == Durable`.
    pub tier: Option<TierSpec>,
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
    /// `MAXMEMORY` given on a tiered namespace (M4-S27, ADR-0068 D1):
    /// `MEM-BUDGET` is a tiered namespace's one budget authority — a
    /// second budget would silently fight demotion with key death.
    MaxmemoryNotAllowedTiered,
    /// `MAXMEMORY`/`EVICTION` hot-reload attempted on a durable namespace
    /// (M4-S27, ADR-0068 D3): durable namespaces never evict, so the keys
    /// have nothing to reload.
    PressureKeysNotHotDurable,
    /// A tiering key given on a non-durable mode (M4-S19, ADR-0062 D1:
    /// tiered is a configuration of `MODE durable`).
    TierRequiresDurable,
    /// A tiering key outside its ADR-0062 D2 range; the reason names the
    /// range.
    InvalidTierConfig(&'static str),
    /// Tiered materialization refused by the aggregate reserved-VA
    /// admission bound (ADR-0062 D4) — checked before any mmap.
    TierVaLimitExceeded {
        requested_bytes: u64,
        admitted_bytes: u64,
        limit_bytes: u64,
    },
    /// `INF.NS SET` with tiering keys on a namespace created without
    /// `MEM-BUDGET` — adding tiering post-hoc is a create-time decision
    /// (drop + recreate), not a reload.
    NotTiered,
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
    /// Every rule [`create`](Self::create) enforces, applied to `spec`
    /// without registering it (ADR-0103 D3: the DDL program validates
    /// before the catalog persist, applies after).
    ///
    /// # Errors
    /// Exactly `create`'s.
    pub fn check(&self, spec: &NsSpec) -> Result<(), NsError> {
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
        if let Some(tier) = &spec.tier {
            if spec.mode != NsMode::Durable {
                return Err(NsError::TierRequiresDurable);
            }
            if spec.maxmemory.is_some() {
                // One budget authority per namespace (ADR-0068 D1):
                // MEM-BUDGET bounds a tiered namespace's memory by
                // demotion; a MAXMEMORY beside it would kill keys that
                // demotion would have preserved.
                return Err(NsError::MaxmemoryNotAllowedTiered);
            }
            tier.validate().map_err(NsError::InvalidTierConfig)?;
        }
        if self.get(&spec.name).is_some() {
            return Err(NsError::Exists);
        }
        if self.get_by_id(spec.id).is_some() {
            debug_assert!(false, "namespace ids are allocated once and never reused");
            return Err(NsError::Exists);
        }
        debug_assert!(spec.id.0 >= FIRST_NAMED_NS_ID, "ids 0..16 are the implicit defaults");
        Ok(())
    }

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
        self.check(&spec)?;
        let mut spec = spec;
        if spec.mode == NsMode::Durable && spec.fsync.is_none() {
            spec.fsync = Some(FsyncClass::Everysec);
        }
        self.named.push(spec);
        Ok(())
    }

    pub fn drop_ns(&mut self, name: &[u8]) -> Result<NsSpec, NsError> {
        if is_default_name(name) {
            return Err(NsError::DefaultImmutable);
        }
        let at = self.named.iter().position(|s| s.name == name).ok_or(NsError::Unknown)?;
        Ok(self.named.remove(at))
    }

    /// Replaces a memory namespace's pressure knobs (M4-S27 hot-reload,
    /// ADR-0068 D3; the caller —
    /// [`Keyspace::ns_set_memory`](crate::Keyspace::ns_set_memory) —
    /// pushes the result into the materialized store). `None` values
    /// return a knob to inheriting the node config.
    ///
    /// # Errors
    /// `Unknown` for an unregistered name; typed refusals for tiered
    /// (`MEM-BUDGET` owns the budget — D1) and durable (never evicts)
    /// namespaces.
    pub fn set_memory_pressure(
        &mut self,
        name: &[u8],
        policy: Option<EvictionPolicy>,
        maxmemory: Option<u64>,
    ) -> Result<(), NsError> {
        let spec = self.named.iter_mut().find(|s| s.name == name).ok_or(NsError::Unknown)?;
        if spec.tier.is_some() {
            return Err(NsError::MaxmemoryNotAllowedTiered);
        }
        if spec.mode != NsMode::Memory {
            return Err(NsError::PressureKeysNotHotDurable);
        }
        spec.policy = policy;
        spec.maxmemory = maxmemory;
        Ok(())
    }

    /// Replaces a tiered entry's tier block (M4-S19 hot-reload; the
    /// caller — [`Keyspace::ns_set_tier`](crate::Keyspace::ns_set_tier)
    /// — validated the block and applied it to the table first).
    ///
    /// # Errors
    /// `Unknown` for an unregistered name; `NotTiered` when the entry
    /// was created without `MEM-BUDGET` (tiering is create-time — D1).
    pub fn set_tier(&mut self, name: &[u8], tier: TierSpec) -> Result<(), NsError> {
        let spec = self.named.iter_mut().find(|s| s.name == name).ok_or(NsError::Unknown)?;
        if spec.tier.is_none() {
            return Err(NsError::NotTiered);
        }
        spec.tier = Some(tier);
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
            tier: None,
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

    /// ADR-0068 D1: a tiered spec carrying `MAXMEMORY` refuses typed —
    /// `MEM-BUDGET` is a tiered namespace's one budget authority.
    #[test]
    fn maxmemory_on_tiered_is_rejected() {
        let mut reg = NsRegistry::default();
        let bad = NsSpec {
            maxmemory: Some(1 << 20),
            tier: Some(TierSpec::for_budget(64 << 20)),
            ..spec(16, b"hot", NsMode::Durable)
        };
        assert_eq!(reg.create(bad), Err(NsError::MaxmemoryNotAllowedTiered));
        assert_eq!(reg.iter().count(), 0);
    }

    /// ADR-0068 D3: the memory-pressure hot-reload applies to memory
    /// namespaces only — durable and tiered entries answer typed
    /// refusals, and a successful reload replaces both knobs.
    #[test]
    fn memory_pressure_reload_scopes_by_mode() {
        let mut reg = NsRegistry::default();
        reg.create(spec(16, b"cache", NsMode::Memory)).expect("create");
        reg.create(spec(17, b"ledger", NsMode::Durable)).expect("create");
        reg.create(NsSpec {
            tier: Some(TierSpec::for_budget(64 << 20)),
            ..spec(18, b"hot", NsMode::Durable)
        })
        .expect("create");
        reg.set_memory_pressure(b"cache", Some(EvictionPolicy::AllKeysLru), Some(1 << 20))
            .expect("memory reloads");
        let spec = reg.get(b"cache").expect("registered");
        assert_eq!(spec.policy, Some(EvictionPolicy::AllKeysLru));
        assert_eq!(spec.maxmemory, Some(1 << 20));
        assert_eq!(
            reg.set_memory_pressure(b"ledger", None, Some(1 << 20)),
            Err(NsError::PressureKeysNotHotDurable)
        );
        assert_eq!(
            reg.set_memory_pressure(b"hot", None, Some(1 << 20)),
            Err(NsError::MaxmemoryNotAllowedTiered)
        );
        assert_eq!(reg.set_memory_pressure(b"missing", None, None), Err(NsError::Unknown));
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

    /// M4-S19 (ADR-0062 D1): tiering is a configuration of `MODE
    /// durable` — a tier block on any other mode refuses typed, and a
    /// valid tiered spec registers with its config intact.
    #[test]
    fn tier_requires_durable_and_registers_on_durable() {
        let mut reg = NsRegistry::default();
        let tier = TierSpec::for_budget(64 << 20);
        let bad = NsSpec { tier: Some(tier), ..spec(16, b"cache", NsMode::Memory) };
        assert_eq!(reg.create(bad), Err(NsError::TierRequiresDurable));
        let good = NsSpec { tier: Some(tier), ..spec(16, b"ledger", NsMode::Durable) };
        reg.create(good).expect("tiered durable registers");
        assert_eq!(reg.get(b"ledger").expect("registered").tier, Some(tier));
    }

    /// The ADR-0062 D2 ranges are typed refusals at registration — one
    /// probe per clamp, including the D6 dead-ratio guardrail (the S16
    /// canary's 10% must be unrepresentable through configuration).
    #[test]
    fn tier_ranges_are_enforced() {
        let base = TierSpec::for_budget(64 << 20);
        assert!(base.validate().is_ok());
        for (bad, what) in [
            (TierSpec { mem_budget_bytes: 0, ..base }, "zero budget"),
            (TierSpec { disk_budget_bytes: 1 << 10, ..base }, "sub-1mb disk budget"),
            (TierSpec { mutable_permille: 0, ..base }, "zero fraction"),
            (TierSpec { mutable_permille: 1_000, ..base }, "fraction above 999"),
            (TierSpec { maintain_slice_bytes: 4 << 10, ..base }, "frame-scale slice (S13)"),
            (TierSpec { cold_read_qd: 0, ..base }, "zero qd"),
            (TierSpec { compaction_dead_ratio_pct: 10, ..base }, "the S16 canary trigger"),
            (TierSpec { compaction_slice_bytes: 1 << 30, ..base }, "oversized slice"),
            (TierSpec { blob_threshold_bytes: 1 << 25, ..base }, "threshold above u24"),
            (TierSpec { tail_stall_timeout_ms: 0, ..base }, "zero timeout"),
            // ADR-0102 D1 (H0): windows under four commit pages.
            (TierSpec { mem_budget_bytes: 1 << 20, ..base }, "1mb budget (the H0 node kill)"),
            (TierSpec { mem_budget_bytes: 2 << 20, ..base }, "2mb budget (window 3 MiB)"),
            (
                TierSpec { mem_budget_bytes: 3 << 20, maintain_slice_bytes: 512 << 10, ..base },
                "3mb budget with a 512kb slice (window 3.5 MiB)",
            ),
            // ADR-0102 D2 (F-L06-01): a threshold the ring cannot hold.
            (
                TierSpec { mem_budget_bytes: 3 << 20, blob_threshold_bytes: 1 << 24, ..base },
                "16 MiB threshold in a 4 MiB ring",
            ),
            (
                TierSpec {
                    mem_budget_bytes: 3 << 20,
                    blob_threshold_bytes: TierSpec::blob_threshold_max(4 << 20) + 1,
                    ..base
                },
                "one byte over the ring's inline bound",
            ),
        ] {
            assert!(bad.validate().is_err(), "{what} must refuse");
        }
        // The floor itself and the bound itself are legal.
        assert!(TierSpec::for_budget(3 << 20).validate().is_ok(), "3mb: exactly four pages");
        assert!(
            TierSpec {
                mem_budget_bytes: 3 << 20,
                blob_threshold_bytes: TierSpec::blob_threshold_max(4 << 20),
                ..base
            }
            .validate()
            .is_ok(),
            "exactly the inline bound is legal"
        );
        let mut reg = NsRegistry::default();
        let bad = NsSpec {
            tier: Some(TierSpec { compaction_dead_ratio_pct: 10, ..base }),
            ..spec(16, b"ledger", NsMode::Durable)
        };
        assert!(
            matches!(reg.create(bad), Err(NsError::InvalidTierConfig(_))),
            "registration runs the same gauntlet"
        );
        assert_eq!(reg.iter().count(), 0);
    }

    /// ADR-0102 D2: the default threshold follows the ring — a quarter
    /// of it, ceilinged at the u24 bound — and always passes the
    /// gauntlet; the bound function is exact at the record overhead.
    #[test]
    fn blob_threshold_default_derives_from_the_ring() {
        for (budget, ring, want) in [
            (3u64 << 20, 4u64 << 20, 1u32 << 20),
            (4 << 20, 8 << 20, 2 << 20),
            (8 << 20, 16 << 20, 4 << 20),
            (31 << 20, 32 << 20, 8 << 20),
            (63 << 20, 64 << 20, 16 << 20),
            (64 << 20, 128 << 20, 16 << 20),
            (1 << 30, 1 << 31, 16 << 20),
        ] {
            let spec = TierSpec::for_budget(budget);
            assert_eq!(spec.ring_bytes(), Some(ring), "budget {budget}");
            assert_eq!(spec.blob_threshold_bytes, want, "budget {budget}");
            assert!(spec.validate().is_ok(), "budget {budget}");
            assert!(want <= TierSpec::blob_threshold_max(ring));
        }
        let overhead = (crate::record::HEADER_LEN + crate::record::MAX_KEY_LEN) as u64;
        assert_eq!(u64::from(TierSpec::blob_threshold_max(4 << 20)), (2 << 20) - overhead + 1);
        assert_eq!(TierSpec::blob_threshold_max(1 << 40), 1 << 24, "saturates at u24");
        // A zero-budget draft (the parser's accumulator) derives nothing
        // and keeps the ADR-0061 default until MEM-BUDGET arrives.
        assert_eq!(TierSpec::for_budget(0).blob_threshold_bytes, BLOB_THRESHOLD_DEFAULT);
    }
}
