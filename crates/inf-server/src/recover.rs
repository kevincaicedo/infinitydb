//! Boot-time per-cell log recovery (M2-S08/S11/S13/S14, ADR-0015 D7 +
//! ADR-0017 + ADR-0018): read `MANIFEST` (the atomic recovery-unit name),
//! load the checkpoint it names through the same `Keyspace::apply_record`
//! blind idempotent upsert as tail frames (ADR-0016 D1), replay the log
//! tail from `begin-LSN`, classify whatever lies beyond the last valid
//! frame (M2-S14), and reopen the tail segment for appending. Boot-time
//! blocking file I/O is the sanctioned exception to the cell denylist
//! (§3.3) and rides the injected `SegmentFs` seam for DST.
//!
//! Recovery-unit resolution (§8.4 — old or new, never neither, never
//! both-partial):
//! - No `MANIFEST` → replay the whole retained log (pre-checkpoint boots,
//!   fresh cells).
//! - `MANIFEST` present → it is the only authority: the named `.ick` must
//!   load (digest-verified) and the floor segment must exist — anything
//!   less is corruption and fail-stop, never a silent fallback to full
//!   replay (the segments below the floor may already be gone).
//! - Stale prefix segments (crash mid-truncation) and unnamed
//!   `.ick`/`.ick.new` orphans (crash between checkpoint publication and
//!   the MANIFEST swap) are garbage-collected here, before the cell
//!   serves.
//!
//! End-of-log policy (M2-S14, ADR-0018; ADR-0031 D4 as amended by
//! ADR-0087 D6): every segment's slack — the bytes beyond its last valid
//! frame — is scanned **right after that segment replays**. The first
//! slack holding a validating, self-located frame marks a **hole**: a
//! frame the writer queued below surviving data and the device lost. A
//! hole exists only if the barrier that would have covered those bytes
//! never completed, so the watermark never passed it and nothing at or
//! past it was acked — everything later is un-acked residue. The v2
//! stamps decide the one remaining question, whether the device is
//! lying: a later frame whose `covered_lsn` attests coverage at or past
//! the hole (or a v1 frame, which cannot attest) proves covered data was
//! lost — fail-stop with a named [`inf_log::LogCorruption`]. Otherwise
//! the tail *pointer* is truncated at the hole (bytes are never
//! rewritten), every later segment — probed for evidence, never replayed
//! — is removed to restore the pristine prealloc invariant, and the cell
//! continues under a fresh epoch. With no hole, the last data-bearing
//! segment's clean end is the resume point; non-validating remnants
//! behind it are the inert residue of an earlier torn-tail resume:
//! tolerated and counted, never fatal. A torn tail may never truncate
//! below the manifest's `begin-LSN`: everything the manifest names was
//! durable at publication, so a shorter log is disk lying.
//!
//! In the floor segment, records below `begin-LSN` are skipped: the
//! checkpoint already holds their final state, and replaying an older
//! post-image over it would regress entries whose last mutation preceded
//! `begin`. Recovery *throughput* gates bind at M2-S13/S22.
//!
//! M2-S15: recovery is a **resumable state machine** ([`Recovery`]) — the
//! plane drives it in bounded MAINTAIN steps so the cell answers
//! `-LOADING` while it replays (L6: boot is a budgeted task, not a
//! stop-the-world phase). [`open_cell_log`] is the same machine run to
//! completion in one call (tests, benches, MemFs tiers) — one code path,
//! so the S13 determinism sweep and the S14 taxonomy suite prove the
//! stepped machine too.

use std::io;
use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::ckpt::{
    IckReader, IckReaderConfig, IckStep, ick_file_name, parse_ick_file_name, read_ick_counts,
};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{
    FrameStamp, LogCorruption, Lsn, Manifest, ReadError, ReaderConfig, RegionEvidence, RegionScan,
    SegmentId, SegmentReader, SegmentRotor, SegmentScan, create_cell_dirs,
    create_cell_dirs_deferred, read_manifest, scan_log_dir_from, scan_region_evidence,
    segment_file_name,
};
use inf_store::{Keyspace, ReplayOutcome, WallAnchor};

use crate::durable::DurableConfig;

/// The recovery pipeline's phases as the outside sees them (M4.5-S39d):
/// what each [`Recovery::step`] is about to do, typed so the driver can
/// attribute the loop clock to it. Audit and probe share a phase — both
/// are the slack/evidence scan — exactly as the board's phase code does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoverPhase {
    /// Dirs, MANIFEST, directory scan, floor checks, checkpoint open.
    Start,
    /// Checkpoint sections streamed into the store.
    Ckpt,
    /// Tail frames replayed.
    Replay,
    /// Slack audit / evidence probe of a segment.
    Audit,
    /// Torn-tail resolution, begin guard, boot GC (stale files), rotor
    /// reopen.
    Finish,
    /// Nothing left: the next step reports completion.
    Complete,
}

impl RecoverPhase {
    /// The board's phase code (`CellRecoverySlot::phase_name`): 1 =
    /// start, 2 = checkpoint, 3 = replay, 4 = audit, 5 = finish, 6 =
    /// complete.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            RecoverPhase::Start => 1,
            RecoverPhase::Ckpt => 2,
            RecoverPhase::Replay => 3,
            RecoverPhase::Audit => 4,
            RecoverPhase::Finish => 5,
            RecoverPhase::Complete => 6,
        }
    }
}

/// Per-phase recovery accounting (M4.5-S39d, ADR-0090 A10): the bytes
/// each phase read and the time it took, so one boot's recovery figure
/// decomposes instead of being compared as a sum. Bytes are counted by
/// the machine itself; durations are credited by the driver from the
/// **injected loop clock** between consecutive steps
/// ([`Recovery::credit_phase_time`]) — cell code never reads an ambient
/// clock (L7). The synchronous tier (`open_cell_log`) has no clock and
/// leaves every duration zero. By construction every credited
/// nanosecond lands in exactly one phase, so `phase_ns` sums to
/// `total_ns` exactly — a property the e2e pins.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoverPhases {
    pub start_ns: u64,
    /// Checkpoint bytes read (sections + header/footer) and load time.
    pub ckpt_bytes: u64,
    pub ckpt_ns: u64,
    /// Tail frame bytes read + validated, frames, and replay time.
    pub replay_bytes: u64,
    pub replay_frames: u64,
    pub replay_ns: u64,
    /// Slack/probe bytes the evidence scans read, the self-located and
    /// foreign-segment frames they CRC-validated, and scan time.
    pub audit_bytes: u64,
    pub audit_valid_frames: u64,
    pub audit_foreign_frames: u64,
    pub audit_ns: u64,
    /// Torn-tail resolution + boot GC + rotor reopen time (the stale
    /// files removed are `RecoverStats::stale_files_removed`).
    pub finish_ns: u64,
    /// First step to completion, loop-clock.
    pub total_ns: u64,
}

impl RecoverPhases {
    /// The phase that took longest (ties resolve in pipeline order) —
    /// the row's "dominating phase"; `None` before any time was credited.
    #[must_use]
    pub fn dominating(&self) -> Option<RecoverPhase> {
        let phases = [
            (RecoverPhase::Start, self.start_ns),
            (RecoverPhase::Ckpt, self.ckpt_ns),
            (RecoverPhase::Replay, self.replay_ns),
            (RecoverPhase::Audit, self.audit_ns),
            (RecoverPhase::Finish, self.finish_ns),
        ];
        let (phase, ns) =
            phases.iter().fold(phases[0], |best, &p| if p.1 > best.1 { p } else { best });
        (ns > 0).then_some(phase)
    }

    /// The durations in pipeline order (the sum-equals-total pin).
    #[must_use]
    pub const fn phase_ns(&self) -> [u64; 5] {
        [self.start_ns, self.ckpt_ns, self.replay_ns, self.audit_ns, self.finish_ns]
    }
}

