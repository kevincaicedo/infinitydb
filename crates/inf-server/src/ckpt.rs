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
use std::path::{Path, PathBuf};

use inf_alloc::AlignedBox;
use inf_log::ckpt::{ICK_BLOCK_ALIGN, ick_file_name, ick_staging_file_name, parse_ick_file_name};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_log::{CkptConfig, IckStream, Lsn, Manifest, SectionLease, SegmentId, StagedAt};
use inf_runtime::{CompletionToken, TokenClass};
use inf_store::Keyspace;

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
    /// Tiered-namespace walk sub-pass (M4-S26, ADR-0057 D1/D3): 0 =
    /// address refs, 1 = RAM images, 2 = live-set + blob-ref sections +
    /// walk end + retirement scan. Section classes never mix inside a
    /// pass, so class seals happen only at pass boundaries.
    pub tier_pass: u8,
    pub walk_done: bool,
    /// The stream opened v2 — sidecar emission is representable
    /// (M4.5-S06; an index converging mid-walk on a v1 stream waits
    /// for the next checkpoint).
    pub v2: bool,
    /// Sidecar emission plan (M4.5-S06, ADR-0078 D1): captured once
    /// when the record walk completes — converged, non-degraded
    /// indexes on the captured durable namespaces. `None` until then.
    pub sidecar_plan: Option<Vec<inf_log::ckpt::IdxSidecarMeta>>,
    /// Position in the plan; the entries below it are finished or
    /// abandoned.
    pub sidecar_at: usize,
    /// The current index's re-seek walk cursor (owns its resume pair —
    /// nothing borrowed across slices, the S01 freeze).
    pub sidecar_cursor: inf_store::OrderedCursor,
    /// Pairs emitted for the current index (the ordinal counter and
    /// the FINAL total).
    pub sidecar_emitted: u64,
    /// Every plan entry emitted or abandoned — the footer may stage.
    pub sidecar_done: bool,
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
    /// 1 when the staging mode in force is `Buffered` (ADR-0088 D3 as
    /// amended) — the disclosed fallback, probed at boot or downgraded
    /// in-band (`io_mode_downgrades`, 0 or 1 per cell life).
    pub io_mode_buffered: u64,
    pub io_mode_downgrades: u64,
    pub last_unix_ms: u64,
    pub last_begin_lsn: u64,
    /// Live checkpoint-buffer domain bytes (0 when idle — L5).
    pub buffer_bytes: u64,
    /// M4.5-S36 (ADR-0088 D4/D7): on-disk bytes of every published
    /// checkpoint, the last one's, the v3 padding inside them, the
    /// derived trigger interval, and records staged since the current
    /// (or last) begin — the write-amplification and trigger observables.
    pub bytes_total: u64,
    pub bytes_last: u64,
    pub padding_bytes: u64,
    pub interval_bytes: u64,
    pub records_since_begin: u64,
}

/// How the `.ick.new` staging file is written (ADR-0088 D3 as amended
/// 2026-08-21): `Direct` is the design (`O_DIRECT`, no page-cache lump);
/// `Buffered` is the **probed** fallback where the filesystem or platform
/// refuses `O_DIRECT` (macOS, some Linux filesystems) — the same v3
/// container, aligned blocks and all, on a buffered fd. Decided once per
/// cell at boot by creating a probe file in the ckpt dir, **writing one
/// aligned block to it and syncing it** (an open can succeed where the
/// first direct write is refused — the review of `2cb6074`), then
/// removing it; never per checkpoint (a per-checkpoint failure would
/// retry every slice and never complete — the unbounded-retained-log
/// posture the review named). One in-band rule backs the probe: a
/// checkpoint op refused with `EINVAL` under `Direct` downgrades the cell
/// to `Buffered` for good and retries at once — a refusal the probe could
/// not see (a per-write constraint) never loops; a real error (`EIO`,
/// `ENOSPC`) aborts with the ordinary backoff and never changes the
/// mode. Disclosed: a boot line, INFO `ckpt_io_mode`, and
/// `ckpt_io_mode_downgrades`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CkptIoMode {
    Direct,
    Buffered,
}

