//! M4.5-S31 reactor-drive equivalence storm (ADR-0084): the **same
//! seeded workload** drives two pipelines — the ADR-0056 D3 seam drive
//! on `MemFs` and the reactor drive on `SimDisk` (rounds staged by
//! `stage_flush_round`, executed through the sim driver's write/fsync
//! entry points exactly as the plane would, effects applied by
//! `complete_flush_round`) — and the outcomes must be identical:
//! flushed watermark, sealed catalog (id, base, length, reason), and
//! **byte-for-byte file images**.
//!
//! Round-scoped invariants asserted every MAINTAIN: no watermark moves
//! between stage and completion (the §3.1 chain — durability facts
//! apply only at the barrier CQE), the claim rule holds after every
//! completion, and writes strictly precede barriers in the op list.

use std::collections::BTreeMap;
use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{NsId, TierDrive, TierFlush, TierFlushConfig, TierIoMode, inspect_tier_bytes};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(41);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small file capacity so the storm rotates files constantly.
const FILE_CAPACITY: u64 = 96 << 10;
const OPS: u64 = 60_000;

fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn keyspace() -> Keyspace {
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
            DemotionConfig::for_budget(BUDGET, PAGE),
            2048,
        )
        .is_ok()
    );
    ks
}

fn pipeline<F: SegmentFs>(fs: F) -> TierFlush<F> {
    TierFlush::new(
        fs,
        TierFlushConfig {
            shard_dir: Path::new("shard-0").to_path_buf(),
            cell: 0,
            ns: NS,
            mode: TierIoMode::Buffered,
            file_capacity: FILE_CAPACITY,
            slice_bytes: PAGE,
        },
        0,
    )
}

/// Executes the staged round the plane's way: every write, then every
/// barrier (fdatasync covers only completed writes), then the effects.
fn run_round(disk: &SimDisk, table: &mut TieredTable, flush: &mut TierFlush<SimDisk>) {
    let flushed_at_stage = table.space().flushed();
    let writes = flush.round_write_count();
    for index in 0..flush.round_op_count() {
        let op = flush.round_op(index);
        assert_eq!(op.is_barrier, index >= writes, "writes strictly precede barriers");
        if op.is_barrier {
            disk.driver_fdatasync(op.fd).expect("driver barrier");
        } else {
            disk.driver_write_at(op.fd, op.offset, op.bytes).expect("driver write");
        }
    }
    // Nothing moved while the round was in flight (ADR-0084 D2).
    assert_eq!(table.space().flushed(), flushed_at_stage, "no watermark before completion");
    let _ = table.complete_flush_round(flush);
    // Claim rule after every completion (the seam storm's invariant).
    if flush.active().is_some() {
        let limit = flush.confirmable_end().expect("active file has a bound");
        assert!(table.space().flushed().to_raw() <= limit, "flushed within the claim bound");
    }
}

/// One MAINTAIN round on the reactor drive: seal → stage round → run
/// round → release, looping until this round's backlog drains.
fn maintain_reactor(ks: &mut Keyspace, disk: &SimDisk, flush: &mut TierFlush<SimDisk>) {
    loop {
        let d = ks.demote_tick();
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let staged = table.stage_flush_round(flush).expect("stage round");
        let mut round_work = 0u64;
        if flush.round_active() {
            round_work = 1 + staged;
            run_round(disk, table, flush);
        }
        if d.sealed_bytes + d.released_bytes + round_work == 0 {
            break;
        }
    }
}

