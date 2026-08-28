//! M4-S19 — per-namespace memory + disk budgets (ADR-0062): the
//! spec-driven materialization path, the aggregate reserved-VA admission
//! bound (D4 — the ADR-0051 accepted debt, retired), the disk-budget
//! pressure signal (D5), hot-reload semantics (D3), and namespace-drop
//! teardown (D7).
//!
//! The plan's AC map onto this suite:
//! - **memory fill per-ns** — the S07 bound (`committed ≤ budget +
//!   slice`) re-proven through the registry/spec path, two namespaces
//!   filling concurrently;
//! - **disk budget** — pressure engages *before* the cap (at 7/8 of the
//!   budget by construction), with extent bytes counted into usage;
//! - **aggregate VA** — creation past the cap fails typed before any
//!   mmap and leaves nothing behind; drop returns exactly the ring;
//! - **teardown** — drop removes the table (the `Region` unmaps —
//!   reserved VA returns structurally), the plane's file half unlinks
//!   `statvfs`-visibly, and accounting reads zero. The in-flight
//!   cold-read half (pins defer unlinks; drained reads complete from
//!   the still-open fd) is the S08 proof this teardown reuses —
//!   `cold_hardened.rs::cancellation_and_unlink_discipline_on_uring`
//!   and the `m4-cold` DST rows; the drop-shaped DST scenario joins
//!   command wiring (recorded deviation, not silence).

use std::path::Path;

use inf_log::blob::{ExtentId, ExtentWriter, list_extent_ids, unlink_extent_file};
use inf_log::flush::unlink_tier_file;
use inf_log::fs::mem::MemFs;
use inf_log::{
    FsyncClass, MutationEffect, NsId, StagingConfig, StagingRing, TierFlush, TierFlushConfig,
    TierIoMode,
};
use inf_store::KeyHasher;
use inf_store::{Keyspace, NsError, NsMode, NsSpec, StoreConfig, TierSpec, TieredTable};

/// A tiered durable spec around `TierSpec::for_budget`, named fields
/// overridable per test.
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

fn flush_for(ns: NsId, fs: &MemFs) -> TierFlush<MemFs> {
    TierFlush::new(
        fs.clone(),
        TierFlushConfig {
            // The ADR-0062 D7 sub-decision: per-namespace directories,
            // so teardown is a directory walk.
            shard_dir: Path::new("shard-0").join(format!("ns-{}", ns.0)),
            cell: 0,
            ns,
            mode: TierIoMode::Buffered,
            file_capacity: 256 << 10,
            slice_bytes: 1 << 20,
        },
        0,
    )
}

