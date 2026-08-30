//! Durability crash-matrix harness. M2 fault-point coverage lives in
//! `m2.toml`; M3 document record/checkpoint cuts live in `m3.toml`.
//! Both are reviewable data, while this crate supplies the shared schema
//! reader and the M2 seeded kill-and-recover oracle.
//!
//! **Crash physics at this tier are process KILL** (MemFs: every
//! completed write survives — the page cache outlives the process). The
//! matrix proves each named point's documented failure path composes
//! with recovery policy; the power-cut physics (un-fsynced writes that
//! vanish, tear, reorder) are the M2-S18 sim disk and the M2-S19 sweep.
//!
//! Oracle vocabulary per run (the S17 "durability oracle + state digest"
//! at kill tier):
//! - the armed point **fired** and the builder observed its documented
//!   injection semantics (ADR-0019 D7);
//! - the recovered digest equals a **reference replay** of the surviving
//!   log (the S13 oracle, reused);
//! - recovery's typed outcome matches the row's `expect`;
//! - the surviving-state model matches key-for-key (`assert_model`), and
//!   the per-policy ack contract holds: under `always` **zero**
//!   acked-durable writes are lost (§8.2 — nothing unfsynced was ever
//!   acked); under `everysec` the acked-but-lost set is exactly the
//!   final unflushed/torn records (within the loss window by
//!   construction here; the ack-stream oracle proper binds at S19);
//! - recovery is idempotent (second recovery digest-equal).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inf_foundation::KeyHasher;
use inf_foundation::fault::{self, FaultSpec};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_log::ckpt::SyncIckWriter;
use inf_log::fs::mem::MemFs;
use inf_log::{
    CkptConfig, FRAME_HEADER_LEN, Lsn, Manifest, MutationEffect, NsId, RecordView, SegmentConfig,
    SegmentId, SegmentReader, SegmentRotor, StagingConfig, StagingRing, create_cell_dirs,
    scan_log_dir, write_manifest,
};
use inf_server::{DurableConfig, RecoverStats, RecoveredManifest, open_cell_log};
use inf_store::{FsyncClass, Keyspace, NsMode, NsSpec, StateDigest, StoreConfig, WallAnchor};

pub const NS: NsId = NsId(16);
pub const CELL: u16 = 0;
const UNIX_BASE: u64 = 1_750_000_000_000;

pub fn now() -> Nanos {
    Nanos::from_millis(1)
}

pub fn anchor() -> WallAnchor {
    WallAnchor { internal_ms: 0, unix_ms: UNIX_BASE }
}

// ---------------------------------------------------------------------
// Matrix definition (m2.toml): hand-rolled reader for exactly this
// schema (`[[row]]` tables of scalars + string lists — the house
// pattern, see inf-bench/src/gates.rs).
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct MatrixRow {
    pub point: String,
    pub policies: Vec<String>,
    pub workloads: Vec<String>,
    pub expect: String,
    /// "memfs" (default — the runner executes it) or "node" (carried by
    /// the named test; counted for coverage, skipped by the runner).
    pub tier: String,
    /// Node-tier rows: the test file that carries the row.
    pub test: String,
}

#[derive(Clone, Debug)]
pub struct MatrixDef {
    pub seeds: u64,
    pub rows: Vec<MatrixRow>,
}

/// Parses the checked-in matrix definition.
///
/// # Panics
/// On malformed lines or unknown fields — the definition is data under
/// review; a typo must fail the build, not skip a row.
#[must_use]
pub fn load_matrix(path: &Path) -> MatrixDef {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("crash-matrix definition {}: {e}", path.display()));
    let mut seeds = 0u64;
    let mut rows: Vec<MatrixRow> = Vec::new();
    let mut current: Option<MatrixRow> = None;
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[row]]" {
            if let Some(row) = current.take() {
                rows.push(row);
            }
            current = Some(MatrixRow { tier: "memfs".into(), ..MatrixRow::default() });
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{}:{}: expected `key = value`", path.display(), lineno + 1));
        let (key, value) = (key.trim(), value.trim());
        let Some(row) = current.as_mut() else {
            match key {
                "schema" => assert_eq!(value, "1", "unknown crash-matrix schema"),
                "milestone" => {}
                "seeds" => seeds = value.parse().expect("seeds: integer"),
                other => panic!("{}:{}: unknown top-level key {other}", path.display(), lineno + 1),
            }
            continue;
        };
        match key {
            "point" => row.point = unquote(value),
            "expect" => row.expect = unquote(value),
            "tier" => row.tier = unquote(value),
            "test" => row.test = unquote(value),
            "policies" => row.policies = string_list(value),
            "workloads" => row.workloads = string_list(value),
            other => panic!("{}:{}: unknown row field {other}", path.display(), lineno + 1),
        }
    }
    if let Some(row) = current.take() {
        rows.push(row);
    }
    assert!(seeds > 0, "matrix definition must set seeds");
    assert!(!rows.is_empty(), "matrix definition has no rows");
    MatrixDef { seeds, rows }
}

