//! FCR-STTIER-01 (ADR-0174 D1), the server tier's red on the injected
//! `MemFs` seam: a cell log whose tiered tail re-appends more than the
//! namespace's RAM window (`MEM-BUDGET + MAINTAIN-SLICE`) boots through
//! `open_cell_log` — replay demotes through the recovered pipeline
//! instead of failing the boot. Red at engine `b5cae02`: the boot refuses
//! with `replay apply failed … OutOfMemory` (`recover.rs:1787`).

use inf_log::MutationEffect;
use inf_log::NsId;
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, TierSpec, TieredLookup};

mod support;
use support::*;

const TIER_NS: NsId = NsId(17);
/// The smallest legal window: `MEM-BUDGET 3mb` + `MAINTAIN-SLICE 1mb`.
const MEM_BUDGET: u64 = 3 << 20;
const WINDOW: u64 = MEM_BUDGET + (1 << 20);
const VALUE_LEN: usize = 4 << 10;
const RECORDS_PER_FRAME: usize = 12;

/// `fresh_keyspace` plus a materialized tiered namespace at the smallest
/// window (no MANIFEST — the first-crash shape: the catalog names it, no
/// checkpoint does).
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

#[test]
#[ignore = "FCR-STTIER-01 stage 3 (ADR-0174 D1): red at b5cae02 until boot replay demotes"]
fn a_tiered_tail_above_the_window_boots_through_open_cell_log() {
    let fs = inf_log::fs::mem::MemFs::new();
    let mut log = LogBuilder::new(&fs, &cfg());
    // Distinct keys until the tail alone exceeds the window: every one
    // re-appends at replay, so the recovered table must demote.
    let value = vec![0xA7u8; VALUE_LEN];
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut tail_bytes = 0u64;
    while tail_bytes <= WINDOW + (1 << 20) {
        let batch: Vec<Vec<u8>> = (0..RECORDS_PER_FRAME)
            .map(|i| format!("tail:{:06}", keys.len() + i).into_bytes())
            .collect();
        let effects: Vec<MutationEffect<'_>> = batch
            .iter()
            .map(|key| MutationEffect::StringSet { ns: TIER_NS, key, value: &value })
            .collect();
        log.frame(&effects);
        for key in &batch {
            tail_bytes += (key.len() + value.len() + 8) as u64;
        }
        keys.extend(batch);
    }
    assert!(tail_bytes > WINDOW, "the regime: the tail alone exceeds the window");
    drop(log);

    let mut ks = tiered_keyspace();
    let (_rotor, _stats) = match recover(&fs, &mut ks) {
        Ok(recovered) => recovered,
        Err(err) => panic!("the boot refused a tail of {tail_bytes} bytes: {err}"),
    };
    for key in &keys {
        let hash = ks.hasher().hash(key);
        let table = ks.tiered_store_mut(TIER_NS).expect("tiered");
        assert!(
            !matches!(table.lookup(key, hash, &[]), TieredLookup::Miss),
            "acknowledged key {:?} absent after the boot",
            String::from_utf8_lossy(key)
        );
    }
}
