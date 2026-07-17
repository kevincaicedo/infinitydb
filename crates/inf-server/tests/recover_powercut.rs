//! M2-S18 integration smoke: seeded power-cut states from the sim disk
//! (`inf_log::fs::sim::SimDisk`) feed the real recovery machine
//! (`open_cell_log` is generic over `SegmentFs` — ADR-0018/ADR-0020).
//! Every surviving image must resolve per the M2-S14 taxonomy as amended
//! by M2.5-S12 (ADR-0031): on honest power-cut physics recovery
//! **succeeds on every seed** — torn tail truncated, resume at or past
//! the last fsync-covered byte, digest equal to a reference replay of the
//! surviving log, deterministic across re-boots. The reorder shape that
//! used to fail-stop (a validating frame beyond lost un-covered bytes —
//! the ADR-0021 D3 refusal) now truncates with stamp evidence and is
//! counted in `beyond_frames_discarded`; the sweep asserts the shape
//! still appears, so the retirement is exercised, not vacuous. Refusals
//! remain for shapes honest physics cannot produce (attested coverage
//! past the data end, v1 frames) — pinned by `recover_torn.rs` and
//! `recover_stamp.rs`.
//!
//! This is the composition proof, not the campaign: the durability
//! oracle over the ack stream and the 10k-seed sweep bind at M2-S19.

use std::path::{Path, PathBuf};

use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_log::fs::sim::SimDisk;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, Lsn, MutationEffect, NsId, SegmentConfig, SegmentReader, SegmentRotor,
    StagingConfig, StagingRing, create_cell_dirs, scan_log_dir, segment_file_name,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StateDigest, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const LOG_DIR: &str = "data/shard-0/log";

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: 1_750_000_000_000 }
}

fn cfg() -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 8 << 10, seal_after_ms: None },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        sync_pipeline: 1,
    }
}

fn fresh_keyspace() -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Always),
        policy: None,
        maxmemory: None,
    })
    .expect("ns");
    ks
}

/// Seeded tail-only workload on the sim disk: frames at random
/// boundaries, occasional explicit fdatasync of the active segment (the
/// covered prefix), then a power cut. Returns the exclusive end of the
/// last fsync-covered byte range (what recovery must never lose).
fn build_and_cut(disk: &SimDisk, seed: u64) -> Lsn {
    let config = cfg();
    let dirs = create_cell_dirs(disk, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(disk.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let mut rng = SplitMix64::new(seed | 1);
    let mut synced_end = Lsn::new(rotor.active_segment(), 0);
    let ops = 120 + (rng.next_below(200)) as usize;
    let mut pending = 0usize;
    for op in 0..ops {
        let key = format!("k:{:02}", rng.next_below(40));
        let len = rng.next_below(160) as usize;
        let value: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        ring.stage(&MutationEffect::StringSet { ns: NS, key: key.as_bytes(), value: &value })
            .expect("stage");
        pending += 1;
        if pending > rng.next_below(5) as usize || op + 1 == ops {
            rotor.maintain(0).expect("maintain");
            if !ring.is_empty() {
                // Seal like the live plane (ADR-0031 D1): the stamp
                // attests the watermark — here, the last explicit
                // fdatasync's coverage.
                let slot = rotor.begin_frame(ring.pending_frame_len(), 0).expect("reserve");
                let lease = ring.seal(slot.first_record_lsn(), synced_end.to_u64());
                let frame = ring.leased_frame(&lease).to_vec();
                rotor.commit_frame(slot, &frame).expect("commit");
                ring.release(lease);
            }
            pending = 0;
            // Roughly one flush in four is followed by an explicit
            // fdatasync of the active segment (the everysec/seal shape:
            // the covered prefix recovery must keep).
            if rng.next_below(4) == 0 {
                let path = dirs.log.join(segment_file_name(rotor.active_segment()));
                let mut file = disk.open_write(&path).expect("active segment");
                file.sync_data().expect("explicit fdatasync");
                synced_end = Lsn::new(rotor.active_segment(), rotor.active_written());
            }
        }
    }
    disk.power_cut(seed ^ 0x0D15_C0C7);
    synced_end
}

/// Reference oracle on the surviving image: full retained-log replay in
/// order, stopping at the first frame failure.
fn reference_replay(disk: &SimDisk) -> StateDigest {
    let mut ks = fresh_keyspace();
    let scan = scan_log_dir(disk, Path::new(LOG_DIR)).expect("scan");
    'segments: for &segment in scan.segments() {
        let mut reader = SegmentReader::open(
            disk,
            Path::new(LOG_DIR),
            segment,
            inf_log::ReaderConfig::default(),
        )
        .expect("open");
        let outcome = reader.apply_frames(|frame| {
            for record in frame.records() {
                let (_, record) = record.expect("valid record in valid frame");
                ks.apply_record(&record, now(), anchor()).expect("apply");
            }
            Ok::<(), std::convert::Infallible>(())
        });
        if outcome.is_err() {
            break 'segments;
        }
    }
    ks.state_digest(now())
}

#[test]
fn power_cut_states_recover_or_fail_stop_per_the_taxonomy() {
    let mut recovered = 0u32;
    let mut beyond_discarded = 0u64;
    for seed in 0..64u64 {
        let disk = SimDisk::new();
        let synced_end = build_and_cut(&disk, 0x5EED_0000 ^ (seed << 3) ^ 1);
        let reference = reference_replay(&disk);

        let mut ks = fresh_keyspace();
        // ADR-0031: honest power-cut physics always recovers — the gap +
        // validating-beyond-frame shape that used to refuse is provably
        // un-covered (no surviving attestation reaches it) and truncates,
        // counted. A refusal here would be a policy regression.
        let (rotor, stats, _unit) =
            open_cell_log(disk.clone(), &mut ks, CELL, &cfg(), anchor(), now()).unwrap_or_else(
                |err| panic!("seed {seed}: honest power-cut image refused to recover: {err}"),
            );
        let resume = Lsn::new(rotor.active_segment(), rotor.active_written());
        assert!(
            resume >= synced_end,
            "seed {seed}: recovery resumed at {resume}, below the fsync-covered end \
             {synced_end} — covered bytes were lost"
        );
        assert_eq!(
            ks.state_digest(now()),
            reference,
            "seed {seed}: recovery diverged from the surviving log's reference replay"
        );
        beyond_discarded += stats.beyond_frames_discarded;
        drop(rotor);
        // Determinism across re-boots of the same surviving image.
        let mut ks2 = fresh_keyspace();
        let (_r2, _s2, _u2) = open_cell_log(disk.clone(), &mut ks2, CELL, &cfg(), anchor(), now())
            .expect("second boot of a recovered image");
        assert_eq!(ks2.state_digest(now()), reference, "seed {seed}: re-recovery diverged");
        recovered += 1;
    }
    assert_eq!(recovered, 64, "every honest power-cut image recovers");
    assert!(
        beyond_discarded > 0,
        "no seed produced the gap + validating-beyond-frame shape — the retired refusal \
         (ADR-0031 D4) was never exercised by this sweep"
    );
    eprintln!(
        "power-cut smoke: {recovered} recovered, {beyond_discarded} un-covered beyond-frames \
         discarded with evidence (the retired ADR-0021 D3 refusals)"
    );
}