/// MAINTAIN slices between a checkpoint abort and the next trigger
/// (the manifest's `RETRY_BACKOFF_SLICES` shape): a persistent create or
/// I/O fault otherwise re-fires the trigger every slice, staging one
/// `CkptBegin` record into the log per attempt.
const ABORT_BACKOFF_SLICES: u32 = RETRY_BACKOFF_SLICES;

pub(crate) struct CkptCell<F: SegmentFs> {
    pub cfg: CkptConfig,
    dir: PathBuf,
    cell: u16,
    fs: F,
    io_mode: CkptIoMode,
    /// In-band `Direct` → `Buffered` downgrades (ADR-0088 D3 as amended):
    /// 0 or 1 per cell life, surfaced as `ckpt_io_mode_downgrades`.
    io_mode_downgrades: u64,
    /// Slices left before an aborted checkpoint may be retried.
    backoff_slices: u32,
    next_id: u64,
    /// Manual trigger latch (`INF.CKPT` via the control handle — S20).
    pub requested: bool,
    /// Latest `INF.CKPT` request epoch seen (M2-S20) — recorded at
    /// request, consumed into `epoch_in_flight` when the begin marker
    /// stages, published to the control board at the MANIFEST commit.
    pub req_epoch: u64,
    pub epoch_in_flight: u64,
    /// `CommitStats::frame_bytes_queued` — on-disk frame bytes, header,
    /// trailer and v3 padding included (ADR-0088 D4; ADR-0086 D3's
    /// obligation) — at the last completed checkpoint's **begin**: the
    /// bytes-since trigger base. Begin, not publish: a paced walk
    /// (ADR-0017) can lag a write burst by a full dataset, and rebasing
    /// at publish would let everything staged mid-walk escape the
    /// trigger — an unbounded retained log if writes then quiesce.
    pub bytes_at_last: u64,
    /// The frame-bytes total captured when the current checkpoint's
    /// begin marker was staged (becomes `bytes_at_last` at publish).
    pub bytes_at_begin: u64,
    /// Records staged at the last begin — the record-cap trigger base
    /// (recovery is bound by record count, ADR-0088 D4).
    pub records_at_last: u64,
    pub records_at_begin: u64,
    /// The derived interval in force (`CkptConfig::derive_interval` of
    /// the last checkpoint's bytes; the floor before the first).
    pub interval_bytes: u64,
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
        let io_mode = probe_direct(&fs, &dir, cell)?;
        Ok(CkptCell {
            cfg,
            dir,
            cell,
            fs,
            io_mode,
            io_mode_downgrades: 0,
            backoff_slices: 0,
            next_id,
            requested: false,
            req_epoch: 0,
            epoch_in_flight: 0,
            bytes_at_last: 0,
            bytes_at_begin: 0,
            records_at_last: 0,
            records_at_begin: 0,
            interval_bytes: cfg.derive_interval(0),
            phase: CkptPhase::Idle,
            stats: CkptStats::default(),
        })
    }

    /// The id the next checkpoint will carry.
    pub fn pending_id(&self) -> u64 {
        self.next_id
    }

    /// The staging-file I/O mode in force: probed at boot, downgraded at
    /// most once by [`abort_refused_direct`](Self::abort_refused_direct).
    #[cfg(test)]
    pub fn io_mode(&self) -> CkptIoMode {
        self.io_mode
    }

    /// A checkpoint op was refused with `EINVAL` under `Direct` (ADR-0088
    /// D3 as amended): the filesystem took the probe's aligned block but
    /// refuses this write — per-write constraints the probe cannot see.
    /// Downgrade the cell to `Buffered` for the rest of its life, abort
    /// the checkpoint, and clear the abort backoff so the next MAINTAIN
    /// slice retries immediately in the new mode; the trigger state is
    /// untouched. Under `Buffered` an `EINVAL` is an ordinary abort (the
    /// caller routes it there). Never silent: a log line and the
    /// `ckpt_io_mode_downgrades` counter.
    pub fn abort_refused_direct(&mut self, what: &str) {
        debug_assert_eq!(self.io_mode, CkptIoMode::Direct);
        self.io_mode = CkptIoMode::Buffered;
        self.io_mode_downgrades += 1;
        eprintln!(
            "cell {}: checkpoint staging downgraded to buffered I/O — O_DIRECT {what} refused \
             (EINVAL) after the boot probe passed; v3 blocks stay aligned, retrying at once \
             (INFO ckpt_io_mode:buffered, ckpt_io_mode_downgrades:{})",
            self.cell, self.io_mode_downgrades
        );
        self.abort(what, "EINVAL under O_DIRECT");
        self.backoff_slices = 0;
    }

    /// True while the staging mode is `Direct` — the caller's guard for
    /// routing an `EINVAL` to [`abort_refused_direct`](Self::abort_refused_direct).
    pub fn io_mode_direct(&self) -> bool {
        self.io_mode == CkptIoMode::Direct
    }

    /// One MAINTAIN slice of abort backoff elapsed; `true` while the
    /// trigger is still held.
    pub fn tick_backoff(&mut self) -> bool {
        if self.backoff_slices > 0 {
            self.backoff_slices -= 1;
            return true;
        }
        false
    }

    /// Trigger check (Idle only): manual request, the derived on-disk
    /// frame-bytes threshold, or the record cap (ADR-0088 D4 — either
    /// bound alone lets the other shape's replay escape the boot gate).
    /// `interval_bytes = 0` (the floor) disables the automatic trigger
    /// entirely, record cap included — manual checkpoints only.
    pub fn should_begin(&self, frame_bytes_total: u64, records_total: u64) -> bool {
        if self.requested {
            return true;
        }
        if self.cfg.interval_bytes == 0 {
            return false;
        }
        let bytes_since = frame_bytes_total.saturating_sub(self.bytes_at_last);
        let records_since = records_total.saturating_sub(self.records_at_last);
        let cap_records = self.cfg.cap_records();
        bytes_since >= self.interval_bytes || (cap_records > 0 && records_since >= cap_records)
    }

    /// Re-derives the interval from the bytes the last checkpoint wrote
    /// (ADR-0088 D4) — called at publish; before the first checkpoint the
    /// interval is the floor (`derive_interval(0)`).
    fn rederive_interval(&mut self, ckpt_bytes_last: u64) {
        self.interval_bytes = self.cfg.derive_interval(ckpt_bytes_last);
        self.stats.interval_bytes = self.interval_bytes;
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
        v2: bool,
        now: inf_foundation::time::Nanos,
    ) -> io::Result<()> {
        // v2 opens the ADR-0057 D3 vocabulary (address refs, live-set,
        // blob refs) plus the ADR-0073 D2 index sidecars — selected iff
        // a tiered namespace exists or an index declaration targets a
        // durable namespace (ADR-0078 D7: registration, not
        // convergence, drives the version — it must be fixed at header
        // time). Cells with neither keep producing v1 byte-identically
        // (the S03 zero-change posture).
        // M4.5-S36 (ADR-0088 D3): every reactor-tier checkpoint is the v3
        // container on an `O_DIRECT` fd — aligned blocks, no page-cache
        // lump. `v2` keeps selecting the *walk* (tiered passes, sidecars);
        // the v3 vocabulary is a superset, so a v1-shaped cell writes the
        // same sections on aligned blocks.
        let mut stream = IckStream::new_v3(&self.cfg);
        let path = self.dir.join(ick_staging_file_name(id));
        let file = match self.io_mode {
            CkptIoMode::Direct => self.fs.create_meta_direct(&path)?,
            CkptIoMode::Buffered => self.fs.create_meta(&path)?,
        };
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
            tier_pass: 0,
            walk_done: false,
            v2,
            sidecar_plan: None,
            sidecar_at: 0,
            sidecar_cursor: inf_store::OrderedCursor::from_start(),
            sidecar_emitted: 0,
            sidecar_done: false,
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
        let ick_bytes = st.stream.file_bytes();
        let padding = st.stream.padding_bytes();
        drop(st); // closes the fd before rename (write handle no longer needed)
        self.fs
            .rename(&self.dir.join(ick_staging_file_name(id)), &self.dir.join(ick_file_name(id)))?;
        self.next_id = id + 1;
        self.bytes_at_last = self.bytes_at_begin;
        self.records_at_last = self.records_at_begin;
        self.stats.bytes_total += ick_bytes;
        self.stats.bytes_last = ick_bytes;
        self.stats.padding_bytes += padding;
        // The next interval is derived from what this checkpoint actually
        // wrote (ADR-0088 D4): the file's size is the measurement.
        self.rederive_interval(ick_bytes);
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
            self.backoff_slices = ABORT_BACKOFF_SLICES;
        }
    }

    /// `records_total` is the cell's records-staged counter (the record
    /// cap's numerator reads against the last begin).
    pub fn stats(&self, records_total: u64) -> CkptStats {
        let buffer_bytes = match &self.phase {
            CkptPhase::Stream(st) => st.stream.resident_bytes() as u64,
            _ => 0,
        };
        let base = if matches!(self.phase, CkptPhase::Idle) {
            self.records_at_last
        } else {
            self.records_at_begin
        };
        CkptStats {
            buffer_bytes,
            interval_bytes: self.interval_bytes,
            records_since_begin: records_total.saturating_sub(base),
            io_mode_buffered: u64::from(self.io_mode == CkptIoMode::Buffered),
            io_mode_downgrades: self.io_mode_downgrades,
            ..self.stats
        }
    }
}

