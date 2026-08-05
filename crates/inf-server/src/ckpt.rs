//! Per-cell fuzzy-checkpoint driver state (M2-S10, ADR-0016): the phase
//! machine `DurableCell::ckpt_slice` advances in bounded MAINTAIN slices
//! under the `GroupClass::Checkpoint` deficit. The owning cell walks its
//! own stores (L1) with the resize-stable post-image cursor, streams
//! sections through the S05 driver ops (never a blocking data write on the
//! loop — ADR-0016 D4), and publishes `ckpt-N.ick.new → ckpt-N.ick` with
//! rename + dir-fsync once the completion fdatasync lands.
//!
//! Failure policy: a checkpoint I/O error **aborts the checkpoint**, never
//! the process — nothing was acked against it and the previous checkpoint
//! (and the whole log) stay valid; the milestone risk table's
//! "checkpoints abort cleanly" rule. This is deliberately narrower than
//! the log path's §8.4 fail-stop.

use std::io;
use std::os::fd::RawFd;
use std::path::PathBuf;

use inf_log::ckpt::{ick_file_name, ick_staging_file_name, parse_ick_file_name};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{CkptConfig, IckStream, Lsn, Manifest, SectionLease, SegmentId, StagedAt};
use inf_runtime::{CompletionToken, TokenClass};

/// Entries pulled from the store per walk call: keeps the byte-budget
/// overshoot bounded by one call's emissions (the staging buffer absorbs
/// it — `IckStream` docs).
pub(crate) const SCAN_CHUNK_ENTRIES: usize = 32;

/// Where a checkpoint stands. One checkpoint in flight per cell, ever;
/// triggers during a walk latch `requested`, they never stack (ADR-0016
/// D7).
pub(crate) enum CkptPhase<File: SegmentFile> {
    Idle,
    /// The `ckpt-begin` marker is staged; its LSN resolves when LOG seals
    /// the covering frame (`DurableCell::seal_log`).
    AwaitBeginLsn {
        id: u64,
        at: StagedAt,
    },
    /// Begin LSN known; the next slice creates the `.ick.new` file.
    Begun {
        id: u64,
        begin_lsn: Lsn,
    },
    Stream(Box<Streaming<File>>),
}

/// Active streaming state. Boxed: it exists only while a checkpoint runs,
/// and moving the box never moves the `IckStream` buffers' heap storage
/// (the `StableBytes` custody argument — `log_bytes.rs`).
pub(crate) struct Streaming<File: SegmentFile> {
    pub id: u64,
    pub begin_lsn: Lsn,
    /// Pacing base (injected clock): the walk may have streamed at most
    /// `stream_bytes_per_sec × elapsed` by any instant (ADR-0017).
    pub opened_at: inf_foundation::time::Nanos,
    /// Record bytes streamed so far (the pacing numerator).
    pub streamed_bytes: u64,
    pub stream: IckStream,
    /// Keeps the fd alive for the driver ops; dropped at publish/abort
    /// (never read — its job is ownership).
    #[allow(dead_code)]
    pub file: File,
    pub fd: RawFd,
    /// Durable namespaces captured at stream open, ascending id. Namespaces
    /// created later have every record above `begin_lsn` — tail-covered.
    pub ns_ids: Vec<u32>,
    pub ns_idx: usize,
    pub cursor: u64,
    pub walk_done: bool,
    pub footer_staged: bool,
    pub sync_issued: bool,
    pub sync_done: bool,
    /// The queued driver write's lease (at most one — the stream enforces
    /// it; released in the REAP arm).
    pub in_flight: Option<SectionLease>,
    pub write_seq: u64,
}

/// Cumulative checkpoint gauges (S21 vocabulary, cell-local).
#[derive(Copy, Clone, Debug, Default)]
pub struct CkptStats {
    pub completed: u64,
    pub aborted: u64,
    pub last_unix_ms: u64,
    pub last_begin_lsn: u64,
    /// Live checkpoint-buffer domain bytes (0 when idle — L5).
    pub buffer_bytes: u64,
}

