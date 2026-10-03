//! FCR-STTIER-N1 (ADR-0174 D4), the red: a tiered namespace that no
//! MANIFEST section names — a crash before its first checkpoint
//! publishes, after one tier flush — must recover through the empty
//! section: its dead-life tier files removed before any flush, so the
//! first flush after `Ready` creates `tier-000000.itier` again. Red by
//! reading at engine `b5cae02` and by this test on `8674af4`: boot
//! removes tier files only for `manifest.tiers`, the next life's
//! pipeline starts at file id 0, and tier creation is `create_new`, so
//! the first flush fails on the existing file.

use std::path::Path;

use inf_log::fs::SegmentFs;
use inf_log::{MutationEffect, NsId, TierFlush, TierFlushConfig, TierIoMode};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, FsyncClass, Keyspace, LogicalAddr, NsMode, NsSpec,
    TierSpec, TieredLookup, TieredTable,
};

mod support;
use support::*;

const TIER_NS: NsId = NsId(17);
const MEM_BUDGET: u64 = 3 << 20;

fn tiered_keyspace() -> Keyspace {
    let mut ks = fresh_keyspace();
    ks.ns_create(NsSpec {
        id: TIER_NS,
        name: b"hot".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
        tier: Some(TierSpec::for_budget(MEM_BUDGET)),
    })
    .expect("tiered ns");
    ks
}

/// The namespace's pipeline as the plane builds it for a fresh life:
/// file ids from 0 under `shard-0/ns-17/`.
fn fresh_pipeline(fs: &inf_log::fs::mem::MemFs) -> TierFlush<inf_log::fs::mem::MemFs> {
    TierFlush::new(
        fs.clone(),
        TierFlushConfig {
            shard_dir: Path::new("data").join(format!("shard-{CELL}")).join("ns-17"),
            cell: u32::from(CELL),
            ns: TIER_NS,
            mode: TierIoMode::Buffered,
            file_capacity: inf_log::TIER_FILE_CAPACITY_DEFAULT,
            slice_bytes: 1 << 20,
        },
        0,
    )
}

#[test]
#[ignore = "FCR-STTIER-01 stage 3 (ADR-0174 D4): red until every tiered namespace recovers \
            through a manifest section"]
fn a_namespace_without_a_manifest_section_removes_its_dead_life_files_before_the_first_flush() {
    let fs = inf_log::fs::mem::MemFs::new();
    let mut log = LogBuilder::new(&fs, &cfg());
    let value = vec![0x3Cu8; 512];
    let keys: Vec<Vec<u8>> = (0..64).map(|i| format!("k:{i:04}").into_bytes()).collect();
    let effects: Vec<MutationEffect<'_>> = keys
        .iter()
        .map(|key| MutationEffect::StringSet { ns: TIER_NS, key, value: &value })
        .collect();
    log.frame(&effects);
    drop(log);
    // The crashed life's one tier flush: the same records sealed and
    // flushed through the namespace's fresh pipeline — `tier-000000.itier`
    // exists, and no MANIFEST names it.
    {
        let demote = DemotionConfig::for_budget(MEM_BUDGET, 1 << 20);
        let mut table = TieredTable::new(
            AddressSpaceConfig {
                reserve_bytes: demote.ring_reserve_bytes().expect("valid"),
                page_bytes: 1 << 20,
                life_origin: LogicalAddr::ZERO,
            },
            demote,
            128,
            inf_store::KeyHasher::default(),
        )
        .expect("ring");
        let mut flush = fresh_pipeline(&fs);
        for key in &keys {
            table.insert(key, &value, table.hash_key(key)).expect("fits");
        }
        let tail = table.space().tail();
        table.space_mut().advance_ro_boundary(tail);
        table.flush_slice(&mut flush).expect("the crashed life's one flush");
        assert!(flush.active().is_some(), "the file exists on disk");
    }
    let cold = Path::new("data").join(format!("shard-{CELL}")).join("ns-17").join("cold");
    assert!(
        fs.list_dir(&cold).expect("cold dir").iter().any(|name| name == "tier-000000.itier"),
        "the dead-life file is on disk before the boot"
    );

    // The boot: no MANIFEST, so the namespace recovers through the empty
    // section (ADR-0174 D4) — its dead-life files removed before any
    // flush — and replays its tail.
    let mut ks = tiered_keyspace();
    recover(&fs, &mut ks).expect("the boot recovers the sectionless namespace");
    for key in &keys {
        let hash = ks.hasher().hash(key);
        let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
        assert!(matches!(table.lookup(key, hash, &[]), TieredLookup::Ram(_)));
    }
    // The first flush after `Ready`: the fresh pipeline creates file 0.
    let mut flush = fresh_pipeline(&fs);
    let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
    let tail = table.space().tail();
    table.space_mut().advance_ro_boundary(tail);
    let outcome = table.flush_slice(&mut flush).expect("the first flush after Ready succeeds");
    assert!(outcome.appended_bytes > 0, "the flush appended the replayed records");
    assert!(
        fs.list_dir(&cold).expect("cold dir").iter().any(|name| name == "tier-000000.itier"),
        "the boot removed the dead-life file (ADR-0174 D4) and the flush created this life's"
    );
}