/// What one cell's recovery did (log lines + `INFO persistence` inputs).
#[derive(Copy, Clone, Debug, Default)]
pub struct RecoverStats {
    /// M4.5-S39d: per-phase bytes + loop-clock durations.
    pub phases: RecoverPhases,
    pub segments: u64,
    pub frames: u64,
    pub records_applied: u64,
    pub records_skipped: u64,
    /// Tail deltas already represented by a fuzzy checkpoint/full image
    /// (ADR-0043 R1). Separate from foreign-record skips.
    pub doc_deltas_skipped_stale: u64,
    /// Tail deltas whose key is absent because the fuzzy checkpoint
    /// captured a later delete/expiry (ADR-0043 R2).
    pub doc_deltas_skipped_missing: u64,
    /// Checkpoint-begin markers seen in the tail (counted so recovery
    /// output stays honest about them).
    pub markers: u64,
    /// Records loaded from the manifest-named checkpoint (M2-S11).
    pub ckpt_records: u64,
    /// Tail records below `begin-LSN` in the floor segment, skipped (the
    /// checkpoint supersedes them).
    pub records_pre_begin: u64,
    /// Boot GC: stale below-floor segments + unnamed `.ick`/`.ick.new`
    /// orphans removed.
    pub stale_files_removed: u64,
    /// M2-S14: `Some` when a torn final write was truncated — the (segment,
    /// offset) the log resumes at. The dropped bytes were never covered by
    /// a completed fsync, so nothing acked is lost (§8.2); callers log it.
    pub torn_truncated_at: Option<Lsn>,
    /// Trailing segments removed with a torn tail (each verified to hold
    /// no validating frame before deletion).
    pub torn_segments_removed: u64,
    /// M4.5-S37 (ADR-0093 A4): rebuilt shadow slots the boot read and
    /// settled by their full key — the ambiguous (two RAM keys with one
    /// hash) and over-cap pairs, never the general index.
    pub shadow_settle_reads: u64,
    /// ADR-0096 D3: quarantined extents the replayed map references,
    /// renamed back before serving — a wrong orphan verdict healed.
    /// Nonzero is the upstream-accounting falsifier signal; logged.
    pub blob_revived: u64,
    /// Sealed segments with non-validating remnant bytes behind their data
    /// end — the inert residue of an earlier torn-tail resume (the reader
    /// never crosses a segment's data end, so they can never replay).
    /// Tolerated, but counted: never silent (§8.4).
    pub sealed_slack_remnants: u64,
    /// M2.5-S12 (ADR-0031 D4, ADR-0087 D6): validating frames beyond a
    /// hole that the stamp evidence proved un-covered (no surviving
    /// attestation reaches them) — discarded with the torn tail instead
    /// of refusing. The retired ADR-0021 D3 refusal class, counted, never
    /// silent. Since ADR-0087 D6 this includes frames in later segments
    /// behind a hole in a sealed one (probed, never replayed).
    pub beyond_frames_discarded: u64,
    /// Epoch-regressed frames ending replay early (discarded-life residue
    /// resurfacing at the data end — ADR-0031 D3/D5). Counted per boot.
    pub epoch_residue_stops: u64,
    /// Segment slacks holding **validating discarded-life residue** —
    /// frames of an earlier life left beyond a data end that a torn-tail
    /// resume truncated the pointer at and never rewrote (ADR-0031 D4's
    /// "bytes are never rewritten"), then sealed past by a later life
    /// that rotated away before overwriting them (the ADR-0086 D4
    /// class-upgrade rotation makes this routine). Proven residue by
    /// epoch — below the replayed prefix's, or below a life observed
    /// beyond it — never a hole: excluded from the attestation decision,
    /// replay continues past it. Counted per boot (ADR-0031 D5 as amended
    /// 2026-08-21).
    pub stale_residue_slacks: u64,
    /// M4.5-S39b (ADR-0090 D2 as amended): replay ended a segment's data
    /// at a **foreign-segment frame** — a decoded frame at its stored
    /// offset but stamped for another segment id, the residue a recycled
    /// file carries from its previous life. A classified end like a zero
    /// tail (replay continues in the next segment), never data, never a
    /// hole. Counted per boot.
    pub segment_residue_stops: u64,
    /// Segment slacks proven **recycled-life residue** by the audit: no
    /// self-located frame, ≥ 1 foreign-segment frame (garbage beside it
    /// allowed — a torn tail over residue is indistinguishable from
    /// residue and nothing acked can sit past a data end). Never torn,
    /// never a hole; trailing ones are removed as stale. Counted per boot.
    pub recycled_residue_slacks: u64,
}

/// The recovered manifest — hands the recovery-time floor + named
/// checkpoint to [`ServerPlane::enable_durable`](crate::ServerPlane) so
/// the truncation slice resumes where the last boot left off.
#[derive(Copy, Clone, Debug)]
pub struct RecoveredManifest {
    pub ckpt_id: u64,
    pub begin_lsn: Lsn,
}

/// Opens (or creates) cell `cell`'s log under the node data dir, loading
/// the manifest-named checkpoint (if any) and replaying the tail into
/// `ks`. Returns the rotor positioned at the tail, ready for
/// `begin_frame_deferred`, plus the recovered manifest seed.
///
/// `fs` is the injected filesystem seam (§3.3): `StdSegmentFs` on the
/// node, `MemFs` in the recovery test tiers, the sim disk at M2-S18.
///
/// # Errors
/// Scan errors, manifest/checkpoint corruption, interior log corruption
/// (M2-S14 — a validating frame beyond a corrupt region fail-stops; only
/// a torn *final* write is truncated and survived), read errors, store
/// apply failures, and rotor open failures — all fail-stop at boot (§8.4).
pub fn open_cell_log<F: SegmentFs + Clone>(
    fs: F,
    ks: &mut Keyspace,
    cell: u16,
    cfg: &DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
) -> io::Result<(SegmentRotor<F>, RecoverStats, Option<RecoveredManifest>)> {
    let mut recovery = Recovery::new(fs, cell, cfg, anchor, now);
    while recovery.step(ks, u64::MAX)? == RecoveryProgress::Working {}
    Ok(recovery.finish())
}

/// Continuity verdict for one validating frame's stamp (ADR-0031 D3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StampVerdict {
    /// Continuity holds — apply the frame's records.
    Replay,
    /// Epoch regressed: discarded-life residue at the data end — replay
    /// ends here, the audit owns the rest.
    ResidueStop,
}

/// What one [`Recovery::step`] call reports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecoveryProgress {
    /// More work remains — call `step` again.
    Working,
    /// Recovery is complete: call [`Recovery::finish`].
    Complete,
}

/// Recovery phase (each `step` performs one bounded unit of the phase).
enum Phase<File: SegmentFile> {
    /// Dirs, MANIFEST, scan, floor checks, checkpoint open + presize —
    /// bounded by directory-entry counts, one step.
    Start,
    /// Checkpoint sections streamed under the step budget.
    Ick {
        reader: Box<IckReader<File>>,
    },
    /// Tail frames replayed under the step budget, one segment at a time.
    Replay {
        idx: usize,
        reader: Option<Box<SegmentReader<File>>>,
    },
    /// M2-S14 slack audit of segment `idx`, right after its replay
    /// (ADR-0087 D6); one segment per step.
    Audit {
        idx: usize,
    },
    /// A hole was found in an earlier segment: segment `idx` is scanned
    /// whole for stamp evidence and never replayed (ADR-0087 D6); one
    /// segment per step.
    Probe {
        idx: usize,
    },
    /// Torn-tail resolution, begin guard, boot GC, rotor reopen.
    Finish,
    Complete,
}

/// Boot recovery as a resumable state machine (M2-S15). Construction is
/// cheap; every I/O happens inside [`step`](Self::step), each call bounded
/// by `budget_bytes` (plus one frame/section overshoot). The phases and
/// their semantics are exactly [`open_cell_log`]'s documented pipeline —
/// that function *is* this machine run to completion.
pub struct Recovery<F: SegmentFs> {
    fs: Option<F>,
    cell: u16,
    cfg: DurableConfig,
    anchor: WallAnchor,
    now: Nanos,
    phase: Phase<F::File>,
    stats: RecoverStats,
    shard_dir: PathBuf,
    log_dir: PathBuf,
    ckpt_dir: PathBuf,
    manifest: Option<Manifest>,
    begin_lsn: Option<Lsn>,
    floor: SegmentId,
    scan: Option<SegmentScan>,
    stale: Vec<SegmentId>,
    segments: Vec<SegmentId>,
    /// Data extent per segment (file size — sparse prealloc makes this an
    /// upper bound; progress credits the slack when a segment completes).
    seg_sizes: Vec<u64>,
    ends: Vec<(u32, Option<ReadError>)>,
    residue: Vec<bool>,
    /// Per segment: its slack's validating frames are discarded-life
    /// residue (ADR-0031 D5 as amended) — counted as remnants, excluded
    /// from the hole/attestation decision.
    stale_slack: Vec<bool>,
    bytes_total: u64,
    bytes_done: u64,
    /// Bytes actually read+validated (frames, checkpoint blocks) — the
    /// throttle currency. `bytes_done` additionally credits skipped
    /// prealloc slack, which is bookkeeping, not I/O.
    bytes_consumed: u64,
    /// M2.5-S01: loop-resident boots defer every boot-metadata fsync off
    /// the ready path — dir creation and the fresh segment happen
    /// unsynced, and the open dir handles below become driver-ridden
    /// boot barriers at the head of the group-commit ledger. The
    /// synchronous tier ([`open_cell_log`]) keeps blocking syncs: it has
    /// no driver to ride.
    defer_boot_sync: bool,
    barrier_dirs: Vec<F::File>,
    /// Checkpoint bytes already credited to `bytes_done` (section blocks);
    /// the header/footer remainder is credited when the footer validates.
    ick_credited: u64,
    /// M2.5-S12 (ADR-0031 D3): the last v2 stamp replayed — the prefix
    /// continuity state (epoch monotone; seq +1 within an epoch, 1 at an
    /// epoch step; covered_lsn nondecreasing within an epoch).
    prev_stamp: Option<FrameStamp>,
    /// A v2 frame was replayed: a later v1 frame in the prefix is a named
    /// refusal (append order makes it unreachable — ADR-0031 D2).
    saw_v2: bool,
    /// Replay ended at an epoch-regressed frame (discarded-life residue —
    /// ADR-0031 D5): remaining segments are residue, never replayed.
    residue_stop: bool,
    /// Highest epoch observed in the valid prefix (the ADR-0031 D5
    /// derivation input, joined with the audit's beyond-frame evidence).
    max_prefix_epoch: u32,
    /// Stamp evidence per segment's slack (whole segment once probed),
    /// gathered by the audit/probe steps (ADR-0031 D4, ADR-0087 D6); the
    /// Finish phase aggregates it from the resume point on.
    evidence: Vec<RegionEvidence>,
    /// Index of the first segment whose slack held a validating frame —
    /// the hole (ADR-0087 D6). Every later segment is probed, never
    /// replayed.
    hole: Option<usize>,
    /// Index of the resume segment, fixed in the Finish phase: the hole's
    /// segment when it holds data, else the last data-bearing segment.
    last_data: usize,
    finished: Option<(SegmentRotor<F>, Option<RecoveredManifest>)>,
    /// Recovered tiered namespaces' plane half (M4-S26, ADR-0057 D6):
    /// flush pipeline + open sealed-file handles + the extent sweep
    /// seed — installed into the plane's tier state at completion.
    recovered_tiers: Vec<RecoveredTierNs<F>>,
    /// End-of-replay tiered checks ran (once, on entering the audit).
    tier_replay_checked: bool,
    /// Sidecar boot loader (M4.5-S06, ADR-0078 D6): consumes tag-0x06
    /// sections during the ick phase, arms `CatchUp` for the tail, and
    /// commits loaded trees at end of replay. Taken (once) on entering
    /// the audit — the commit point.
    #[cfg(feature = "doc")]
    sidecar: Option<inf_store::SidecarLoader>,
}

