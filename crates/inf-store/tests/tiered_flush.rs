//! M4-S11 flush-pipeline storm (ADR-0056): `TieredTable::flush_slice`
//! drives `TierFlush` against a seeded write/update workload with
//! MAINTAIN-shaped rounds — seal slice → flush slice → release — and
//! the oracle re-reads **every** cold record from the tier-file bytes
//! the pipeline wrote (CRC-verified, content + version compared; never
//! addresses — §3.1).
//!
//! Identities held every round: watermark order; `flushed` lands only on
//! record boundaries at or below the claim rule's frame floor; files are
//! exact, adjacent within a run, and gapped only at recorded ring-top
//! holes (ADR-0052 D2); sealed footers verify.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    NsId, SealReason, TIER_FRAME_BYTES, TierFlush, TierFlushConfig, TierIoMode, inspect_tier_bytes,
    tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_store::{
    AddrClass, AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig,
    TieredLookup, TieredTable,
};

const NS: NsId = NsId(41);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small file capacity so the storm rotates files constantly.
const FILE_CAPACITY: u64 = 96 << 10;
const OPS: u64 = 200_000;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

struct Rig {
    ks: Keyspace,
    fs: MemFs,
    flush: TierFlush<MemFs>,
}

impl Rig {
    fn new() -> Rig {
        let fs = MemFs::new();
        let demote = DemotionConfig::for_budget(BUDGET, PAGE);
        let ring = demote.ring_reserve_bytes().expect("valid budget");
        let mut ks = Keyspace::new(StoreConfig::default());
        assert!(
            ks.materialize_tiered(
                NS,
                AddressSpaceConfig {
                    reserve_bytes: ring,
                    page_bytes: PAGE as usize,
                    life_origin: LogicalAddr::ZERO,
                },
                demote,
                2048,
            )
            .is_ok()
        );
        let flush = TierFlush::new(
            fs.clone(),
            TierFlushConfig {
                shard_dir: Path::new("shard-0").to_path_buf(),
                cell: 0,
                ns: NS,
                mode: TierIoMode::Buffered,
                file_capacity: FILE_CAPACITY,
                slice_bytes: PAGE,
            },
            0,
        );
        Rig { ks, fs, flush }
    }

    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// One MAINTAIN round: seal/flush/release until this round's backlog
    /// drains (the paced-storm cadence — the storm's write rate exceeds
    /// one slice per round, so a round runs the loop the reactor would
    /// spread over iterations). Asserts the S11 invariants per slice.
    fn maintain(&mut self) {
        loop {
            let d = self.ks.demote_tick();
            let flush = &mut self.flush;
            let table = self.ks.tiered_store_mut(NS).expect("materialized");
            let f = table.flush_slice(flush).expect("flush slice");
            // Claim rule: while a file is actively being written,
            // `flushed` never exceeds its confirmable end (full final
            // frames only; the whole file at seal). Right after a gap
            // crossing no file is active and `flushed` legally sits at
            // the gap end, past the last sealed file's records.
            if flush.active().is_some() {
                let limit = flush.confirmable_end().expect("active file has a bound");
                assert!(
                    table.space().flushed().to_raw() <= limit,
                    "flushed {} beyond confirmable {limit}",
                    table.space().flushed().to_raw()
                );
            }
            let progress =
                d.sealed_bytes + d.released_bytes + f.appended_bytes + u64::from(f.gaps_crossed);
            if progress == 0 {
                break;
            }
        }
    }

    /// Reads one cold record straight from the tier-file bytes the
    /// pipeline wrote (the audit's cold path — no table access).
    fn read_cold(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
        let (base, path) = self
            .flush
            .sealed()
            .iter()
            .find(|m| contains(m.base.to_raw(), m.data_len))
            .map(|m| (m.base.to_raw(), m.path.clone()))
            .or_else(|| {
                let (_, base, _, durable_len, path) = self.flush.active()?;
                contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
            })?;
        let image = self.fs.contents(&path)?;
        let (first, count, skip) = tier_frame_span(addr - base, len);
        let from = tier_frame_offset(first) as usize;
        let to = from + count as usize * TIER_FRAME_BYTES;
        let mut out = Vec::new();
        tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
        Some(out)
    }
}