pub(crate) struct CkptCell<F: SegmentFs> {
    pub cfg: CkptConfig,
    dir: PathBuf,
    cell: u16,
    fs: F,
    next_id: u64,
    /// Manual trigger latch (`INF.CKPT` via the control handle — S20).
    pub requested: bool,
    /// Latest `INF.CKPT` request epoch seen (M2-S20) — recorded at
    /// request, consumed into `epoch_in_flight` when the begin marker
    /// stages, published to the control board at the MANIFEST commit.
    pub req_epoch: u64,
    pub epoch_in_flight: u64,
    /// `StagingStats::append_bytes` at the last completed checkpoint's
    /// **begin** — the bytes-appended-since trigger base. Begin, not
    /// publish: a paced walk (ADR-0017) can lag a write burst by a full
    /// dataset, and rebasing at publish would let everything staged
    /// mid-walk escape the trigger — an unbounded retained log if writes
    /// then quiesce.
    pub bytes_at_last: u64,
    /// The staged-bytes total captured when the current checkpoint's
    /// begin marker was staged (becomes `bytes_at_last` at publish).
    pub bytes_at_begin: u64,
    pub phase: CkptPhase<F::File>,
    stats: CkptStats,
}

impl<F: SegmentFs> CkptCell<F> {
    /// Scans the cell's ckpt dir for the next id (boot-time listing — the
    /// rotor-prealloc metadata class). `.ick.new` orphans from a crashed
    /// walk are left in place; S11's manifest GC owns their removal.
    pub fn new(fs: F, dir: PathBuf, cell: u16, cfg: CkptConfig) -> io::Result<CkptCell<F>> {
        let next_id = fs
            .list_dir(&dir)?
            .iter()
            .filter_map(|name| parse_ick_file_name(name))
            .max()
            .map_or(1, |max| max + 1);
        Ok(CkptCell {
            cfg,
            dir,
            cell,
            fs,
            next_id,
            requested: false,
            req_epoch: 0,
            epoch_in_flight: 0,
            bytes_at_last: 0,
            bytes_at_begin: 0,
            phase: CkptPhase::Idle,
            stats: CkptStats::default(),
        })
    }

    /// The id the next checkpoint will carry.
    pub fn pending_id(&self) -> u64 {
        self.next_id
    }

    /// Trigger check (Idle only): manual request, or the bytes-appended
    /// threshold (`interval_bytes = 0` disables the automatic trigger).
    pub fn should_begin(&self, staged_bytes_total: u64) -> bool {
        self.requested
            || (self.cfg.interval_bytes > 0
                && staged_bytes_total.saturating_sub(self.bytes_at_last) >= self.cfg.interval_bytes)
    }

    /// `LOG` sealed the frame carrying the begin marker: resolve its LSN.
    /// The marker is always in the first frame sealed after it was staged
    /// (one staging buffer, generation bumps only at seal), so `lsn_of` is
    /// exact by construction.
    pub fn on_frame_sealed(&mut self, lease: &inf_log::FrameLease) {
        if let CkptPhase::AwaitBeginLsn { id, at } = &self.phase {
            self.phase = CkptPhase::Begun { id: *id, begin_lsn: lease.lsn_of(*at) };
        }
    }

