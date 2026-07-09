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
//! End-of-log policy (M2-S14, ADR-0018): after replay, every segment's
//! slack — the bytes beyond its last valid frame — is scanned. A
//! validating, self-located frame there means interior data would be
//! silently lost (a gap the replay skipped, or a survivor a future append
//! could resurrect out of order): fail-stop with a named
//! [`inf_log::LogCorruption`]. In the resume region (the last data-bearing
//! segment and everything after it — where future appends flow), torn
//! remnants or a failed final frame classify as a torn tail: the tail
//! *pointer* is
//! truncated to the last valid frame (bytes are never rewritten), trailing
//! segments — verified frame-free — are removed to restore the pristine
//! prealloc invariant, and the cell continues. In sealed slack behind the
//! resume point, non-validating remnants are the inert residue of an
//! earlier torn-tail resume: tolerated and counted, never fatal. A torn
//! tail may never truncate below the manifest's `begin-LSN`: everything
//! the manifest names was durable at publication, so a shorter log is
//! disk lying.
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
    LogCorruption, Lsn, Manifest, ReadError, ReaderConfig, RegionScan, SegmentId, SegmentReader,
    SegmentRotor, SegmentScan, create_cell_dirs, create_cell_dirs_deferred, read_manifest,
    scan_log_dir_from, scan_region, segment_file_name,
};
use inf_store::{Keyspace, ReplayOutcome, WallAnchor};

use crate::durable::DurableConfig;

