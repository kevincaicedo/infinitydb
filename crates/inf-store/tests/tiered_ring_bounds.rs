//! Review of 2026-08-30 — the ring-invariant class (H0 / F-L06-01 /
//! F-L06-05, ADR-0102): ADR-0052 D1's `R ≥ 2 × RECORD_INLINE_MAX` and
//! the four-page ring floor are enforced typed — at the `TierSpec` range
//! gauntlet for operators and structurally inside `TieredTable` for
//! every caller — never by `AddressSpace`'s release asserts, which are
//! reachable from a client `MEM-BUDGET` / `SET` today.
//!
//! Falsifiers (each run red on `5f22be3` with the review's signature):
//! - `inline_record_above_half_ring_refuses_typed` — the pre-fix panic
//!   `allocation exceeds half the ring` in `AddressSpace::alloc`
//!   (F-L06-01: a 5 MiB value under the 16 MiB default threshold in an
//!   8 MiB ring);
//! - `blob_stall_probe_never_sizes_from_the_value` — the same assert in
//!   `AddressSpace::stall_target` (F-L06-05: the blob path handed the
//!   value, not the 24-byte reference, to the stall probe);
//! - `sub_floor_budget_refuses_at_the_spec_gauntlet` — the pre-fix panic
//!   `ring smaller than four pages` in `AddressSpace::new` (H0:
//!   `MEM-BUDGET 1mb`);
//! - `default_blob_threshold_derives_from_the_ring` — the pre-fix default
//!   (16 MiB regardless of the ring) is exactly what let F-L06-01 in.

use inf_alloc::REGION_PAGE_BYTES;
use inf_log::{FsyncClass, NsId};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, KeyHasher, Keyspace, LogicalAddr, MAX_KEY_LEN, NsError,
    NsMode, NsSpec, OpError, StoreConfig, TierSpec, TieredTable,
};

/// An 8 MiB ring: `MEM-BUDGET 4mb` + the 1 MiB default slice, the
/// smallest ring the lane's scenario named (`ring / 2` = 4 MiB).
fn eight_mib_ring() -> TieredTable {
    let demote = DemotionConfig::for_budget(4 << 20, REGION_PAGE_BYTES as u64);
    let reserve_bytes = demote.ring_reserve_bytes().expect("valid budget");
    assert_eq!(reserve_bytes, 8 << 20, "next_pow2(4 MiB + 1 MiB)");
    TieredTable::new(
        AddressSpaceConfig {
            reserve_bytes,
            page_bytes: REGION_PAGE_BYTES,
            life_origin: LogicalAddr::ZERO,
        },
        demote,
        64,
        KeyHasher::default(),
    )
    .expect("ring")
}

fn tiered_spec(id: u32, name: &[u8], tier: TierSpec) -> NsSpec {
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: Some(tier),
    }
}

/// F-L06-01: a value between `ring / 2` and the blob threshold was
/// routed inline and tripped `alloc`'s release assert. The store now
/// clamps its effective threshold to the ring's inline bound (so the
/// plane routes such a value out of line) and, independently, refuses
/// an over-bound inline record typed — two enforcement points for one
/// invariant (INFINITY_STYLE: pair assertions across a boundary).
#[test]
fn inline_record_above_half_ring_refuses_typed() {
    let mut table = eight_mib_ring();
    let inline_max = table.inline_record_max();
    assert_eq!(inline_max, 4 << 20, "half the ring");
    // The structural clamp: the effective threshold never lets the
    // largest inline record (header + 255-byte key + value) exceed
    // `ring / 2`, whatever the registered spec says.
    let threshold = table.blob_config().threshold_bytes as usize;
    assert!(
        TieredTable::RECORD_HEADER_LEN + MAX_KEY_LEN + threshold - 1 <= inline_max,
        "effective threshold {threshold} admits an inline record above {inline_max}"
    );
    // The typed refusal: pre-fix this call panicked inside `alloc`.
    let value = vec![0u8; 5 << 20];
    let hash = table.hash_key(b"big");
    assert!(
        matches!(table.insert(b"big", &value, hash), Err(OpError::TooLarge)),
        "an over-bound inline record refuses typed, never panics"
    );
    assert_eq!(table.len(), 0, "the refusal mutated nothing");
}