/// One recovered tiered namespace's plane-side pieces (M4-S26).
pub(crate) struct RecoveredTierNs<F: SegmentFs> {
    pub ns: inf_log::NsId,
    pub flush: inf_log::TierFlush<F>,
    /// Creation-mode fds for the manifested files (ADR-0054: one fd,
    /// one mode) — the cold-read table inherits them.
    pub files: Vec<(u32, F::File)>,
    pub extents_listed: Vec<u64>,
    /// `.quarantine`-named ids (ADR-0096 D3) — swept for the second
    /// verdict; referenced ids revive before the node serves.
    pub extents_quarantined: Vec<u64>,
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// Prepares recovery for cell `cell` under `cfg.data_dir`. No I/O
    /// happens here — the first [`step`](Self::step) does.
    #[must_use]
    pub fn new(fs: F, cell: u16, cfg: &DurableConfig, anchor: WallAnchor, now: Nanos) -> Self {
        let shard_dir = cfg.data_dir.join(format!("shard-{cell}"));
        Recovery {
            fs: Some(fs),
            cell,
            cfg: cfg.clone(),
            anchor,
            now,
            phase: Phase::Start,
            stats: RecoverStats::default(),
            log_dir: shard_dir.join("log"),
            ckpt_dir: shard_dir.join("ckpt"),
            shard_dir,
            manifest: None,
            begin_lsn: None,
            floor: SegmentId(0),
            scan: None,
            stale: Vec::new(),
            segments: Vec::new(),
            seg_sizes: Vec::new(),
            ends: Vec::new(),
            residue: Vec::new(),
            stale_slack: Vec::new(),
            bytes_total: 0,
            bytes_done: 0,
            bytes_consumed: 0,
            defer_boot_sync: false,
            barrier_dirs: Vec::new(),
            ick_credited: 0,
            prev_stamp: None,
            saw_v2: false,
            residue_stop: false,
            max_prefix_epoch: 0,
            evidence: Vec::new(),
            hole: None,
            last_data: 0,
            finished: None,
            recovered_tiers: Vec::new(),
            tier_replay_checked: false,
            #[cfg(feature = "doc")]
            sidecar: Some(inf_store::SidecarLoader::default()),
        }
    }

    /// Switch this machine to deferred boot-metadata durability
    /// (M2.5-S01): no blocking fsync runs on the ready path; the caller
    /// takes [`take_boot_barrier_dirs`](Self::take_boot_barrier_dirs)
    /// after `Complete` and registers the barriers. Loop-resident boots
    /// only — a synchronous run-to-completion has no driver for barriers.
    #[must_use]
    pub fn deferred_boot_sync(mut self) -> Self {
        self.defer_boot_sync = true;
        self
    }

    /// The boot directory handles whose driver-ridden fdatasyncs are the
    /// boot barriers (empty unless
    /// [`deferred_boot_sync`](Self::deferred_boot_sync) was set). Take
    /// them after `Complete`, before [`finish`](Self::finish).
    pub fn take_boot_barrier_dirs(&mut self) -> Vec<F::File> {
        core::mem::take(&mut self.barrier_dirs)
    }

    /// Code of the phase the next [`step`](Self::step) will run — the
    /// RecoveryBoard's stuck-cell observable (M2.5-S01: published
    /// *before* the step so a stalled step is visible from outside).
    /// 1 = start, 2 = checkpoint, 3 = replay, 4 = audit, 5 = finish,
    /// 6 = complete.
    #[must_use]
    pub fn phase_code(&self) -> u8 {
        self.phase().code()
    }

    /// The phase the next [`step`](Self::step) runs (M4.5-S39d).
    #[must_use]
    pub fn phase(&self) -> RecoverPhase {
        match &self.phase {
            Phase::Start => RecoverPhase::Start,
            Phase::Ick { .. } => RecoverPhase::Ckpt,
            Phase::Replay { .. } => RecoverPhase::Replay,
            Phase::Audit { .. } | Phase::Probe { .. } => RecoverPhase::Audit,
            Phase::Finish => RecoverPhase::Finish,
            Phase::Complete => RecoverPhase::Complete,
        }
    }

    /// Credits `ns` of the driver's clock to `phase` (M4.5-S39d): the
    /// driver samples its injected clock around consecutive steps and
    /// attributes the delta to the phase the earlier step ran. Credits to
    /// `Complete` are the sample after the last real step — nothing ran,
    /// so they are dropped on the floor rather than invented as a phase.
    pub fn credit_phase_time(&mut self, phase: RecoverPhase, ns: u64) {
        let phases = &mut self.stats.phases;
        let slot = match phase {
            RecoverPhase::Start => &mut phases.start_ns,
            RecoverPhase::Ckpt => &mut phases.ckpt_ns,
            RecoverPhase::Replay => &mut phases.replay_ns,
            RecoverPhase::Audit => &mut phases.audit_ns,
            RecoverPhase::Finish => &mut phases.finish_ns,
            RecoverPhase::Complete => return,
        };
        *slot += ns;
        phases.total_ns += ns;
    }

    /// Progress numerator: bytes validated + applied so far (checkpoint
    /// blocks + replayed frames; a completed segment credits its slack).
    #[must_use]
    pub fn bytes_done(&self) -> u64 {
        self.bytes_done
    }