    /// Creates `ckpt-{id}.ick.new` and stages the header block; the caller
    /// queues the returned state's first write. The create carries no
    /// durability (M2-S12): the `CkptSync` fdatasync owns the data, the
    /// publication dir-fsync owns the name — no device barrier here.
    ///
    /// # Errors
    /// File creation failure (ENOSPC class) — the caller aborts the
    /// checkpoint, not the process.
    pub fn open_stream(
        &mut self,
        id: u64,
        begin_lsn: Lsn,
        ns_ids: Vec<u32>,
        now: inf_foundation::time::Nanos,
    ) -> io::Result<()> {
        let mut stream = IckStream::new(&self.cfg);
        let file = self.fs.create_meta(&self.dir.join(ick_staging_file_name(id)))?;
        let fd = file.raw_fd().ok_or_else(|| io::Error::other("std segment tier has fds"))?;
        let lease = stream.begin(self.cell, id, begin_lsn, &ns_ids);
        self.phase = CkptPhase::Stream(Box::new(Streaming {
            id,
            begin_lsn,
            opened_at: now,
            streamed_bytes: 0,
            stream,
            file,
            fd,
            ns_ids,
            ns_idx: 0,
            cursor: 0,
            walk_done: false,
            footer_staged: false,
            sync_issued: false,
            sync_done: false,
            in_flight: Some(lease),
            write_seq: 0,
        }));
        Ok(())
    }

    /// Publishes a finished checkpoint: **rename only** — barrier-free on
    /// the loop. The ckpt-dir fsync that makes the name durable rides the
    /// `ManifestCell` swap machine through the driver (M2-S12, ADR-0017);
    /// until it lands, the file is footer-complete either way (a crash
    /// resolves via the old manifest, the un-named `.ick` is GC'd). The
    /// interval trigger re-bases to the *begin-time* staging total (field
    /// docs on `bytes_at_last`).
    ///
    /// # Errors
    /// Rename failure — the caller aborts (the `.new` file stays an
    /// orphan; the old checkpoint remains the valid one).
    pub fn publish(&mut self, st: Box<Streaming<F::File>>, unix_now_ms: u64) -> io::Result<()> {
        // Release asserts (M2.5-S13): publishing an `.ick` whose bytes are
        // not fsync-durable would let the MANIFEST name a short/torn
        // recovery unit — silent durable corruption. Per-checkpoint, free.
        assert!(st.sync_done, "publish before the completion fdatasync landed");
        assert!(st.in_flight.is_none(), "publish with a section write still in flight");
        let id = st.id;
        let begin = st.begin_lsn;
        drop(st); // closes the fd before rename (write handle no longer needed)
        self.fs
            .rename(&self.dir.join(ick_staging_file_name(id)), &self.dir.join(ick_file_name(id)))?;
        self.next_id = id + 1;
        self.bytes_at_last = self.bytes_at_begin;
        self.stats.completed += 1;
        self.stats.last_unix_ms = unix_now_ms;
        self.stats.last_begin_lsn = begin.to_u64();
        self.phase = CkptPhase::Idle;
        Ok(())
    }

    /// Aborts the in-progress checkpoint (I/O error path): the `.new`
    /// orphan is removed best-effort, the buffers are freed, the trigger
    /// state is untouched — the next trigger simply starts checkpoint
    /// `id + 1`. Never a process fail-stop (module docs).
    pub fn abort(&mut self, what: &str, detail: &str) {
        let id = match &self.phase {
            CkptPhase::Idle => None,
            CkptPhase::AwaitBeginLsn { id, .. } | CkptPhase::Begun { id, .. } => Some(*id),
            CkptPhase::Stream(st) => Some(st.id),
        };
        if let Some(id) = id {
            eprintln!("cell {}: checkpoint {id} aborted at {what}: {detail}", self.cell);
            self.phase = CkptPhase::Idle; // drops the stream + fd first
            let _ = self.fs.remove_file(&self.dir.join(ick_staging_file_name(id)));
            self.next_id = id + 1;
            self.stats.aborted += 1;
        }
    }

    pub fn stats(&self) -> CkptStats {
        let buffer_bytes = match &self.phase {
            CkptPhase::Stream(st) => st.stream.resident_bytes() as u64,
            _ => 0,
        };
        CkptStats { buffer_bytes, ..self.stats }
    }
}