/// One MAINTAIN round on the seam drive (the storm oracle's shape).
fn maintain_seam(ks: &mut Keyspace, flush: &mut TierFlush<MemFs>) {
    loop {
        let d = ks.demote_tick();
        let table = ks.tiered_store_mut(NS).expect("materialized");
        let f = table.flush_slice(flush).expect("flush slice");
        if d.sealed_bytes + d.released_bytes + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

fn sim_image(disk: &SimDisk, path: &Path) -> Vec<u8> {
    let file = disk.open_read(path).expect("file exists");
    let size = file.file_size().expect("size") as usize;
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = file.read_at(read as u64, &mut bytes[read..]).expect("read");
        assert!(n > 0, "no EOF inside the image");
        read += n;
    }
    bytes
}

/// The equivalence storm: both drives, one op sequence, one outcome.
#[test]
fn reactor_drive_matches_the_seam_drive_byte_for_byte() {
    let mem = MemFs::new();
    let disk = SimDisk::new();
    let mut seam_ks = keyspace();
    let mut reactor_ks = keyspace();
    let mut seam_flush = pipeline(mem.clone());
    let mut reactor_flush = pipeline(disk.clone());
    reactor_flush.set_drive(TierDrive::Reactor);

    let mut seed = 0x54EE_DF1Cu64;
    let keys = 900u64;
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
            let mut placed_version = 0u32;
            let mut placed_len = 0usize;
            for ks in [&mut seam_ks, &mut reactor_ks] {
                let table = ks.tiered_store_mut(NS).expect("materialized");
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
                placed_version = parts.version;
                placed_len = parts.encoded_len;
            }
            model.insert(key, (value, placed_version, placed_len));
        }
        maintain_seam(&mut seam_ks, &mut seam_flush);
        maintain_reactor(&mut reactor_ks, &disk, &mut reactor_flush);
        // Lockstep: every MAINTAIN leaves both drives at one watermark.
        assert_eq!(
            seam_ks.tiered_store_mut(NS).expect("t").space().flushed(),
            reactor_ks.tiered_store_mut(NS).expect("t").space().flushed(),
            "drives agree on the flushed watermark after every round"
        );
    }

    // Catalog equivalence: same files, same ranges, same reasons.
    let seam_meta: Vec<_> =
        seam_flush.sealed().iter().map(|m| (m.id, m.base.to_raw(), m.data_len, m.reason)).collect();
    let reactor_meta: Vec<_> = reactor_flush
        .sealed()
        .iter()
        .map(|m| (m.id, m.base.to_raw(), m.data_len, m.reason))
        .collect();
    assert_eq!(seam_meta, reactor_meta, "identical sealed catalogs");
    assert!(seam_meta.len() > 3, "capacity rotation happened ({})", seam_meta.len());

    // Image equivalence: every sealed file byte-identical across drives,
    // and self-verifying.
    for (seam, reactor) in seam_flush.sealed().iter().zip(reactor_flush.sealed()) {
        let seam_image = mem.contents(&seam.path).expect("seam file exists");
        let reactor_image = sim_image(&disk, &reactor.path);
        assert_eq!(seam_image, reactor_image, "file {} images match", seam.id);
        let summary = inspect_tier_bytes(&reactor_image).expect("sealed image parses");
        assert_eq!(summary.sealed.expect("sealed").data_len, reactor.data_len);
        assert_eq!(summary.first_bad_frame, None, "every frame verifies");
    }

    // Both tables agree on every key's resolution class and content.
    let mut cold = 0u64;
    for (key, (want_value, _, _)) in &model {
        let hash = TieredTable::hash_key(key);
        let seam_table = seam_ks.tiered_store_mut(NS).expect("t");
        let seam_hit = seam_table.lookup(key, hash, &[]);
        let reactor_table = reactor_ks.tiered_store_mut(NS).expect("t");
        match (seam_hit, reactor_table.lookup(key, hash, &[])) {
            (TieredLookup::Ram(a), TieredLookup::Ram(b)) => {
                assert_eq!(a, b, "same RAM address");
                assert_eq!(reactor_table.record(b).value, &want_value[..]);
            }
            (TieredLookup::Cold(a), TieredLookup::Cold(b)) => {
                assert_eq!(a, b, "same cold address");
                cold += 1;
            }
            (s, r) => panic!("resolution diverged for {key:?}: {s:?} vs {r:?}"),
        }
    }
    assert!(cold > 50, "the storm demoted a real cold set ({cold})");
}
