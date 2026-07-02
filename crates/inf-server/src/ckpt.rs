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
use inf_log::fs::{SegmentFile, SegmentFs, StdSegmentFs};
use inf_log::{CkptConfig, IckStream, Lsn, SectionLease, StagedAt};
use inf_runtime::{CompletionToken, TokenClass};

/// Entries pulled from the store per walk call: keeps the byte-budget
/// overshoot bounded by one call's emissions (the staging buffer absorbs
/// it — `IckStream` docs).
pub(crate) const SCAN_CHUNK_ENTRIES: usize = 32;

/// Where a checkpoint stands. One checkpoint in flight per cell, ever;
/// triggers during a walk latch `requested`, they never stack (ADR-0016
/// D7).
pub(crate) enum CkptPhase {
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
    Stream(Box<Streaming>),
}

/// Active streaming state. Boxed: it exists only while a checkpoint runs,
/// and moving the box never moves the `IckStream` buffers' heap storage
/// (the `StableBytes` custody argument — `log_bytes.rs`).
pub(crate) struct Streaming {
    pub id: u64,
    pub begin_lsn: Lsn,
    pub stream: IckStream,
    /// Keeps the fd alive for the driver ops; dropped at publish/abort
    /// (never read — its job is ownership).
    #[allow(dead_code)]
    pub file: <StdSegmentFs as SegmentFs>::File,
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

pub(crate) struct CkptCell {
    pub cfg: CkptConfig,
    dir: PathBuf,
    cell: u16,
    fs: StdSegmentFs,
    next_id: u64,
    /// Manual trigger latch (`INF.CKPT` via the control handle — S20).
    pub requested: bool,
    /// `StagingStats::append_bytes` at the last completed checkpoint — the
    /// bytes-appended-since trigger base.
    pub bytes_at_last: u64,
    pub phase: CkptPhase,
    stats: CkptStats,
}

impl CkptCell {
    /// Scans the cell's ckpt dir for the next id (boot-time listing — the
    /// rotor-prealloc metadata class). `.ick.new` orphans from a crashed
    /// walk are left in place; S11's manifest GC owns their removal.
    pub fn new(dir: PathBuf, cell: u16, cfg: CkptConfig) -> io::Result<CkptCell> {
        let fs = StdSegmentFs;
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
            bytes_at_last: 0,
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
    /// queues the returned state's first write.
    ///
    /// # Errors
    /// File creation failure (ENOSPC class) — the caller aborts the
    /// checkpoint, not the process.
    pub fn open_stream(&mut self, id: u64, begin_lsn: Lsn, ns_ids: Vec<u32>) -> io::Result<()> {
        let mut stream = IckStream::new(&self.cfg);
        let file = self.fs.create_segment(&self.dir.join(ick_staging_file_name(id)), 0)?;
        let fd = file.raw_fd().ok_or_else(|| io::Error::other("std segment tier has fds"))?;
        let lease = stream.begin(self.cell, id, begin_lsn, &ns_ids);
        self.phase = CkptPhase::Stream(Box::new(Streaming {
            id,
            begin_lsn,
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

    /// Publishes a finished checkpoint: rename + dir-fsync (the `meta.rs`
    /// protocol class). `staged_bytes_total` re-bases the interval trigger.
    ///
    /// # Errors
    /// Rename/dir-fsync failure — the caller aborts (the `.new` file stays
    /// an orphan; the old checkpoint remains the valid one).
    pub fn publish(
        &mut self,
        st: Box<Streaming>,
        unix_now_ms: u64,
        staged_bytes_total: u64,
    ) -> io::Result<()> {
        debug_assert!(st.sync_done && st.in_flight.is_none());
        let id = st.id;
        let begin = st.begin_lsn;
        drop(st); // closes the fd before rename (write handle no longer needed)
        self.fs
            .rename(&self.dir.join(ick_staging_file_name(id)), &self.dir.join(ick_file_name(id)))?;
        self.fs.sync_dir(&self.dir)?;
        self.next_id = id + 1;
        self.bytes_at_last = staged_bytes_total;
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
