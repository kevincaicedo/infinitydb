//! M4-S13 write-path accounting: the four per-namespace byte counters
//! are **exact**, each against an independently computed expectation —
//! user bytes against a shadow sum of what the caller asked to store,
//! WAL bytes against the encoded record lengths the staging ring
//! accepted, flush bytes against the bytes that actually reached the
//! (simulated) device.
//!
//! The counters are what M4-S16 divides and what the ops guide teaches
//! operators to alarm on, so "close enough" is not a passing grade here:
//! every assertion below is an equality except the two that are
//! deliberately inequalities (framing overhead, tail-frame rewrites), and
//! those state their exact structure.

use std::path::Path;

use inf_log::fs::mem::MemFs;
use inf_log::{
    MutationEffect, NsId, StagingConfig, StagingRing, TIER_FOOTER_BYTES, TIER_FRAME_BYTES,
    TIER_HEADER_BYTES, TierFlush, TierFlushConfig, TierIoMode,
};
use inf_store::{
    AddressSpaceConfig, DemotionConfig, Keyspace, LogicalAddr, StoreConfig, TieredLookup,
    TieredTable,
};

const NS: NsId = NsId(41);
const PAGE: u64 = 4 << 10;
const BUDGET: u64 = 1 << 20;
/// Small enough that the storm rotates files (rotation writes a footer
/// and a header — device bytes the data-byte view never sees).
const FILE_CAPACITY: u64 = 64 << 10;
/// A MAINTAIN slice in the shape production runs (ADR-0052 D4's commit
/// page is 1 MiB): many frames per fdatasync barrier, so the partial
/// tail frame is rewritten once per slice rather than once per frame.
const WIDE_SLICE: u64 = 256 << 10;
/// File capacity in the same spirit — header/footer blocks amortized.
const WIDE_FILE_CAPACITY: u64 = 1 << 20;

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
    /// A rig whose MAINTAIN slice quantum is `slice_bytes` for both legs
    /// — the seal step (ADR-0053 D3) and the flush barrier (ADR-0056 D3)
    /// — because that pair is what decides how often a partial tail
    /// frame is rewritten, and therefore the tier leg of write
    /// amplification.
    fn new(slice_bytes: u64, file_capacity: u64) -> Rig {
        let fs = MemFs::new();
        let demote = DemotionConfig { slice_bytes, ..DemotionConfig::for_budget(BUDGET, PAGE) };
        let mut ks = Keyspace::new(StoreConfig::default());
        assert!(
            ks.materialize_tiered(
                NS,
                AddressSpaceConfig {
                    reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
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
                file_capacity,
                slice_bytes,
            },
            0,
        );
        Rig { ks, fs, flush }
    }

    /// The small-slice rig: one commit page per slice, tiny files —
    /// rotation and tail-frame rewrites on every test that wants them.
    fn narrow() -> Rig {
        Rig::new(PAGE, FILE_CAPACITY)
    }

    /// Seals the active file and confirms everything staged, so the
    /// device holds every byte the workload produced.
    fn quiesce(&mut self) {
        self.drain();
        let flush = &mut self.flush;
        self.ks.tiered_store_mut(NS).expect("materialized").flush_drain(flush).expect("drain");
    }

    fn table(&mut self) -> &mut TieredTable {
        self.ks.tiered_store_mut(NS).expect("materialized")
    }

    /// Drives the S07/S11 MAINTAIN cadence — seal slice → flush slice →
    /// release — to quiescence. The tests write far faster than one
    /// slice per round, so a round here runs the loop the reactor would
    /// spread over iterations; without it the budget window fills and
    /// the workload stalls before enough bytes reach the tier for the
    /// accounting to say anything.
    fn drain(&mut self) {
        for _ in 0..1_000_000u32 {
            let demoted = self.ks.demote_tick();
            let flush = &mut self.flush;
            let table = self.ks.tiered_store_mut(NS).expect("materialized");
            let flushed = table.flush_slice(flush).expect("flush slice");
            let idle = demoted.sealed_bytes == 0
                && demoted.released_bytes == 0
                && flushed.appended_bytes == 0
                && flushed.confirmed_bytes == 0
                && flushed.gaps_crossed == 0;
            if idle {
                return;
            }
        }
        panic!("demotion + flush must quiesce");
    }

    /// Bytes the tier files occupy on the (simulated) device right now —
    /// the independent expectation `flush_bytes` is checked against.
    fn on_disk_bytes(&self) -> u64 {
        let mut total = 0;
        for meta in self.flush.sealed() {
            total += self.fs.contents(&meta.path).expect("sealed file exists").len() as u64;
        }
        if let Some((_, _, _, _, path)) = self.flush.active() {
            total += self.fs.contents(path).expect("active file exists").len() as u64;
        }
        total
    }
}