/// F-L06-05: the blob path's refusal handler sized its stall probe
/// from the out-of-line value (up to 1 GiB) instead of the 24-byte
/// reference record — a release assert above `ring / 2`, a wrong wait
/// target below it. The probe now has an extent arm sized from the
/// reference, and the inline probe answers an over-bound length with
/// `None` (no watermark progress can ever fit it) instead of aborting.
#[test]
fn blob_stall_probe_never_sizes_from_the_value() {
    let mut table = eight_mib_ring();
    let value = vec![0u8; 5 << 20];
    // Pre-fix: `assert!(len <= ring / 2)` in `stall_target`.
    assert_eq!(table.write_stall_target(b"big", &value), None, "over-bound: nothing can help");
    // The extent arm probes the reference record's length — it never
    // sees the value, so it is total over every legal blob size.
    assert_eq!(table.extent_stall_target(b"big"), None, "the space has room: fits now");
    // Drive the window to refusal (no MAINTAIN drains it here), then the
    // extent arm names a real wait key — the F-L06-05 "wrong target"
    // half: the reference record's target, not a 5 MiB value's.
    let chunk = vec![7u8; 3 << 10];
    let mut id = 0u64;
    let refused = loop {
        let key = format!("fill:{id:06}").into_bytes();
        let hash = table.hash_key(&key);
        match table.insert(&key, &chunk, hash) {
            Ok(_) => id += 1,
            Err(OpError::OutOfMemory) => break id,
            Err(e) => panic!("unexpected refusal {e:?}"),
        }
        assert!(id < 100_000, "the window never bound");
    };
    assert!(refused > 0, "the fill placed records before refusing");
    // The extent probe is the inline probe of a 24-byte value: same
    // record length, same answer — whichever it is at this tail.
    let reference = [0u8; inf_store::EXTENT_REF_LEN];
    assert_eq!(table.extent_stall_target(b"big"), table.write_stall_target(b"big", &reference));
    // A 3 KiB refusal leaves page room for a 50-byte record; pack the
    // tail with tiny records until even the reference record is refused,
    // then the probe names real watermark progress and counts the stall.
    let tiny = [1u8; 8];
    let mut id = 0u64;
    loop {
        let key = format!("tiny:{id:07}").into_bytes();
        let hash = table.hash_key(&key);
        match table.insert(&key, &tiny, hash) {
            Ok(_) => id += 1,
            Err(OpError::OutOfMemory) => break,
            Err(e) => panic!("unexpected refusal {e:?}"),
        }
        assert!(id < 2_000_000, "the window never bound for tiny records");
    }
    let stalls_before = table.space().counters().tail_alloc_stalls;
    let target = table.extent_stall_target(b"big").expect("a flushed-watermark target");
    assert!(target.to_raw() > 0, "the target names watermark progress");
    assert_eq!(table.space().counters().tail_alloc_stalls, stalls_before + 1, "the stall counted");
}