/// Boot-time `O_DIRECT` probe for the ckpt dir (ADR-0088 D3 as amended):
/// create a probe file direct, **write one aligned block and sync it**,
/// remove it. `Unsupported` / `InvalidInput` (the kernel's `EINVAL` for a
/// filesystem without `O_DIRECT`, at the open or at the first write)
/// select `Buffered`, loudly; any other error is the boot's (a ckpt dir
/// that cannot take a file cannot take a checkpoint either). Boot-time,
/// blocking, before the cell loop runs — the same class as the create
/// and remove beside it.
fn probe_direct<F: SegmentFs>(fs: &F, dir: &Path, cell: u16) -> io::Result<CkptIoMode> {
    let probe = dir.join(".direct-probe");
    let _ = fs.remove_file(&probe);
    let refused = |err: &io::Error| {
        matches!(err.kind(), io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput)
    };
    let attempt = fs.create_meta_direct(&probe).and_then(|mut file| {
        let block = AlignedBox::new(ICK_BLOCK_ALIGN);
        file.write_at(0, block.bytes())?;
        file.sync_data()
    });
    match attempt {
        Ok(()) => {
            fs.remove_file(&probe)?;
            Ok(CkptIoMode::Direct)
        }
        Err(err) if refused(&err) => {
            let _ = fs.remove_file(&probe);
            eprintln!(
                "cell {cell}: checkpoint staging falls back to buffered I/O — O_DIRECT refused \
                 on {} ({err}); v3 blocks stay aligned, the page-cache lump ADR-0088 D3 removes \
                 is back for this cell (INFO ckpt_io_mode:buffered)",
                dir.display()
            );
            Ok(CkptIoMode::Buffered)
        }
        Err(err) => {
            let _ = fs.remove_file(&probe);
            Err(err)
        }
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
    /// M4.5-S36 (ADR-0088 D7): MANIFEST bytes written and barriers
    /// issued (metered under `IoClass::Checkpoint`, never deferred).
    pub bytes_written: u64,
    pub syncs_issued: u64,
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
        ks: &mut Keyspace,
        tier: Option<&mut crate::tier_cell::TierCell<F>>,
    ) -> u32 {
        let mut tier = tier;
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
                    self.stats.syncs_issued += 1;
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
                // Tier sections (M4-S26; ADR-0057 D5): one per tiered
                // namespace — retiring files excluded (ADR-0059 D3/D5,
                // the walk that fed this swap emitted no reference into
                // them). Encoding stays epoch 1 when the set is empty.
                let mut tiers = Vec::new();
                if let Some(tc) = tier.as_deref_mut() {
                    for t in &tc.namespaces {
                        if let Some(table) = ks.tiered_store(t.ns) {
                            tiers.push(table.tier_manifest(t.ns.0, &t.flush));
                        }
                    }
                }
                let manifest = Manifest {
                    ckpt_id: pending.ckpt_id,
                    begin_lsn: pending.begin_lsn,
                    segments: (floor.0..=active.0).map(SegmentId).collect(),
                    tiers,
                };
                let staged_write = self.fs.create_meta(&staged).and_then(|mut file| {
                    let envelope = inf_log::manifest_envelope(&manifest);
                    file.write_at(0, &envelope)?;
                    Ok((file, envelope.len() as u64))
                });
                match staged_write {
                    Ok((file, bytes)) => {
                        let fd = file.raw_fd().expect("std tier has fds");
                        let token = self.next_token();
                        self.stats.bytes_written += bytes;
                        self.stats.syncs_issued += 1;
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
                        self.stats.syncs_issued += 1;
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
                // Commit: the new recovery unit is fully durable. Files
                // this swap's checkpoint retired are now unreferenced by
                // every durable artifact — commit their retirement; the
                // pin-gated unlink runs in the tiered MAINTAIN (M4-S26,
                // ADR-0059 D3 phases 2-3).
                if let Some(tc) = tier {
                    for t in &mut tc.namespaces {
                        let Some(table) = ks.tiered_store_mut(t.ns) else { continue };
                        for id in table.commit_retirement() {
                            if let Some(meta) = t.flush.detach_sealed(id) {
                                t.note_retired(meta);
                            }
                        }
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::time::Nanos;
    use inf_log::create_cell_dirs;
    use inf_log::fs::sim::SimDisk;

    fn cell(fs: &SimDisk) -> CkptCell<SimDisk> {
        let dirs = create_cell_dirs(fs, Path::new("data/shard-0")).expect("dirs");
        CkptCell::new(fs.clone(), dirs.ckpt, 0, CkptConfig::default()).expect("cell")
    }

    /// The boot probe decides the staging mode once (ADR-0088 D3 as
    /// amended): a filesystem that refuses `O_DIRECT` selects `Buffered`
    /// and the stream is still created — on `create_meta` — instead of
    /// aborting every checkpoint; a filesystem that takes it stays
    /// `Direct`. The probe file never survives the decision.
    #[test]
    fn probe_selects_buffered_where_direct_is_refused_and_still_opens_streams() {
        let fs = SimDisk::new();
        fs.refuse_direct_meta();
        let mut ckpt = cell(&fs);
        assert_eq!(ckpt.io_mode(), CkptIoMode::Buffered);
        assert_eq!(ckpt.stats(0).io_mode_buffered, 1);
        assert!(
            fs.list_dir(Path::new("data/shard-0/ckpt")).expect("dir").is_empty(),
            "no probe file left behind"
        );
        ckpt.open_stream(1, Lsn::new(SegmentId(0), 40), vec![16], false, Nanos::ZERO)
            .expect("buffered staging file");
        assert!(matches!(ckpt.phase, CkptPhase::Stream(_)));
        assert_eq!(
            fs.list_dir(Path::new("data/shard-0/ckpt")).expect("dir"),
            vec![ick_staging_file_name(1)]
        );

        let direct = cell(&SimDisk::new());
        assert_eq!(direct.io_mode(), CkptIoMode::Direct);
        assert_eq!(direct.stats(0).io_mode_buffered, 0);
    }

    /// The probe writes (ADR-0088 D3 as amended): a filesystem that takes
    /// the `O_DIRECT` open but refuses the first direct write selects
    /// `Buffered` at boot — and the probe's own block is what catches it.
    #[test]
    fn probe_writes_a_block_so_a_refused_direct_write_selects_buffered() {
        let fs = SimDisk::new();
        fs.refuse_direct_writes_after(0);
        let ckpt = cell(&fs);
        assert_eq!(ckpt.io_mode(), CkptIoMode::Buffered);
        assert_eq!(ckpt.stats(0).io_mode_downgrades, 0, "a boot decision, not a downgrade");
        assert!(
            fs.list_dir(Path::new("data/shard-0/ckpt")).expect("dir").is_empty(),
            "no probe file left behind"
        );
        // One allowed write: the probe's block passes, the mode is Direct.
        let fs = SimDisk::new();
        fs.refuse_direct_writes_after(1);
        assert_eq!(cell(&fs).io_mode(), CkptIoMode::Direct);
    }

    /// The in-band downgrade (ADR-0088 D3 as amended): `EINVAL` on a
    /// checkpoint op under `Direct` flips the cell to `Buffered` for
    /// good, aborts without the backoff, and the next stream opens
    /// buffered — `create_meta`, which the refusing disk accepts.
    #[test]
    fn refused_direct_write_downgrades_once_and_retries_at_once() {
        let fs = SimDisk::new();
        fs.refuse_direct_writes_after(1);
        let mut ckpt = cell(&fs);
        assert!(ckpt.io_mode_direct());
        ckpt.requested = true;
        ckpt.open_stream(1, Lsn::new(SegmentId(0), 40), vec![16], false, Nanos::ZERO)
            .expect("direct stream");
        ckpt.abort_refused_direct("I/O");
        assert!(matches!(ckpt.phase, CkptPhase::Idle));
        assert!(!ckpt.io_mode_direct());
        assert_eq!(ckpt.io_mode(), CkptIoMode::Buffered);
        let stats = ckpt.stats(0);
        assert_eq!((stats.aborted, stats.io_mode_buffered, stats.io_mode_downgrades), (1, 1, 1));
        assert!(!ckpt.tick_backoff(), "no backoff: the mode changed, retry now");
        assert!(ckpt.should_begin(0, 0), "the trigger state is untouched");
        ckpt.open_stream(2, Lsn::new(SegmentId(0), 80), vec![16], false, Nanos::ZERO)
            .expect("buffered stream after the downgrade");
        assert!(matches!(ckpt.phase, CkptPhase::Stream(_)));
    }

    /// An aborted checkpoint holds the trigger for a backoff of slices —
    /// a persistent fault no longer stages one `CkptBegin` per slice.
    #[test]
    fn abort_backs_off_before_the_trigger_refires() {
        let fs = SimDisk::new();
        let mut ckpt = cell(&fs);
        assert!(!ckpt.tick_backoff(), "fresh cell: no backoff");
        ckpt.requested = true;
        ckpt.open_stream(1, Lsn::new(SegmentId(0), 40), vec![16], false, Nanos::ZERO)
            .expect("stream");
        ckpt.abort("test", "injected");
        assert!(matches!(ckpt.phase, CkptPhase::Idle));
        assert_eq!(ckpt.stats(0).aborted, 1);
        let mut held = 0;
        while ckpt.tick_backoff() {
            held += 1;
        }
        assert_eq!(held, ABORT_BACKOFF_SLICES);
        assert!(ckpt.should_begin(0, 0), "the manual request survives the backoff");
        assert!(!ckpt.tick_backoff(), "backoff is one-shot per abort");
    }
}