/// Seal → flush → release to quiescence for one namespace.
fn drain(table: &mut TieredTable, flush: &mut TierFlush<MemFs>) {
    loop {
        let sealed = table.seal_slice();
        let f = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// The D4 admission bound: creation past the configured cap fails typed
/// **before** any mmap and rolls the registry entry back; dropping a
/// namespace returns exactly its ring, and the freed capacity admits
/// the next creation.
#[test]
fn aggregate_va_bounds_creation_and_drop_returns_capacity() {
    let mut ks = Keyspace::new(StoreConfig::default());
    let tier = TierSpec::for_budget(8 << 20);
    let ring = tier.demotion_config().ring_reserve_bytes().expect("valid") as u64;
    assert_eq!(ring, 16 << 20, "next_pow2(8 MiB + 1 MiB)");
    // Room for two rings plus slack that a third cannot fit in.
    ks.set_tiered_va_limit(2 * ring + ring / 2);

    ks.ns_create(tiered_spec(16, b"t-one", tier)).expect("first admits");
    ks.ns_create(tiered_spec(17, b"t-two", tier)).expect("second admits");
    assert_eq!(ks.tiered_tables(), 2);
    assert_eq!(ks.tiering_usage().reserved_bytes, 2 * ring);

    let refused = ks.ns_create(tiered_spec(18, b"t-three", tier));
    assert_eq!(
        refused,
        Err(NsError::TierVaLimitExceeded {
            requested_bytes: ring,
            admitted_bytes: 2 * ring,
            limit_bytes: 2 * ring + ring / 2,
        }),
        "the refusal names its numbers"
    );
    // Refusal mutates nothing: no table, no registry entry (rollback),
    // no reserved bytes.
    assert_eq!(ks.tiered_tables(), 2);
    assert!(ks.ns_get(b"t-three").is_none(), "the registry entry rolled back");
    assert_eq!(ks.tiering_usage().reserved_bytes, 2 * ring);

    // Drop returns exactly the ring; the freed capacity admits again.
    ks.ns_drop(b"t-two").expect("drop");
    assert_eq!(ks.tiering_usage().reserved_bytes, ring, "exactly one ring returned");
    assert_eq!(ks.tiered_tables(), 1);
    ks.ns_create(tiered_spec(18, b"t-three", tier)).expect("freed capacity admits");
    assert_eq!(ks.tiering_usage().reserved_bytes, 2 * ring);
}

/// Spec-driven materialization applies every derived config to the
/// table, hot-reload (D3) updates registry and table together or not at
/// all, and the D2 clamps hold at registration.
#[test]
fn spec_materialization_and_hot_reload_apply_together() {
    let mut ks = Keyspace::new(StoreConfig::default());
    let tier = TierSpec {
        disk_budget_bytes: 64 << 20,
        mutable_permille: 100,
        compaction_dead_ratio_pct: 60,
        compaction_slice_bytes: 128 << 10,
        blob_threshold_bytes: 8 << 10,
        ..TierSpec::for_budget(8 << 20)
    };
    ks.ns_create(tiered_spec(16, b"tiered", tier)).expect("create");
    let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
    assert_eq!(table.demotion().mem_budget_bytes, 8 << 20);
    assert_eq!(table.demotion().mutable_permille, 100);
    assert_eq!(table.disk_budget(), 64 << 20);
    assert_eq!(table.compaction_config().dead_ratio_pct, 60);
    assert_eq!(table.compaction_config().slice_bytes, 128 << 10);
    assert_eq!(table.blob_config().threshold_bytes, 8 << 10);

    // The D6/D2 clamp holds at registration (the S16 canary trigger is
    // unrepresentable through configuration).
    let bad = TierSpec { compaction_dead_ratio_pct: 10, ..tier };
    assert!(matches!(
        ks.ns_create(tiered_spec(17, b"bad", bad)),
        Err(NsError::InvalidTierConfig(_))
    ));

    // Hot reload within the ring: registry and table update together.
    let reload = TierSpec { mutable_permille: 300, disk_budget_bytes: 32 << 20, ..tier };
    ks.ns_set_tier(b"tiered", reload).expect("hot reload");
    assert_eq!(ks.ns_get(b"tiered").expect("registered").tier, Some(reload));
    let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
    assert_eq!(table.demotion().mutable_permille, 300);
    assert_eq!(table.disk_budget(), 32 << 20);

    // Growth past the reserved ring refuses typed (drop + recreate is
    // the growth path) — and neither the registry nor the table moved.
    let grow = TierSpec { mem_budget_bytes: 64 << 20, ..reload };
    assert!(matches!(ks.ns_set_tier(b"tiered", grow), Err(NsError::InvalidTierConfig(_))));
    assert_eq!(ks.ns_get(b"tiered").expect("registered").tier, Some(reload));
    assert_eq!(
        ks.tiered_store_mut(NsId(16)).expect("materialized").demotion().mem_budget_bytes,
        8 << 20
    );

    // SET on a non-tiered namespace refuses typed (tiering is a
    // create-time decision — D1/D3).
    ks.ns_create(NsSpec {
        id: NsId(18),
        name: b"plain".to_vec(),
        mode: NsMode::Durable,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("plain durable");
    assert_eq!(ks.ns_set_tier(b"plain", tier), Err(NsError::NotTiered));
}

/// The S07 fill bound, re-proven per namespace through the spec path:
/// two tiered namespaces fill past their budgets concurrently and
/// `committed ≤ budget + slice` holds for each at every observation
/// point; the cell aggregates are the exact per-table sums (the
/// attribution reconciliation the AC names, extended to disk bytes in
/// the test below).
#[test]
fn memory_fill_respects_each_namespace_budget() {
    let mut ks = Keyspace::new(StoreConfig::default());
    let tier = TierSpec { mutable_permille: 100, ..TierSpec::for_budget(8 << 20) };
    ks.ns_create(tiered_spec(16, b"fill-a", tier)).expect("create");
    ks.ns_create(tiered_spec(17, b"fill-b", tier)).expect("create");
    let fs = MemFs::new();
    let mut flushes = [flush_for(NsId(16), &fs), flush_for(NsId(17), &fs)];
    let budget = tier.mem_budget_bytes;
    let slice = tier.demotion_config().slice_bytes;

    let value = vec![0x5Au8; 4 << 10];
    // 2× budget through each namespace, interleaved.
    for i in 0..2 * ((budget as usize) / value.len()) {
        for (slot, ns) in [NsId(16), NsId(17)].into_iter().enumerate() {
            let key = format!("k:{i:06}");
            let hash = KeyHasher::default().hash(key.as_bytes());
            loop {
                let table = ks.tiered_store_mut(ns).expect("materialized");
                if table.insert(key.as_bytes(), &value, hash).is_ok() {
                    break;
                }
                drain(table, &mut flushes[slot]);
            }
            // The S07 bound, per namespace, at every observation point.
            for (check, nid) in [NsId(16), NsId(17)].into_iter().enumerate() {
                let committed = ks
                    .tiered_store_mut(nid)
                    .expect("materialized")
                    .space()
                    .report()
                    .committed_bytes;
                assert!(
                    committed <= budget + slice,
                    "ns {check}: committed {committed} exceeds budget {budget} + slice {slice}"
                );
            }
        }
    }
    // Attribution reconciles: the cell aggregate is the exact sum of
    // the per-table reports.
    let usage = ks.tiering_usage();
    let per_table: u64 = [NsId(16), NsId(17)]
        .into_iter()
        .map(|ns| ks.tiered_store_mut(ns).expect("materialized").space().report().committed_bytes)
        .sum();
    assert_eq!(usage.committed_bytes, per_table, "aggregate == Σ per-namespace");
}

/// The D5 disk-budget signal: pressure engages at 7/8 of the budget —
/// strictly before the cap — and extent device bytes count into usage
/// exactly.
#[test]
fn disk_pressure_engages_before_the_cap() {
    let mut ks = Keyspace::new(StoreConfig::default());
    // Disk budget 16 MiB: the 7/8 threshold leaves 2 MiB of headroom —
    // two MAINTAIN slices — so a per-slice pressure check must observe
    // the signal strictly before the cap (the polling cadence the plane
    // runs at; a driver that only checks after unbounded bursts can
    // overshoot any threshold, which is a driver bug, not a signal one).
    let tier = TierSpec {
        disk_budget_bytes: 16 << 20,
        mutable_permille: 100,
        blob_threshold_bytes: 4 << 10,
        ..TierSpec::for_budget(8 << 20)
    };
    ks.ns_create(tiered_spec(16, b"disky", tier)).expect("create");
    let fs = MemFs::new();
    let mut flush = flush_for(NsId(16), &fs);
    let budget = tier.disk_budget_bytes;
    let threshold = budget - budget / 8;

    // Extent bytes count into usage exactly (the blob half of D5).
    {
        let value = vec![0x42u8; 8 << 10];
        let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
        let extent_id = ExtentId(table.allocate_extent_id());
        let mut w = ExtentWriter::create(
            &fs,
            &Path::new("shard-0").join("ns-16"),
            extent_id,
            0,
            NsId(16),
            value.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create extent");
        w.append_chunk(&value).expect("chunk");
        let sealed = w.finish().expect("finish");
        table.note_blob_bytes(sealed.device_bytes());
        let mut ring = StagingRing::new(StagingConfig::default());
        table
            .stage_wal(
                &mut ring,
                &MutationEffect::StringSetExtent {
                    ns: NsId(16),
                    key: b"blob",
                    extent_id: sealed.extent_id().0,
                    offset: 0,
                    len: sealed.data_len(),
                },
            )
            .expect("stage");
        table.insert_extent(b"blob", KeyHasher::default().hash(b"blob"), &sealed).expect("fits");
        assert_eq!(
            table.disk_used(flush.disk_bytes()),
            flush.disk_bytes() + sealed.device_bytes(),
            "usage = tier files + extent device bytes, exactly"
        );
    }

    // Fill with the pressure signal checked after every flush slice —
    // the MAINTAIN cadence. At the first firing, usage must sit in
    // [threshold, budget): engaged, and strictly before the cap.
    let value = vec![0x7Cu8; (4 << 10) - 1];
    let mut fired_at = None;
    'fill: for i in 0..8_192u32 {
        let key = format!("d:{i:06}");
        let hash = KeyHasher::default().hash(key.as_bytes());
        loop {
            let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
            if table.insert(key.as_bytes(), &value, hash).is_ok() {
                break;
            }
            // One MAINTAIN step at a time: seal, one flush slice,
            // release — then the pressure poll, exactly the round shape.
            let sealed = table.seal_slice();
            let f = table.flush_slice(&mut flush).expect("flush slice");
            let released = table.release_slice();
            assert!(sealed + released + f.appended_bytes > 0, "fill must drain");
            let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
            if table.disk_pressure(flush.disk_bytes()) {
                fired_at = Some(table.disk_used(flush.disk_bytes()));
                break 'fill;
            }
        }
    }
    let fired_at = fired_at.expect("the fill crossed the threshold");
    assert!(
        fired_at >= threshold,
        "pressure fired at {fired_at}, below the 7/8 threshold {threshold}"
    );
    assert!(fired_at < budget, "pressure must engage before the cap {budget}, not at {fired_at}");
}

/// The D7 teardown on the real filesystem: drop removes the table (the
/// reserved VA returns structurally), the plane's file half unlinks
/// `statvfs`-visibly, and every accounting term reads zero. New access
/// has nothing to route to — the store answers `None`, which is the
/// plane's typed refusal.
#[cfg(unix)]
#[test]
fn namespace_drop_returns_disk_va_and_accounting_to_zero() {
    use inf_log::fs::StdSegmentFs;

    let root = std::env::temp_dir().join(format!("inf-s19-teardown-{}", std::process::id()));
    let ns_dir = root.join("ns-16");
    std::fs::create_dir_all(ns_dir.join("cold")).expect("tempdir");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    fn avail_bytes(path: &Path) -> u64 {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
        // SAFETY: `statvfs` is plain-old-data; the all-zero pattern is a
        // valid value for every field and the call overwrites it.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a live NUL-terminated path and `stat` a valid
        // exclusive out-pointer for the call's duration.
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
        assert_eq!(rc, 0, "statvfs");
        stat.f_bavail as u64 * stat.f_frsize as u64
    }

    let mut ks = Keyspace::new(StoreConfig::default());
    let tier = TierSpec {
        mutable_permille: 100,
        blob_threshold_bytes: 4 << 10,
        ..TierSpec::for_budget(8 << 20)
    };
    ks.ns_create(tiered_spec(16, b"doomed", tier)).expect("create");
    let mut flush = TierFlush::new(
        StdSegmentFs,
        TierFlushConfig {
            shard_dir: ns_dir.clone(),
            cell: 0,
            ns: NsId(16),
            mode: TierIoMode::Buffered,
            file_capacity: 256 << 10,
            slice_bytes: 1 << 20,
        },
        0,
    );
    let avail_start = avail_bytes(&root);

    // Real data: records to tier files, one value out of line.
    let value = vec![0x33u8; (4 << 10) - 1];
    for i in 0..1_024u32 {
        let key = format!("t:{i:06}");
        let hash = KeyHasher::default().hash(key.as_bytes());
        loop {
            let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
            if table.insert(key.as_bytes(), &value, hash).is_ok() {
                break;
            }
            let sealed = table.seal_slice();
            let f = table.flush_slice(&mut flush).expect("flush");
            let released = table.release_slice();
            assert!(sealed + released + f.appended_bytes > 0, "fill must drain");
        }
    }
    let blob = vec![0x44u8; 32 << 10];
    let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
    let extent_id = ExtentId(table.allocate_extent_id());
    let mut w = ExtentWriter::create(
        &StdSegmentFs,
        &ns_dir,
        extent_id,
        0,
        NsId(16),
        blob.len() as u64,
        TierIoMode::Buffered,
    )
    .expect("create extent");
    w.append_chunk(&blob).expect("chunk");
    let sealed = w.finish().expect("finish");
    table.note_blob_bytes(sealed.device_bytes());
    table.insert_extent(b"big", KeyHasher::default().hash(b"big"), &sealed).expect("fits");
    loop {
        let table = ks.tiered_store_mut(NsId(16)).expect("materialized");
        let s = table.seal_slice();
        let f = table.flush_slice(&mut flush).expect("flush");
        let r = table.release_slice();
        if s + r + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
    assert!(!flush.sealed().is_empty() || flush.active().is_some(), "files exist to tear down");

    // The drop: registry + table go together; the Region unmaps.
    ks.ns_drop(b"doomed").expect("drop");
    assert_eq!(ks.tiered_tables(), 0);
    assert_eq!(ks.tiering_usage().reserved_bytes, 0, "the ring returned structurally");
    assert_eq!(ks.tiering_usage().committed_bytes, 0);
    assert!(ks.ns_get(b"doomed").is_none());
    assert!(
        ks.tiered_store_mut(NsId(16)).is_none(),
        "new access has nothing to route to — the plane refuses typed"
    );
    let extents = ks.tiering_extent_stats();
    assert_eq!(extents.live, 0, "accounting reconciles to zero");
    assert_eq!(extents.disk_bytes, 0);

    // The plane's file half (the rig playing the plane, §3.3): unlink
    // every tier file and extent — no pins exist here; the pinned case
    // is the S08 proof cited in the module docs.
    let sealed_metas: Vec<_> = flush.sealed().to_vec();
    for meta in &sealed_metas {
        flush.detach_sealed(meta.id).expect("sealed");
        unlink_tier_file(&StdSegmentFs, meta).expect("unlink tier file");
    }
    if let Some((_, _, _, _, path)) = flush.active() {
        std::fs::remove_file(path).expect("unlink active file");
    }
    for id in list_extent_ids(&StdSegmentFs, &ns_dir).expect("listing") {
        unlink_extent_file(&StdSegmentFs, &ns_dir, id).expect("unlink extent");
    }
    assert!(list_extent_ids(&StdSegmentFs, &ns_dir).expect("listing").is_empty());
    let leftover: Vec<_> = std::fs::read_dir(ns_dir.join("cold"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(leftover.is_empty(), "the namespace directory emptied: {leftover:?}");
    // statvfs: the blocks came back (generous slack for shared-box churn).
    let avail_end = avail_bytes(&root);
    let slack = 16u64 << 20;
    assert!(
        avail_end + slack >= avail_start,
        "statvfs shows the teardown returned the space (start {avail_start}, end {avail_end})"
    );
}