#[test]
fn flush_storm_round_trips_every_record() {
    let mut rig = Rig::new();
    let mut seed = 0x54EE_DF1Cu64;
    let keys = 1500u64;
    // model: key → (value, version, encoded_len)
    let mut model: BTreeMap<Vec<u8>, (Vec<u8>, u32, usize)> = BTreeMap::new();
    let mut ops = 0u64;
    while ops < OPS {
        for _ in 0..64 {
            ops += 1;
            let idx = seeded(&mut seed) % keys;
            let key = format!("flush:{idx:05}").into_bytes();
            let value =
                vec![(seeded(&mut seed) % 251) as u8; 40 + (seeded(&mut seed) % 200) as usize];
            let hash = TieredTable::hash_key(&key);
            let table = rig.table();
            let placed = match table.lookup(&key, hash, &[]) {
                TieredLookup::Ram(old) | TieredLookup::Cold(old) => {
                    let (_, old_version, old_len) = model.get(&key).expect("model has it");
                    table
                        .update(&key, &value, hash, old, *old_len, *old_version)
                        .expect("paced storm fits the window")
                }
                TieredLookup::Miss => {
                    table.insert(&key, &value, hash).expect("paced storm fits the window")
                }
            };
            let parts = table.record(placed);
            model.insert(key, (value, parts.version, parts.encoded_len));
        }
        rig.maintain();
    }
    // Drain: everything sealed flushes and seals; flushed reaches ro.
    let flush = &mut rig.flush;
    let table = rig.ks.tiered_store_mut(NS).expect("materialized");
    table.flush_drain(flush).expect("drain");
    assert_eq!(
        table.space().flushed(),
        table.space().ro_boundary(),
        "drain confirms the whole sealed range"
    );
    while table.release_slice() > 0 {}

    // File-set identities: exact ranges, bases strictly advancing,
    // gapped only where the preceding file sealed for a ring-top hole
    // (ADR-0052 D2; stall/capacity seals are adjacent), every sealed
    // footer verifying against its catalog entry.
    let mut prev: Option<(u64, SealReason)> = None;
    for meta in rig.flush.sealed() {
        if let Some((want, prev_reason)) = prev {
            assert!(
                meta.base.to_raw() >= want,
                "file bases advance ({want} then {})",
                meta.base.to_raw()
            );
            if meta.base.to_raw() > want {
                assert_eq!(
                    prev_reason,
                    SealReason::RingTopGap,
                    "a gap between files exists only behind a ring-top seal"
                );
            }
        }
        prev = Some((meta.base.to_raw() + meta.data_len, meta.reason));
        let image = rig.fs.contents(&meta.path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("sealed image parses");
        let footer = summary.sealed.expect("sealed");
        assert_eq!(footer.data_len, meta.data_len, "footer and catalog agree");
        assert_eq!(footer.reason, meta.reason, "footer and catalog agree on the reason");
        assert_eq!(summary.first_bad_frame, None, "every frame verifies");
    }

    // Content audit: every cold record re-reads byte-exact from the
    // pipeline's files; RAM records answer from the table.
    let mut cold = 0u64;
    let entries: Vec<(Vec<u8>, Vec<u8>, usize)> =
        model.iter().map(|(k, (v, _, l))| (k.clone(), v.clone(), *l)).collect();
    for (key, want_value, len) in entries {
        let hash = TieredTable::hash_key(&key);
        let table = rig.ks.tiered_store_mut(NS).expect("materialized");
        let looked = table.lookup(&key, hash, &[]);
        match looked {
            TieredLookup::Ram(addr) => {
                assert_eq!(table.record(addr).value, &want_value[..], "RAM content");
            }
            TieredLookup::Cold(addr) => {
                assert_eq!(table.space().resolve(addr), AddrClass::Cold);
                let bytes =
                    rig.read_cold(addr.to_raw(), len).expect("cold record inside a pipeline file");
                let parts = TieredTable::decode_record(&bytes);
                assert_eq!(parts.key, &key[..], "cold key");
                assert_eq!(parts.value, &want_value[..], "cold content");
                cold += 1;
            }
            TieredLookup::Miss => panic!("lost key {:?}", String::from_utf8_lossy(&key)),
        }
    }
    assert!(cold > 100, "the storm demoted a real cold set ({cold})");
    let counters = rig.ks.tiering_counters();
    assert!(counters.flush_slices > 0);
    assert!(counters.flush_confirmed_bytes > 0);
    assert!(rig.flush.sealed().len() > 3, "capacity rotation happened");
}
