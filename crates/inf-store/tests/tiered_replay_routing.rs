//! M4-S26 — tiered replay routing at the `Keyspace` seam (ADR-0057 D4,
//! ADR-0059 D9): records naming a tiered namespace dispatch to the
//! table's appliers, never to the named CellStore shell; `ColdDisplace`
//! markers park in the bounded register and drain — exact by
//! `(hash, old_addr)` — at their paired mutation; adjacency and bound
//! violations are typed decode errors, not silent skips. Content is
//! compared, never addresses (§3.1).

use inf_foundation::time::Nanos;
use inf_log::{FsyncClass, NsId, RecordView};
use inf_store::{
    Keyspace, NsMode, NsSpec, ReplayError, ReplayOutcome, StoreConfig, TierSpec, TieredLookup,
    TieredTable, WallAnchor,
};

const NS: NsId = NsId(41);
const NOW: Nanos = Nanos(1_000_000);
const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

fn tiered_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"tiered".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: Some(TierSpec::for_budget(8 << 20)),
    })
    .expect("create tiered namespace");
    ks
}

fn ram_addr(ks: &mut Keyspace, key: &[u8]) -> inf_store::LogicalAddr {
    let table = ks.tiered_store_mut(NS).expect("materialized");
    match table.lookup(key, TieredTable::hash_key(key), &[]) {
        TieredLookup::Ram(addr) => addr,
        other => panic!("expected a RAM entry for {key:?}, got {other:?}"),
    }
}

/// A tiered `StringPostImage` lands in the tiered table; the namespace's
/// CellStore shell stays empty (the resurrect hazard the routing kills).
#[test]
fn tiered_records_route_to_the_table_not_the_shell() {
    let mut ks = tiered_keyspace();
    let rec = RecordView::StringPostImage { ns: NS, key: b"k", value: b"v1" };
    assert!(matches!(ks.apply_record(&rec, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    let addr = ram_addr(&mut ks, b"k");
    let table = ks.tiered_store_mut(NS).expect("materialized");
    assert_eq!(table.record(addr).value, b"v1");
    let shell = ks.ns_store_mut(NS).expect("shell exists");
    assert_eq!(shell.get(b"k", NOW), None, "the shell must never see tiered records");
}

/// `ColdDisplace` + mutation pairing (D4 rules 1 + 2): the register
/// drains at the mutation, the old slot dies by exact address, and the
/// pairing leaves exactly the new content live.
#[test]
fn displace_drains_at_its_paired_mutation() {
    let mut ks = tiered_keyspace();
    let set = RecordView::StringPostImage { ns: NS, key: b"k", value: b"v1" };
    ks.apply_record(&set, NOW, ANCHOR).expect("seed");
    let old = ram_addr(&mut ks, b"k").to_raw();

    let marker = RecordView::ColdDisplace { ns: NS, old_addr: old };
    assert!(matches!(ks.apply_record(&marker, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    assert_eq!(ks.displace_register_len(), 1, "the marker parks until its mutation");

    let set2 = RecordView::StringPostImage { ns: NS, key: b"k", value: b"v2" };
    assert!(matches!(ks.apply_record(&set2, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    assert_eq!(ks.displace_register_len(), 0, "the mutation drained the register");
    let addr = ram_addr(&mut ks, b"k");
    let table = ks.tiered_store_mut(NS).expect("materialized");
    assert_eq!(table.record(addr).value, b"v2");
    assert_ne!(addr.to_raw(), old, "the displaced slot was removed, not updated in place");
}

/// The delete half of the pairing: displace + `Delete` leaves no entry.
#[test]
fn displace_delete_pairing_removes_the_key() {
    let mut ks = tiered_keyspace();
    let set = RecordView::StringPostImage { ns: NS, key: b"k", value: b"v1" };
    ks.apply_record(&set, NOW, ANCHOR).expect("seed");
    let old = ram_addr(&mut ks, b"k").to_raw();

    let marker = RecordView::ColdDisplace { ns: NS, old_addr: old };
    ks.apply_record(&marker, NOW, ANCHOR).expect("marker");
    let del = RecordView::Delete { ns: NS, key: b"k" };
    assert!(matches!(ks.apply_record(&del, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    assert_eq!(ks.displace_register_len(), 0);
    let table = ks.tiered_store_mut(NS).expect("materialized");
    let hash = TieredTable::hash_key(b"k");
    assert!(matches!(table.lookup(b"k", hash, &[]), TieredLookup::Miss));
}

/// A marker followed by anything but its paired mutation (here: a record
/// of a different namespace) is a typed decode error — ADR-0057 D4's
/// same-frame adjacency, enforced.
#[test]
fn orphan_marker_is_a_decode_error() {
    let mut ks = tiered_keyspace();
    let marker = RecordView::ColdDisplace { ns: NS, old_addr: 4096 };
    ks.apply_record(&marker, NOW, ANCHOR).expect("marker parks");
    let foreign = RecordView::StringPostImage { ns: NsId(0), key: b"k", value: b"v" };
    assert!(matches!(ks.apply_record(&foreign, NOW, ANCHOR), Err(ReplayError::Displacement(_))));
}

/// More markers than one mutation can stage (ADR-0059 D9 bounds the
/// origin list at 3 + the current address) is corrupt input.
#[test]
fn displace_register_overflow_is_a_decode_error() {
    let mut ks = tiered_keyspace();
    for i in 0..4u64 {
        let marker = RecordView::ColdDisplace { ns: NS, old_addr: 4096 + i * 64 };
        ks.apply_record(&marker, NOW, ANCHOR).expect("within the D9 bound");
    }
    let fifth = RecordView::ColdDisplace { ns: NS, old_addr: 9999 };
    assert!(matches!(ks.apply_record(&fifth, NOW, ANCHOR), Err(ReplayError::Displacement(_))));
}

/// A marker naming a namespace with no tiered table (dropped, or a
/// foreign log) skips without arming the register — its paired mutation
/// skips the same way, so the pairing cannot desync.
#[test]
fn marker_for_unknown_namespace_skips() {
    let mut ks = tiered_keyspace();
    let marker = RecordView::ColdDisplace { ns: NsId(99), old_addr: 4096 };
    assert!(matches!(ks.apply_record(&marker, NOW, ANCHOR), Ok(ReplayOutcome::SkippedUnknownNs)));
    assert_eq!(ks.displace_register_len(), 0);
}

/// Expiry and document records naming a tiered namespace skip typed —
/// neither is command-reachable on tiered namespaces in M4, and letting
/// them fall through would build CellStore-shell state.
#[test]
fn expiry_on_tiered_namespace_skips_reserved() {
    let mut ks = tiered_keyspace();
    let rec = RecordView::ExpireAt { ns: NS, at_unix_ms: 1_750_000_000_000, key: b"k" };
    assert!(matches!(ks.apply_record(&rec, NOW, ANCHOR), Ok(ReplayOutcome::SkippedReserved)));
}

/// A tail `StringExtentRef` routes to the extent applier: the reference
/// map counts it and the record kind is the out-of-line one.
#[test]
fn extent_ref_routes_to_the_extent_applier() {
    let mut ks = tiered_keyspace();
    let rec = RecordView::StringExtentRef { ns: NS, key: b"big", extent_id: 7, offset: 0, len: 64 };
    assert!(matches!(ks.apply_record(&rec, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    let table = ks.tiered_store_mut(NS).expect("materialized");
    assert_eq!(table.extent_refcount(7), 1, "the reference map counts the replayed ref");
}