/// Checkpoint completion tokens: seq split across slot/generation, same
/// packing as the frame-write tokens.
pub(crate) fn ckpt_token(class: TokenClass, seq: u64) -> CompletionToken {
    CompletionToken::new(class, (seq & 0xFF_FFFF) as u32, (seq >> 24) as u32)
}

/// A published `.ick` awaiting its MANIFEST (M2-S11): the swap is gated on
/// `begin_lsn ≤` the durability watermark, so the manifest never names a
/// recovery unit whose begin marker might not be on disk (ADR-0016 D4's
/// publication guard, decided in ADR-0017).
#[derive(Copy, Clone, Debug)]
pub(crate) struct PendingManifest {
    pub ckpt_id: u64,
    pub begin_lsn: Lsn,
}

/// Cumulative manifest/truncation gauges (S21 vocabulary, cell-local).
#[derive(Copy, Clone, Debug, Default)]
pub struct ManifestStats {
    pub published: u64,
    /// Failed swaps (counted, old recovery unit kept — never fail-stop:
    /// nothing was acked against the new manifest, ADR-0017).
    pub aborted: u64,
    pub truncated_segments: u64,
    pub ick_orphans_removed: u64,
}

/// Per-slice cap on truncation/GC unlinks: MAINTAIN metadata ops stay
/// bounded regardless of how many segments a long checkpoint interval
/// covered (L3 — batch, never burst).
pub(crate) const MAX_UNLINKS_PER_SLICE: usize = 2;

/// Adaptive truncation ceiling (M2.5-S11, ADR-0022 D8.4): when the covered
/// backlog grows — a fast writer outrunning the fixed 2/slice drain — the
/// truncation budget scales with the backlog up to this bound. Each unit is
/// rotor bookkeeping plus one delegated unlink request (no device work on
/// the loop), so the slice stays cheap; the bound keeps it a slice, not a
/// burst (L3).
pub(crate) const MAX_TRUNC_PER_SLICE_ADAPTIVE: usize = 16;

/// Where the recovery-unit transition stands (M2-S11/S12, ADR-0017).
/// Every fsync-class step rides the driver (`TokenClass::ManifestSync`) —
/// a device barrier on the loop is exactly the foreground stall the S12
/// gate forbids; the loop performs only barrier-free metadata ops
/// (create, page-cache write, rename, unlink). Blocking ops happen in
/// MAINTAIN slices; REAP completions only flip phases.
/// MAINTAIN slices between retry attempts of an aborted MANIFEST swap
/// (M2-S20): bounds per-fault log lines and file ops while a persistent
/// error lasts; one slice per loop iteration makes this milliseconds.
pub(crate) const RETRY_BACKOFF_SLICES: u32 = 64;

pub(crate) enum SwapPhase<File: SegmentFile> {
    /// An aborted swap waiting out its retry backoff (M2-S20, ADR-0021
    /// D6): the pending recovery-unit transition is retried — "the swap
    /// succeeds once the fault clears" (the S16 sync-tier semantic,
    /// honored on the reactor tier) — so `INF.CKPT WAIT` cannot hang on
    /// a counted abort. Backoff keeps a persistent fault from spamming
    /// one attempt per MAINTAIN slice.
    Backoff {
        pending: PendingManifest,
        slices: u32,
    },
    Idle,
    /// `.ick` renamed by the checkpoint driver; its ckpt-dir fsync is not
    /// yet queued (queued next MAINTAIN slice).
    IckDirPending {
        pending: PendingManifest,
    },
    /// ckpt-dir fsync in flight — the `.ick` name is being made durable.
    IckDirQueued {
        pending: PendingManifest,
        /// Keeps the dir fd alive for the driver op (never read).
        #[allow(dead_code)]
        dir: File,
    },
    /// `.ick` durably named; waiting for the durability watermark to
    /// cover `begin_lsn` (the publication guard).
    WatermarkWait {
        pending: PendingManifest,
    },
    /// `MANIFEST.new` written; its fdatasync in flight.
    StageQueued {
        manifest: Manifest,
        #[allow(dead_code)]
        file: File,
    },
    /// Staging durable; rename + dir-fsync queue next MAINTAIN slice.
    StageSynced {
        manifest: Manifest,
    },
    /// Renamed onto `MANIFEST`; the commit dir-fsync in flight.
    DirQueued {
        manifest: Manifest,
        #[allow(dead_code)]
        dir: File,
    },
    /// Dir-fsync landed — commit (floor advance + GC) next slice.
    DirSynced {
        manifest: Manifest,
    },
    /// An in-flight step failed; clean up next slice (old unit kept).
    Failed,
}

