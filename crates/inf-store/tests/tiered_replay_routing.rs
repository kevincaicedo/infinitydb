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

// ---- M4.5-S31 rider (ADR-0084 D5): in-place updates stage no
// current-address marker — the replay-pairing proof, both halves. ----

/// Positive space: an in-place SET's replay is a bare key-verified
/// upsert (rule 2) — with no `ColdDisplace` ahead of it, the pair
/// insert-then-in-place-update replays to a single RAM slot holding the
/// final value. This is the exact WAL shape the rider now stages.
#[test]
fn in_place_set_replays_without_its_displacement_marker() {
    let mut ks = tiered_keyspace();
    let insert = RecordView::StringPostImage { ns: NS, key: b"k", value: b"aaaa" };
    assert!(matches!(ks.apply_record(&insert, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    // The live path rewrote in place and staged only the post-image;
    // replay sees exactly this record next. Its applier is the rule-2
    // key-verified upsert — addresses are never compared across lives
    // (§3.1), only content and slot cardinality.
    let update = RecordView::StringPostImage { ns: NS, key: b"k", value: b"bbbb" };
    assert!(matches!(ks.apply_record(&update, NOW, ANCHOR), Ok(ReplayOutcome::Applied)));
    let addr_after = ram_addr(&mut ks, b"k");
    let table = ks.tiered_store_mut(NS).expect("materialized");
    assert_eq!(table.record(addr_after).value, b"bbbb", "the final value wins");
    // No second slot: displacing the sole address empties the key.
    let hash = TieredTable::hash_key(b"k");
    assert!(table.apply_displace(hash, addr_after), "exactly one slot existed");
    assert!(
        matches!(table.lookup(b"k", hash, &[]), TieredLookup::Miss),
        "no stale duplicate slot survived the marker-less replay"
    );
}

/// The relocation arm of the rider argument, both halves at the applier
/// seam. A checkpoint-restored **cold** ref is unreachable by rule 2
/// (key-verified upserts ignore cold candidates), so: with the origin
/// marker the rider keeps, the pair (displace C_old, image) converges
/// to one RAM slot; without any marker — the shape the rider must NOT
/// extend to — the stale cold slot provably survives. The negative half
/// is why moved overwrites keep their current-address marker.
#[test]
fn origin_marker_repairs_the_cold_ref_and_its_absence_leaks() {
    use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr};
    // A recovered-life table: pre-life (manifested) addresses below the
    // origin are cold refs, exactly what `apply_ref` restores. The
    // live set needs the manifested file covering them (ADR-0058 D4).
    let origin = LogicalAddr::from_raw(1 << 16).expect("fits");
    let mut table = TieredTable::new(
        AddressSpaceConfig { reserve_bytes: 1 << 16, page_bytes: 1 << 12, life_origin: origin },
        DemotionConfig::for_budget(1 << 16, 1 << 12),
        64,
    )
    .expect("reservation");
    table.seed_recovered_files(
        &[inf_log::TierFileMeta {
            id: 0,
            base: LogicalAddr::ZERO,
            data_len: origin.to_raw(),
            reason: inf_log::SealReason::Recovered,
            path: std::path::PathBuf::from("shard-0/cold/tier-000000.itier"),
        }],
        1,
    );

    // Repaired half: ref → origin marker → image ⇒ one RAM slot.
    let hash_a = TieredTable::hash_key(b"relocated");
    let cold_a = LogicalAddr::from_raw(0x100).expect("fits");
    table.apply_ref(hash_a, cold_a);
    assert!(table.apply_displace(hash_a, cold_a), "the origin marker kills the cold ref");
    table.apply_image(b"relocated", b"v2", hash_a).expect("fits");
    assert!(
        matches!(table.lookup(b"relocated", hash_a, &[]), TieredLookup::Ram(_)),
        "one RAM slot, no cold residue"
    );

    // Leak half (negative space): image over a cold ref with NO marker
    // leaves the stale cold slot alive next to the new RAM slot.
    let hash_b = TieredTable::hash_key(b"leaky");
    let cold_b = LogicalAddr::from_raw(0x900).expect("fits");
    table.apply_ref(hash_b, cold_b);
    table.apply_image(b"leaky", b"v2", hash_b).expect("fits");
    assert!(
        table.apply_displace(hash_b, cold_b),
        "the stale cold slot survived — the marker is load-bearing for \
         cold-displacing overwrites, so the rider keeps it there"
    );
}