fn unquote(value: &str) -> String {
    value.trim_matches('"').to_owned()
}

fn string_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner.split(',').map(|item| unquote(item.trim())).filter(|item| !item.is_empty()).collect()
}

// ---------------------------------------------------------------------
// Workload shapes.
// ---------------------------------------------------------------------

/// One workload shape: segment geometry + op mix knobs. Shapes exist to
/// reach different fault sites (frame commits, rotations, publications).
#[derive(Copy, Clone, Debug)]
pub struct Workload {
    pub name: &'static str,
    pub segment_bytes: u32,
    pub ops: usize,
    /// Max value length (values are seeded random bytes).
    pub value_max: u64,
    /// Checkpoint publications to attempt (ckpt_cycle only).
    pub ckpts: u32,
}

#[must_use]
pub fn workload(name: &str) -> Workload {
    match name {
        // Mixed set/del/expire, few rotations: the frame-commit sites.
        "steady" => {
            Workload { name: "steady", segment_bytes: 16 << 10, ops: 260, value_max: 96, ckpts: 0 }
        }
        // Fat values + tiny segments: seals, preallocs, rotations.
        "rotation_heavy" => Workload {
            name: "rotation_heavy",
            segment_bytes: 4 << 10,
            ops: 160,
            value_max: 512,
            ckpts: 0,
        },
        // Checkpoint publications + MANIFEST swaps interleaved.
        "ckpt_cycle" => Workload {
            name: "ckpt_cycle",
            segment_bytes: 8 << 10,
            ops: 300,
            value_max: 96,
            ckpts: 3,
        },
        other => panic!("unknown workload shape {other:?}"),
    }
}

#[must_use]
pub fn config(segment_bytes: u32) -> DurableConfig {
    DurableConfig {
        data_dir: PathBuf::from("data"),
        staging: StagingConfig::default(),
        segment: SegmentConfig { segment_bytes, ..Default::default() },
        ckpt: CkptConfig::default(),
        recover: Default::default(),
        flush_bound: 1,
        fua_p50_us_probed: 0,
        device: Default::default(),
        fill: Default::default(),
        group: Default::default(),
    }
}

#[must_use]
pub fn fresh_keyspace(policy: FsyncClass) -> Keyspace {
    let mut ks = Keyspace::new(StoreConfig::default());
    ks.ns_create(NsSpec {
        id: NS,
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(policy),
        policy: None,
        maxmemory: None,
        tier: None,
    })
    .expect("ns");
    ks
}

#[must_use]
pub fn policy(name: &str) -> FsyncClass {
    match name {
        "always" => FsyncClass::Always,
        "everysec" => FsyncClass::Everysec,
        other => panic!("unknown fsync policy {other:?}"),
    }
}

// ---------------------------------------------------------------------
// The kill-and-recover run.
// ---------------------------------------------------------------------

/// Latest value + optional expiry deadline (unix ms) per key — the
/// surviving-state expectation.
pub type Model = BTreeMap<Vec<u8>, (Vec<u8>, Option<u64>)>;

