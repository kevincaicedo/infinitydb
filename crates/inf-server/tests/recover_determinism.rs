//! M2-S13 recovery determinism and crash-consistency oracles (L7,
//! ADR-0018), on the injected `MemFs` seam:
//!
//! - **Determinism (the CI assert):** recovering the same files twice —
//!   the same image rebuilt byte-identically, and the same directory
//!   re-recovered after boot GC — yields byte-identical `StateDigest`s
//!   and identical post-recovery LSNs.
//! - **Crash sweep:** seeded workloads (sets, deletes, expiries,
//!   checkpoint publications) crash at random flush boundaries, some with
//!   a torn final write; the manifest-aware recovery (ick + tail from
//!   begin, pre-begin skip, torn-tail truncation) must digest-match a
//!   **reference replay** of the full retained log — the equivalence that
//!   proves the checkpoint/manifest path loses and invents nothing.
//!
//! The DST power-cut leg (lose/tear/reorder of un-fsynced writes across
//! 10k seeds) binds at M2-S18/S19 with the sim disk, as recorded in the
//! milestone plan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inf_foundation::time::Nanos;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::mem::MemFs;
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentConfig,
    SegmentId, SegmentReader, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs,
    scan_log_dir, segment_file_name, write_manifest,
};
use inf_server::{DurableConfig, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StateDigest, StoreConfig, WallAnchor};

const NS: NsId = NsId(16);
const CELL: u16 = 0;
const UNIX_BASE: u64 = 1_750_000_000_000;

fn now() -> Nanos {
    Nanos::from_millis(1)
}

fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: UNIX_BASE }
}

fn cfg() -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes: 8 << 10, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
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
        tier: None,
    })
    .expect("ns");
    ks
}

/// Deterministic xorshift64* (L7: no ambient randomness).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The model entry a checkpoint publishes: latest value + optional
/// expiry deadline (unix ms).
type Model = BTreeMap<Vec<u8>, (Vec<u8>, Option<u64>)>;

/// Build one seeded crashed-cell image on `fs`: random mutations flushed
/// at random frame boundaries, `ckpts` checkpoint publications, and an
/// optional torn final write. Returns whether a tear was injected.
fn build_crashed_image(fs: &MemFs, seed: u64, ops: usize, ckpts: u32, tear: bool) -> bool {
    let config = cfg();
    let dirs = create_cell_dirs(fs, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), config.segment).expect("rotor");
    let mut ring = StagingRing::new(config.staging);
    let mut rng = Rng(seed | 1);
    let mut model: Model = BTreeMap::new();
    let mut pending: Vec<inf_log::StagedAt> = Vec::new();
    let mut last_frame_base: Option<Lsn> = None;
    let mut latest_begin: Option<Lsn> = None;
    let mut published = 0u32;
    // Flush the pending records as one frame, returning (frame base,
    // first-record LSN).
    let flush = |ring: &mut StagingRing,
                 rotor: &mut SegmentRotor<MemFs>,
                 pending: &mut Vec<inf_log::StagedAt>|
     -> Option<(Lsn, Lsn)> {
        rotor.maintain(0).expect("maintain");
        let lease = ring.flush_into(rotor, 0).expect("flush")?;
        let first = lease.lsn_of(*pending.first().expect("flushed frames have records"));
        pending.clear();
        ring.release(lease);
        Some((Lsn::new(first.segment, first.offset - FRAME_HEADER_LEN as u32), first))
    };

    for op in 0..ops {
        let key = format!("k:{:02}", rng.below(48));
        match rng.below(10) {
            0..=6 => {
                let val_len = rng.below(96) as usize;
                let value: Vec<u8> = (0..val_len).map(|_| (rng.next() & 0xFF) as u8).collect();
                let at = ring
                    .stage(&MutationEffect::StringSet {
                        ns: NS,
                        key: key.as_bytes(),
                        value: &value,
                    })
                    .expect("stage");
                pending.push(at);
                model.insert(key.into_bytes(), (value, None));
            }
            7 => {
                let at = ring
                    .stage(&MutationEffect::Delete { ns: NS, key: key.as_bytes() })
                    .expect("stage");
                pending.push(at);
                model.remove(key.as_bytes());
            }
            _ => {
                // Mostly-future deadlines, occasionally already past.
                let at_unix_ms = if rng.below(4) == 0 {
                    UNIX_BASE.saturating_sub(1_000)
                } else {
                    UNIX_BASE + 100_000 + rng.below(100_000)
                };
                let at = ring
                    .stage(&MutationEffect::ExpireAt { ns: NS, at_unix_ms, key: key.as_bytes() })
                    .expect("stage");
                pending.push(at);
                if let Some(entry) = model.get_mut(key.as_bytes()) {
                    entry.1 = Some(at_unix_ms);
                }
            }
        }
        let boundary = pending.len() > rng.below(8) as usize;
        if (boundary || op + 1 == ops)
            && let Some((base, _)) = flush(&mut ring, &mut rotor, &mut pending)
        {
            last_frame_base = Some(base);
        }
        // Occasionally publish a checkpoint: begin marker in its own
        // frame, ick = model snapshot at the marker, manifest swap.
        if published < ckpts && boundary && rng.below(6) == 0 {
            published += 1;
            let at = ring
                .stage(&MutationEffect::CkptBegin { ckpt_id: u64::from(published) })
                .expect("stage marker");
            pending.push(at);
            let (base, begin) = flush(&mut ring, &mut rotor, &mut pending).expect("marker frame");
            last_frame_base = Some(base);
            latest_begin = Some(begin);
            let mut w = SyncIckWriter::create(
                fs.clone(),
                &dirs.ckpt,
                &config.ckpt,
                CELL,
                u64::from(published),
                begin,
                &[NS.0],
            )
            .expect("ick create");
            for (key, (value, expire)) in &model {
                w.append(&RecordView::StringPostImage { ns: NS, key, value }).expect("ick set");
                if let Some(at_unix_ms) = expire {
                    w.append(&RecordView::ExpireAt { ns: NS, at_unix_ms: *at_unix_ms, key })
                        .expect("ick expire");
                }
            }
            w.finish().expect("ick publish");
            let segments: Vec<SegmentId> =
                (begin.segment.0..=rotor.active_segment().0).map(SegmentId).collect();
            write_manifest(
                fs,
                Path::new("data/shard-0"),
                &Manifest {
                    ckpt_id: u64::from(published),
                    begin_lsn: begin,
                    segments,
                    tiers: Vec::new(),
                },
            )
            .expect("manifest swap");
        }
    }

    // Crash. Optionally tear the final frame — but never the frame that
    // carries the newest begin marker (a tear below begin is the
    // fail-stop guard's territory, tested separately).
    let tear_target = last_frame_base.filter(|base| {
        tear && latest_begin.is_none_or(|begin| {
            begin.offset > base.offset + FRAME_HEADER_LEN as u32 || begin.segment != base.segment
        })
    });
    if let Some(base) = tear_target {
        let path = Path::new("data/shard-0/log").join(segment_file_name(base.segment));
        let mut file = fs.open_write(&path).expect("open");
        file.write_at(u64::from(base.offset + FRAME_HEADER_LEN as u32), &[0x5C, 0x5C])
            .expect("tear");
        return true;
    }
    false
}