    /// Progress denominator: checkpoint file size + segment file extents
    /// (includes preallocated slack — an upper bound, disclosed).
    #[must_use]
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total
    }

    /// Bytes actually read + validated so far (excludes the slack credits
    /// in [`bytes_done`](Self::bytes_done)) — what pacing should meter.
    #[must_use]
    pub fn bytes_consumed(&self) -> u64 {
        self.bytes_consumed
    }

    /// Segments fully replayed / total (0/0 before the scan phase runs).
    #[must_use]
    pub fn segments_progress(&self) -> (u64, u64) {
        let done = match &self.phase {
            Phase::Start | Phase::Ick { .. } => 0,
            Phase::Replay { idx, .. } | Phase::Probe { idx } => *idx as u64,
            Phase::Audit { idx } => *idx as u64 + 1,
            Phase::Finish | Phase::Complete => self.segments.len() as u64,
        };
        (done, self.segments.len() as u64)
    }

    /// Stats so far (complete once [`finish`](Self::finish) is reachable).
    #[must_use]
    pub fn stats(&self) -> &RecoverStats {
        &self.stats
    }

    fn fs(&self) -> &F {
        self.fs.as_ref().expect("fs present until finish")
    }

    /// Folds one evidence scan's cost into the audit phase's accounting
    /// (M4.5-S39d). Cumulative: a probe later lifted into a replay still
    /// read its bytes.
    fn note_audit(&mut self, evidence: &RegionEvidence) {
        let phases = &mut self.stats.phases;
        phases.audit_bytes += evidence.bytes_read;
        phases.audit_valid_frames += evidence.valid_frames;
        phases.audit_foreign_frames += evidence.foreign_frames;
    }

    /// Runs one bounded recovery step: at most ~`budget_bytes` of
    /// checkpoint/replay input (one whole-frame/section overshoot), or one
    /// audit/GC unit. Returns [`RecoveryProgress::Complete`] when
    /// [`finish`](Self::finish) may be called.
    ///
    /// # Errors
    /// Exactly [`open_cell_log`]'s fail-stop taxonomy; a failed machine
    /// must not be stepped again.
    pub fn step(&mut self, ks: &mut Keyspace, budget_bytes: u64) -> io::Result<RecoveryProgress> {
        match core::mem::replace(&mut self.phase, Phase::Complete) {
            Phase::Start => self.step_start(ks),
            Phase::Ick { reader } => self.step_ick(ks, reader, budget_bytes),
            Phase::Replay { idx, reader } => self.step_replay(ks, idx, reader, budget_bytes),
            Phase::Audit { idx } => self.step_audit(idx),
            // Replay is over once probing starts or Finish is reached:
            // the end-of-replay checks run exactly once, here.
            Phase::Probe { idx } => {
                self.end_of_replay_checks(ks)?;
                self.step_probe(idx)
            }
            // Finish reports `Working` and the *next* step `Complete`
            // (M4.5-S39d): the driver samples its clock around every
            // step, so the finish step's time is only attributable once a
            // later call exists — one extra polling iteration at boot.
            Phase::Finish => {
                self.end_of_replay_checks(ks)?;
                // Either outcome (complete, or a lifted hole resuming
                // replay) is more stepping from the driver's view.
                self.step_finish().map(|_| RecoveryProgress::Working)
            }
            Phase::Complete => Ok(RecoveryProgress::Complete),
        }
    }

    /// The recovered rotor + stats + manifest seed.
    ///
    /// # Panics
    /// If recovery has not reported [`RecoveryProgress::Complete`].
    #[must_use]
    pub fn finish(self) -> (SegmentRotor<F>, RecoverStats, Option<RecoveredManifest>) {
        let (rotor, seed) = self.finished.expect("recovery complete before finish()");
        (rotor, self.stats, seed)
    }

    fn step_start(&mut self, ks: &mut Keyspace) -> io::Result<RecoveryProgress> {
        // M2.5-S01: on the loop-resident tier a blocking dir-fsync here
        // can stall the reactor for minutes behind foreign journal
        // writeback (the captured cell-2 wedge) — ready must not wait on
        // the device; the handles become ledger-fronted driver barriers.
        let dirs = if self.defer_boot_sync {
            let (dirs, handles) = create_cell_dirs_deferred(self.fs(), &self.shard_dir)?;
            self.barrier_dirs = handles;
            dirs
        } else {
            create_cell_dirs(self.fs(), &self.shard_dir)?
        };
        debug_assert_eq!(dirs.log, self.log_dir);
        let manifest = read_manifest(self.fs(), &self.shard_dir)?;
        // ADR-0094 D6: the manifest names the secret that placed its
        // checkpoint's refs; a boot holding another one refuses here,
        // before the checkpoint loads. `infinityd` pre-scans every shard
        // before any cell starts; this is the guard every boot passes —
        // the simulators' and the embedded one's included.
        if let Some(m) = &manifest {
            let secret = ks.hasher().identity();
            if m.key_hash_id != secret {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cell {}: MANIFEST names key-hash id {} but the node's secret is {}: the \
                         secret was replaced after that checkpoint was placed (ADR-0094 D6) — \
                         every cold ref would be silently unreachable; restore the directory's \
                         original key-hash.toml (fail-stop)",
                        self.cell, m.key_hash_id, secret
                    ),
                ));
            }
        }

        // The manifest names the recovery unit; without one, the whole
        // retained log is the unit.
        self.floor = manifest.as_ref().map_or(SegmentId(0), Manifest::floor);
        let outcome =
            scan_log_dir_from(self.fs(), &self.log_dir, self.floor).map_err(io_invalid)?;
        let scan = outcome.scan;
        self.stale = outcome.stale;
        self.segments = scan.segments().to_vec();
        for &segment in &self.segments {
            let file = self
                .fs()
                .open_read(&self.log_dir.join(segment_file_name(segment)))
                .map_err(io_invalid)?;
            self.seg_sizes.push(file.file_size().map_err(io_invalid)?);
        }
        self.bytes_total = self.seg_sizes.iter().sum();

        let next = if let Some(manifest) = manifest {
            // The floor segment holds the begin marker and is never
            // truncated: its absence means the disk lost named state —
            // fail-stop.
            if scan.segments().first() != Some(&self.floor) {
                return Err(io_msg(format!(
                    "MANIFEST names ckpt {} with floor {} but the floor segment is missing \
                     (present: {:?})",
                    manifest.ckpt_id,
                    self.floor,
                    scan.segments()
                )));
            }
            if let (Some(last_listed), Some(tail)) = (manifest.segments.last(), scan.tail())
                && *last_listed > tail
            {
                return Err(io_msg(format!(
                    "MANIFEST lists segment {last_listed} but the log ends at {tail}"
                )));
            }
            // Tiered namespaces recover first (M4-S26; ADR-0057 D6
            // steps 1-2): map manifested files, seed the new life at the
            // manifested flushed watermark, and swap the recovered table
            // in — checkpoint entries and tail records then apply onto
            // the recovered life, never the fresh one.
            self.recover_tiers(ks, &manifest)?;
            let ick_path = self.ckpt_dir.join(ick_file_name(manifest.ckpt_id));
            // Presize from the footer counts before streaming (M2-S13):
            // the bulk apply must not pay a doubling-rehash storm.
            let counts = read_ick_counts(self.fs(), &ick_path, IckReaderConfig::default())
                .map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            for &(ns, entries) in &counts {
                ks.reserve_ns(inf_log::NsId(ns), entries);
            }
            let reader = IckReader::open(self.fs(), &ick_path, IckReaderConfig::default())
                .map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            let info = reader.info();
            if info.ckpt_id != manifest.ckpt_id
                || info.begin_lsn != manifest.begin_lsn
                || info.cell != self.cell
            {
                return Err(io_msg(format!(
                    "checkpoint header disagrees with MANIFEST: ick {{id {}, begin {}, cell \
                     {}}} vs manifest {{id {}, begin {}, cell {}}}",
                    info.ckpt_id,
                    info.begin_lsn,
                    info.cell,
                    manifest.ckpt_id,
                    manifest.begin_lsn,
                    self.cell
                )));
            }
            self.bytes_total += reader.file_size();
            self.begin_lsn = Some(manifest.begin_lsn);
            self.manifest = Some(manifest);
            Phase::Ick { reader: Box::new(reader) }
        } else if scan.is_empty() {
            // Fresh cell: nothing to replay, nothing to audit.
            let fs = self.fs.take().expect("fs present");
            let rotor = if self.defer_boot_sync {
                SegmentRotor::create_fresh_deferred(fs, self.log_dir.clone(), self.cfg.segment)
                    .map_err(io_invalid)?
            } else {
                SegmentRotor::create_fresh(fs, self.log_dir.clone(), self.cfg.segment)
                    .map_err(io_invalid)?
            };
            self.finished = Some((rotor, None));
            self.scan = Some(scan);
            self.phase = Phase::Complete;
            return Ok(RecoveryProgress::Complete);
        } else {
            Phase::Replay { idx: 0, reader: None }
        };
        self.scan = Some(scan);
        self.phase = next;
        Ok(RecoveryProgress::Working)
    }

    /// The end-of-replay checks (tiered state, sidecar commit), once.
    fn end_of_replay_checks(&mut self, ks: &mut Keyspace) -> io::Result<()> {
        if !self.tier_replay_checked {
            self.finish_tier_replay(ks)?;
            self.finish_index_replay(ks);
        }
        Ok(())
    }

    /// Recovers every manifested tiered namespace (M4-S26, ADR-0057
    /// D6): map + verify tier files, seed the new life at the
    /// manifested flushed watermark, install the recovered table, and
    /// retain creation-mode fds for the plane's cold-read table.
    fn recover_tiers(&mut self, ks: &mut Keyspace, manifest: &Manifest) -> io::Result<()> {
        for tier in &manifest.tiers {
            let ns = inf_log::NsId(tier.ns);
            let Some(spec) = ks.ns_get_by_id(ns).and_then(|spec| spec.tier) else {
                return Err(io_msg(format!(
                    "MANIFEST carries a tier section for ns {} the catalog does not know",
                    tier.ns
                )));
            };
            let demote = spec.demotion_config();
            let reserve_bytes = demote
                .ring_reserve_bytes()
                .ok_or_else(|| io_msg("tier spec ring reservation unrepresentable".into()))?;
            let recovered = inf_store::recover_tiered_ns(
                self.fs().clone(),
                tier,
                manifest.ckpt_id,
                inf_log::TierFlushConfig {
                    shard_dir: self.shard_dir.join(format!("ns-{}", tier.ns)),
                    cell: u32::from(self.cell),
                    ns,
                    mode: spec.tier_io_mode,
                    file_capacity: inf_log::flush::TIER_FILE_CAPACITY_DEFAULT,
                    slice_bytes: spec.maintain_slice_bytes,
                },
                inf_store::AddressSpaceConfig {
                    reserve_bytes,
                    page_bytes: inf_alloc::REGION_PAGE_BYTES,
                    life_origin: inf_store::LogicalAddr::ZERO, // overridden by the manifest
                },
                demote,
                1024,
                ks.hasher(),
            )?;
            ks.install_recovered_tiered(ns, recovered.table);
            let mut files = Vec::with_capacity(recovered.flush.sealed().len());
            for meta in recovered.flush.sealed() {
                let handle = self.fs().open_tier(&meta.path, spec.tier_io_mode)?;
                files.push((meta.id, handle));
            }
            self.recovered_tiers.push(RecoveredTierNs {
                ns,
                flush: recovered.flush,
                files,
                extents_listed: recovered.extents_listed,
                extents_quarantined: recovered.extents_quarantined,
            });
        }
        Ok(())
    }

    /// End-of-replay tiered checks (M4-S26), run once when replay hands
    /// to the audit: a non-empty displacement register means the log
    /// ended between a marker and its paired mutation — corrupt input
    /// by the ADR-0057 D4 same-frame rule (fail-stop, never a skip);
    /// then the extent orphan sweep seeds from the boot listing
    /// (ADR-0061 D6 — liveness is post-replay refcount truth).
    fn finish_tier_replay(&mut self, ks: &mut Keyspace) -> io::Result<()> {
        self.tier_replay_checked = true;
        if ks.displace_register_len() > 0 {
            return Err(io_msg(format!(
                "log ends with {} unpaired displacement marker(s) (ADR-0057 D4)",
                ks.displace_register_len()
            )));
        }
        for tier in &self.recovered_tiers {
            if let Some(table) = ks.tiered_store_mut(tier.ns) {
                let revive =
                    table.extent_sweep_seed(&tier.extents_listed, &tier.extents_quarantined);
                // ADR-0096 D3: a quarantined extent the replayed map
                // references was a wrong orphan verdict — rename it back
                // before the node serves, so reads never see the twin.
                // A rename I/O failure is a recovery fail-stop (§8.4);
                // "nothing to revive" is fine (already revived or gone —
                // reads answer the latter typed).
                for extent_id in revive {
                    let shard = self.shard_dir.join(format!("ns-{}", tier.ns.0));
                    inf_log::blob::revive_extent_file(
                        self.fs(),
                        &shard,
                        inf_log::ExtentId(extent_id),
                    )?;
                    self.stats.blob_revived += 1;
                }
                // M4.5-S37 (ADR-0093 D5/A4′): the shadow ticket set is a
                // projection of the finished index — rebuild it once the
                // checkpoint and WAL tail have replayed, before serving.
                // The rebuild is a cursor: the slots it cannot pair by
                // construction (two RAM keys with one hash beside a cold
                // twin) or beyond the ticket cap are handed back one at a
                // time, read and settled by their full key here — exactly
                // those, never the general index, never a list of them.
                // An unreadable or unsettleable slot is a recovery
                // fail-stop (corrupt input, ADR-0057's posture).
                let ns = tier.ns;
                let stats = &mut self.stats;
                table
                    .rebuild_shadow_tickets(|slot| -> io::Result<Vec<u8>> {
                        let addr = slot.cold.to_raw();
                        let read = |len: usize| -> io::Result<Vec<u8>> {
                            tier.flush.read_span_blocking(addr, len)?.ok_or_else(|| {
                                io_msg("lies in no catalogued tier range".to_owned())
                            })
                        };
                        let head = read(inf_store::TieredTable::RECORD_HEADER_LEN)?;
                        let len = inf_store::TieredTable::record_len_from_header(&head);
                        let image = read(len)?;
                        stats.shadow_settle_reads += 1;
                        Ok(image)
                    })
                    .map_err(|err| io_msg(format!("ns {}: {err} (ADR-0093 A4′)", ns.0)))?;
            }
        }
        Ok(())
    }

    /// End-of-replay sidecar commit (M4.5-S06, ADR-0078 D6), run once
    /// on entering the audit: loaded trees are tail-caught-up — mark
    /// them converged + cell `Ready`, disarm replay maintenance, and
    /// log every per-index rebuild-vs-load decision (the was-ready
    /// downgrade loudly — L10). No-checkpoint boots commit an empty
    /// loader: every declaration records `rebuilt (no-sidecar)`.
    fn finish_index_replay(&mut self, ks: &mut Keyspace) {
        #[cfg(feature = "doc")]
        if let Some(sidecar) = self.sidecar.take() {
            for row in sidecar.commit_ready(ks) {
                match row.decision {
                    inf_store::SidecarBootDecision::Loaded { entries } => eprintln!(
                        "cell {}: index {}/{} sidecar loaded ({entries} entries; tail caught up)",
                        self.cell, row.ns.0, row.id.0
                    ),
                    inf_store::SidecarBootDecision::Rebuilt { reason } => {
                        let downgrade =
                            if row.was_ready { " — was serving before the crash" } else { "" };
                        eprintln!(
                            "cell {}: index {}/{} rebuilding ({}){downgrade}",
                            self.cell,
                            row.ns.0,
                            row.id.0,
                            reason.name()
                        );
                    }
                }
            }
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = ks;
        }
    }

    /// Hands the recovered tiered plane halves to the caller (the plane
    /// installs them into its tier state at completion — M4-S26).
    pub(crate) fn take_recovered_tiers(&mut self) -> Vec<RecoveredTierNs<F>> {
        std::mem::take(&mut self.recovered_tiers)
    }

    fn step_ick(
        &mut self,
        ks: &mut Keyspace,
        mut reader: Box<IckReader<F::File>>,
        budget_bytes: u64,
    ) -> io::Result<RecoveryProgress> {
        let manifest = self.manifest.as_ref().expect("ick phase");
        let ick_path = self.ckpt_dir.join(ick_file_name(manifest.ckpt_id));
        // Tiered section routing (M4-S26; ADR-0057 D3/D6): the per-ns
        // manifested flushed watermark cross-checks every ref section.
        let tier_flushed: Vec<(u32, u64)> =
            manifest.tiers.iter().map(|t| (t.ns, t.flushed)).collect();
        let flushed_of = |ns: u32| tier_flushed.iter().find(|(id, _)| *id == ns).map(|(_, f)| *f);
        let (now, anchor) = (self.now, self.anchor);
        // One mutable keyspace, four exclusive callbacks: the reader
        // invokes exactly one at a time, which the borrow checker cannot
        // see — a RefCell makes the exclusivity dynamic (single-threaded,
        // non-reentrant by construction).
        let ks = std::cell::RefCell::new(ks);
        let mut spent = 0u64;
        loop {
            let step = reader
                .next_step_hybrid(
                    |record| {
                        ks.borrow_mut()
                            .apply_record(&record, now, anchor)
                            .map(|_| ())
                            .map_err(io_invalid)
                    },
                    |refs| {
                        let flushed = flushed_of(refs.ns).ok_or_else(|| {
                            io_msg(format!("ref section for unmanifested tier ns {}", refs.ns))
                        })?;
                        let mut ks = ks.borrow_mut();
                        let table =
                            ks.tiered_store_mut(inf_log::NsId(refs.ns)).ok_or_else(|| {
                                io_msg(format!("ref section for unknown tier ns {}", refs.ns))
                            })?;
                        inf_store::apply_ref_section(table, &refs, flushed)
                    },
                    |live| {
                        let mut ks = ks.borrow_mut();
                        let table =
                            ks.tiered_store_mut(inf_log::NsId(live.ns)).ok_or_else(|| {
                                io_msg(format!("live-set section for unknown ns {}", live.ns))
                            })?;
                        inf_store::apply_live_set_section(table, &live);
                        Ok(())
                    },
                    |blob| {
                        let mut ks = ks.borrow_mut();
                        let table =
                            ks.tiered_store_mut(inf_log::NsId(blob.ns)).ok_or_else(|| {
                                io_msg(format!("blob-ref section for unknown ns {}", blob.ns))
                            })?;
                        inf_store::apply_blob_ref_section(table, &blob);
                        Ok(())
                    },
                    // Index sidecars (M4.5-S06, ADR-0078 D4/D6): every
                    // verdict is a loader-state transition — body damage
                    // is counted and the read continues; a boot is never
                    // refused past the file's framing audit.
                    |step| {
                        #[cfg(feature = "doc")]
                        {
                            let sidecar =
                                self.sidecar.as_mut().expect("loader present until the audit");
                            match step {
                                inf_log::IckIdxSidecarStep::Section(section) => {
                                    let mut guard = ks.borrow_mut();
                                    sidecar.apply_section(&mut guard, &section);
                                }
                                inf_log::IckIdxSidecarStep::Damaged { at } => {
                                    eprintln!(
                                        "cell {}: damaged index-sidecar section at offset {at} — \
                                         unattributed; affected streams resolve as incomplete \
                                         (ADR-0078 D4)",
                                        self.cell
                                    );
                                    sidecar.note_damaged();
                                }
                            }
                            Ok(())
                        }
                        // A slim build refuses index-bearing catalogs at
                        // seed (ADR-0075 D2.5), so this arm is typed,
                        // never reached on a healthy node.
                        #[cfg(not(feature = "doc"))]
                        {
                            let at = match step {
                                inf_log::IckIdxSidecarStep::Section(ref s) => u64::from(s.ns),
                                inf_log::IckIdxSidecarStep::Damaged { at } => at,
                            };
                            Err(io_msg(format!(
                                "index-sidecar section (near {at}) but this build carries no \
                                 document support"
                            )))
                        }
                    },
                )
                .map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            match step {
                IckStep::Section { bytes } => {
                    spent += bytes;
                    self.ick_credited += bytes;
                    self.bytes_done += bytes;
                    self.bytes_consumed += bytes;
                    if spent >= budget_bytes {
                        self.phase = Phase::Ick { reader };
                        return Ok(RecoveryProgress::Working);
                    }
                }
                IckStep::Done(summary) => {
                    self.stats.ckpt_records = summary.records;
                    // Sidecar streams close with the checkpoint: open
                    // ones discard as incomplete, and `CatchUp` arms on
                    // every loaded namespace *before* the first tail
                    // record applies (ADR-0078 D6).
                    #[cfg(feature = "doc")]
                    if let Some(sidecar) = self.sidecar.as_mut() {
                        let mut guard = ks.borrow_mut();
                        sidecar.finish_load(&mut guard);
                    }
                    // Header + footer bytes complete the file's credit.
                    let rest = reader.file_size().saturating_sub(self.ick_credited);
                    self.bytes_done += rest;
                    self.bytes_consumed += rest;
                    self.stats.phases.ckpt_bytes = self.bytes_consumed;
                    self.phase = Phase::Replay { idx: 0, reader: None };
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    fn step_replay(
        &mut self,
        ks: &mut Keyspace,
        idx: usize,
        reader: Option<Box<SegmentReader<F::File>>>,
        budget_bytes: u64,
    ) -> io::Result<RecoveryProgress> {
        // ADR-0031 D5: replay already ended at an epoch-regressed frame —
        // every later segment is discarded-life residue. Record an empty
        // end (offset 0) so the audit scans it whole as slack evidence.
        if self.residue_stop {
            self.stats.segments += 1;
            self.ends.push((0, None));
            self.bytes_done += self.seg_sizes[idx];
            self.phase = Phase::Audit { idx };
            return Ok(RecoveryProgress::Working);
        }
        let mut reader = match reader {
            Some(reader) => reader,
            None => Box::new(
                SegmentReader::open(
                    self.fs(),
                    &self.log_dir,
                    self.segments[idx],
                    ReaderConfig::default(),
                )
                .map_err(io_invalid)?,
            ),
        };
        let start_offset = u64::from(reader.offset());
        // Replay this segment's frames, recording where its valid data
        // ends. A frame-level failure marks the data end at the failed
        // frame's base and replay continues with the next segment: whether
        // those bytes were a torn tail, inert residue, or interior
        // corruption is decided by the slack scans (Audit phase) — on
        // evidence, not on which code path noticed them (M2-S14,
        // ADR-0018). Store failures and read I/O errors fail-stop as-is.
        // v2 stamp continuity (ADR-0031 D3) is checked before any record
        // of the frame applies.
        loop {
            match reader.next_frame() {
                Ok(Some(frame)) => {
                    let frame_base =
                        frame.first_lsn().offset - u32::try_from(frame.header_len()).expect("40");
                    let verdict = self.classify_stamp(
                        frame.stamp(),
                        self.segments[idx],
                        frame_base,
                        frame.first_lsn(),
                    )?;
                    match verdict {
                        StampVerdict::Replay => {}
                        StampVerdict::ResidueStop => {
                            // The frame validates but belongs to a
                            // discarded life: end this segment's data here
                            // and stop replay — the audit scans the rest.
                            self.residue_stop = true;
                            self.stats.epoch_residue_stops += 1;
                            self.stats.segments += 1;
                            self.ends.push((frame_base, None));
                            let consumed = u64::from(frame_base).saturating_sub(start_offset);
                            self.bytes_done += consumed;
                            self.bytes_consumed += consumed;
                            self.bytes_done +=
                                self.seg_sizes[idx].saturating_sub(u64::from(frame_base));
                            self.phase = Phase::Audit { idx };
                            return Ok(RecoveryProgress::Working);
                        }
                    }
                    self.stats.frames += 1;
                    let at = frame.first_lsn();
                    for record in frame.records() {
                        let (lsn, record) = record
                            .map_err(io_invalid)
                            .map_err(|error| replay_apply_failed(at, &error))?;
                        // Floor-segment records below begin are superseded
                        // by the checkpoint (module docs) — only the floor
                        // segment can hold any (earlier segments were
                        // truncated or are stale, never replayed).
                        if self.begin_lsn.is_some_and(|begin| lsn < begin) {
                            self.stats.records_pre_begin += 1;
                            continue;
                        }
                        let outcome = ks
                            .apply_record(&record, self.now, self.anchor)
                            .map_err(io_invalid)
                            .map_err(|error| replay_apply_failed(at, &error))?;
                        match outcome {
                            ReplayOutcome::Applied => self.stats.records_applied += 1,
                            ReplayOutcome::SkippedUnknownNs | ReplayOutcome::SkippedReserved => {
                                self.stats.records_skipped += 1;
                            }
                            ReplayOutcome::SkippedMarker => self.stats.markers += 1,
                            ReplayOutcome::SkippedDocDeltaStale => {
                                self.stats.doc_deltas_skipped_stale += 1;
                            }
                            ReplayOutcome::SkippedDocDeltaMissing => {
                                self.stats.doc_deltas_skipped_missing += 1;
                            }
                        }
                    }
                    let consumed = u64::from(reader.offset()) - start_offset;
                    if consumed >= budget_bytes {
                        self.bytes_done += consumed;
                        self.bytes_consumed += consumed;
                        self.phase = Phase::Replay { idx, reader: Some(reader) };
                        return Ok(RecoveryProgress::Working);
                    }
                }
                Ok(None) => {
                    let end = reader.read_end().expect("clean exhaustion records an end");
                    // Continuity never spans a segment boundary (ADR-0031
                    // D3): a torn rotation-tail frame leaves the same
                    // clean-end shape as ordinary prealloc slack, so a seq
                    // step across segments is legal physics, not
                    // malformation. Epoch monotonicity and the
                    // cross-segment attestation check still span.
                    self.prev_stamp = None;
                    self.stats.segments += 1;
                    self.ends.push((end.at(), None));
                    // Credit the consumed bytes plus the segment's slack
                    // (progress only — slack is skipped, not read).
                    let consumed = u64::from(reader.offset()) - start_offset;
                    self.bytes_done += consumed;
                    self.bytes_consumed += consumed;
                    self.bytes_done +=
                        self.seg_sizes[idx].saturating_sub(u64::from(reader.offset()));
                    self.phase = Phase::Audit { idx };
                    return Ok(RecoveryProgress::Working);
                }
                Err(err @ ReadError::Io { .. }) => return Err(io_invalid(err)),
                Err(ReadError::ForeignSegment { offset, .. }) => {
                    // ADR-0090 D2 as amended: a decoded frame at its own
                    // offset stamped for another segment id is recycled-
                    // life residue — this segment's data ends here, as
                    // cleanly as at a zero tail. Replay goes on in the
                    // next segment (a foreign frame says nothing about
                    // the segments after it); the audit proves the slack.
                    self.prev_stamp = None;
                    self.stats.segments += 1;
                    self.stats.segment_residue_stops += 1;
                    self.ends.push((offset, None));
                    let consumed = u64::from(offset).saturating_sub(start_offset);
                    self.bytes_done += consumed;
                    self.bytes_consumed += consumed;
                    self.bytes_done += self.seg_sizes[idx].saturating_sub(u64::from(offset));
                    self.phase = Phase::Audit { idx };
                    return Ok(RecoveryProgress::Working);
                }
                Err(err) => {
                    let offset = match &err {
                        ReadError::Frame { offset, .. } | ReadError::LsnMismatch { offset, .. } => {
                            *offset
                        }
                        ReadError::Io { .. } | ReadError::ForeignSegment { .. } => {
                            unreachable!("Io and foreign-segment reads are handled above")
                        }
                    };
                    // The contiguity run breaks here (ADR-0031 D3): frames
                    // in later segments are a fresh baseline — the audit
                    // owns everything at and beyond this break. Epoch
                    // monotonicity still spans breaks (`max_prefix_epoch`).
                    self.prev_stamp = None;
                    self.stats.segments += 1;
                    self.ends.push((offset, Some(err)));
                    let consumed = u64::from(reader.offset()) - start_offset;
                    self.bytes_done += consumed;
                    self.bytes_consumed += consumed;
                    self.bytes_done += self.seg_sizes[idx].saturating_sub(u64::from(offset));
                    self.phase = Phase::Audit { idx };
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    /// M2.5-S12 (ADR-0031 D3): pre-apply continuity verdict for one
    /// validating frame's stamp against the prefix state. `Replay` admits
    /// the frame; `ResidueStop` ends replay at a discarded-life frame;
    /// malformed continuity between byte-adjacent validating frames is a
    /// named fail-stop (no honest writer emits it).
    fn classify_stamp(
        &mut self,
        stamp: Option<FrameStamp>,
        segment: SegmentId,
        frame_base: u32,
        first_lsn: Lsn,
    ) -> io::Result<StampVerdict> {
        let corruption = |detail: String| {
            io_msg(
                LogCorruption {
                    segment,
                    offset: frame_base,
                    evidence_segment: segment,
                    evidence_offset: frame_base,
                    detail,
                }
                .to_string(),
            )
        };
        let Some(stamp) = stamp else {
            if self.saw_v2 {
                return Err(corruption(
                    "a format-v1 frame follows a v2 frame in the replay prefix — append order \
                     makes this unreachable by any honest writer (ADR-0031 D2)"
                        .to_owned(),
                ));
            }
            return Ok(StampVerdict::Replay);
        };
        debug_assert!(
            stamp.covered_lsn <= first_lsn.to_u64(),
            "decode_frame admits covered_lsn <= first_lsn only (BadStamp)"
        );
        // Epochs only grow in append order, across segment rotations and
        // replay breaks alike: a lower-epoch frame anywhere later is
        // discarded-life residue (ADR-0031 D5).
        if stamp.epoch < self.max_prefix_epoch {
            return Ok(StampVerdict::ResidueStop);
        }
        // Cross-segment attestation (ADR-0031 D4): coverage is a prefix,
        // so a claim reaching into an earlier segment must lie within
        // that segment's surviving data — anything past it is proof the
        // disk lost covered bytes (a lie format v1 could not even see).
        // Claims below the manifest floor are pre-checkpoint history.
        let covered = Lsn::from_u64(stamp.covered_lsn);
        if covered.segment < segment
            && let Ok(idx) = self.segments.binary_search(&covered.segment)
            && covered.offset > self.ends[idx].0
        {
            return Err(corruption(format!(
                "frame attests fsync coverage up to {}, past segment {}'s surviving data end \
                 {:#x} — covered data was lost (ADR-0031 D4)",
                covered, covered.segment, self.ends[idx].0
            )));
        }
        if let Some(prev) = self.prev_stamp {
            if stamp.epoch == prev.epoch {
                if stamp.seq != prev.seq + 1 {
                    return Err(corruption(format!(
                        "frame seq {} follows seq {} within epoch {} — adjacent validating \
                         frames must be seq-contiguous (ADR-0031 D3)",
                        stamp.seq, prev.seq, stamp.epoch
                    )));
                }
                if stamp.covered_lsn < prev.covered_lsn {
                    return Err(corruption(format!(
                        "frame attestation regressed ({:#x} after {:#x}) within epoch {} — \
                         the watermark is monotone (ADR-0031 D3)",
                        stamp.covered_lsn, prev.covered_lsn, stamp.epoch
                    )));
                }
            } else if stamp.seq != 1 {
                return Err(corruption(format!(
                    "epoch stepped {} → {} but seq is {} — a new life stamps seq 1 \
                     (ADR-0031 D3)",
                    prev.epoch, stamp.epoch, stamp.seq
                )));
            }
        }
        self.saw_v2 = true;
        self.max_prefix_epoch = self.max_prefix_epoch.max(stamp.epoch);
        self.prev_stamp = Some(stamp);
        Ok(StampVerdict::Replay)
    }

    /// Slack audit of segment `idx`, immediately after its replay
    /// (ADR-0087 D6 — the order is the point: a later segment is never
    /// applied to the keyspace before this segment's slack is known
    /// clean). `scan_region_evidence` aggregates the v2 stamp facts in
    /// O(1) memory. A validating frame marks the hole: every later segment
    /// is probed for evidence instead of replayed, and the Finish phase
    /// decides refuse-or-truncate at the hole. Non-validating residue
    /// classifies by position there as before: torn final write at the
    /// resume point, tolerated inert remnants behind it.
    fn step_audit(&mut self, idx: usize) -> io::Result<RecoveryProgress> {
        let segment = self.segments[idx];
        let (end, failed) = (self.ends[idx].0, self.ends[idx].1.is_some());
        let evidence =
            scan_region_evidence(self.fs(), &self.log_dir, segment, end, ReaderConfig::default())?;
        self.note_audit(&evidence);
        let residue = match evidence.summary() {
            RegionScan::ValidFrame { .. } | RegionScan::Garbage { .. } => true,
            RegionScan::AllZero => failed,
        };
        // ADR-0031 D5 as amended (2026-08-21): validating frames whose
        // epochs all sit below the replayed prefix's are discarded-life
        // residue — a recovery already resumed past them and a later life
        // wrote the prefix above them — never a hole of this life. (A hole
        // of the current life carries its own epoch; a lying device that
        // lost covered bytes of this life leaves this life's frames, or
        // attestations in later segments the cross-segment check refuses.)
        // The residue-stop that may have ended this segment's replay at
        // such a frame is lifted with it: the segments after it are the
        // live log, not residue.
        let stale_by_epoch = evidence.valid_frames > 0
            && !evidence.any_v1
            && evidence.max_epoch < self.max_prefix_epoch;
        if stale_by_epoch {
            self.stats.stale_residue_slacks += 1;
            self.residue_stop = false;
        }
        // ADR-0090 D2 as amended: a slack with no self-located frame and
        // at least one foreign-segment frame is recycled-life residue —
        // proven by the frames' own stamps, never a hole (there is no
        // frame of this life to be a hole *to*), never torn. Same
        // disposition as the epoch-proven class: excluded from the
        // attestation decision, trailing ones removed as stale.
        let recycled = evidence.is_recycled_residue();
        if recycled {
            self.stats.recycled_residue_slacks += 1;
        }
        // Proven residue is treated exactly like pre-zeroed slack for the
        // torn/trailing decisions: an empty recycled next segment stays
        // the tail (resume at 0, write-through from the first frame —
        // the file is fully allocated), a recycled tail resumes at its
        // data end, and nothing is reported torn. The reader's
        // foreign-segment stop makes the bytes behind the data end
        // unreachable on every later boot, which is the property zeros
        // gave (ADR-0090 D2/D3 as amended).
        let residue = residue && !recycled;
        self.residue.push(residue);
        self.stale_slack.push(stale_by_epoch);
        self.evidence.push(evidence);
        let next = idx + 1;
        self.phase = if evidence.valid_frames > 0 && !stale_by_epoch {
            self.hole = Some(idx);
            if next == self.segments.len() { Phase::Finish } else { Phase::Probe { idx: next } }
        } else if next == self.segments.len() {
            Phase::Finish
        } else {
            Phase::Replay { idx: next, reader: None }
        };
        Ok(RecoveryProgress::Working)
    }

    /// Evidence-only scan of segment `idx`, behind a hole (ADR-0087 D6):
    /// the whole file is residue by construction (nothing at or past the
    /// hole was acked), so it is never replayed — scanned for the stamp
    /// facts the Finish decision needs, then removed there. Records an
    /// empty data end and credits the segment's bytes to progress.
    fn step_probe(&mut self, idx: usize) -> io::Result<RecoveryProgress> {
        debug_assert!(self.hole.is_some_and(|hole| hole < idx), "probe only behind a hole");
        let segment = self.segments[idx];
        let evidence =
            scan_region_evidence(self.fs(), &self.log_dir, segment, 0, ReaderConfig::default())?;
        self.note_audit(&evidence);
        self.stats.segments += 1;
        self.ends.push((0, None));
        self.residue.push(!matches!(evidence.summary(), RegionScan::AllZero));
        self.stale_slack.push(false);
        self.evidence.push(evidence);
        self.bytes_done += self.seg_sizes[idx];
        let next = idx + 1;
        self.phase =
            if next == self.segments.len() { Phase::Finish } else { Phase::Probe { idx: next } };
        Ok(RecoveryProgress::Working)
    }

    fn step_finish(&mut self) -> io::Result<RecoveryProgress> {
        debug_assert_eq!(self.ends.len(), self.segments.len(), "every segment has an end");
        debug_assert_eq!(self.evidence.len(), self.segments.len(), "every segment was audited");
        // ADR-0031 D5 as amended (2026-08-21), the non-local half: a hole
        // whose slack frames all carry an epoch below one probed *beyond*
        // it is discarded-life residue the later life resumed past (the
        // prefix and the residue share a life, so the audit could not tell
        // locally; the later segments can). The probed segments are the
        // live log: drop their probe bookkeeping and replay them — nothing
        // of theirs was applied, so the replay order ADR-0087 D6 fixes is
        // kept (this segment's slack is now known to be residue).
        if let Some(hole) = self.hole {
            let later_max_epoch =
                self.evidence[hole + 1..].iter().map(|e| e.max_epoch).max().unwrap_or(0);
            let at_hole = &self.evidence[hole];
            if !at_hole.any_v1 && at_hole.max_epoch < later_max_epoch {
                self.stats.stale_residue_slacks += 1;
                self.stale_slack[hole] = true;
                self.hole = None;
                self.residue_stop = false;
                for idx in hole + 1..self.segments.len() {
                    self.stats.segments -= 1;
                    self.bytes_done -= self.seg_sizes[idx];
                }
                self.ends.truncate(hole + 1);
                self.residue.truncate(hole + 1);
                self.stale_slack.truncate(hole + 1);
                self.evidence.truncate(hole + 1);
                self.phase = Phase::Replay { idx: hole + 1, reader: None };
                return Ok(RecoveryProgress::Working);
            }
        }
        let ends = &self.ends;
        // The resume segment (ADR-0087 D6): the hole's, when it holds
        // data; else the last data-bearing segment before it (a hole at
        // offset 0 keeps nothing of its own — the previous data end is
        // the resume point and the segment is removed like any trailing
        // one). With no hole: the last data-bearing segment.
        let last_with_data = |upto: usize| (0..upto).rev().find(|&i| ends[i].0 > 0).unwrap_or(0);
        let last_data = match self.hole {
            Some(hole) if ends[hole].0 > 0 => hole,
            Some(hole) => last_with_data(hole),
            None => last_with_data(ends.len()),
        };
        self.last_data = last_data;
        // Residue beyond the resume point: torn (this life's, or
        // garbage) or stale (a discarded life's validating frames — a
        // remnant, removed with the trailing segments but never a
        // truncation of this life's data).
        let torn = (last_data..self.residue.len()).any(|i| self.residue[i] && !self.stale_slack[i]);
        let stale_trailing =
            (last_data..self.residue.len()).any(|i| self.residue[i] && self.stale_slack[i]);
        self.stats.sealed_slack_remnants =
            self.residue[..last_data].iter().filter(|&&r| r).count() as u64;
        // Evidence from the resume segment's slack and everything after
        // it — the frames a truncation at the resume point would discard.
        // Stale residue attests a discarded life's watermark, not this
        // one's: excluded.
        let mut resume_evidence = RegionEvidence::default();
        let mut resume_evidence_anchor = None;
        for (i, evidence) in self.evidence.iter().enumerate().skip(last_data) {
            if self.stale_slack[i] {
                continue;
            }
            if resume_evidence_anchor.is_none()
                && let Some(offset) = evidence.first_valid
            {
                resume_evidence_anchor = Some((self.segments[i], offset));
            }
            resume_evidence.absorb(evidence);
        }

        let scan = self.scan.as_ref().expect("scan set in start");
        // The resume point: the last data-bearing segment's data end
        // whenever anything is truncated or removed behind it (torn, or
        // stale trailing residue — every later segment goes, so the tail
        // *is* that segment); otherwise the scan's tail at its own end (a
        // clean empty next segment resumes at 0 — pinned in
        // `recover_stamp.rs`: reading the removed segment's end here once
        // reopened the live segment at offset 0).
        let (resume_segment, resume_offset) = if torn || stale_trailing {
            (self.segments[last_data], ends[last_data].0)
        } else {
            (scan.tail().expect("non-empty scan"), ends.last().expect("non-empty scan").0)
        };
        let resume = Lsn::new(resume_segment, resume_offset);
        // ADR-0031 D4: validating frames beyond the data end are refused
        // when they cannot be proven un-covered — a v1 frame attests
        // nothing, and a surviving attestation at or past the data end is
        // proof the gap sat in covered territory. Otherwise they are the
        // legal remainder of a reorder hole in the un-covered suffix and
        // truncate with the torn tail (the retired ADR-0021 D3 refusal).
        if resume_evidence.valid_frames > 0 {
            let (evidence_segment, evidence_offset) =
                resume_evidence_anchor.expect("evidence anchor set with a valid frame");
            let corruption = |detail: String| {
                io_msg(
                    LogCorruption {
                        segment: resume_segment,
                        offset: resume_offset,
                        evidence_segment,
                        evidence_offset,
                        detail,
                    }
                    .to_string(),
                )
            };
            if resume_evidence.any_v1 {
                return Err(corruption(
                    "a format-v1 frame beyond the data end cannot attest whether the gap \
                     before it was fsync-covered (ADR-0031 D4)"
                        .to_owned(),
                ));
            }
            if resume_evidence.max_covered_lsn > resume.to_u64() {
                return Err(corruption(format!(
                    "a surviving frame attests fsync coverage up to {:#x}, past the valid \
                     data end {} — covered data was lost (ADR-0031 D4)",
                    resume_evidence.max_covered_lsn, resume
                )));
            }
            debug_assert!(torn, "beyond-frame evidence implies resume-region residue");
            self.stats.beyond_frames_discarded = resume_evidence.valid_frames;
        }
        // Everything the manifest names was durable at publication (the
        // watermark-≥-begin staging guard): a log ending below begin is
        // lost covered state, never a torn un-synced tail.
        if let Some(begin) = self.begin_lsn
            && resume < begin
        {
            return Err(io_msg(format!(
                "the log's valid data ends at {resume}, below the MANIFEST begin-LSN {begin}: \
                 fsync-covered bytes are missing — refusing to start (§8.4)"
            )));
        }
        if torn {
            self.stats.torn_truncated_at = Some(resume);
        }
        if torn || stale_trailing {
            // Every later segment is un-acked residue (frame-free, probed
            // and proven un-covered above, or a discarded life's) — remove
            // them so appends resume in the truncated segment with the
            // pristine-prealloc invariant restored.
            for &trailing in &self.segments[last_data + 1..] {
                self.fs().remove_file(&self.log_dir.join(segment_file_name(trailing)))?;
                self.stats.torn_segments_removed += 1;
            }
            if self.stats.torn_segments_removed > 0 {
                self.scan = Some(
                    scan_log_dir_from(self.fs(), &self.log_dir, self.floor)
                        .map_err(io_invalid)?
                        .scan,
                );
            }
        }
        let tail_offset = resume_offset;

        // Boot GC: stale below-floor segments (crash mid-truncation) and
        // checkpoint-dir orphans (unnamed `.ick` from a crash before the
        // MANIFEST swap; `.ick.new` from a crashed walk). All are garbage
        // the named recovery unit fully covers; removal is safe in any
        // order and needs no dir-fsync (resurrection re-collects next
        // boot).
        for i in 0..self.stale.len() {
            let stale = self.stale[i];
            self.fs().remove_file(&self.log_dir.join(segment_file_name(stale)))?;
            self.stats.stale_files_removed += 1;
        }
        let named = self.manifest.as_ref().map(|m| m.ckpt_id);
        for name in self.fs().list_dir(&self.ckpt_dir)? {
            let stale_ick = parse_ick_file_name(&name).is_some_and(|id| Some(id) != named);
            if stale_ick || name.ends_with(".ick.new") {
                self.fs().remove_file(&self.ckpt_dir.join(name))?;
                self.stats.stale_files_removed += 1;
            }
        }

        let fs = self.fs.take().expect("fs present");
        let scan = self.scan.as_ref().expect("scan set");
        let mut rotor = SegmentRotor::open_existing(
            fs,
            self.log_dir.clone(),
            self.cfg.segment,
            scan,
            tail_offset,
        )
        .map_err(io_invalid)?;
        // ADR-0031 D5: the resumed life's epoch tops every epoch observed
        // this boot — the valid prefix and every validating beyond-frame
        // (anything capable of resurrecting was durably whole, so the
        // audit saw it). Discarded-life residue that later resurfaces at
        // the new data end then fails the prefix's epoch-monotonicity rule
        // instead of replaying.
        // ADR-0090 D2 as amended: foreign-segment frames carry epochs at or
        // below the prefix's by append order (a recycled file was sealed
        // before every segment above the floor); folding them in makes the
        // "every epoch observed this boot" rule literal at the cost of one
        // max over the audited segments.
        let max_foreign_epoch =
            self.evidence.iter().map(|e| e.max_foreign_epoch).max().unwrap_or(0);
        let observed = self.max_prefix_epoch.max(resume_evidence.max_epoch).max(max_foreign_epoch);
        let epoch = observed.checked_add(1).ok_or_else(|| {
            io_msg(format!("log epoch space exhausted at {observed} — refusing to start"))
        })?;
        rotor.set_resume_epoch(epoch);
        let seed = self
            .manifest
            .as_ref()
            .map(|m| RecoveredManifest { ckpt_id: m.ckpt_id, begin_lsn: m.begin_lsn });
        self.finished = Some((rotor, seed));
        self.phase = Phase::Complete;
        let phases = &mut self.stats.phases;
        phases.replay_bytes = self.bytes_consumed.saturating_sub(phases.ckpt_bytes);
        phases.replay_frames = self.stats.frames;
        Ok(RecoveryProgress::Complete)
    }
}

/// The `open_cell_log` replay-failure message (kept byte-compatible with
/// the pre-S15 `ApplyError::Apply` surface the S13/S14 suites pin).
fn replay_apply_failed(at: Lsn, error: &io::Error) -> io::Error {
    io_msg(format!("replay apply failed at {at}: {error}"))
}

fn io_invalid(err: impl std::fmt::Debug) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{err:?}"))
}

fn io_msg(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