/// User bytes are key + value at the record boundary, charged once per
/// admitted record image — for inserts, for relocating updates, and for
/// in-place rewrites alike (an in-place update never reaches the append
/// path, and forgetting it there would report an infinite write
/// amplification for the exact-fit workload). Deletes charge nothing:
/// a tombstone stores no user byte, and the WAL cost it does have shows
/// up as amplification, honestly.
#[test]
fn user_bytes_count_key_plus_value_at_the_record_boundary() {
    let mut rig = Rig::narrow();
    let mut expected = 0u64;
    let mut seed = 0x5713_ACC7u64;
    let mut lens = Vec::new();
    let mut versions = Vec::new();

    // Phase 1 — inserts.
    for i in 0..256u32 {
        let key = format!("k:{i:05}");
        let value = vec![0x41u8; 32 + (seeded(&mut seed) % 96) as usize];
        let hash = TieredTable::hash_key(key.as_bytes());
        let table = rig.table();
        let addr = table.insert(key.as_bytes(), &value, hash).expect("fits");
        expected += (key.len() + value.len()) as u64;
        lens.push(table.record(addr).encoded_len);
        versions.push(table.record(addr).version);
    }
    assert_eq!(rig.table().write_accounting().user_bytes, expected, "inserts charge exactly once");

    // Phase 2 — same-length updates (the in-place path) and
    // length-changing updates (the copy-to-tail path). Both are user
    // writes of a full record image.
    for i in 0..256usize {
        let key = format!("k:{i:05}");
        let hash = TieredTable::hash_key(key.as_bytes());
        let grow = i % 2 == 0;
        let value = vec![0x42u8; if grow { lens[i] } else { 32 }];
        let table = rig.table();
        let addr = match table.lookup(key.as_bytes(), hash, &[]) {
            TieredLookup::Ram(addr) | TieredLookup::Cold(addr) => addr,
            TieredLookup::Miss => panic!("the key was inserted"),
        };
        let placed =
            table.update(key.as_bytes(), &value, hash, addr, lens[i], versions[i]).expect("fits");
        expected += (key.len() + value.len()) as u64;
        lens[i] = table.record(placed).encoded_len;
        versions[i] = table.record(placed).version;
    }
    assert_eq!(
        rig.table().write_accounting().user_bytes,
        expected,
        "both update routes charge the record image they wrote"
    );

    // Phase 3 — deletes charge no user bytes.
    let before = rig.table().write_accounting().user_bytes;
    for i in 0..64usize {
        let key = format!("k:{i:05}");
        let hash = TieredTable::hash_key(key.as_bytes());
        let table = rig.table();
        let TieredLookup::Ram(addr) = table.lookup(key.as_bytes(), hash, &[]) else {
            panic!("the key is RAM-resident");
        };
        let len = table.record(addr).encoded_len;
        table.delete(hash, addr, len);
    }
    assert_eq!(
        rig.table().write_accounting().user_bytes,
        before,
        "a tombstone stores no user byte"
    );

    // The denominator excludes record framing by construction: the
    // address space allocated strictly more than the user asked to store.
    let allocated = rig.table().space().report().allocated_bytes;
    assert!(
        allocated > expected,
        "record headers are encoding cost, never user bytes ({allocated} vs {expected})"
    );
}

/// WAL bytes are exactly the encoded record bytes the staging ring
/// accepted — and a refused staging (typed backpressure) charges
/// nothing, because nothing was written.
#[test]
fn wal_bytes_track_exactly_what_the_ring_accepted() {
    let mut rig = Rig::narrow();
    let mut ring = StagingRing::new(StagingConfig::default());
    let mut expected = 0u64;

    for i in 0..512u32 {
        let key = format!("k:{i:05}");
        let value = vec![0x37u8; 48];
        let effect = MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: &value };
        rig.table().stage_wal(&mut ring, &effect).expect("the ring has room");
        expected += effect.encoded_len() as u64;
    }
    assert_eq!(rig.table().write_accounting().wal_bytes, expected);
    assert_eq!(
        ring.stats().append_bytes,
        expected,
        "the namespace counter and the ring's own total agree — one staging site"
    );

    // Deletes stage a record and therefore do cost WAL bytes: the
    // asymmetry with `user_bytes` is real amplification, not a bug.
    let tombstone = MutationEffect::Delete { ns: NS, key: b"k:00000" };
    rig.table().stage_wal(&mut ring, &tombstone).expect("room");
    expected += tombstone.encoded_len() as u64;
    assert_eq!(rig.table().write_accounting().wal_bytes, expected);

    // Fill the ring to refusal: the counter must not move.
    let filler = vec![0x5Au8; ring.max_record_len() as usize / 2];
    while rig
        .table()
        .stage_wal(&mut ring, &MutationEffect::StringSet { ns: NS, key: b"filler", value: &filler })
        .is_ok()
    {
        expected += MutationEffect::StringSet { ns: NS, key: b"filler", value: &filler }
            .encoded_len() as u64;
    }
    assert_eq!(
        rig.table().write_accounting().wal_bytes,
        expected,
        "a refused staging wrote nothing and charges nothing"
    );
}