/// Per-cell MANIFEST + truncation driver (M2-S11, ADR-0017). Owns the
/// shard-dir swap state machine, the truncation floor, and the bounded
/// orphan-GC queue. Metadata ops are MAINTAIN-slice blocking calls on the
/// injected-seam class (S02 rotor-prealloc class; named fault points at
/// S16); every fsync-class barrier rides the driver.
pub(crate) struct ManifestCell<F: SegmentFs> {
    shard_dir: PathBuf,
    ckpt_dir: PathBuf,
    cell: u16,
    fs: F,
    /// Truncation floor from the durable manifest (`None` until the first
    /// manifest is published or recovered).
    floor: Option<SegmentId>,
    /// The checkpoint id the durable manifest names.
    named_ckpt: Option<u64>,
    /// The `INF.CKPT` request epoch the in-flight transition satisfies
    /// (M2-S20; 0 for interval-triggered checkpoints before any request).
    pending_epoch: u64,
    /// Set at the swap's dir-fsync commit: `(epoch, ckpt id)` — consumed
    /// by `manifest_slice` to publish the control board slot.
    just_published: Option<(u64, u64)>,
    pub phase: SwapPhase<F::File>,
    sync_seq: u64,
    /// Stale-file unlink queue (filled once per publish from one dir
    /// listing; drained ≤ [`MAX_UNLINKS_PER_SLICE`] per slice).
    gc_queue: Vec<PathBuf>,
    stats: ManifestStats,
}

impl<F: SegmentFs> ManifestCell<F> {
    pub fn new(
        fs: F,
        shard_dir: PathBuf,
        ckpt_dir: PathBuf,
        cell: u16,
        recovered: Option<PendingManifest>,
    ) -> ManifestCell<F> {
        // `recovered` is the manifest recovery loaded (not a pending swap):
        // seed the floor + named id so truncation resumes where the last
        // boot left off.
        ManifestCell {
            shard_dir,
            ckpt_dir,
            cell,
            fs,
            floor: recovered.map(|m| m.begin_lsn.segment),
            named_ckpt: recovered.map(|m| m.ckpt_id),
            pending_epoch: 0,
            just_published: None,
            phase: SwapPhase::Idle,
            sync_seq: 0,
            gc_queue: Vec::new(),
            stats: ManifestStats::default(),
        }
    }

    /// The truncation floor: sealed segments below it are deletable.
    pub fn floor(&self) -> Option<SegmentId> {
        self.floor
    }

    /// True when no recovery-unit transition is in flight — the gate for
    /// starting the next checkpoint (one transition in flight, ever).
    pub fn idle(&self) -> bool {
        matches!(self.phase, SwapPhase::Idle)
    }

    /// M5 retention hook (stubbed, M2 plan §4 S11): topic segments will be
    /// exempt from truncation until retention releases them — their
    /// lifecycle belongs to the retention policy, not the checkpoint
    /// floor. M2 has no topic namespaces, so nothing is exempt.
    pub fn truncation_exempt(&self, _segment: SegmentId) -> bool {
        false
    }