/// What one seeded run observed (the runner's assertion inputs).
#[derive(Clone, Debug, Default)]
pub struct RunOutcome {
    /// Times the armed point fired (0 = vacuous row → the runner fails).
    pub fired: u64,
    /// The typed error the injection surfaced, when the point injects one
    /// (`torn_frame` succeeds silently — lying-disk physics).
    pub typed_error: Option<String>,
    /// `torn_frame`: base LSN of the frame that lied (recovery must
    /// truncate exactly here).
    pub torn_base: Option<Lsn>,
    /// Surviving-state expectation after recovery drops what the point's
    /// semantics say it drops.
    pub model: Model,
    /// Records acked-then-lost under the run's policy: `always` must
    /// keep this at **zero** (§8.2); `everysec` counts the final
    /// unflushed/torn records (the ≤ 1 s window at kill tier).
    pub acked_lost: u64,
    /// ckpt_cycle: last publication whose MANIFEST swap fully succeeded.
    pub committed_ckpt: Option<u64>,
    /// ckpt_cycle: the publication the fault interrupted.
    pub attempted_ckpt: Option<u64>,
}

/// Runs one seeded workload with `point` armed per its documented
/// semantics, up to the fault (or completion), then "kills the process"
/// (drops the writer). Returns the surviving MemFs image + observations.
///
/// # Panics
/// On any deviation from the point's documented injection semantics —
/// e.g. a typed error where the point promises silent success.
#[must_use]
pub fn run_workload(
    fs: &MemFs,
    point: &str,
    class: FsyncClass,
    wl: Workload,
    seed: u64,
) -> RunOutcome {
    fault::disarm_all();
    let cfg = config(wl.segment_bytes);
    let dirs = create_cell_dirs(fs, Path::new("data/shard-0")).expect("dirs");
    let mut rotor =
        SegmentRotor::create_fresh(fs.clone(), dirs.log.clone(), cfg.segment).expect("rotor");
    let mut ring = StagingRing::new(cfg.staging);
    let mut rng = SplitMix64::new(seed | 1);
    let mut out = RunOutcome::default();

    // Pending ops mirror the staged-but-unflushed records: they join the
    // model only when their frame lands un-faulted. Under `everysec` a
    // staged record is acked at apply — before its flush — so every
    // pending record lost to the crash (torn frame, failed append, the
    // kill itself) counts into `acked_lost`. Under `always` nothing
    // un-fsynced was ever acked, so `acked_lost` stays 0 structurally.
    let mut pending: Vec<(Vec<u8>, Op)> = Vec::new();
    let mut published = 0u32;
    // Frame-commit points arm at a seeded flush index (Nth(1) fires the
    // chosen flush); rotation points arm at a seeded op index (the next
    // rotation-class site fires); publication points arm immediately
    // before a seeded publication (deterministic schedule below).
    let arm_at_flush = 2 + rng.next_below(6);
    let arm_at_op = wl.ops as u64 / 3 + rng.next_below(wl.ops as u64 / 3);
    let frame_point = matches!(point, "torn_frame" | "log_append_short_write");
    let rotation_point = matches!(
        point,
        "fsync_err" | "power_cut_after_seal" | "prealloc_no_space" | "dir_fsync_fail"
    );
    // dir_fsync_fail fires at two sites; the workload picks the row's
    // intent: publications exist only in ckpt_cycle.
    let rotation_arm = rotation_point && !(point == "dir_fsync_fail" && wl.ckpts > 0);
    let publication_arm =
        matches!(point, "manifest_rename_fail" | "dir_fsync_fail") && wl.ckpts > 0;
    let arm_at_publication = 1 + rng.next_below(u64::from(wl.ckpts.max(1)));

    let mut flushes = 0u64;
    let mut frame_armed = false;
    'run: for op in 0..wl.ops {
        if rotation_arm && op as u64 == arm_at_op {
            let spec = if point == "prealloc_no_space" {
                FaultSpec::FromNth(1)
            } else {
                FaultSpec::Nth(1)
            };
            fault::arm(lookup_point(point), spec);
        }
        let key = format!("k:{:02}", rng.next_below(48)).into_bytes();
        let action = rng.next_below(10);
        if action <= 6 {
            let len = rng.next_below(wl.value_max) as usize;
            let value: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            stage(&mut ring, &MutationEffect::StringSet { ns: NS, key: &key, value: &value });
            pending.push((key, Op::Set(value)));
        } else if action == 7 {
            stage(&mut ring, &MutationEffect::Delete { ns: NS, key: &key });
            pending.push((key, Op::Del));
        } else {
            // Strictly-future deadlines: model equality stays exact under
            // the injected clock.
            let at_unix_ms = UNIX_BASE + 100_000 + rng.next_below(100_000);
            stage(&mut ring, &MutationEffect::ExpireAt { ns: NS, at_unix_ms, key: &key });
            pending.push((key, Op::ExpireAt(at_unix_ms)));
        }

        let boundary = pending.len() > rng.next_below(6) as usize;
        if !(boundary || op + 1 == wl.ops) {
            continue;
        }
        flushes += 1;
        if frame_point && flushes == arm_at_flush {
            fault::arm(lookup_point(point), FaultSpec::Nth(1));
            frame_armed = true;
        }
        if let Err(err) = rotor.maintain(0) {
            // Rotation-class injection surfaces here (prealloc ENOSPC /
            // prealloc dir barrier): typed refusal, stop, crash.
            out.typed_error = Some(err.to_string());
            break 'run;
        }
        match ring.flush_into(&mut rotor, 0) {
            Ok(Some(lease)) => {
                let torn = point == "torn_frame"
                    && fault::fired(lookup_point(point)) > 0
                    && out.torn_base.is_none();
                if torn {
                    // The disk lied: the call succeeded, only a prefix
                    // landed. Recovery truncates the whole frame; its
                    // records stay in `pending` (lost — acked only under
                    // everysec's ack-on-apply).
                    let first = lease.first_record_lsn();
                    out.torn_base =
                        Some(Lsn::new(first.segment, first.offset - FRAME_HEADER_LEN as u32));
                    ring.release(lease);
                    break 'run; // torn is only meaningful as the final write
                }
                apply_pending(&mut out.model, &mut pending);
                ring.release(lease);
            }
            Ok(None) => {}
            Err(err) => {
                // Failed append (short write / seal fsync / power cut at
                // seal): the frame never landed; the refusal is synchronous.
                out.typed_error = Some(err.to_string());
                break 'run;
            }
        }

        // ckpt_cycle: deterministic publication schedule (a seeded draw
        // could leave an envelope row vacuous — a rotted row).
        if wl.ckpts > 0
            && published < wl.ckpts
            && op >= (published as usize + 1) * wl.ops / (wl.ckpts as usize + 1)
        {
            published += 1;
            if publication_arm && u64::from(published) == arm_at_publication {
                fault::arm(lookup_point(point), FaultSpec::Nth(1));
                out.attempted_ckpt = Some(u64::from(published));
            }
            // A frame point must not fire on the publication's begin-marker
            // frame: live publication is watermark-guarded (staging begins
            // only once the marker is fsync-covered — ADR-0017), so a torn
            // marker under a published MANIFEST is an unreachable state.
            let park_frame_fault = frame_armed && fault::fired(lookup_point(point)) == 0;
            if park_frame_fault {
                fault::disarm(lookup_point(point));
            }
            let result = publish(fs, &mut rotor, &mut ring, &cfg, &out.model, published);
            if park_frame_fault {
                fault::arm(lookup_point(point), FaultSpec::Nth(1));
            }
            match result {
                Ok(()) => {
                    if out.attempted_ckpt != Some(u64::from(published)) {
                        out.committed_ckpt = Some(u64::from(published));
                    }
                }
                Err(err) => {
                    out.typed_error = Some(err);
                    break 'run; // crash at the interrupted publication
                }
            }
        }
    }

    // Kill: the process dies here. Staged records that never landed in a
    // surviving frame were acked (and are now lost) only under everysec.
    if class == FsyncClass::Everysec {
        out.acked_lost = pending.len() as u64;
    }
    out.fired = fault::fired(lookup_point(point));
    fault::disarm_all();
    out
}