/// What one cell's recovery did (log lines + `INFO persistence` inputs).
#[derive(Copy, Clone, Debug, Default)]
pub struct RecoverStats {
    pub segments: u64,
    pub frames: u64,
    pub records_applied: u64,
    pub records_skipped: u64,
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
    /// Sealed segments with non-validating remnant bytes behind their data
    /// end — the inert residue of an earlier torn-tail resume (the reader
    /// never crosses a segment's data end, so they can never replay).
    /// Tolerated, but counted: never silent (§8.4).
    pub sealed_slack_remnants: u64,
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
pub fn open_cell_log<F: SegmentFs>(
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
    /// M2-S14 slack audit, one segment per step.
    Audit {
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
    finished: Option<(SegmentRotor<F>, Option<RecoveredManifest>)>,
}

impl<F: SegmentFs> Recovery<F> {
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
            bytes_total: 0,
            bytes_done: 0,
            bytes_consumed: 0,
            defer_boot_sync: false,
            barrier_dirs: Vec::new(),
            ick_credited: 0,
            finished: None,
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
        match &self.phase {
            Phase::Start => 1,
            Phase::Ick { .. } => 2,
            Phase::Replay { .. } => 3,
            Phase::Audit { .. } => 4,
            Phase::Finish => 5,
            Phase::Complete => 6,
        }
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
            Phase::Replay { idx, .. } => *idx as u64,
            Phase::Audit { .. } | Phase::Finish | Phase::Complete => self.segments.len() as u64,
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
            Phase::Finish => self.step_finish(),
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

    fn step_ick(
        &mut self,
        ks: &mut Keyspace,
        mut reader: Box<IckReader<F::File>>,
        budget_bytes: u64,
    ) -> io::Result<RecoveryProgress> {
        let ick_path =
            self.ckpt_dir.join(ick_file_name(self.manifest.as_ref().expect("ick phase").ckpt_id));
        let mut spent = 0u64;
        loop {
            let step = reader
                .next_step(|record| {
                    ks.apply_record(&record, self.now, self.anchor).map(|_| ()).map_err(io_invalid)
                })
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
                    // Header + footer bytes complete the file's credit.
                    let rest = reader.file_size().saturating_sub(self.ick_credited);
                    self.bytes_done += rest;
                    self.bytes_consumed += rest;
                    self.phase = Phase::Replay { idx: 0, reader: None };
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    fn step_replay(
        &mut self,
        ks: &mut Keyspace,
        mut idx: usize,
        reader: Option<Box<SegmentReader<F::File>>>,
        budget_bytes: u64,
    ) -> io::Result<RecoveryProgress> {
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
        loop {
            match reader.next_frame() {
                Ok(Some(frame)) => {
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
                    self.stats.segments += 1;
                    self.ends.push((end.at(), None));
                    // Credit the consumed bytes plus the segment's slack
                    // (progress only — slack is skipped, not read).
                    let consumed = u64::from(reader.offset()) - start_offset;
                    self.bytes_done += consumed;
                    self.bytes_consumed += consumed;
                    self.bytes_done +=
                        self.seg_sizes[idx].saturating_sub(u64::from(reader.offset()));
                    idx += 1;
                    self.phase = if idx == self.segments.len() {
                        Phase::Audit { idx: 0 }
                    } else {
                        Phase::Replay { idx, reader: None }
                    };
                    return Ok(RecoveryProgress::Working);
                }
                Err(err @ ReadError::Io { .. }) => return Err(io_invalid(err)),
                Err(err) => {
                    let offset = match &err {
                        ReadError::Frame { offset, .. } | ReadError::LsnMismatch { offset, .. } => {
                            *offset
                        }
                        ReadError::Io { .. } => unreachable!("Io read errors fail-stop above"),
                    };
                    self.stats.segments += 1;
                    self.ends.push((offset, Some(err)));
                    let consumed = u64::from(reader.offset()) - start_offset;
                    self.bytes_done += consumed;
                    self.bytes_consumed += consumed;
                    self.bytes_done += self.seg_sizes[idx].saturating_sub(u64::from(offset));
                    idx += 1;
                    self.phase = if idx == self.segments.len() {
                        Phase::Audit { idx: 0 }
                    } else {
                        Phase::Replay { idx, reader: None }
                    };
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    fn step_audit(&mut self, idx: usize) -> io::Result<RecoveryProgress> {
        // M2-S14 slack scan: beyond each segment's data end, a validating
        // self-located frame is unreachable interior data — fail-stop.
        // Every other residue classifies by position (Finish phase): at or
        // after the last data-bearing segment it is a torn final write
        // (truncate the tail pointer, never bytes); behind it, the inert
        // residue of an earlier torn-tail resume (the reader never crosses
        // a segment's data end, so it can never replay) — tolerated and
        // counted, never fatal: treating it as corruption would poison
        // every boot after a benign torn tail seals.
        let segment = self.segments[idx];
        let (end, failed) = &self.ends[idx];
        match scan_region(self.fs(), &self.log_dir, segment, *end, ReaderConfig::default())? {
            RegionScan::ValidFrame { offset } => {
                return Err(io_msg(
                    LogCorruption {
                        segment,
                        offset: *end,
                        evidence_segment: segment,
                        evidence_offset: offset,
                        detail: failed.as_ref().map_or_else(
                            || "a dropped-write gap or torn remnants precede it".to_owned(),
                            ToString::to_string,
                        ),
                    }
                    .to_string(),
                ));
            }
            RegionScan::Garbage { .. } => self.residue.push(true),
            RegionScan::AllZero => self.residue.push(failed.is_some()),
        }
        self.phase = if idx + 1 == self.segments.len() {
            Phase::Finish
        } else {
            Phase::Audit { idx: idx + 1 }
        };
        Ok(RecoveryProgress::Working)
    }

    fn step_finish(&mut self) -> io::Result<RecoveryProgress> {
        let ends = &self.ends;
        let last_data = (0..ends.len()).rev().find(|&i| ends[i].0 > 0).unwrap_or(0);
        let torn = self.residue[last_data..].iter().any(|&r| r);
        self.stats.sealed_slack_remnants =
            self.residue[..last_data].iter().filter(|&&r| r).count() as u64;

        let scan = self.scan.as_ref().expect("scan set in start");
        let (resume_segment, resume_offset) = if torn {
            (self.segments[last_data], ends[last_data].0)
        } else {
            (scan.tail().expect("non-empty scan"), ends.last().expect("non-empty scan").0)
        };
        let resume = Lsn::new(resume_segment, resume_offset);
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
            // Trailing segments hold no validating frame (the audit) —
            // remove them so appends resume in the truncated segment with
            // the pristine-prealloc invariant restored.
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
        let rotor = SegmentRotor::open_existing(
            fs,
            self.log_dir.clone(),
            self.cfg.segment,
            scan,
            tail_offset,
        )
        .map_err(io_invalid)?;
        let seed = self
            .manifest
            .as_ref()
            .map(|m| RecoveredManifest { ckpt_id: m.ckpt_id, begin_lsn: m.begin_lsn });
        self.finished = Some((rotor, seed));
        self.phase = Phase::Complete;
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
