use core::fmt;

use inf_log::{SegmentId, SegmentLifecycleError, SegmentMaintenance};
use inf_runtime::{
    Completion, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode, IoOp, RawFd,
    TokenClass,
};

use crate::log_writer::LogWriteIo;

/// Cold serving-plane state for preparing the next log segment.
///
/// `inf-log` decides when a next segment is required. This adapter owns the
/// runtime file-op sequence that makes the segment durable before
/// `LogWriteIo` may rotate to it.
#[derive(Debug)]
pub struct LogSegmentMaintenance {
    log_dir: RawFd,
    token: CompletionToken,
    in_flight: Option<InFlightSegmentMaintenance>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct LogSegmentMaintenanceConfig {
    log_dir: RawFd,
    token_slot: u32,
    token_generation: u32,
}

impl LogSegmentMaintenanceConfig {
    pub const fn new(log_dir: RawFd, token_slot: u32) -> LogSegmentMaintenanceConfig {
        LogSegmentMaintenanceConfig { log_dir, token_slot, token_generation: 0 }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> LogSegmentMaintenanceConfig {
        self.token_generation = token_generation;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum InFlightSegmentMaintenance {
    Open { segment: SegmentId, len_bytes: u32 },
    Preallocate { segment: SegmentId, fd: RawFd, len_bytes: u32 },
    CloseFailedPreallocate { segment: SegmentId, fd: RawFd, errno: i32 },
    UnlinkFailedPreallocate { segment: SegmentId, errno: i32 },
    SyncFile { segment: SegmentId, fd: RawFd },
    SyncDir { segment: SegmentId, fd: RawFd },
}

impl LogSegmentMaintenance {
    pub fn new(config: LogSegmentMaintenanceConfig) -> LogSegmentMaintenance {
        LogSegmentMaintenance {
            log_dir: config.log_dir,
            token: CompletionToken::new(
                TokenClass::File,
                config.token_slot,
                config.token_generation,
            ),
            in_flight: None,
        }
    }

    #[inline]
    pub const fn token(&self) -> CompletionToken {
        self.token
    }

    #[inline]
    pub const fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    pub fn drive(
        &mut self,
        writer: &LogWriteIo,
        out: &mut Vec<IoOp>,
    ) -> Result<(), LogSegmentMaintenanceError> {
        if self.in_flight.is_some() {
            return Ok(());
        }
        let Some(request) =
            writer.maintenance_request().map_err(LogSegmentMaintenanceError::Segment)?
        else {
            return Ok(());
        };
        match request {
            SegmentMaintenance::Preallocate { segment, len_bytes } => {
                out.push(IoOp::FileOpen {
                    dir: self.log_dir,
                    name: segment.file_name(),
                    mode: FileOpenMode::ReadWriteCreate,
                    token: self.token,
                });
                self.in_flight = Some(InFlightSegmentMaintenance::Open { segment, len_bytes });
            }
        }
        Ok(())
    }

    pub fn on_completion(
        &mut self,
        writer: &mut LogWriteIo,
        completion: Completion,
        out: &mut Vec<IoOp>,
    ) -> Result<LogSegmentMaintenanceEvent, LogSegmentMaintenanceError> {
        if completion.token != self.token {
            return Err(LogSegmentMaintenanceError::UnexpectedToken {
                expected: self.token,
                got: completion.token,
            });
        }
        let Some(in_flight) = self.in_flight.take() else {
            return Err(LogSegmentMaintenanceError::NoOperationInFlight);
        };

        match (in_flight, completion.result) {
            (
                InFlightSegmentMaintenance::Open { segment, len_bytes },
                CompletionResult::FileOpened { fd },
            ) => {
                out.push(IoOp::FilePreallocate {
                    fd,
                    len_bytes: u64::from(len_bytes),
                    token: self.token,
                });
                self.in_flight =
                    Some(InFlightSegmentMaintenance::Preallocate { segment, fd, len_bytes });
                Ok(LogSegmentMaintenanceEvent::Progress)
            }
            (
                InFlightSegmentMaintenance::Open { segment, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LogSegmentMaintenanceError::Open { segment, errno }),
            (
                InFlightSegmentMaintenance::Preallocate { segment, fd, .. },
                CompletionResult::FileDone,
            ) => {
                out.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: self.token });
                self.in_flight = Some(InFlightSegmentMaintenance::SyncFile { segment, fd });
                Ok(LogSegmentMaintenanceEvent::Progress)
            }
            (
                InFlightSegmentMaintenance::Preallocate { segment, fd, .. },
                CompletionResult::Error { errno, buf: None },
            ) => {
                out.push(IoOp::FileClose { fd, token: self.token });
                self.in_flight =
                    Some(InFlightSegmentMaintenance::CloseFailedPreallocate { segment, fd, errno });
                Ok(LogSegmentMaintenanceEvent::PreallocateFailed { segment, fd, errno })
            }
            (
                InFlightSegmentMaintenance::CloseFailedPreallocate { segment, errno, .. },
                CompletionResult::FileClosed,
            ) => {
                out.push(IoOp::FileUnlink {
                    dir: self.log_dir,
                    name: segment.file_name(),
                    token: self.token,
                });
                self.in_flight =
                    Some(InFlightSegmentMaintenance::UnlinkFailedPreallocate { segment, errno });
                Ok(LogSegmentMaintenanceEvent::Progress)
            }
            (
                InFlightSegmentMaintenance::CloseFailedPreallocate { segment, fd, errno },
                CompletionResult::Error { errno: close_errno, buf: None },
            ) => Err(LogSegmentMaintenanceError::CloseAfterPreallocate {
                segment,
                fd,
                errno,
                close_errno,
            }),
            (
                InFlightSegmentMaintenance::UnlinkFailedPreallocate { .. },
                CompletionResult::FileDone,
            ) => Ok(LogSegmentMaintenanceEvent::Progress),
            (
                InFlightSegmentMaintenance::UnlinkFailedPreallocate { segment, errno },
                CompletionResult::Error { errno: unlink_errno, buf: None },
            ) => Err(LogSegmentMaintenanceError::UnlinkAfterPreallocate {
                segment,
                errno,
                unlink_errno,
            }),
            (InFlightSegmentMaintenance::SyncFile { segment, fd }, CompletionResult::FileDone) => {
                out.push(IoOp::FileSync {
                    fd: self.log_dir,
                    mode: FileSyncMode::Full,
                    token: self.token,
                });
                self.in_flight = Some(InFlightSegmentMaintenance::SyncDir { segment, fd });
                Ok(LogSegmentMaintenanceEvent::Progress)
            }
            (
                InFlightSegmentMaintenance::SyncFile { segment, fd },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LogSegmentMaintenanceError::SyncFile { segment, fd, errno }),
            (InFlightSegmentMaintenance::SyncDir { segment, fd }, CompletionResult::FileDone) => {
                writer
                    .mark_preallocated(segment, fd)
                    .map_err(LogSegmentMaintenanceError::Segment)?;
                Ok(LogSegmentMaintenanceEvent::Prepared { segment, fd })
            }
            (
                InFlightSegmentMaintenance::SyncDir { segment, fd },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LogSegmentMaintenanceError::SyncDir { segment, dir: self.log_dir, fd, errno }),
            (phase, CompletionResult::Error { errno, buf: Some(_) }) => {
                Err(LogSegmentMaintenanceError::UnexpectedErrorBuffer {
                    phase: phase.name(),
                    errno,
                })
            }
            (phase, other) => Err(LogSegmentMaintenanceError::UnexpectedCompletionKind {
                phase: phase.name(),
                result: result_name(&other),
            }),
        }
    }
}

impl InFlightSegmentMaintenance {
    const fn name(self) -> &'static str {
        match self {
            InFlightSegmentMaintenance::Open { .. } => "open",
            InFlightSegmentMaintenance::Preallocate { .. } => "preallocate",
            InFlightSegmentMaintenance::CloseFailedPreallocate { .. } => "close_failed_preallocate",
            InFlightSegmentMaintenance::UnlinkFailedPreallocate { .. } => {
                "unlink_failed_preallocate"
            }
            InFlightSegmentMaintenance::SyncFile { .. } => "sync_file",
            InFlightSegmentMaintenance::SyncDir { .. } => "sync_dir",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LogSegmentMaintenanceEvent {
    Progress,
    Prepared { segment: SegmentId, fd: RawFd },
    PreallocateFailed { segment: SegmentId, fd: RawFd, errno: i32 },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogSegmentMaintenanceError {
    UnexpectedToken { expected: CompletionToken, got: CompletionToken },
    NoOperationInFlight,
    Open { segment: SegmentId, errno: i32 },
    Preallocate { segment: SegmentId, fd: RawFd, errno: i32 },
    CloseAfterPreallocate { segment: SegmentId, fd: RawFd, errno: i32, close_errno: i32 },
    UnlinkAfterPreallocate { segment: SegmentId, errno: i32, unlink_errno: i32 },
    SyncFile { segment: SegmentId, fd: RawFd, errno: i32 },
    SyncDir { segment: SegmentId, dir: RawFd, fd: RawFd, errno: i32 },
    UnexpectedErrorBuffer { phase: &'static str, errno: i32 },
    UnexpectedCompletionKind { phase: &'static str, result: &'static str },
    Segment(SegmentLifecycleError),
}

impl fmt::Display for LogSegmentMaintenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogSegmentMaintenanceError::UnexpectedToken { expected, got } => {
                write!(f, "unexpected log segment maintenance token {got:?}, expected {expected:?}")
            }
            LogSegmentMaintenanceError::NoOperationInFlight => {
                write!(f, "no log segment maintenance operation in flight")
            }
            LogSegmentMaintenanceError::Open { segment, errno } => {
                write!(f, "open prepared segment {} failed with errno {errno}", segment.file_name())
            }
            LogSegmentMaintenanceError::Preallocate { segment, fd, errno } => write!(
                f,
                "preallocate prepared segment {} on fd {fd} failed with errno {errno}",
                segment.file_name()
            ),
            LogSegmentMaintenanceError::CloseAfterPreallocate {
                segment,
                fd,
                errno,
                close_errno,
            } => write!(
                f,
                "close prepared segment {} fd {fd} after preallocate errno {errno} failed with errno {close_errno}",
                segment.file_name()
            ),
            LogSegmentMaintenanceError::UnlinkAfterPreallocate { segment, errno, unlink_errno } => {
                write!(
                    f,
                    "unlink prepared segment {} after preallocate errno {errno} failed with errno {unlink_errno}",
                    segment.file_name()
                )
            }
            LogSegmentMaintenanceError::SyncFile { segment, fd, errno } => write!(
                f,
                "sync prepared segment {} on fd {fd} failed with errno {errno}",
                segment.file_name()
            ),
            LogSegmentMaintenanceError::SyncDir { segment, dir, fd, errno } => write!(
                f,
                "sync log directory fd {dir} after prepared segment {} fd {fd} failed with errno {errno}",
                segment.file_name()
            ),
            LogSegmentMaintenanceError::UnexpectedErrorBuffer { phase, errno } => write!(
                f,
                "log segment maintenance {phase} error with errno {errno} unexpectedly returned a buffer"
            ),
            LogSegmentMaintenanceError::UnexpectedCompletionKind { phase, result } => {
                write!(f, "log segment maintenance {phase} got unexpected completion kind {result}")
            }
            LogSegmentMaintenanceError::Segment(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LogSegmentMaintenanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LogSegmentMaintenanceError::Segment(source) => Some(source),
            _ => None,
        }
    }
}

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
    use inf_log::SegmentConfig;

    const ACTIVE_FD: RawFd = 11;
    const LOG_DIR_FD: RawFd = 12;
    const PREPARED_FD: RawFd = 13;
    const TOKEN_SLOT: u32 = 31;
    const TOKEN_GEN: u32 = 7;
    const ENOSPC: i32 = 28;

    fn config() -> SegmentConfig {
        SegmentConfig::new(128, 96, 96).unwrap()
    }

    fn writer() -> LogWriteIo {
        LogWriteIo::open(ACTIVE_FD, SegmentId::ZERO, 80, config(), 30, TOKEN_GEN).unwrap()
    }

    fn maintenance() -> LogSegmentMaintenance {
        LogSegmentMaintenance::new(
            LogSegmentMaintenanceConfig::new(LOG_DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN),
        )
    }

    #[test]
    fn drive_opens_next_segment_when_threshold_is_reached() {
        let writer = writer();
        let mut maintenance = maintenance();
        let mut ops = Vec::new();

        maintenance.drive(&writer, &mut ops).unwrap();

        assert_eq!(ops.len(), 1);
        match &ops[0] {
            IoOp::FileOpen { dir, name, mode, token } => {
                assert_eq!(*dir, LOG_DIR_FD);
                assert_eq!(name, &SegmentId::new(1).unwrap().file_name());
                assert_eq!(*mode, FileOpenMode::ReadWriteCreate);
                assert_eq!(*token, maintenance.token());
            }
            other => panic!("unexpected op {other:?}"),
        }
        assert!(maintenance.in_flight());
    }

    #[test]
    fn completion_protocol_marks_writer_preallocated_after_dir_sync() {
        let mut writer = writer();
        let mut maintenance = maintenance();
        let mut ops = Vec::new();

        maintenance.drive(&writer, &mut ops).unwrap();
        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion {
                        token: maintenance.token(),
                        result: CompletionResult::FileOpened { fd: PREPARED_FD },
                    },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::Progress
        );
        assert!(matches!(
            ops.as_slice(),
            [IoOp::FilePreallocate { fd: PREPARED_FD, len_bytes: 128, .. }]
        ));

        ops.clear();
        maintenance
            .on_completion(
                &mut writer,
                Completion { token: maintenance.token(), result: CompletionResult::FileDone },
                &mut ops,
            )
            .unwrap();
        assert!(matches!(
            ops.as_slice(),
            [IoOp::FileSync { fd: PREPARED_FD, mode: FileSyncMode::DataOnly, .. }]
        ));

        ops.clear();
        maintenance
            .on_completion(
                &mut writer,
                Completion { token: maintenance.token(), result: CompletionResult::FileDone },
                &mut ops,
            )
            .unwrap();
        assert!(matches!(
            ops.as_slice(),
            [IoOp::FileSync { fd: LOG_DIR_FD, mode: FileSyncMode::Full, .. }]
        ));

        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion { token: maintenance.token(), result: CompletionResult::FileDone },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::Prepared {
                segment: SegmentId::new(1).unwrap(),
                fd: PREPARED_FD
            }
        );
        assert!(ops.is_empty());
        assert_eq!(writer.maintenance_request().unwrap(), None);
    }

    #[test]
    fn preallocate_failure_reports_event_and_cleans_created_segment() {
        let mut writer = writer();
        let mut maintenance = maintenance();
        let mut ops = Vec::new();

        maintenance.drive(&writer, &mut ops).unwrap();
        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion {
                        token: maintenance.token(),
                        result: CompletionResult::FileOpened { fd: PREPARED_FD },
                    },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::Progress
        );
        assert!(matches!(
            ops.as_slice(),
            [IoOp::FilePreallocate { fd: PREPARED_FD, len_bytes: 128, .. }]
        ));

        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion {
                        token: maintenance.token(),
                        result: CompletionResult::Error { errno: ENOSPC, buf: None },
                    },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::PreallocateFailed {
                segment: SegmentId::new(1).unwrap(),
                fd: PREPARED_FD,
                errno: ENOSPC,
            }
        );
        assert!(matches!(ops.as_slice(), [IoOp::FileClose { fd: PREPARED_FD, .. }]));
        assert!(maintenance.in_flight());

        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion { token: maintenance.token(), result: CompletionResult::FileClosed },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::Progress
        );
        assert!(matches!(
            ops.as_slice(),
            [IoOp::FileUnlink { dir: LOG_DIR_FD, name, .. }]
                if name == &SegmentId::new(1).unwrap().file_name()
        ));

        ops.clear();
        assert_eq!(
            maintenance
                .on_completion(
                    &mut writer,
                    Completion { token: maintenance.token(), result: CompletionResult::FileDone },
                    &mut ops,
                )
                .unwrap(),
            LogSegmentMaintenanceEvent::Progress
        );
        assert!(ops.is_empty());
        assert!(!maintenance.in_flight());
        assert!(
            writer.maintenance_request().unwrap().is_some(),
            "writer still wants a successor segment; the plane owns the degraded admission latch"
        );
    }
}