#[derive(Clone, Debug)]
enum Op {
    Set(Vec<u8>),
    Del,
    ExpireAt(u64),
}

fn apply_pending(model: &mut Model, pending: &mut Vec<(Vec<u8>, Op)>) {
    for (key, op) in pending.drain(..) {
        match op {
            Op::Set(value) => {
                model.insert(key, (value, None));
            }
            Op::Del => {
                model.remove(&key);
            }
            Op::ExpireAt(at) => {
                if let Some(entry) = model.get_mut(&key) {
                    entry.1 = Some(at);
                }
            }
        }
    }
}

fn stage(ring: &mut StagingRing, effect: &MutationEffect<'_>) {
    ring.stage(effect).expect("staging sized for the workload");
}

fn lookup_point(point: &str) -> &'static str {
    inf_log::fault::ALL
        .iter()
        .chain(inf_server::fault::ALL)
        .find(|&&name| name == point)
        .copied()
        .unwrap_or_else(|| panic!("matrix names unknown fault point {point:?}"))
}

/// One checkpoint publication: begin marker frame → `.ick` → MANIFEST
/// swap (the recover_determinism pattern).
fn publish(
    fs: &MemFs,
    rotor: &mut SegmentRotor<MemFs>,
    ring: &mut StagingRing,
    cfg: &DurableConfig,
    model: &Model,
    id: u32,
) -> Result<(), String> {
    let at = ring
        .stage(&MutationEffect::CkptBegin { ckpt_id: u64::from(id) })
        .expect("marker fits staging");
    rotor.maintain(0).map_err(|e| e.to_string())?;
    let lease = ring.flush_into(rotor, 0).map_err(|e| e.to_string())?.expect("marker frame");
    let begin = lease.lsn_of(at);
    ring.release(lease);
    let mut w = SyncIckWriter::create(
        fs.clone(),
        Path::new("data/shard-0/ckpt"),
        &cfg.ckpt,
        CELL,
        u64::from(id),
        begin,
        &[NS.0],
    )
    .map_err(|e| e.to_string())?;
    for (key, (value, expire)) in model {
        w.append(&RecordView::StringPostImage { ns: NS, key, value }).map_err(|e| e.to_string())?;
        if let Some(at_unix_ms) = expire {
            w.append(&RecordView::ExpireAt { ns: NS, at_unix_ms: *at_unix_ms, key })
                .map_err(|e| e.to_string())?;
        }
    }
    w.finish().map_err(|e| e.to_string())?;
    let segments: Vec<SegmentId> =
        (begin.segment.0..=rotor.active_segment().0).map(SegmentId).collect();
    write_manifest(
        fs,
        Path::new("data/shard-0"),
        &Manifest {
            ckpt_id: u64::from(id),
            begin_lsn: begin,
            segments,
            tiers: Vec::new(),
            key_hash_id: KeyHasher::default().identity(),
        },
    )
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Oracles (shared with the S13 determinism suite's shapes).
// ---------------------------------------------------------------------

/// Reference replay: the full retained log in order — no manifest, no
/// checkpoint, no pre-begin skip — stopping at the first frame failure
/// (the same bytes recovery classifies as the torn tail). Must run
/// **before** recovery (boot GC deletes below-floor segments it needs).
#[must_use]
pub fn reference_replay(fs: &MemFs, class: FsyncClass) -> StateDigest {
    let mut ks = fresh_keyspace(class);
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

/// Recovery under the real boot path; returns the recovered keyspace and
/// its digest + typed outcome + the resolved recovery unit.
pub struct Recovered {
    pub ks: Keyspace,
    pub digest: StateDigest,
    pub stats: RecoverStats,
    pub unit: Option<RecoveredManifest>,
    pub resume: Lsn,
}

#[must_use]
pub fn recover(fs: &MemFs, class: FsyncClass, segment_bytes: u32) -> Recovered {
    let mut ks = fresh_keyspace(class);
    let (rotor, stats, unit) =
        open_cell_log(fs.clone(), &mut ks, CELL, &config(segment_bytes), anchor(), now())
            .expect("matrix rows recover; fail-stop rows assert separately");
    let resume = Lsn::new(rotor.active_segment(), rotor.active_written());
    let digest = ks.state_digest(now());
    Recovered { ks, digest, stats, unit, resume }
}

/// Key-for-key surviving-state check: every model entry recovers to its
/// exact value, and the recovered entry count matches (no extras).
pub fn assert_model(recovered: &mut Recovered, model: &Model, context: &str) {
    assert_eq!(
        recovered.digest.entries,
        model.len() as u64,
        "{context}: recovered entry count vs model"
    );
    let store = recovered.ks.ns_store_mut(NS).expect("ns store");
    for (key, (value, _expire)) in model {
        let got = store.get(key, now()).map(<[u8]>::to_vec);
        assert_eq!(
            got.as_deref(),
            Some(value.as_slice()),
            "{context}: model key {:?} diverged",
            String::from_utf8_lossy(key)
        );
    }
}
