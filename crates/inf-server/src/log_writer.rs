use core::fmt;

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use inf_log::{
    FrameMeta, FramePlacement, Lsn, SegmentConfig, SegmentId, SegmentLifecycle,
    SegmentLifecycleError, SegmentMaintenance, SegmentSeal,
};
use inf_runtime::{
    Completion, CompletionResult, CompletionToken, FileSyncMode, IoOp, RawFd, TokenClass,
};

use crate::durability::{DurabilityCell, MutationStageError};

/// Server-side bridge from staged log frames to runtime file writes.
///
/// `inf-log` owns frame bytes and segment placement. `inf-runtime` owns the
/// syscall seam. This adapter owns only server-local orchestration: fds,
/// tokens, in-flight buffer custody, and the point where write failures become
/// fatal to the durability layer.
#[derive(Debug)]
pub struct LogWriteIo {
    active_fd: RawFd,
    lifecycle: SegmentLifecycle,
    token: CompletionToken,
    in_flight: Option<InFlightLogIo>,
    pending_unsynced: Option<PendingSync>,
    sync_due: Option<FileSyncMode>,
    prepared_fd: Option<PreparedSegmentFd>,
    finalized_seal: Option<SegmentSeal>,
    frame_scratch: Vec<u8>,
}

#[derive(Copy, Clone, Debug)]
struct PreparedSegmentFd {
    segment: SegmentId,
    fd: RawFd,
}

#[derive(Copy, Clone, Debug)]
struct PendingSync {
    fd: RawFd,
    meta: FrameMeta,
}

#[derive(Copy, Clone, Debug)]
enum InFlightLogIo {
    Write(InFlightWrite),
    Sync(InFlightSync),
    SealTruncate(InFlightSeal),
    SealSync(InFlightSeal),
}

impl InFlightLogIo {
    #[inline]
    const fn segment(self) -> SegmentId {
        match self {
            InFlightLogIo::Write(write) => write.segment,
            InFlightLogIo::Sync(sync) => sync.segment,
            InFlightLogIo::SealTruncate(seal) | InFlightLogIo::SealSync(seal) => {
                seal.seal.segment()
            }
        }
    }