/// H0: `INF.NS CREATE … MEM-BUDGET 1mb` reached `AddressSpace::new`'s
/// release assert (`ring smaller than four pages`) before any typed
/// check — the guarded range was simply wrong (`2mb` refused typed,
/// `1mb` killed the node). The floor is now a `TierSpec::validate`
/// rule, so the registry refuses it typed and the constructor's assert
/// is a genuine internal invariant again.
#[test]
fn sub_floor_budget_refuses_at_the_spec_gauntlet() {
    for (budget, what) in
        [(64u64 << 10, "64kb"), (256 << 10, "256kb"), (1 << 20, "1mb"), (2 << 20, "2mb")]
    {
        let spec = TierSpec::for_budget(budget);
        let err = spec.validate().expect_err(what);
        assert!(err.contains("MEM-BUDGET + MAINTAIN-SLICE"), "{what}: {err}");
        // Pre-fix: `ns_create` panicked here for 64kb..=1mb.
        let mut ks = Keyspace::new(StoreConfig::default());
        assert!(
            matches!(
                ks.ns_create(tiered_spec(16, b"tiny", spec)),
                Err(NsError::InvalidTierConfig(_))
            ),
            "{what}: the registry refuses typed"
        );
        assert_eq!(ks.ns_iter().count(), 0, "{what}: nothing registered");
    }
    // 3mb is the smallest legal budget at the default slice (window =
    // 3 MiB + 1 MiB = four commit pages).
    let spec = TierSpec::for_budget(3 << 20);
    assert!(spec.validate().is_ok());
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(tiered_spec(16, b"small", spec)).expect("the floor budget materializes");
}

/// ADR-0102 D2: the default `BLOB-THRESHOLD` is a function of the ring
/// — a quarter of it, ceilinged at the u24 inline bound — so a
/// namespace created with `MEM-BUDGET` alone can never admit an inline
/// record the ring cannot hold; an explicit threshold above the ring's
/// inline bound is refused at the gauntlet.
#[test]
fn default_blob_threshold_derives_from_the_ring() {
    let small = TierSpec::for_budget(4 << 20); // ring 8 MiB
    assert_eq!(small.blob_threshold_bytes, 2 << 20, "ring / 4");
    assert!(small.validate().is_ok());
    let big = TierSpec::for_budget(64 << 20); // ring 128 MiB
    assert_eq!(big.blob_threshold_bytes, 1 << 24, "ceilinged at the u24 inline bound");
    let explicit = TierSpec { blob_threshold_bytes: 1 << 24, ..TierSpec::for_budget(4 << 20) };
    let err = explicit.validate().expect_err("16 MiB threshold in an 8 MiB ring");
    assert!(err.contains("BLOB-THRESHOLD"), "{err}");
    // The bound itself: the largest inline record fits half the ring.
    let cap = TierSpec::blob_threshold_max(8 << 20);
    assert_eq!(TieredTable::RECORD_HEADER_LEN + MAX_KEY_LEN + cap as usize - 1, 4 << 20);
    assert!(TierSpec { blob_threshold_bytes: cap, ..small }.validate().is_ok());
    assert!(TierSpec { blob_threshold_bytes: cap + 1, ..small }.validate().is_err());
}

/// ADR-0102 D3: a catalog written before the rule (a small budget with
/// the old 16 MiB default) boots — the threshold normalizes to the
/// ring's inline bound at seed time, counted, never a refused boot and
/// never a silent panic on the first large value.
#[test]
fn legacy_catalog_threshold_normalizes_at_seed() {
    let mut source = Keyspace::new(StoreConfig::default());
    source.ns_create(tiered_spec(16, b"legacy", TierSpec::for_budget(4 << 20))).expect("create");
    let mut catalog = source.export_catalog(17, 0, 1);
    // Forge the pre-rule shape: the registry never admits it, so write
    // the entry directly.
    catalog.entries[0].tier.as_mut().expect("tiered").blob_threshold_bytes = 1 << 24;
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.seed_catalog(&catalog).expect("a legacy catalog boots");
    let seeded = ks.ns_get(b"legacy").expect("seeded").tier.expect("tiered");
    assert_eq!(seeded.blob_threshold_bytes, TierSpec::blob_threshold_max(8 << 20));
    assert_eq!(ks.seed_normalized_thresholds(), 1, "counted, never silent");
    let table = ks.tiered_store(NsId(16)).expect("materialized");
    assert_eq!(table.blob_config().threshold_bytes, seeded.blob_threshold_bytes);
}
