//! Shared fixture for the `tiered_*` integration tests (review
//! 2026-08-30 L18 R13, batch 65): the page/budget shape, the xorshift
//! key generator, the flush and address-space configs, the MAINTAIN
//! drain loop and the tier-file cold reader each binary used to carry
//! as its own copy. Every test keeps its own `Rig` — the scenarios
//! differ — and delegates these to here.
#![allow(dead_code, reason = "four test binaries each use a subset of this fixture")]

use std::path::Path;

use inf_foundation::time::Nanos;
use inf_log::ckpt::{IckApplyError, IckInfo, IckReaderConfig};
use inf_log::fs::mem::{MemFile, MemFs};
use inf_log::{
    BootFlush, FsyncClass, HandedOver, NsId, TIER_FRAME_BYTES, TierFlush, TierFlushConfig,
    TierIoMode, decode_record, tier_extract, tier_frame_offset, tier_frame_span,
};
use inf_log::{IckSummary, read_ick_hybrid};
use inf_store::{
    AddressSpaceConfig, BootHandedOver, DemotionConfig, KeyHasher, Keyspace, LogicalAddr, NsMode,
    NsSpec, ReplayError, ReplaySpill, ReplayWork, SettleProgress, StoreConfig, TierReplay,
    TierSpec, TieredTable, WallAnchor, apply_blob_ref_section, apply_live_set_section,
    apply_ref_section,
};

pub const PAGE: u64 = 4 << 10;
pub const BUDGET: u64 = 1 << 20;
pub const SHARD: &str = "shard-0";