    /// The checkpoint driver renamed `ckpt-N.ick`: begin the transition
    /// (its dir-fsync queues on the next MAINTAIN slice).
    pub fn note_published_ick(&mut self, ckpt_id: u64, begin_lsn: Lsn, epoch: u64) {
        // Release assert (M2.5-S13): a second swap while one is in flight
        // could publish a MANIFEST whose begin marker is not yet
        // watermark-covered. Per-checkpoint, free.
        assert!(self.idle(), "one recovery-unit transition in flight");
        self.pending_epoch = epoch;
        self.phase = SwapPhase::IckDirPending { pending: PendingManifest { ckpt_id, begin_lsn } };
    }

    /// The owning cell (the control-board slot index — M2-S20).
    pub fn cell(&self) -> u16 {
        self.cell
    }

    /// Takes the just-committed publication `(epoch, ckpt id)` (M2-S20):
    /// set exactly once per durable MANIFEST swap, at the dir-fsync
    /// commit — the point `INF.CKPT WAIT` is allowed to observe.
    pub fn take_published(&mut self) -> Option<(u64, u64)> {
        self.just_published.take()
    }

    /// REAP: a `ManifestSync` barrier landed — advance the phase (flag
    /// flip only; the follow-up metadata ops run in the next MAINTAIN
    /// slice, never on the completion path).
    pub fn on_synced(&mut self) {
        self.phase = match std::mem::replace(&mut self.phase, SwapPhase::Failed) {
            SwapPhase::IckDirQueued { pending, .. } => SwapPhase::WatermarkWait { pending },
            SwapPhase::StageQueued { manifest, .. } => SwapPhase::StageSynced { manifest },
            SwapPhase::DirQueued { manifest, .. } => SwapPhase::DirSynced { manifest },
            other => {
                let _ = other;
                panic!("ManifestSync completion with no barrier in flight")
            }
        };
    }

    /// REAP: a `ManifestSync` barrier failed — old recovery unit kept,
    /// cleanup next slice (the checkpoint-abort class, ADR-0017).
    pub fn on_sync_error(&mut self, errno: i32) {
        eprintln!(
            "cell {}: MANIFEST-swap barrier failed (errno {errno}; old recovery unit kept, \
             retrying after backoff)",
            self.cell
        );
        self.stats.aborted += 1;
        let pending = match std::mem::replace(&mut self.phase, SwapPhase::Idle) {
            SwapPhase::IckDirQueued { pending, .. } => pending,
            SwapPhase::StageQueued { manifest, .. } | SwapPhase::DirQueued { manifest, .. } => {
                PendingManifest { ckpt_id: manifest.ckpt_id, begin_lsn: manifest.begin_lsn }
            }
            other => {
                let _ = other;
                panic!("ManifestSync error with no barrier in flight")
            }
        };
        self.phase = SwapPhase::Backoff { pending, slices: RETRY_BACKOFF_SLICES };
    }

    fn next_token(&mut self) -> inf_runtime::CompletionToken {
        self.sync_seq += 1;
        ckpt_token(TokenClass::ManifestSync, self.sync_seq)
    }