    #[inline]
    const fn offset_bytes(self) -> u32 {
        match self {
            InFlightLogIo::Write(write) => write.offset_bytes,
            InFlightLogIo::Sync(sync) => sync.offset_bytes,
            InFlightLogIo::SealTruncate(seal) | InFlightLogIo::SealSync(seal) => {
                seal.seal.used_bytes()
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct InFlightWrite {
    fd: RawFd,
    segment: SegmentId,
    offset_bytes: u32,
    buf: BufferId,
    meta: FrameMeta,
    sync_mode: Option<FileSyncMode>,
}

#[derive(Copy, Clone, Debug)]
struct InFlightSync {
    segment: SegmentId,
    offset_bytes: u32,
    meta: FrameMeta,
}

#[derive(Copy, Clone, Debug)]
struct InFlightSeal {
    fd: RawFd,
    seal: SegmentSeal,
}

#[derive(Copy, Clone, Debug)]
struct FrameWritePlan {
    buf: BufferId,
    frame_len_bytes: u32,
    next_lifecycle: SegmentLifecycle,
    placement: FramePlacement,
    sync_mode: Option<FileSyncMode>,
}

impl LogWriteIo {
    pub fn open(
        active_fd: RawFd,
        active_segment: SegmentId,
        active_offset_bytes: u32,
        config: SegmentConfig,
        token_slot: u32,
        token_generation: u32,
    ) -> Result<LogWriteIo, SegmentLifecycleError> {
        Ok(LogWriteIo {
            active_fd,
            lifecycle: SegmentLifecycle::open(active_segment, active_offset_bytes, config)?,
            token: CompletionToken::new(TokenClass::File, token_slot, token_generation),
            in_flight: None,
            pending_unsynced: None,
            sync_due: None,
            prepared_fd: None,
            finalized_seal: None,
            frame_scratch: Vec::new(),
        })
    }

    #[inline]
    pub const fn token(&self) -> CompletionToken {
        self.token
    }

    #[inline]
    pub const fn active_segment(&self) -> SegmentId {
        self.lifecycle.active_segment()
    }

    #[inline]
    pub const fn active_offset_bytes(&self) -> u32 {
        self.lifecycle.active_offset_bytes()
    }

    #[inline]
    pub const fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    #[inline]
    pub const fn has_pending_unsynced(&self) -> bool {
        self.pending_unsynced.is_some()
    }

    pub fn maintenance_request(&self) -> Result<Option<SegmentMaintenance>, SegmentLifecycleError> {
        self.lifecycle.maintenance_request()
    }

    pub fn mark_preallocated(
        &mut self,
        segment: SegmentId,
        fd: RawFd,
    ) -> Result<(), SegmentLifecycleError> {
        self.lifecycle.mark_preallocated(segment)?;
        self.prepared_fd = Some(PreparedSegmentFd { segment, fd });
        Ok(())
    }

    /// Queue one staged frame as one positioned file write.
    ///
    /// Returns `Ok(Some(meta))` when a write was queued, `Ok(None)` when no
    /// frame was pending. The caller submits the returned op through the
    /// normal reactor batch, preserving one backend entry per iteration.
    pub fn queue_frame(
        &mut self,
        durability: &mut DurabilityCell,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
    ) -> Result<Option<FrameMeta>, LogWriteIoError> {
        self.queue_frame_with_sync(durability, pool, out, None)
    }

    /// Queue one staged frame and require a `FileSync` completion before the
    /// frame is reported durable.
    pub fn queue_frame_synced(
        &mut self,
        durability: &mut DurabilityCell,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
        sync_mode: FileSyncMode,
    ) -> Result<Option<FrameMeta>, LogWriteIoError> {
        self.queue_frame_with_sync(durability, pool, out, Some(sync_mode))
    }

    /// Queue a `FileSync` for the latest unsynced frame, or remember that a
    /// sync is due as soon as the current write completes.
    pub fn queue_pending_sync(
        &mut self,
        out: &mut Vec<IoOp>,
        sync_mode: FileSyncMode,
    ) -> Result<LogSyncQueue, LogWriteIoError> {
        match self.in_flight {
            Some(InFlightLogIo::Write(_)) => {
                self.sync_due = Some(sync_mode);
                return Ok(LogSyncQueue::Deferred);
            }
            Some(
                InFlightLogIo::Sync(_)
                | InFlightLogIo::SealTruncate(_)
                | InFlightLogIo::SealSync(_),
            ) => return Ok(LogSyncQueue::Deferred),
            None => {}
        }

        let Some(pending) = self.pending_unsynced.take() else {
            return Ok(LogSyncQueue::NoPending);
        };
        self.queue_sync_for_pending(out, pending, sync_mode);
        Ok(LogSyncQueue::Queued(pending.meta))
    }

    fn queue_frame_with_sync(
        &mut self,
        durability: &mut DurabilityCell,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
        sync_mode: Option<FileSyncMode>,
    ) -> Result<Option<FrameMeta>, LogWriteIoError> {
        if let Some(in_flight) = self.in_flight {
            return Err(LogWriteIoError::WriteAlreadyInFlight {
                segment: in_flight.segment(),
                offset_bytes: in_flight.offset_bytes(),
            });
        }

        let Some(frame_len_bytes) = durability.pending_frame_len_bytes() else {
            return Ok(None);
        };
        if frame_len_bytes as usize > pool.buf_size() {
            return Err(LogWriteIoError::WriteBufferTooSmall {
                frame_len_bytes,
                buffer_len_bytes: pool.buf_size(),
            });
        }

        let (next_lifecycle, placement) = self.preview_placement(frame_len_bytes)?;
        if let Some(seal) = self.seal_to_finalize_before_write(placement) {
            self.queue_seal_finalization(out, seal)?;
            return Ok(None);
        }

        let Some(buf) = pool.try_lease(LeaseKind::Send) else {
            return Err(LogWriteIoError::WriteBufferUnavailable);
        };

        match self.queue_frame_with_buffer(
            durability,
            pool,
            out,
            FrameWritePlan { buf, frame_len_bytes, next_lifecycle, placement, sync_mode },
        ) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) => {
                pool.release(buf);
                Err(error)
            }
        }
    }

    fn queue_frame_with_buffer(
        &mut self,
        durability: &mut DurabilityCell,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
        plan: FrameWritePlan,
    ) -> Result<FrameMeta, LogWriteIoError> {
        let FrameWritePlan { buf, frame_len_bytes, next_lifecycle, placement, sync_mode } = plan;
        let fd = self.fd_for_placement(placement.segment())?;
        if let Some(pending) = self.pending_unsynced
            && pending.fd != fd
        {
            return Err(LogWriteIoError::PendingSyncBeforeSegmentSwitch {
                pending_segment: pending.meta.frame_end().segment(),
                write_segment: placement.segment(),
            });
        }
        let frame_start = Lsn::new(placement.segment().get(), placement.offset_bytes());

        self.frame_scratch.clear();
        let meta = durability
            .drain_frame(frame_start, &mut self.frame_scratch)
            .map_err(LogWriteIoError::Stage)?
            .expect("pending frame length implies a staged frame");
        assert_eq!(meta.frame_len(), frame_len_bytes);
        assert_eq!(self.frame_scratch.len(), frame_len_bytes as usize);

        pool.bytes_mut(buf)[..frame_len_bytes as usize].copy_from_slice(&self.frame_scratch);
        self.frame_scratch.clear();
        self.apply_placement(next_lifecycle, placement.segment(), fd);
        self.in_flight = Some(InFlightLogIo::Write(InFlightWrite {
            fd,
            segment: placement.segment(),
            offset_bytes: placement.offset_bytes(),
            buf,
            meta,
            sync_mode,
        }));
        out.push(IoOp::FileWriteAt {
            fd,
            offset_bytes: u64::from(placement.offset_bytes()),
            buf,
            len: frame_len_bytes,
            token: self.token,
        });
        Ok(meta)
    }

    fn preview_placement(
        &self,
        frame_len_bytes: u32,
    ) -> Result<(SegmentLifecycle, FramePlacement), LogWriteIoError> {
        let mut next_lifecycle = self.lifecycle;
        let placement =
            next_lifecycle.place_frame(frame_len_bytes).map_err(LogWriteIoError::Segment)?;
        Ok((next_lifecycle, placement))
    }

    fn seal_to_finalize_before_write(&self, placement: FramePlacement) -> Option<SegmentSeal> {
        if placement.segment() == self.lifecycle.active_segment() {
            return None;
        }
        let seal = placement.sealed().expect("segment switch must report a sealed segment");
        (self.finalized_seal != Some(seal)).then_some(seal)
    }

    fn queue_seal_finalization(
        &mut self,
        out: &mut Vec<IoOp>,
        seal: SegmentSeal,
    ) -> Result<(), LogWriteIoError> {
        let fd = self.fd_for_placement(seal.segment())?;
        if let Some(pending) = self.pending_unsynced
            && pending.fd != fd
        {
            return Err(LogWriteIoError::PendingSyncBeforeSegmentSwitch {
                pending_segment: pending.meta.frame_end().segment(),
                write_segment: seal.segment(),
            });
        }
        let in_flight = InFlightSeal { fd, seal };
        if seal.used_bytes() < self.lifecycle.config().segment_size_bytes() {
            out.push(IoOp::FileTruncate {
                fd,
                len_bytes: u64::from(seal.used_bytes()),
                token: self.token,
            });
            self.in_flight = Some(InFlightLogIo::SealTruncate(in_flight));
        } else {
            self.queue_seal_sync(out, in_flight);
        }
        Ok(())
    }

    fn fd_for_placement(&self, segment: SegmentId) -> Result<RawFd, LogWriteIoError> {
        if segment == self.lifecycle.active_segment() {
            return Ok(self.active_fd);
        }
        if let Some(prepared) = self.prepared_fd
            && prepared.segment == segment
        {
            return Ok(prepared.fd);
        }
        Err(LogWriteIoError::PreparedSegmentFdMissing { segment })
    }

    fn apply_placement(
        &mut self,
        next_lifecycle: SegmentLifecycle,
        write_segment: SegmentId,
        write_fd: RawFd,
    ) {
        let previous_active = self.lifecycle.active_segment();
        self.lifecycle = next_lifecycle;
        if write_segment != previous_active {
            self.active_fd = write_fd;
            self.prepared_fd = None;
            self.finalized_seal = None;
        }
    }

    pub fn on_completion(
        &mut self,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
        completion: Completion,
    ) -> Result<LogWriteCompletion, LogWriteIoError> {
        if completion.token != self.token {
            return Err(LogWriteIoError::UnexpectedToken {
                expected: self.token,
                got: completion.token,
            });
        }

        match completion.result {
            CompletionResult::FileWritten { buf } => self.complete_written(pool, out, buf),
            CompletionResult::FileDone => self.complete_file_done(out),
            CompletionResult::Error { errno, buf } => self.complete_error(pool, errno, buf),
            other => Err(LogWriteIoError::UnexpectedCompletionKind { result: result_name(&other) }),
        }
    }

    fn complete_written(
        &mut self,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
        buf: BufferId,
    ) -> Result<LogWriteCompletion, LogWriteIoError> {
        let write = match self.in_flight {
            Some(InFlightLogIo::Write(write)) => write,
            Some(
                InFlightLogIo::Sync(_)
                | InFlightLogIo::SealTruncate(_)
                | InFlightLogIo::SealSync(_),
            ) => {
                return Err(LogWriteIoError::UnexpectedCompletionKind { result: "FileWritten" });
            }
            None => return Err(LogWriteIoError::NoWriteInFlight),
        };
        assert_eq!(buf, write.buf, "file write completion returned the wrong buffer");
        self.in_flight = None;
        pool.release(buf);
        if let Some(mode) = write.sync_mode.or_else(|| self.sync_due.take()) {
            self.pending_unsynced = None;
            self.queue_sync_for_pending(out, PendingSync { fd: write.fd, meta: write.meta }, mode);
            return Ok(LogWriteCompletion::SyncQueued { meta: write.meta });
        }
        self.remember_unsynced(write.fd, write.meta)?;
        Ok(LogWriteCompletion::FrameWritten(write.meta))
    }

    fn complete_file_done(
        &mut self,
        out: &mut Vec<IoOp>,
    ) -> Result<LogWriteCompletion, LogWriteIoError> {
        match self.in_flight {
            Some(InFlightLogIo::Sync(sync)) => {
                self.in_flight = None;
                Ok(LogWriteCompletion::FrameDurable(sync.meta))
            }
            Some(InFlightLogIo::SealTruncate(seal)) => {
                self.queue_seal_sync(out, seal);
                Ok(LogWriteCompletion::SealProgress { seal: seal.seal })
            }
            Some(InFlightLogIo::SealSync(seal)) => {
                self.in_flight = None;
                if self.pending_unsynced.is_some_and(|pending| pending.fd == seal.fd) {
                    self.pending_unsynced = None;
                }
                self.finalized_seal = Some(seal.seal);
                Ok(LogWriteCompletion::SealFinalized { seal: seal.seal })
            }
            Some(InFlightLogIo::Write(_)) => {
                Err(LogWriteIoError::UnexpectedCompletionKind { result: "FileDone" })
            }
            None => Err(LogWriteIoError::NoWriteInFlight),
        }
    }

    fn remember_unsynced(&mut self, fd: RawFd, meta: FrameMeta) -> Result<(), LogWriteIoError> {
        match self.pending_unsynced {
            Some(pending) if pending.fd == fd => {
                self.pending_unsynced = Some(PendingSync { fd, meta });
                Ok(())
            }
            Some(pending) => Err(LogWriteIoError::PendingSyncBeforeSegmentSwitch {
                pending_segment: pending.meta.frame_end().segment(),
                write_segment: SegmentId::new(meta.frame_start().segment())
                    .expect("frame meta segment is a valid segment id"),
            }),
            None => {
                self.pending_unsynced = Some(PendingSync { fd, meta });
                Ok(())
            }
        }
    }

    fn queue_sync_for_pending(
        &mut self,
        out: &mut Vec<IoOp>,
        pending: PendingSync,
        mode: FileSyncMode,
    ) {
        out.push(IoOp::FileSync { fd: pending.fd, mode, token: self.token });
        self.in_flight = Some(InFlightLogIo::Sync(InFlightSync {
            segment: SegmentId::new(pending.meta.frame_end().segment())
                .expect("frame meta segment is a valid segment id"),
            offset_bytes: pending.meta.frame_start().offset(),
            meta: pending.meta,
        }));
    }

    fn queue_seal_sync(&mut self, out: &mut Vec<IoOp>, seal: InFlightSeal) {
        out.push(IoOp::FileSync { fd: seal.fd, mode: FileSyncMode::DataOnly, token: self.token });
        self.in_flight = Some(InFlightLogIo::SealSync(seal));
    }

    fn complete_error(
        &mut self,
        pool: &mut BufferPool,
        errno: i32,
        buf: Option<BufferId>,
    ) -> Result<LogWriteCompletion, LogWriteIoError> {
        let Some(in_flight) = self.in_flight.take() else {
            return Err(LogWriteIoError::NoWriteInFlight);
        };
        match in_flight {
            InFlightLogIo::Write(write) => {
                let Some(buf) = buf else {
                    return Err(LogWriteIoError::MissingErrorBuffer {
                        segment: write.segment,
                        offset_bytes: write.offset_bytes,
                    });
                };
                assert_eq!(buf, write.buf, "file write error returned the wrong buffer");
                pool.release(buf);
                Err(LogWriteIoError::FileWrite {
                    segment: write.segment,
                    offset_bytes: write.offset_bytes,
                    errno,
                })
            }
            InFlightLogIo::Sync(sync) => {
                if let Some(buf) = buf {
                    pool.release(buf);
                }
                Err(LogWriteIoError::Fsync {
                    segment: sync.segment,
                    offset_bytes: sync.offset_bytes,
                    errno,
                })
            }
            InFlightLogIo::SealTruncate(seal) => {
                if let Some(buf) = buf {
                    pool.release(buf);
                }
                Err(LogWriteIoError::SealTruncate {
                    segment: seal.seal.segment(),
                    used_bytes: seal.seal.used_bytes(),
                    errno,
                })
            }
            InFlightLogIo::SealSync(seal) => {
                if let Some(buf) = buf {
                    pool.release(buf);
                }
                Err(LogWriteIoError::SealFsync {
                    segment: seal.seal.segment(),
                    used_bytes: seal.seal.used_bytes(),
                    errno,
                })
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LogWriteCompletion {
    FrameWritten(FrameMeta),
    FrameDurable(FrameMeta),
    SyncQueued { meta: FrameMeta },
    SealProgress { seal: SegmentSeal },
    SealFinalized { seal: SegmentSeal },
}

impl LogWriteCompletion {
    #[inline]
    pub fn frame_meta(self) -> FrameMeta {
        match self {
            LogWriteCompletion::FrameWritten(meta)
            | LogWriteCompletion::FrameDurable(meta)
            | LogWriteCompletion::SyncQueued { meta } => meta,
            LogWriteCompletion::SealProgress { .. } | LogWriteCompletion::SealFinalized { .. } => {
                panic!("seal completion has no frame metadata")
            }
        }
    }

    #[inline]
    pub const fn is_durable(self) -> bool {
        matches!(self, LogWriteCompletion::FrameDurable(_))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LogSyncQueue {
    Queued(FrameMeta),
    Deferred,
    NoPending,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogWriteIoError {
    WriteAlreadyInFlight { segment: SegmentId, offset_bytes: u32 },
    WriteBufferTooSmall { frame_len_bytes: u32, buffer_len_bytes: usize },
    WriteBufferUnavailable,
    PreparedSegmentFdMissing { segment: SegmentId },
    UnexpectedToken { expected: CompletionToken, got: CompletionToken },
    NoWriteInFlight,
    UnexpectedCompletionKind { result: &'static str },
    MissingErrorBuffer { segment: SegmentId, offset_bytes: u32 },
    PendingSyncBeforeSegmentSwitch { pending_segment: u32, write_segment: SegmentId },
    FileWrite { segment: SegmentId, offset_bytes: u32, errno: i32 },
    Fsync { segment: SegmentId, offset_bytes: u32, errno: i32 },
    SealTruncate { segment: SegmentId, used_bytes: u32, errno: i32 },
    SealFsync { segment: SegmentId, used_bytes: u32, errno: i32 },
    Segment(SegmentLifecycleError),
    Stage(MutationStageError),
}

impl fmt::Display for LogWriteIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogWriteIoError::WriteAlreadyInFlight { segment, offset_bytes } => write!(
                f,
                "segment {} already has log I/O in flight at offset {offset_bytes}",
                segment.file_name()
            ),
            LogWriteIoError::WriteBufferTooSmall { frame_len_bytes, buffer_len_bytes } => write!(
                f,
                "log frame {frame_len_bytes} bytes exceeds write buffer size {buffer_len_bytes}"
            ),
            LogWriteIoError::WriteBufferUnavailable => {
                write!(f, "could not lease a log write buffer")
            }
            LogWriteIoError::PreparedSegmentFdMissing { segment } => {
                write!(f, "prepared segment {} has no open fd", segment.file_name())
            }
            LogWriteIoError::UnexpectedToken { expected, got } => {
                write!(f, "unexpected log write token {got:?}, expected {expected:?}")
            }
            LogWriteIoError::NoWriteInFlight => write!(f, "no log write or sync in flight"),
            LogWriteIoError::UnexpectedCompletionKind { result } => {
                write!(f, "unexpected log write completion kind {result}")
            }
            LogWriteIoError::MissingErrorBuffer { segment, offset_bytes } => write!(
                f,
                "segment {} file write error at offset {offset_bytes} did not return its buffer",
                segment.file_name()
            ),
            LogWriteIoError::PendingSyncBeforeSegmentSwitch { pending_segment, write_segment } => {
                write!(
                    f,
                    "segment {pending_segment:06} has unsynced log bytes before write to {}",
                    write_segment.file_name()
                )
            }
            LogWriteIoError::FileWrite { segment, offset_bytes, errno } => write!(
                f,
                "segment {} file write at offset {offset_bytes} failed with errno {errno}",
                segment.file_name()
            ),
            LogWriteIoError::Fsync { segment, offset_bytes, errno } => write!(
                f,
                "segment {} fdatasync after frame at offset {offset_bytes} failed with errno {errno}",
                segment.file_name()
            ),
            LogWriteIoError::SealTruncate { segment, used_bytes, errno } => write!(
                f,
                "segment {} truncate to {used_bytes} bytes before rotate failed with errno {errno}",
                segment.file_name()
            ),
            LogWriteIoError::SealFsync { segment, used_bytes, errno } => write!(
                f,
                "segment {} fdatasync after seal at {used_bytes} bytes failed with errno {errno}",
                segment.file_name()
            ),
            LogWriteIoError::Segment(error) => error.fmt(f),
            LogWriteIoError::Stage(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LogWriteIoError {}

fn result_name(result: &CompletionResult) -> &'static str {
    match result {
        CompletionResult::Accepted { .. } => "Accepted",
        CompletionResult::Recv { .. } => "Recv",
        CompletionResult::RecvDropped => "RecvDropped",
        CompletionResult::Sent { .. } => "Sent",
        CompletionResult::Closed => "Closed",
        CompletionResult::FileOpened { .. } => "FileOpened",
        CompletionResult::FileRead { .. } => "FileRead",
        CompletionResult::FileWritten { .. } => "FileWritten",
        CompletionResult::FileDone => "FileDone",
        CompletionResult::FileClosed => "FileClosed",
        CompletionResult::Error { .. } => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::{checkpoint_begin_record_len, decode_checkpoint_begin_record};
    use inf_alloc::BufferPool;
    use inf_foundation::fault::{FaultTrigger, FaultTriggerState};
    use inf_log::{
        CheckpointId, NamespaceId, RecordKind, decode_batch_frame,
        fault::{FSYNC_ERR, LOG_APPEND_SHORT_WRITE},
    };
    use inf_store::MutationEffect;

    const ACTIVE_FD: RawFd = 7;
    const PREPARED_FD: RawFd = 11;
    const TEST_EIO: i32 = 5;

    fn token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, 3, 4)
    }

    fn small_config() -> SegmentConfig {
        SegmentConfig::new(96, 64, 64).unwrap()
    }

    fn stage_delete(cell: &mut DurabilityCell, key: &'static [u8]) {
        cell.stage_mutation_effect(NamespaceId::new(1), MutationEffect::Delete { key }).unwrap();
    }

    fn writer(offset_bytes: u32) -> LogWriteIo {
        LogWriteIo::open(ACTIVE_FD, SegmentId::ZERO, offset_bytes, small_config(), 3, 4).unwrap()
    }

    fn complete_file_written(
        writer: &mut LogWriteIo,
        pool: &mut BufferPool,
        buf: BufferId,
    ) -> LogWriteCompletion {
        let mut ops = Vec::new();
        let completed = writer
            .on_completion(
                pool,
                &mut ops,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();
        assert!(ops.is_empty());
        completed
    }

    #[test]
    fn queue_frame_maps_staging_to_one_file_write() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(8);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        let meta = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();

        assert_eq!(ops.len(), 1);
        let (buf, len) = match ops[0] {
            IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(offset_bytes, 8);
                assert_eq!(token, writer.token());
                (buf, len)
            }
            ref other => panic!("unexpected op {other:?}"),
        };
        assert_eq!(len, meta.frame_len());
        assert_eq!(pool.leased(), 1);
        assert_eq!(cell.log_staging_bytes(), 0);
        assert_eq!(writer.active_offset_bytes(), 8 + meta.frame_len());

        let decoded = decode_batch_frame(&pool.bytes(buf)[..len as usize]).unwrap();
        let records: Vec<_> = decoded.records().collect();
        assert_eq!(decoded.first_lsn(), meta.first_lsn());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record().kind(), RecordKind::Delete);
    }

    #[test]
    fn queue_frame_maps_checkpoint_begin_to_one_synced_log_write() {
        let checkpoint = CheckpointId::new(7).unwrap();
        let mut cell =
            DurabilityCell::with_capacity(checkpoint_begin_record_len().unwrap()).unwrap();
        cell.stage_checkpoint_begin(checkpoint).unwrap();
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        let meta = writer
            .queue_frame_synced(&mut cell, &mut pool, &mut ops, FileSyncMode::DataOnly)
            .unwrap()
            .unwrap();

        assert_eq!(meta.record_count(), 1);
        assert_eq!(meta.first_lsn(), Lsn::new(0, inf_log::FRAME_HEADER_LEN as u32));
        assert_eq!(ops.len(), 1);
        let (buf, len) = match ops[0] {
            IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(offset_bytes, 0);
                assert_eq!(token, writer.token());
                (buf, len)
            }
            ref other => panic!("unexpected op {other:?}"),
        };
        let decoded = decode_batch_frame(&pool.bytes(buf)[..len as usize]).unwrap();
        let records: Vec<_> = decoded.records().collect();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lsn(), meta.first_lsn());
        assert_eq!(records[0].record().kind(), RecordKind::CheckpointBegin);
        assert_eq!(decode_checkpoint_begin_record(records[0].record()), Ok(checkpoint));
    }

    #[test]
    fn queue_frame_does_not_touch_empty_staging() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        assert_eq!(writer.queue_frame(&mut cell, &mut pool, &mut ops), Ok(None));
        assert!(ops.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn queue_frame_rejects_small_or_unavailable_buffers_without_draining() {
        let mut small_cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut small_cell, b"k");
        let before = small_cell.log_staging_bytes();
        let mut small_writer = writer(0);
        let mut small_pool = BufferPool::new(1, 8);

        assert!(matches!(
            small_writer.queue_frame(&mut small_cell, &mut small_pool, &mut Vec::new()),
            Err(LogWriteIoError::WriteBufferTooSmall { .. })
        ));
        assert_eq!(small_cell.log_staging_bytes(), before);
        assert_eq!(small_pool.reconcile(), Ok(()));

        let mut dry_cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut dry_cell, b"k");
        let before = dry_cell.log_staging_bytes();
        let mut dry_writer = writer(0);
        let mut dry_pool = BufferPool::new(1, 128);
        let leased = dry_pool.try_lease(LeaseKind::Send).unwrap();

        assert_eq!(
            dry_writer.queue_frame(&mut dry_cell, &mut dry_pool, &mut Vec::new()),
            Err(LogWriteIoError::WriteBufferUnavailable)
        );
        assert_eq!(dry_cell.log_staging_bytes(), before);
        dry_pool.release(leased);
        assert_eq!(dry_pool.reconcile(), Ok(()));
    }

    #[test]
    fn queue_frame_allows_only_one_in_flight_write() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(2, 128);
        let mut ops = Vec::new();
        let meta = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        stage_delete(&mut cell, b"next");

        assert_eq!(
            writer.queue_frame(&mut cell, &mut pool, &mut ops),
            Err(LogWriteIoError::WriteAlreadyInFlight {
                segment: SegmentId::ZERO,
                offset_bytes: 0,
            })
        );
        assert_eq!(writer.active_offset_bytes(), meta.frame_len());
    }

    #[test]
    fn rotation_defers_frame_until_non_exact_seal_is_truncated_and_synced() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"rotate-key-0123456789");
        let mut writer = writer(80);
        writer.mark_preallocated(SegmentId::new(1).unwrap(), PREPARED_FD).unwrap();
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let staged_bytes = cell.log_staging_bytes();

        assert_eq!(writer.queue_frame(&mut cell, &mut pool, &mut ops), Ok(None));
        assert_eq!(cell.log_staging_bytes(), staged_bytes);
        assert_eq!(pool.leased(), 0);
        assert_eq!(ops.len(), 1);
        match ops.pop().unwrap() {
            IoOp::FileTruncate { fd, len_bytes, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(len_bytes, 80);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }

        let mut sync_ops = Vec::new();
        let progress = writer
            .on_completion(
                &mut pool,
                &mut sync_ops,
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        match progress {
            LogWriteCompletion::SealProgress { seal } => {
                assert_eq!(seal.segment(), SegmentId::ZERO);
                assert_eq!(seal.used_bytes(), 80);
            }
            other => panic!("unexpected completion {other:?}"),
        }
        assert_eq!(sync_ops.len(), 1);
        match sync_ops.pop().unwrap() {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }

        let finalized = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        match finalized {
            LogWriteCompletion::SealFinalized { seal } => {
                assert_eq!(seal.segment(), SegmentId::ZERO);
                assert_eq!(seal.used_bytes(), 80);
            }
            other => panic!("unexpected completion {other:?}"),
        }
        assert!(!writer.in_flight());
        assert_eq!(cell.log_staging_bytes(), staged_bytes);

        let meta = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        assert_eq!(ops.len(), 1);
        match ops.pop().unwrap() {
            IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                assert_eq!(fd, PREPARED_FD);
                assert_eq!(offset_bytes, 0);
                assert_eq!(len, meta.frame_len());
                assert_eq!(token, writer.token());
                pool.release(buf);
            }
            other => panic!("unexpected op {other:?}"),
        }
        assert_eq!(writer.active_segment(), SegmentId::new(1).unwrap());
        assert_eq!(writer.active_offset_bytes(), meta.frame_len());
        assert_eq!(cell.log_staging_bytes(), 0);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn completion_returns_buffer_and_frame_meta() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let queued = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };

        let completed = complete_file_written(&mut writer, &mut pool, buf);

        assert_eq!(completed, LogWriteCompletion::FrameWritten(queued));
        assert!(!completed.is_durable());
        assert!(writer.has_pending_unsynced());
        assert!(!writer.in_flight());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn synced_completion_queues_fsync_before_frame_is_durable() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let queued = writer
            .queue_frame_synced(&mut cell, &mut pool, &mut ops, FileSyncMode::DataOnly)
            .unwrap()
            .unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };

        let mut sync_ops = Vec::new();
        let completion = writer
            .on_completion(
                &mut pool,
                &mut sync_ops,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();

        assert_eq!(completion, LogWriteCompletion::SyncQueued { meta: queued });
        assert!(!completion.is_durable());
        assert_eq!(completion.frame_meta(), queued);
        assert_eq!(pool.leased(), 0);
        assert!(writer.in_flight());
        assert_eq!(sync_ops.len(), 1);
        match sync_ops.pop().unwrap() {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }

        let durable = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();

        assert_eq!(durable, LogWriteCompletion::FrameDurable(queued));
        assert!(durable.is_durable());
        assert!(!writer.in_flight());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn pending_sync_due_while_write_in_flight_syncs_on_write_completion() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let queued = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };

        assert_eq!(
            writer.queue_pending_sync(&mut ops, FileSyncMode::DataOnly),
            Ok(LogSyncQueue::Deferred)
        );
        assert!(ops.is_empty());
        let completion = writer
            .on_completion(
                &mut pool,
                &mut ops,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();

        assert_eq!(completion, LogWriteCompletion::SyncQueued { meta: queued });
        assert_eq!(ops.len(), 1);
        match ops.pop().unwrap() {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        let durable = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        assert_eq!(durable, LogWriteCompletion::FrameDurable(queued));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn fsync_err_fault_is_fatal_before_frame_is_durable() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let queued = writer
            .queue_frame_synced(&mut cell, &mut pool, &mut ops, FileSyncMode::DataOnly)
            .unwrap()
            .unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };
        let mut sync_ops = Vec::new();
        let completion = writer
            .on_completion(
                &mut pool,
                &mut sync_ops,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();
        let mut fault = FaultTriggerState::new(FaultTrigger::nth(1).unwrap());

        assert_eq!(completion, LogWriteCompletion::SyncQueued { meta: queued });
        assert!(!completion.is_durable());
        assert_eq!(sync_ops.len(), 1);
        assert_eq!(
            writer.on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion {
                    token: token(),
                    result: if fault.should_fire(FSYNC_ERR) {
                        CompletionResult::Error { errno: TEST_EIO, buf: None }
                    } else {
                        CompletionResult::FileDone
                    },
                },
            ),
            Err(LogWriteIoError::Fsync {
                segment: SegmentId::ZERO,
                offset_bytes: 0,
                errno: TEST_EIO,
            })
        );
        assert_eq!(fault.occurrences(), 1);
        assert!(!writer.in_flight());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn wrong_completion_token_preserves_in_flight_write() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        let queued = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };
        let wrong = CompletionToken::new(TokenClass::File, 99, 4);

        assert_eq!(
            writer.on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: wrong, result: CompletionResult::FileDone },
            ),
            Err(LogWriteIoError::UnexpectedToken { expected: token(), got: wrong })
        );
        assert!(writer.in_flight());
        assert_eq!(pool.leased(), 1);

        let completed = complete_file_written(&mut writer, &mut pool, buf);

        assert_eq!(completed, LogWriteCompletion::FrameWritten(queued));
        assert!(writer.has_pending_unsynced());
        assert!(!writer.in_flight());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn log_append_short_write_fault_is_fatal_and_returns_buffer() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"k");
        let mut writer = writer(0);
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();
        writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { buf, .. } => buf,
            other => panic!("unexpected op {other:?}"),
        };
        let mut fault = FaultTriggerState::new(FaultTrigger::nth(1).unwrap());

        assert_eq!(
            writer.on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion {
                    token: token(),
                    result: if fault.should_fire(LOG_APPEND_SHORT_WRITE) {
                        CompletionResult::Error { errno: TEST_EIO, buf: Some(buf) }
                    } else {
                        CompletionResult::FileWritten { buf }
                    },
                },
            ),
            Err(LogWriteIoError::FileWrite {
                segment: SegmentId::ZERO,
                offset_bytes: 0,
                errno: TEST_EIO,
            })
        );
        assert_eq!(fault.occurrences(), 1);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn exact_fill_preserves_prepared_fd_for_next_rotation() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"fill");
        let frame_len = cell.pending_frame_len_bytes().unwrap();
        let mut writer = writer(96 - frame_len);
        writer.mark_preallocated(SegmentId::new(1).unwrap(), PREPARED_FD).unwrap();
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        let meta = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();
        let buf = match ops.pop().unwrap() {
            IoOp::FileWriteAt { fd, offset_bytes, buf, .. } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(offset_bytes, u64::from(96 - frame_len));
                buf
            }
            other => panic!("unexpected op {other:?}"),
        };
        let completion = writer
            .on_completion(
                &mut pool,
                &mut ops,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();
        assert_eq!(completion, LogWriteCompletion::FrameWritten(meta));

        let sync = writer.queue_pending_sync(&mut ops, FileSyncMode::DataOnly).unwrap();
        assert_eq!(sync, LogSyncQueue::Queued(meta));
        let sync_op = ops.pop().expect("sync op");
        match sync_op {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        let durable = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        assert_eq!(durable, LogWriteCompletion::FrameDurable(meta));

        stage_delete(&mut cell, b"next");
        assert_eq!(writer.queue_frame(&mut cell, &mut pool, &mut ops), Ok(None));
        match ops.pop().expect("seal sync op") {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        let completion = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        match completion {
            LogWriteCompletion::SealFinalized { seal } => {
                assert_eq!(seal.segment(), SegmentId::ZERO);
                assert_eq!(seal.used_bytes(), 96);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap();

        match ops[0] {
            IoOp::FileWriteAt { fd, offset_bytes, .. } => {
                assert_eq!(fd, PREPARED_FD);
                assert_eq!(offset_bytes, 0);
            }
            ref other => panic!("unexpected op {other:?}"),
        }
    }

    #[test]
    fn rotation_uses_preallocated_segment_fd_and_lsn() {
        let mut cell = DurabilityCell::with_capacity(128).unwrap();
        stage_delete(&mut cell, b"rotate");
        let mut writer = writer(80);
        writer.mark_preallocated(SegmentId::new(1).unwrap(), PREPARED_FD).unwrap();
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        assert_eq!(writer.queue_frame(&mut cell, &mut pool, &mut ops), Ok(None));
        match ops.pop().expect("seal truncate op") {
            IoOp::FileTruncate { fd, len_bytes, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(len_bytes, 80);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        let mut sync_ops = Vec::new();
        let completion = writer
            .on_completion(
                &mut pool,
                &mut sync_ops,
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        match completion {
            LogWriteCompletion::SealProgress { seal } => {
                assert_eq!(seal.segment(), SegmentId::ZERO);
                assert_eq!(seal.used_bytes(), 80);
            }
            other => panic!("unexpected completion {other:?}"),
        }
        match sync_ops.pop().expect("seal sync op") {
            IoOp::FileSync { fd, mode, token } => {
                assert_eq!(fd, ACTIVE_FD);
                assert_eq!(mode, FileSyncMode::DataOnly);
                assert_eq!(token, writer.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        let completion = writer
            .on_completion(
                &mut pool,
                &mut Vec::new(),
                Completion { token: token(), result: CompletionResult::FileDone },
            )
            .unwrap();
        match completion {
            LogWriteCompletion::SealFinalized { seal } => {
                assert_eq!(seal.segment(), SegmentId::ZERO);
                assert_eq!(seal.used_bytes(), 80);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        let meta = writer.queue_frame(&mut cell, &mut pool, &mut ops).unwrap().unwrap();

        match ops[0] {
            IoOp::FileWriteAt { fd, offset_bytes, len, .. } => {
                assert_eq!(fd, PREPARED_FD);
                assert_eq!(offset_bytes, 0);
                assert_eq!(len, meta.frame_len());
            }
            ref other => panic!("unexpected op {other:?}"),
        }
        assert_eq!(meta.frame_start(), Lsn::new(1, 0));
        assert_eq!(writer.active_segment(), SegmentId::new(1).unwrap());
        assert_eq!(writer.active_offset_bytes(), meta.frame_len());
    }
}