/// xorshift64 — the tests' deterministic key stream.
pub fn seeded(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

pub fn flush_config(ns: NsId, file_capacity: u64) -> TierFlushConfig {
    TierFlushConfig {
        shard_dir: Path::new(SHARD).to_path_buf(),
        cell: 0,
        ns,
        mode: TierIoMode::Buffered,
        file_capacity,
        slice_bytes: PAGE,
    }
}

pub fn space_config(demote: DemotionConfig, origin: u64) -> AddressSpaceConfig {
    AddressSpaceConfig {
        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
        page_bytes: PAGE as usize,
        life_origin: LogicalAddr::from_raw(origin).expect("48-bit"),
    }
}

/// One MAINTAIN drain: seal / flush / release until a round makes no
/// progress (the reactor spreads these over iterations).
pub fn maintain(table: &mut TieredTable, flush: &mut TierFlush<MemFs>) {
    loop {
        let sealed = table.seal_slice();
        let f = table.flush_slice(flush).expect("flush slice");
        let released = table.release_slice();
        if sealed + released + f.appended_bytes + u64::from(f.gaps_crossed) == 0 {
            break;
        }
    }
}

/// Reads one cold record straight from the tier-file bytes the pipeline
/// wrote (the audit's cold path — no table access): sealed files first,
/// then the active file up to its durable length.
pub fn read_cold(flush: &TierFlush<MemFs>, fs: &MemFs, addr: u64, len: usize) -> Option<Vec<u8>> {
    let contains = |base: u64, flen: u64| addr >= base && addr + len as u64 <= base + flen;
    let (base, path) = flush
        .sealed()
        .iter()
        .find(|m| contains(m.base.to_raw(), m.data_len))
        .map(|m| (m.base.to_raw(), m.path.clone()))
        .or_else(|| {
            let (_, base, _, durable_len, path) = flush.active()?;
            contains(base.to_raw(), durable_len).then(|| (base.to_raw(), path.to_path_buf()))
        })?;
    let image = fs.contents(&path)?;
    let (first, count, skip) = tier_frame_span(addr - base, len);
    let from = tier_frame_offset(first) as usize;
    let to = from + count as usize * TIER_FRAME_BYTES;
    let mut out = Vec::new();
    tier_extract(image.get(from..to)?, skip, len, &mut out).ok()?;
    Some(out)
}

/// A boot replay machine over a fresh pipeline — the empty-section shape
/// (ADR-0174 D4): no catalogue, no handles, file ids from 0.
pub fn boot(fs: &MemFs, ns: NsId, file_capacity: u64) -> TierReplay<MemFs> {
    let flush = TierFlush::new(fs.clone(), flush_config(ns, file_capacity), 0);
    TierReplay::new(BootFlush::new(flush, Vec::new()), PAGE, PAGE)
}

/// The replay clock and wall anchor (tiered records carry no expiry).
pub const NOW: Nanos = Nanos(1_000_000);
pub const ANCHOR: WallAnchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

/// The replay seam the tests lend through `Keyspace::apply_record`: one
/// boot machine per namespace, as the recovery driver holds them, and
/// the log of the work the seam's owner drained from them.
pub struct TestSpill {
    machines: Vec<(NsId, TierReplay<MemFs>)>,
    /// One entry per non-empty drain, in order.
    pub drained: Vec<ReplayWork>,
}

impl ReplaySpill for TestSpill {
    type Fs = MemFs;

    fn replay_mut(&mut self, ns: NsId) -> Option<&mut TierReplay<MemFs>> {
        self.machines.iter_mut().find(|(id, _)| *id == ns).map(|(_, m)| m)
    }
}

impl TestSpill {
    pub fn new(ns: NsId, machine: TierReplay<MemFs>) -> TestSpill {
        TestSpill { machines: vec![(ns, machine)], drained: Vec::new() }
    }

    pub fn machine(&self, ns: NsId) -> &TierReplay<MemFs> {
        self.machines.iter().find(|(id, _)| *id == ns).map(|(_, m)| m).expect("lent machine")
    }

    pub fn machine_mut(&mut self, ns: NsId) -> &mut TierReplay<MemFs> {
        self.replay_mut(ns).expect("lent machine")
    }

    /// The machine back from the seam (for its hand-over).
    pub fn take(&mut self, ns: NsId) -> TierReplay<MemFs> {
        let i = self.machines.iter().position(|(id, _)| *id == ns).expect("lent machine");
        self.machines.swap_remove(i).1
    }

    /// The seam owner's drain at a frame or step boundary: each machine's
    /// work since the last, logged when there was any.
    pub fn drain(&mut self) {
        for (_, machine) in &mut self.machines {
            let work = machine.take_work();
            if !work.is_zero() {
                self.drained.push(work);
            }
        }
    }

    /// Everything drained so far.
    pub fn charged(&self) -> ReplayWork {
        let mut sum = ReplayWork::default();
        for w in &self.drained {
            sum.tier_bytes += w.tier_bytes;
            sum.barriers += w.barriers;
            sum.settle_reads += w.settle_reads;
            sum.walked_bytes += w.walked_bytes;
        }
        sum
    }
}

/// A keyspace whose tiered namespace `ns` holds `table` — the recovery
/// driver's shape: the catalog materializes the namespace, the recovered
/// table takes its place (the driver's `install_recovered_tiered` also
/// applies the catalog's knobs; the tests keep the table's own).
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

/// The recovered table out of the keyspace, after the dispatcher's work
/// (a placeholder takes its slot).
pub fn take_table(ks: &mut Keyspace, ns: NsId) -> TieredTable {
    let demote = DemotionConfig::for_budget(BUDGET, PAGE);
    let placeholder =
        TieredTable::new(space_config(demote, 0), demote, 1, KeyHasher::default()).expect("ring");
    std::mem::replace(ks.tiered_store_mut(ns).expect("materialized"), placeholder)
}

/// Loads a hybrid checkpoint the recovery driver's way: every image
/// through the shipped dispatcher with the namespace's machine lent (the
/// seam's owner draining per image), the ref, live-set and
/// blob-reference sections to the table, then the end of the checkpoint
/// (ADR-0174 R9). A replay refusal is the load's typed error.
pub fn load_checkpoint(
    fs: &MemFs,
    path: &Path,
    ks: &mut Keyspace,
    spill: &mut TestSpill,
    ns: NsId,
    manifested_flushed: u64,
) -> Result<(IckInfo, IckSummary), IckApplyError<ReplayError>> {
    let parts = std::cell::RefCell::new((&mut *ks, &mut *spill));
    let loaded = read_ick_hybrid(
        fs,
        path,
        IckReaderConfig::default(),
        |record| {
            let mut guard = parts.borrow_mut();
            let (ks, spill) = &mut *guard;
            let applied = ks.apply_record(&record, NOW, ANCHOR, &mut **spill);
            spill.drain();
            applied.map(|_| ())
        },
        |section| {
            let mut guard = parts.borrow_mut();
            let table = guard.0.tiered_store_mut(ns).expect("materialized");
            apply_ref_section(table, &section, manifested_flushed).expect("refs inside the unit");
            Ok(())
        },
        |section| {
            let mut guard = parts.borrow_mut();
            apply_live_set_section(guard.0.tiered_store_mut(ns).expect("materialized"), &section);
            Ok(())
        },
        |section| {
            let mut guard = parts.borrow_mut();
            apply_blob_ref_section(guard.0.tiered_store_mut(ns).expect("materialized"), &section);
            Ok(())
        },
        |_| panic!("no index-sidecar sections in this image"),
    );
    if loaded.is_ok() {
        let table = ks.tiered_store_mut(ns).expect("materialized");
        spill.machine_mut(ns).end_of_checkpoint(table);
    }
    loaded
}

/// Replays one modeled WAL tail of record-v1 encodings through the
/// shipped dispatcher, `Keyspace::apply_record` (markers park in its
/// register for their mutation — ADR-0059 D9), the seam's owner draining
/// once per record.
pub fn replay_tail(ks: &mut Keyspace, spill: &mut TestSpill, tail: &[u8]) {
    let mut rest = tail;
    while !rest.is_empty() {
        let (record, consumed) = decode_record(rest).expect("tail records decode");
        ks.apply_record(&record, NOW, ANCHOR, spill).expect("replays");
        spill.drain();
        rest = &rest[consumed..];
    }
    assert_eq!(ks.displace_register_len(), 0, "a trailing displace marker is a stream error");
}

/// The boot's end as the recovery driver plays it (ADR-0174 R10): the
/// end of replay, the settle steps to the tail (drained each), the
/// hand-over. Returns the plane's pipeline, the held handles and the
/// machine's final counters.
pub fn finish_boot(ks: &mut Keyspace, spill: &mut TestSpill, ns: NsId) -> BootHandedOver<MemFs> {
    let table = ks.tiered_store_mut(ns).expect("materialized");
    spill.machine_mut(ns).end_of_replay(table);
    while spill.machine_mut(ns).settle_step(table, PAGE).expect("settle step")
        == SettleProgress::More
    {
        spill.drain();
    }
    spill.drain();
    spill.take(ns).hand_over(table).expect("hands over")
}

/// The pipeline and handles of a hand-over.
pub fn handed(done: BootHandedOver<MemFs>) -> (TierFlush<MemFs>, Vec<(u32, MemFile)>) {
    let HandedOver { flush, handles } = done.handed;
    (flush, handles)
}