    /// One MAINTAIN slice of the swap machine. `watermark` is the durable
    /// watermark (packed LSN); `active` the rotor's tail segment. Returns
    /// Maintenance units (≈ one per file op). Every arm does O(1) file
    /// ops and queues at most one barrier.
    pub fn swap_slice(
        &mut self,
        cx: &mut inf_runtime::LoopCx<'_>,
        watermark: Option<u64>,
        active: SegmentId,
    ) -> u32 {
        match std::mem::replace(&mut self.phase, SwapPhase::Idle) {
            SwapPhase::Idle => 0,
            SwapPhase::Backoff { pending, slices } => {
                self.phase = if slices == 0 {
                    // Retry: WatermarkWait re-checks (already satisfied)
                    // and re-stages — stale `.new` debris is unlinked there.
                    SwapPhase::WatermarkWait { pending }
                } else {
                    SwapPhase::Backoff { pending, slices: slices - 1 }
                };
                0
            }
            SwapPhase::Failed => {
                // Debris (.new staging) is cleared by the next attempt's
                // stale-staging unlink or boot GC; count and go idle.
                let _ = self
                    .fs
                    .remove_file(&self.shard_dir.join(inf_log::manifest::MANIFEST_STAGING_FILE));
                self.stats.aborted += 1;
                1
            }
            SwapPhase::IckDirPending { pending } => match self.fs.open_dir(&self.ckpt_dir) {
                Ok(dir) => {
                    let fd = dir.raw_fd().expect("std tier has fds");
                    let token = self.next_token();
                    cx.push(inf_runtime::IoOp::Fdatasync { fd, token });
                    self.phase = SwapPhase::IckDirQueued { pending, dir };
                    1
                }
                Err(err) => {
                    eprintln!(
                        "cell {}: ckpt-dir open failed ({err}); retrying after backoff",
                        self.cell
                    );
                    self.stats.aborted += 1;
                    self.phase = SwapPhase::Backoff { pending, slices: RETRY_BACKOFF_SLICES };
                    1
                }
            },
            SwapPhase::WatermarkWait { pending } => {
                if watermark.is_none_or(|w| w < pending.begin_lsn.to_u64()) {
                    self.phase = SwapPhase::WatermarkWait { pending };
                    return 0;
                }
                // Stage MANIFEST.new: unlink debris, create (no barrier),
                // page-cache write, then the driver fdatasync.
                let staged = self.shard_dir.join(inf_log::manifest::MANIFEST_STAGING_FILE);
                match self.fs.remove_file(&staged) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        eprintln!(
                            "cell {}: stale MANIFEST.new unlink failed ({err}); retrying",
                            self.cell
                        );
                        self.stats.aborted += 1;
                        self.phase = SwapPhase::Backoff { pending, slices: RETRY_BACKOFF_SLICES };
                        return 1;
                    }
                }
                let floor = pending.begin_lsn.segment;
                let manifest = Manifest {
                    ckpt_id: pending.ckpt_id,
                    begin_lsn: pending.begin_lsn,
                    segments: (floor.0..=active.0).map(SegmentId).collect(),
                    // Tier sections join when command wiring materializes tiered
                    // namespaces on this path (ADR-0057 D5; epoch 1 until then).
                    tiers: Vec::new(),
                };
                let staged_write = self.fs.create_meta(&staged).and_then(|mut file| {
                    file.write_at(0, &inf_log::manifest_envelope(&manifest))?;
                    Ok(file)
                });
                match staged_write {
                    Ok(file) => {
                        let fd = file.raw_fd().expect("std tier has fds");
                        let token = self.next_token();
                        cx.push(inf_runtime::IoOp::Fdatasync { fd, token });
                        self.phase = SwapPhase::StageQueued { manifest, file };
                        3
                    }
                    Err(err) => {
                        eprintln!(
                            "cell {}: MANIFEST staging failed ({err}; old recovery unit kept, \
                             retrying after backoff)",
                            self.cell
                        );
                        self.stats.aborted += 1;
                        self.phase = SwapPhase::Backoff { pending, slices: RETRY_BACKOFF_SLICES };
                        1
                    }
                }
            }
            SwapPhase::StageSynced { manifest } => {
                // The commit point once durable: rename (barrier-free),
                // then the shard-dir fsync through the driver. The named
                // `manifest_rename_fail` point fires here on the reactor
                // tier (same semantic as the sync-tier envelope step 5 —
                // the dir_fsync_fail multi-site precedent; ADR-0021 D6).
                let staged = self.shard_dir.join(inf_log::manifest::MANIFEST_STAGING_FILE);
                let committed = self.shard_dir.join(inf_log::manifest::MANIFEST_FILE);
                let renamed = if inf_foundation::fault::fire(inf_log::fault::MANIFEST_RENAME_FAIL) {
                    Err(io::Error::other("injected fault: manifest_rename_fail"))
                } else {
                    self.fs
                        .rename(&staged, &committed)
                        .and_then(|()| self.fs.open_dir(&self.shard_dir))
                };
                match renamed {
                    Ok(dir) => {
                        let fd = dir.raw_fd().expect("std tier has fds");
                        let token = self.next_token();
                        cx.push(inf_runtime::IoOp::Fdatasync { fd, token });
                        self.phase = SwapPhase::DirQueued { manifest, dir };
                        2
                    }
                    Err(err) => {
                        eprintln!(
                            "cell {}: MANIFEST rename/dir-open failed ({err}; old recovery unit \
                             kept, retrying after backoff)",
                            self.cell
                        );
                        self.stats.aborted += 1;
                        let pending = PendingManifest {
                            ckpt_id: manifest.ckpt_id,
                            begin_lsn: manifest.begin_lsn,
                        };
                        self.phase = SwapPhase::Backoff { pending, slices: RETRY_BACKOFF_SLICES };
                        1
                    }
                }
            }
            SwapPhase::DirSynced { manifest } => {
                // Commit: the new recovery unit is fully durable.
                self.floor = Some(manifest.floor());
                self.named_ckpt = Some(manifest.ckpt_id);
                self.stats.published += 1;
                self.just_published = Some((self.pending_epoch, manifest.ckpt_id));
                self.fill_gc_queue();
                1
            }
            in_flight @ (SwapPhase::IckDirQueued { .. }
            | SwapPhase::StageQueued { .. }
            | SwapPhase::DirQueued { .. }) => {
                self.phase = in_flight;
                0
            }
        }
    }

    /// One dir listing per publish: every `.ick` the new manifest does not
    /// name and every `.ick.new` orphan joins the bounded unlink queue.
    fn fill_gc_queue(&mut self) {
        let Ok(names) = self.fs.list_dir(&self.ckpt_dir) else {
            return; // listing failure: orphans wait for the next boot GC
        };
        for name in names {
            let stale_ick =
                parse_ick_file_name(&name).is_some_and(|id| Some(id) != self.named_ckpt);
            let orphan_new = name.ends_with(".ick.new");
            if stale_ick || orphan_new {
                self.gc_queue.push(self.ckpt_dir.join(name));
            }
        }
    }

    /// A path whose delegated unlink must be retried next slice (control
    /// queue was full) — it joins the GC queue, so nothing is ever lost.
    pub fn defer_unlink(&mut self, path: PathBuf) {
        self.gc_queue.push(path);
    }

    /// Inline unlink for tiers without a control thread (never fatal:
    /// survivors are re-collected by boot GC).
    pub fn unlink_now(&self, path: &std::path::Path) {
        let _ = self.fs.remove_file(path);
    }

    /// Drain up to `budget` queued stale-file unlinks — **delegated** to
    /// the control thread when one exists (freeing a large file's pages
    /// is O(size); ADR-0017), inline otherwise. Returns ops done.
    pub fn gc_slice(&mut self, budget: usize, control: Option<&crate::ControlHandle>) -> u32 {
        let mut done = 0u32;
        while done < budget as u32 {
            let Some(path) = self.gc_queue.pop() else { break };
            match control {
                Some(control) => {
                    if !control.request_unlink(path.clone()) {
                        // Queue full: keep it for the next slice.
                        self.gc_queue.push(path);
                        break;
                    }
                }
                None => self.unlink_now(&path),
            }
            self.stats.ick_orphans_removed += 1;
            done += 1;
        }
        done
    }

    pub fn note_truncated(&mut self, n: u64) {
        self.stats.truncated_segments += n;
    }

    pub fn stats(&self) -> ManifestStats {
        self.stats
    }
}