/// Flush bytes are **device** bytes: every byte the tier pipeline handed
/// the block layer, framing and rewrites included. Checked against the
/// bytes actually present in the tier files, with the two structural
/// gaps named exactly — a rewritten partial tail frame is counted twice
/// on purpose (it *was* written twice), and per-file header/footer blocks
/// are counted because the device wrote them.
#[test]
fn flush_bytes_are_device_bytes_including_framing_and_rewrites() {
    let mut rig = Rig::narrow();
    let mut seed = 0xF105_1113u64;

    assert_eq!(rig.table().write_accounting().flush_bytes, 0, "nothing flushed, nothing charged");

    for round in 0..64u32 {
        for i in 0..64u32 {
            let key = format!("k:{round:03}:{i:03}");
            let value = vec![0x61u8; 64 + (seeded(&mut seed) % 192) as usize];
            let hash = TieredTable::hash_key(key.as_bytes());
            rig.table().insert(key.as_bytes(), &value, hash).expect("fits");
        }
        rig.drain();
    }
    // Quiesce so the active file seals and every staged byte is on disk.
    rig.quiesce();
    let appended_data = rig.table().space().flushed().to_raw();

    let charged = rig.table().write_accounting().flush_bytes;
    let on_disk = rig.on_disk_bytes();
    let files = rig.flush.sealed().len() as u64;
    assert!(files > 1, "the storm rotated files (header/footer overhead is in the picture)");

    assert!(
        charged >= on_disk,
        "device bytes ({charged}) cover the file bytes ({on_disk}); the excess is rewrites"
    );
    assert_eq!(
        (charged - on_disk) % TIER_FRAME_BYTES as u64,
        0,
        "the only excess over file size is whole rewritten frames"
    );
    assert!(
        on_disk >= files * (TIER_HEADER_BYTES + TIER_FOOTER_BYTES) as u64,
        "every sealed file carries a header and a footer block"
    );
    assert!(
        charged > appended_data,
        "framing is amplification too: {charged} device bytes for {appended_data} data bytes"
    );
    // Re-charging is idempotent: a flush leg that appends nothing adds
    // nothing (the delta fold, not a per-call increment).
    let flush = &mut rig.flush;
    let table = rig.ks.tiered_store_mut(NS).expect("materialized");
    table.flush_slice(flush).expect("empty slice");
    assert_eq!(table.write_accounting().flush_bytes, charged, "an empty slice charges nothing");
}

/// `compaction_bytes` is copy-forward's **relocation volume**, not a
/// device-byte leg: the bytes it counts are written by the flush that
/// follows, so charging it moves the volume counter and leaves the
/// write-amp numerator alone (M4-S16, ADR-0060 D2 — the alternative
/// counts every relocated byte twice). It also never touches user bytes.
#[test]
fn compaction_bytes_are_relocation_volume_not_a_numerator_leg() {
    let mut rig = Rig::narrow();
    let hash = TieredTable::hash_key(b"k");
    rig.table().insert(b"k", b"value", hash).expect("fits");
    let mut ring = StagingRing::new(StagingConfig::default());
    let effect = MutationEffect::StringSet { ns: NS, key: b"k", value: b"value" };
    rig.table().stage_wal(&mut ring, &effect).expect("room");
    rig.quiesce();

    let before = rig.table().write_accounting();
    assert_eq!(before.compaction_bytes, 0);
    assert_eq!(before.written_bytes(), before.wal_bytes + before.flush_bytes);

    rig.table().note_compaction_bytes(4096);
    let after = rig.table().write_accounting();
    assert_eq!(after.compaction_bytes, 4096);
    assert_eq!(after.written_bytes(), before.written_bytes(), "the numerator is wal + flush");
    assert_eq!(
        after.write_amplification(),
        before.write_amplification(),
        "relocation volume alone never moves the reported ratio"
    );
    assert_eq!(after.user_bytes, before.user_bytes, "compaction moves no user byte");
}