/// Reference oracle: replay the full retained log in order — no manifest,
/// no checkpoint, no pre-begin skip — stopping at the first frame failure
/// (the same bytes recovery classifies as the torn tail).
fn reference_replay(fs: &MemFs) -> StateDigest {
    let mut ks = fresh_keyspace();
    let log_dir = Path::new("data/shard-0/log");
    let scan = scan_log_dir(fs, log_dir).expect("scan");
    'segments: for &segment in scan.segments() {
        let mut reader =
            SegmentReader::open(fs, log_dir, segment, inf_log::ReaderConfig::default())
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

fn recover_digest(fs: &MemFs) -> (StateDigest, Lsn, inf_server::RecoverStats) {
    let mut ks = fresh_keyspace();
    let (rotor, stats, _seed) =
        open_cell_log(fs.clone(), &mut ks, CELL, &cfg(), anchor(), now()).expect("recover");
    let resume = Lsn::new(rotor.active_segment(), rotor.active_written());
    (ks.state_digest(now()), resume, stats)
}

/// The L7 CI assert: same files → byte-identical digests and identical
/// post-recovery LSNs, across independent rebuilds of the image and
/// across repeated recoveries of the same directory (boot GC included).
#[test]
fn recovering_the_same_files_twice_is_byte_identical() {
    let seed = 0xD5E_ED00;
    let fs_a = MemFs::new();
    let fs_b = MemFs::new();
    assert!(build_crashed_image(&fs_a, seed, 300, 2, true), "the seed must tear");
    assert!(build_crashed_image(&fs_b, seed, 300, 2, true), "identical rebuild");

    let (digest_a, resume_a, stats_a) = recover_digest(&fs_a);
    let (digest_b, resume_b, _) = recover_digest(&fs_b);
    assert_eq!(digest_a, digest_b, "independent identical images digest equal");
    assert_eq!(resume_a, resume_b, "identical post-recovery LSNs");
    assert!(stats_a.torn_truncated_at.is_some(), "the torn tail was exercised");
    assert!(stats_a.ckpt_records > 0, "the checkpoint path was exercised");

    // Second recovery of the SAME directory (after boot GC + torn-tail
    // truncation): state and LSN must be unchanged.
    let (digest_a2, resume_a2, stats_a2) = recover_digest(&fs_a);
    assert_eq!(digest_a, digest_a2, "re-recovery digests equal");
    assert_eq!(resume_a, resume_a2, "re-recovery LSN equal");
    assert_eq!(stats_a2.stale_files_removed, 0, "boot GC is idempotent");
}

/// The crash sweep: recovery (manifest → ick → tail-from-begin, torn-tail
/// truncation) must be digest-equivalent to a reference replay of the
/// whole retained log, at random crash points, with and without tears.
#[test]
fn crash_at_random_points_recovery_matches_reference_replay() {
    let mut torn_runs = 0u32;
    let mut ckpt_runs = 0u32;
    for seed in 0..24u64 {
        let fs = MemFs::new();
        let ops = 120 + (seed as usize * 37) % 400;
        let ckpts = (seed % 3) as u32;
        let tear = seed % 2 == 0;
        let torn = build_crashed_image(&fs, 0x0BAD_5EED ^ (seed << 8), ops, ckpts, tear);

        // Reference first: recovery's boot GC deletes below-floor
        // segments the reference still needs.
        let reference = reference_replay(&fs);
        let (recovered, _resume, stats) = recover_digest(&fs);
        assert_eq!(
            recovered, reference,
            "seed {seed}: recovery (ckpt={}, torn={}) diverged from the reference replay",
            stats.ckpt_records, torn
        );
        torn_runs += u32::from(torn);
        ckpt_runs += u32::from(stats.ckpt_records > 0);
    }
    assert!(torn_runs >= 6, "the sweep must exercise torn tails ({torn_runs})");
    assert!(ckpt_runs >= 6, "the sweep must exercise the manifest path ({ckpt_runs})");
}
