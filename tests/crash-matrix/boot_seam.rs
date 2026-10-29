//! Included only by carrying test targets; no product dependency edge.
//! A booting cell as the recovery driver holds it: the recovered tiered
//! table inside a keyspace and the namespace's boot machine lent through
//! the replay seam, so every replayed record enters through
//! `Keyspace::apply_record` — the shipped dispatcher, never a copy of it.

use inf_foundation::time::Nanos;
use inf_log::fs::mem::MemFs;
use inf_log::{FsyncClass, NsId};
use inf_store::{
    Keyspace, NsMode, NsSpec, ReplaySpill, StoreConfig, TierReplay, TierSpec, TieredTable,
    WallAnchor,
};

/// The replay clock and wall anchor (tiered records carry no expiry).
pub const NOW: Nanos = Nanos(1_000_000);
pub const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

/// The seam the recovery driver lends: one namespace's boot machine.
pub struct Lent {
    pub ns: NsId,
    pub machine: TierReplay<MemFs>,
}

impl ReplaySpill for Lent {
    type Fs = MemFs;

    fn replay_mut(&mut self, ns: NsId) -> Option<&mut TierReplay<MemFs>> {
        (ns == self.ns).then_some(&mut self.machine)
    }
}

/// A keyspace whose tiered namespace `ns` holds the recovered `table`:
/// the catalog materializes the namespace, the recovered table takes its
/// place (the driver also applies the catalog's knobs; the carriers keep
/// the table's own).
pub fn keyspace_with(ns: NsId, table: TieredTable) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: ns,
        name: format!("tiered-{}", ns.0).into_bytes(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: Some(TierSpec::for_budget(4 << 20)),
    })
    .expect("create the tiered namespace");
    *ks.tiered_store_mut(ns).expect("materialized") = table;
    ks
}