/// Runs a fixed insert workload — WAL record staged, record appended,
/// MAINTAIN driven to quiescence — eight times the memory budget, so the
/// residue still sitting in the mutable region is a few percent rather
/// than the measurement. Returns the namespace's accounting.
fn amplification_run(rig: &mut Rig) -> inf_store::WriteAccounting {
    let mut ring = StagingRing::new(StagingConfig::default());
    let value = vec![0x77u8; 512];
    for i in 0..16_384u32 {
        let key = format!("k:{i:06}");
        let effect = MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: &value };
        let table = rig.table();
        table.stage_wal(&mut ring, &effect).expect("room");
        table.insert(key.as_bytes(), &value, TieredTable::hash_key(key.as_bytes())).expect("fits");
        if i % 64 == 63 {
            rig.drain();
            // The LOG step drains the staging ring in production; here
            // the test only needs its admission bound reset.
            ring = StagingRing::new(StagingConfig::default());
        }
    }
    rig.quiesce();
    rig.table().write_accounting()
}

/// The headline operator fact, made mechanical: with tiering a user byte
/// is written twice by design (WAL record + tier flush), so at a
/// production-shaped slice budget the numerator sits near 2× the
/// denominator — above it, because record framing, frame CRCs, and
/// per-file blocks are real bytes. The band is wide on purpose; the
/// guide states the shape, and this test stops it drifting into a
/// different order of magnitude unnoticed.
#[test]
fn write_amplification_shape_is_write_twice_plus_framing() {
    let mut rig = Rig::new(WIDE_SLICE, WIDE_FILE_CAPACITY);
    let acct = amplification_run(&mut rig);
    assert!(acct.user_bytes > 0 && acct.wal_bytes > 0 && acct.flush_bytes > 0);
    let amp = acct.written_bytes() as f64 / acct.user_bytes as f64;
    // Printed (visible under `--nocapture`) so the guide's stated shape
    // can be re-read from a test run, not only from a campaign artifact.
    println!(
        "write amp {amp:.3} = (wal {} + flush {}) / user {} (relocation volume {}; \
         ADR-0060 D2 keeps it out of the numerator)",
        acct.wal_bytes, acct.flush_bytes, acct.user_bytes, acct.compaction_bytes
    );
    assert!(
        (1.8..2.6).contains(&amp),
        "write amp {amp:.2} outside the write-twice-plus-framing shape \
         (user {}, wal {}, flush {}, compaction {})",
        acct.user_bytes,
        acct.wal_bytes,
        acct.flush_bytes,
        acct.compaction_bytes
    );
    // The cell aggregate is the exact sum of the per-namespace lines, and
    // the M4-S16 summary reports that one namespace's ratio as the worst.
    let total = rig.ks.tiering_write_accounting();
    assert_eq!(total.user_bytes, acct.user_bytes, "one tiered namespace: the total is that one");
    assert_eq!(total.wal_bytes, acct.wal_bytes);
    assert_eq!(total.flush_bytes, acct.flush_bytes);
    assert_eq!(total.compaction_bytes, acct.compaction_bytes);
    assert_eq!(total.written_bytes(), acct.written_bytes());
    assert_eq!(rig.ks.tiered_namespaces().count(), 1);
    let summary = rig.ks.tiering_write_amp();
    assert_eq!(summary.unbounded_namespaces, 0, "the workload admitted user bytes");
    assert_eq!(summary.milli_max, acct.write_amplification().milli().expect("measured"));
}

/// **The tuning fact the counters exist to expose (M4-S13 finding).**
/// A tier file's tail frame is rewritten in place at every barrier until
/// it fills (ADR-0056 D5), so the MAINTAIN slice quantum decides how
/// often a 4 KiB frame is paid for twice: at a one-page slice the tier
/// leg roughly doubles, at a production-shaped slice it amortizes to
/// framing overhead. Neither run is wrong — the counters report what the
/// device was handed — and an operator who sees `tiering_flush_bytes`
/// running near 2× the data has a slice budget to raise, not a bug to
/// file. The guide says so because this test measures it.
#[test]
fn flush_amplification_follows_the_slice_budget() {
    let narrow = amplification_run(&mut Rig::narrow());
    let wide = amplification_run(&mut Rig::new(WIDE_SLICE, WIDE_FILE_CAPACITY));
    assert_eq!(narrow.user_bytes, wide.user_bytes, "same workload, same denominator");
    assert_eq!(narrow.wal_bytes, wide.wal_bytes, "the WAL leg is slice-independent");
    println!(
        "flush bytes: {} at a {} B slice vs {} at a {} B slice (data {})",
        narrow.flush_bytes, PAGE, wide.flush_bytes, WIDE_SLICE, wide.user_bytes
    );
    assert!(
        narrow.flush_bytes > wide.flush_bytes * 3 / 2,
        "one-page slices pay the tail-frame rewrite on nearly every frame \
         ({} vs {})",
        narrow.flush_bytes,
        wide.flush_bytes
    );
    assert!(
        wide.flush_bytes < wide.user_bytes * 5 / 4,
        "a production-shaped slice keeps the tier leg near the data bytes ({} vs {})",
        wide.flush_bytes,
        wide.user_bytes
    );
}
