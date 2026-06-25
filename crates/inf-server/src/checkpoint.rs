use core::fmt;
use std::io;

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use inf_foundation::{CellId, time::Nanos};
use inf_log::{
    CHECKPOINT_FOOTER_LEN, CHECKPOINT_IMAGE_HEADER_FIXED_LEN, CHECKPOINT_SECTION_HEADER_LEN,
    CHECKPOINT_SECTION_TRAILER_LEN, CheckpointDigest, CheckpointFooter, CheckpointHeader,
    CheckpointImageError, CheckpointRef, CheckpointSectionKind, CheckpointSectionMeta,
    CheckpointSectionRef, LogStaging, MAX_CHECKPOINT_HEADER_NAMESPACES,
    MAX_CHECKPOINT_IMAGE_SECTIONS, MAX_CHECKPOINT_SECTION_PAYLOAD_LEN, NamespaceId,
    RECOVERY_MANIFEST_FILE, RECOVERY_MANIFEST_TEMP_FILE, RecoveryManifest, RecoveryManifestError,
    checkpoint_header_len_from_prefix, decode_checkpoint_footer, decode_checkpoint_header,
    decode_checkpoint_section, decode_checkpoint_section_frame_header, encode_checkpoint_footer,
    encode_checkpoint_header, encode_checkpoint_section_frame_parts, encode_recovery_manifest,
    validate_checkpoint_footer,
};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode, IoOp,
    RawFd, TokenClass, Wait,
};
use inf_store::{
    CellStore, CheckpointStoreRecord, CheckpointWalk, CheckpointWalkBudget, CheckpointWalkError,
    Keyspace, MutationEffect, NsCatalogError, NsId, NsMode, encode_namespace_catalog,
};

use crate::durability::{MutationStageError, stage_mutation_effect};

pub const DEFAULT_CHECKPOINT_IMAGE_REAP_LIMIT: u32 = 32;
const CHECKPOINT_KEYSPACE_SNAPSHOT_SECTION_COUNT: u32 = 2;
const ENOENT_ERRNO: i32 = 2;

#[derive(Copy, Clone, Debug)]
pub struct CheckpointImagePublishConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl CheckpointImagePublishConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> CheckpointImagePublishConfig {
        CheckpointImagePublishConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_CHECKPOINT_IMAGE_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> CheckpointImagePublishConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> CheckpointImagePublishConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> CheckpointImagePublishConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CheckpointImageLoadConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl CheckpointImageLoadConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> CheckpointImageLoadConfig {
        CheckpointImageLoadConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_CHECKPOINT_IMAGE_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> CheckpointImageLoadConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> CheckpointImageLoadConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> CheckpointImageLoadConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PublishedCheckpointImage {
    checkpoint: CheckpointRef,
    footer: CheckpointFooter,
    bytes_written: u64,
}

impl PublishedCheckpointImage {
    #[inline]
    pub const fn checkpoint(self) -> CheckpointRef {
        self.checkpoint
    }

    #[inline]
    pub const fn footer(self) -> CheckpointFooter {
        self.footer
    }

    #[inline]
    pub const fn bytes_written(self) -> u64 {
        self.bytes_written
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EncodedCheckpointKeyspaceSnapshotParts {
    namespaces: Vec<NamespaceId>,
    catalog: Vec<u8>,
    records: Vec<u8>,
    snapshot: EncodedCheckpointKeyspaceSnapshot,
}

impl EncodedCheckpointKeyspaceSnapshotParts {
    #[inline]
    pub fn snapshot(&self) -> EncodedCheckpointKeyspaceSnapshot {
        self.snapshot
    }
}

#[derive(Copy, Clone, Debug)]
pub struct LiveCheckpointPublishConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
}

impl LiveCheckpointPublishConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> LiveCheckpointPublishConfig {
        LiveCheckpointPublishConfig { dir, token_slot, token_generation: 0 }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> LiveCheckpointPublishConfig {
        self.token_generation = token_generation;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LiveCheckpointPublishEvent {
    Idle,
    Pending,
    Completed,
}

pub struct LiveCheckpointPublisher {
    dir: RawFd,
    token: CompletionToken,
    state: LiveCheckpointPublishState,
}

impl LiveCheckpointPublisher {
    pub fn new(config: LiveCheckpointPublishConfig) -> LiveCheckpointPublisher {
        LiveCheckpointPublisher {
            dir: config.dir,
            token: CompletionToken::new(
                TokenClass::File,
                config.token_slot,
                config.token_generation,
            ),
            state: LiveCheckpointPublishState::Idle,
        }
    }

    pub const fn token(&self) -> CompletionToken {
        self.token
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, LiveCheckpointPublishState::Idle)
    }

    pub fn start(
        &mut self,
        checkpoint: CheckpointRef,
        image: Vec<u8>,
        manifest: &RecoveryManifest,
    ) -> Result<(), LiveCheckpointPublishError> {
        assert!(self.is_idle(), "live checkpoint publisher already active");
        let mut manifest_bytes = Vec::new();
        encode_recovery_manifest(manifest, &mut manifest_bytes)
            .map_err(LiveCheckpointPublishError::ManifestEncode)?;
        let image_name = checkpoint.id().file_name();
        self.state = LiveCheckpointPublishState::OpenTemp {
            file: LiveCheckpointFile {
                temp_name: format!("{image_name}.tmp"),
                final_name: image_name,
                bytes: image,
                next: LiveCheckpointNext::Manifest(manifest_bytes),
            },
        };
        Ok(())
    }

    pub fn drive(
        &mut self,
        pool: &mut BufferPool,
        ops: &mut Vec<IoOp>,
    ) -> Result<LiveCheckpointPublishEvent, LiveCheckpointPublishError> {
        let state = core::mem::replace(&mut self.state, LiveCheckpointPublishState::Idle);
        match state {
            LiveCheckpointPublishState::Idle => {
                self.state = LiveCheckpointPublishState::Idle;
                Ok(LiveCheckpointPublishEvent::Idle)
            }
            LiveCheckpointPublishState::OpenTemp { file } => {
                ops.push(IoOp::FileOpen {
                    dir: self.dir,
                    name: file.temp_name.clone(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: self.token,
                });
                self.state = LiveCheckpointPublishState::OpeningTemp { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            LiveCheckpointPublishState::WriteTemp { fd, file, offset } => {
                if offset >= file.bytes.len() {
                    self.state = LiveCheckpointPublishState::SyncTemp { fd, file };
                    return self.drive(pool, ops);
                }
                let chunk_len = pool.buf_size().min(file.bytes.len() - offset);
                let Some(buf) = pool.try_lease(LeaseKind::Send) else {
                    self.state = LiveCheckpointPublishState::WriteTemp { fd, file, offset };
                    return Ok(LiveCheckpointPublishEvent::Pending);
                };
                pool.bytes_mut(buf)[..chunk_len]
                    .copy_from_slice(&file.bytes[offset..offset + chunk_len]);
                ops.push(IoOp::FileWriteAt {
                    fd,
                    offset_bytes: offset as u64,
                    buf,
                    len: chunk_len as u32,
                    token: self.token,
                });
                self.state =
                    LiveCheckpointPublishState::WritingTemp { fd, file, offset, chunk_len, buf };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            LiveCheckpointPublishState::SyncTemp { fd, file } => {
                ops.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: self.token });
                self.state = LiveCheckpointPublishState::SyncingTemp { fd, file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            LiveCheckpointPublishState::CloseTemp { fd, file } => {
                ops.push(IoOp::FileClose { fd, token: self.token });
                self.state = LiveCheckpointPublishState::ClosingTemp { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            LiveCheckpointPublishState::Rename { file } => {
                ops.push(IoOp::FileRename {
                    old_dir: self.dir,
                    old_name: file.temp_name.clone(),
                    new_dir: self.dir,
                    new_name: file.final_name.clone(),
                    token: self.token,
                });
                self.state = LiveCheckpointPublishState::Renaming { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            LiveCheckpointPublishState::SyncDir { file } => {
                ops.push(IoOp::FileSync {
                    fd: self.dir,
                    mode: FileSyncMode::Full,
                    token: self.token,
                });
                self.state = LiveCheckpointPublishState::SyncingDir { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            inflight => {
                self.state = inflight;
                Ok(LiveCheckpointPublishEvent::Pending)
            }
        }
    }

    pub fn on_completion(
        &mut self,
        pool: &mut BufferPool,
        completion: Completion,
    ) -> Result<LiveCheckpointPublishEvent, LiveCheckpointPublishError> {
        let phase = self.state.phase().expect("checkpoint publish completion with no active phase");
        if completion.token != self.token {
            release_live_checkpoint_result_buffer(pool, &completion.result);
            return Err(LiveCheckpointPublishError::UnexpectedToken {
                phase,
                expected: self.token,
                got: completion.token,
            });
        }

        let state = core::mem::replace(&mut self.state, LiveCheckpointPublishState::Idle);
        match (state, completion.result) {
            (
                LiveCheckpointPublishState::OpeningTemp { file },
                CompletionResult::FileOpened { fd },
            ) => {
                self.state = LiveCheckpointPublishState::WriteTemp { fd, file, offset: 0 };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            (
                LiveCheckpointPublishState::OpeningTemp { file },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::OpenTemp { name: file.temp_name, errno }),
            (
                LiveCheckpointPublishState::WritingTemp { fd, file, offset, chunk_len, buf },
                CompletionResult::FileWritten { buf: got },
            ) => {
                assert_eq!(got, buf, "checkpoint publish write returned the wrong buffer");
                pool.release(got);
                self.state =
                    LiveCheckpointPublishState::WriteTemp { fd, file, offset: offset + chunk_len };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            (
                LiveCheckpointPublishState::WritingTemp { fd, offset, buf, .. },
                CompletionResult::Error { errno, buf: Some(got) },
            ) => {
                assert_eq!(got, buf, "checkpoint publish write error returned wrong buffer");
                pool.release(got);
                Err(LiveCheckpointPublishError::WriteTemp {
                    fd,
                    offset_bytes: offset as u64,
                    errno,
                })
            }
            (
                LiveCheckpointPublishState::WritingTemp { fd, offset, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::MissingWriteBuffer {
                fd,
                offset_bytes: offset as u64,
                errno,
            }),
            (LiveCheckpointPublishState::SyncingTemp { fd, file }, CompletionResult::FileDone) => {
                self.state = LiveCheckpointPublishState::CloseTemp { fd, file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            (
                LiveCheckpointPublishState::SyncingTemp { fd, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::SyncTemp { fd, errno }),
            (LiveCheckpointPublishState::ClosingTemp { file }, CompletionResult::FileClosed) => {
                self.state = LiveCheckpointPublishState::Rename { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            (
                LiveCheckpointPublishState::ClosingTemp { file },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::CloseTemp { name: file.temp_name, errno }),
            (LiveCheckpointPublishState::Renaming { file }, CompletionResult::FileDone) => {
                self.state = LiveCheckpointPublishState::SyncDir { file };
                Ok(LiveCheckpointPublishEvent::Pending)
            }
            (
                LiveCheckpointPublishState::Renaming { file },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::Rename {
                old_name: file.temp_name,
                new_name: file.final_name,
                errno,
            }),
            (LiveCheckpointPublishState::SyncingDir { file }, CompletionResult::FileDone) => {
                match file.next {
                    LiveCheckpointNext::Manifest(bytes) => {
                        self.state = LiveCheckpointPublishState::OpenTemp {
                            file: LiveCheckpointFile {
                                temp_name: RECOVERY_MANIFEST_TEMP_FILE.to_string(),
                                final_name: RECOVERY_MANIFEST_FILE.to_string(),
                                bytes,
                                next: LiveCheckpointNext::Done,
                            },
                        };
                        Ok(LiveCheckpointPublishEvent::Pending)
                    }
                    LiveCheckpointNext::Done => {
                        self.state = LiveCheckpointPublishState::Idle;
                        Ok(LiveCheckpointPublishEvent::Completed)
                    }
                }
            }
            (
                LiveCheckpointPublishState::SyncingDir { .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(LiveCheckpointPublishError::SyncDir { fd: self.dir, errno }),
            (state, result) => {
                self.state = state;
                Err(unexpected_live_checkpoint_completion(pool, phase, result))
            }
        }
    }
}

struct LiveCheckpointFile {
    temp_name: String,
    final_name: String,
    bytes: Vec<u8>,
    next: LiveCheckpointNext,
}

enum LiveCheckpointNext {
    Manifest(Vec<u8>),
    Done,
}

enum LiveCheckpointPublishState {
    Idle,
    OpenTemp {
        file: LiveCheckpointFile,
    },
    OpeningTemp {
        file: LiveCheckpointFile,
    },
    WriteTemp {
        fd: RawFd,
        file: LiveCheckpointFile,
        offset: usize,
    },
    WritingTemp {
        fd: RawFd,
        file: LiveCheckpointFile,
        offset: usize,
        chunk_len: usize,
        buf: BufferId,
    },
    SyncTemp {
        fd: RawFd,
        file: LiveCheckpointFile,
    },
    SyncingTemp {
        fd: RawFd,
        file: LiveCheckpointFile,
    },
    CloseTemp {
        fd: RawFd,
        file: LiveCheckpointFile,
    },
    ClosingTemp {
        file: LiveCheckpointFile,
    },
    Rename {
        file: LiveCheckpointFile,
    },
    Renaming {
        file: LiveCheckpointFile,
    },
    SyncDir {
        file: LiveCheckpointFile,
    },
    SyncingDir {
        file: LiveCheckpointFile,
    },
}

impl LiveCheckpointPublishState {
    fn phase(&self) -> Option<LiveCheckpointPublishPhase> {
        match self {
            LiveCheckpointPublishState::Idle => None,
            LiveCheckpointPublishState::OpenTemp { .. }
            | LiveCheckpointPublishState::OpeningTemp { .. } => {
                Some(LiveCheckpointPublishPhase::OpenTemp)
            }
            LiveCheckpointPublishState::WriteTemp { .. }
            | LiveCheckpointPublishState::WritingTemp { .. } => {
                Some(LiveCheckpointPublishPhase::WriteTemp)
            }
            LiveCheckpointPublishState::SyncTemp { .. }
            | LiveCheckpointPublishState::SyncingTemp { .. } => {
                Some(LiveCheckpointPublishPhase::SyncTemp)
            }
            LiveCheckpointPublishState::CloseTemp { .. }
            | LiveCheckpointPublishState::ClosingTemp { .. } => {
                Some(LiveCheckpointPublishPhase::CloseTemp)
            }
            LiveCheckpointPublishState::Rename { .. }
            | LiveCheckpointPublishState::Renaming { .. } => {
                Some(LiveCheckpointPublishPhase::Rename)
            }
            LiveCheckpointPublishState::SyncDir { .. }
            | LiveCheckpointPublishState::SyncingDir { .. } => {
                Some(LiveCheckpointPublishPhase::SyncDir)
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LiveCheckpointPublishPhase {
    OpenTemp,
    WriteTemp,
    SyncTemp,
    CloseTemp,
    Rename,
    SyncDir,
}

#[derive(Debug)]
pub enum LiveCheckpointPublishError {
    ManifestEncode(RecoveryManifestError),
    UnexpectedToken {
        phase: LiveCheckpointPublishPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    OpenTemp {
        name: String,
        errno: i32,
    },
    WriteTemp {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    MissingWriteBuffer {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    SyncTemp {
        fd: RawFd,
        errno: i32,
    },
    CloseTemp {
        name: String,
        errno: i32,
    },
    Rename {
        old_name: String,
        new_name: String,
        errno: i32,
    },
    SyncDir {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletion {
        phase: LiveCheckpointPublishPhase,
        result: &'static str,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadedCheckpointImage {
    cell: CellId,
    checkpoint: CheckpointRef,
    namespaces: Vec<NamespaceId>,
    sections: Vec<CheckpointSectionMeta>,
    footer: CheckpointFooter,
    bytes_read: u64,
}

impl LoadedCheckpointImage {
    #[inline]
    pub const fn cell(&self) -> CellId {
        self.cell
    }

    #[inline]
    pub const fn checkpoint(&self) -> CheckpointRef {
        self.checkpoint
    }

    #[inline]
    pub fn namespaces(&self) -> &[NamespaceId] {
        &self.namespaces
    }

    #[inline]
    pub fn sections(&self) -> &[CheckpointSectionMeta] {
        &self.sections
    }

    #[inline]
    pub const fn footer(&self) -> CheckpointFooter {
        self.footer
    }

    #[inline]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadedCheckpointImagePayloads {
    image: LoadedCheckpointImage,
    namespace_catalog: Vec<u8>,
    records: Vec<u8>,
}

impl LoadedCheckpointImagePayloads {
    #[inline]
    pub const fn image(&self) -> &LoadedCheckpointImage {
        &self.image
    }

    #[inline]
    pub fn namespace_catalog(&self) -> &[u8] {
        &self.namespace_catalog
    }

    #[inline]
    pub fn records(&self) -> &[u8] {
        &self.records
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CheckpointRecordsSectionConfig {
    pub namespace: NamespaceId,
    pub cursor: u64,
    pub walk_budget: CheckpointWalkBudget,
    pub now: Nanos,
    pub max_payload_bytes: usize,
}

impl CheckpointRecordsSectionConfig {
    pub const fn new(
        namespace: NamespaceId,
        cursor: u64,
        walk_budget: CheckpointWalkBudget,
        now: Nanos,
    ) -> CheckpointRecordsSectionConfig {
        CheckpointRecordsSectionConfig {
            namespace,
            cursor,
            walk_budget,
            now,
            max_payload_bytes: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
        }
    }

    pub const fn with_max_payload_bytes(
        mut self,
        max_payload_bytes: usize,
    ) -> CheckpointRecordsSectionConfig {
        self.max_payload_bytes = max_payload_bytes;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct EncodedCheckpointRecordsSection {
    walk: CheckpointWalk,
    payload_len: usize,
}

impl EncodedCheckpointRecordsSection {
    #[inline]
    pub const fn walk(self) -> CheckpointWalk {
        self.walk
    }

    #[inline]
    pub const fn payload_len(self) -> usize {
        self.payload_len
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CheckpointKeyspaceSnapshotConfig {
    pub now: Nanos,
    pub max_records_payload_bytes: usize,
}

impl CheckpointKeyspaceSnapshotConfig {
    pub const fn new(now: Nanos) -> CheckpointKeyspaceSnapshotConfig {
        CheckpointKeyspaceSnapshotConfig {
            now,
            max_records_payload_bytes: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
        }
    }

    pub const fn with_max_records_payload_bytes(
        mut self,
        max_records_payload_bytes: usize,
    ) -> CheckpointKeyspaceSnapshotConfig {
        self.max_records_payload_bytes = max_records_payload_bytes;
        self
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CheckpointKeyspacePublishConfig {
    pub snapshot: CheckpointKeyspaceSnapshotConfig,
    pub image: CheckpointImagePublishConfig,
}

impl CheckpointKeyspacePublishConfig {
    pub const fn new(
        snapshot: CheckpointKeyspaceSnapshotConfig,
        image: CheckpointImagePublishConfig,
    ) -> CheckpointKeyspacePublishConfig {
        CheckpointKeyspacePublishConfig { snapshot, image }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct EncodedCheckpointKeyspaceSnapshot {
    namespace_count: usize,
    default_stores_walked: usize,
    named_stores_walked: usize,
    records_emitted: usize,
    catalog_payload_len: usize,
    records_payload_len: usize,
}

impl EncodedCheckpointKeyspaceSnapshot {
    #[inline]
    pub const fn namespace_count(self) -> usize {
        self.namespace_count
    }

    #[inline]
    pub const fn default_stores_walked(self) -> usize {
        self.default_stores_walked
    }

    #[inline]
    pub const fn named_stores_walked(self) -> usize {
        self.named_stores_walked
    }

    #[inline]
    pub const fn records_emitted(self) -> usize {
        self.records_emitted
    }

    #[inline]
    pub const fn catalog_payload_len(self) -> usize {
        self.catalog_payload_len
    }

    #[inline]
    pub const fn records_payload_len(self) -> usize {
        self.records_payload_len
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PublishedCheckpointKeyspaceSnapshot {
    image: PublishedCheckpointImage,
    snapshot: EncodedCheckpointKeyspaceSnapshot,
}

impl PublishedCheckpointKeyspaceSnapshot {
    #[inline]
    pub const fn image(self) -> PublishedCheckpointImage {
        self.image
    }

    #[inline]
    pub const fn snapshot(self) -> EncodedCheckpointKeyspaceSnapshot {
        self.snapshot
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CheckpointImagePublishPhase {
    OpenTemp,
    WriteTemp,
    SyncTemp,
    CloseTemp,
    Rename,
    UnlinkTemp,
    SyncDir,
}

impl fmt::Display for CheckpointImagePublishPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImagePublishPhase::OpenTemp => write!(f, "open-temp"),
            CheckpointImagePublishPhase::WriteTemp => write!(f, "write-temp"),
            CheckpointImagePublishPhase::SyncTemp => write!(f, "sync-temp"),
            CheckpointImagePublishPhase::CloseTemp => write!(f, "close-temp"),
            CheckpointImagePublishPhase::Rename => write!(f, "rename"),
            CheckpointImagePublishPhase::UnlinkTemp => write!(f, "unlink-temp"),
            CheckpointImagePublishPhase::SyncDir => write!(f, "sync-dir"),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CheckpointImageLoadPhase {
    Open,
    Read,
    Close,
}

impl fmt::Display for CheckpointImageLoadPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImageLoadPhase::Open => write!(f, "open"),
            CheckpointImageLoadPhase::Read => write!(f, "read"),
            CheckpointImageLoadPhase::Close => write!(f, "close"),
        }
    }
}

impl fmt::Display for LiveCheckpointPublishPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveCheckpointPublishPhase::OpenTemp => write!(f, "open-temp"),
            LiveCheckpointPublishPhase::WriteTemp => write!(f, "write-temp"),
            LiveCheckpointPublishPhase::SyncTemp => write!(f, "sync-temp"),
            LiveCheckpointPublishPhase::CloseTemp => write!(f, "close-temp"),
            LiveCheckpointPublishPhase::Rename => write!(f, "rename"),
            LiveCheckpointPublishPhase::SyncDir => write!(f, "sync-dir"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointRecordsSectionError {
    ZeroPayloadBudget,
    PayloadBudgetTooLarge { max_payload_bytes: usize, max_allowed: usize },
    Walk(CheckpointWalkError),
    Stage(MutationStageError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointKeyspaceSnapshotError {
    NamespaceSetTooLarge { count: usize, max_count: usize },
    Catalog(NsCatalogError),
    Records { namespace: NamespaceId, source: CheckpointRecordsSectionError },
    IncompleteStoreWalk { namespace: NamespaceId, next_cursor: u64 },
}

#[derive(Debug)]
pub enum CheckpointKeyspacePublishError {
    Snapshot(CheckpointKeyspaceSnapshotError),
    Header(CheckpointImageError),
    Section(CheckpointImageError),
    Publish(CheckpointImagePublishError),
}

impl fmt::Display for CheckpointRecordsSectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointRecordsSectionError::ZeroPayloadBudget => {
                write!(f, "checkpoint records section payload budget must be nonzero")
            }
            CheckpointRecordsSectionError::PayloadBudgetTooLarge {
                max_payload_bytes,
                max_allowed,
            } => write!(
                f,
                "checkpoint records section payload budget {max_payload_bytes} exceeds max {max_allowed}"
            ),
            CheckpointRecordsSectionError::Walk(error) => error.fmt(f),
            CheckpointRecordsSectionError::Stage(error) => {
                write!(f, "checkpoint records section staging failed: {error}")
            }
        }
    }
}

impl fmt::Display for CheckpointKeyspaceSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointKeyspaceSnapshotError::NamespaceSetTooLarge { count, max_count } => write!(
                f,
                "checkpoint keyspace snapshot has {count} namespaces, above max {max_count}"
            ),
            CheckpointKeyspaceSnapshotError::Catalog(error) => {
                write!(f, "checkpoint namespace catalog encode failed: {error:?}")
            }
            CheckpointKeyspaceSnapshotError::Records { namespace, source } => write!(
                f,
                "checkpoint records snapshot for namespace {} failed: {source}",
                namespace.get()
            ),
            CheckpointKeyspaceSnapshotError::IncompleteStoreWalk { namespace, next_cursor } => {
                write!(
                    f,
                    "checkpoint records snapshot for namespace {} stopped at cursor {next_cursor}",
                    namespace.get()
                )
            }
        }
    }
}

impl fmt::Display for CheckpointKeyspacePublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointKeyspacePublishError::Snapshot(error) => {
                write!(f, "checkpoint keyspace snapshot failed: {error}")
            }
            CheckpointKeyspacePublishError::Header(error) => {
                write!(f, "checkpoint keyspace header encode failed: {error}")
            }
            CheckpointKeyspacePublishError::Section(error) => {
                write!(f, "checkpoint keyspace section encode failed: {error}")
            }
            CheckpointKeyspacePublishError::Publish(error) => {
                write!(f, "checkpoint keyspace image publish failed: {error}")
            }
        }
    }
}

impl fmt::Display for LiveCheckpointPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveCheckpointPublishError::ManifestEncode(error) => {
                write!(f, "checkpoint manifest encode failed: {error}")
            }
            LiveCheckpointPublishError::UnexpectedToken { phase, expected, got } => {
                write!(f, "checkpoint publish {phase} got token {got:?}, expected {expected:?}")
            }
            LiveCheckpointPublishError::OpenTemp { name, errno } => {
                write!(f, "open checkpoint temp file {name:?} failed with errno {errno}")
            }
            LiveCheckpointPublishError::WriteTemp { fd, offset_bytes, errno } => write!(
                f,
                "write checkpoint temp fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            LiveCheckpointPublishError::MissingWriteBuffer { fd, offset_bytes, errno } => write!(
                f,
                "write checkpoint temp fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            LiveCheckpointPublishError::SyncTemp { fd, errno } => {
                write!(f, "sync checkpoint temp fd {fd} failed with errno {errno}")
            }
            LiveCheckpointPublishError::CloseTemp { name, errno } => {
                write!(f, "close checkpoint temp file {name:?} failed with errno {errno}")
            }
            LiveCheckpointPublishError::Rename { old_name, new_name, errno } => write!(
                f,
                "rename checkpoint {old_name:?} to {new_name:?} failed with errno {errno}"
            ),
            LiveCheckpointPublishError::SyncDir { fd, errno } => {
                write!(f, "sync checkpoint directory fd {fd} failed with errno {errno}")
            }
            LiveCheckpointPublishError::UnexpectedCompletion { phase, result } => {
                write!(f, "checkpoint publish {phase} got unexpected completion kind {result}")
            }
        }
    }
}

impl std::error::Error for CheckpointRecordsSectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointRecordsSectionError::Walk(error) => Some(error),
            CheckpointRecordsSectionError::Stage(error) => Some(error),
            _ => None,
        }
    }
}

impl std::error::Error for CheckpointKeyspaceSnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointKeyspaceSnapshotError::Records { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl std::error::Error for CheckpointKeyspacePublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointKeyspacePublishError::Snapshot(error) => Some(error),
            CheckpointKeyspacePublishError::Header(error)
            | CheckpointKeyspacePublishError::Section(error) => Some(error),
            CheckpointKeyspacePublishError::Publish(error) => Some(error),
        }
    }
}

impl std::error::Error for LiveCheckpointPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LiveCheckpointPublishError::ManifestEncode(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum CheckpointImagePublishError {
    Encode(CheckpointImageError),
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    SectionCountTooLarge {
        count: usize,
        max_count: u32,
    },
    SectionCountMismatch {
        header: u32,
        sections: usize,
    },
    SectionOrdinalMismatch {
        expected: u32,
        got: u32,
    },
    WriteBufferUnavailable,
    Backend {
        phase: CheckpointImagePublishPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: CheckpointImagePublishPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: CheckpointImagePublishPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    ReapLimitExceeded {
        phase: CheckpointImagePublishPhase,
        token: CompletionToken,
        attempts: u32,
    },
    OpenTemp {
        name: String,
        errno: i32,
    },
    WriteTemp {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    MissingWriteBuffer {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    SyncTemp {
        fd: RawFd,
        errno: i32,
    },
    CloseTemp {
        fd: RawFd,
        errno: i32,
    },
    Rename {
        old_name: String,
        new_name: String,
        errno: i32,
    },
    UnlinkTemp {
        name: String,
        errno: i32,
    },
    SyncDir {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: CheckpointImagePublishPhase,
        result: &'static str,
    },
}

#[derive(Debug)]
pub enum CheckpointImageLoadError {
    Decode(CheckpointImageError),
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    ReadBufferUnavailable,
    Backend {
        phase: CheckpointImageLoadPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: CheckpointImageLoadPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: CheckpointImageLoadPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    ReapLimitExceeded {
        phase: CheckpointImageLoadPhase,
        token: CompletionToken,
        attempts: u32,
    },
    Missing {
        name: String,
    },
    Open {
        name: String,
        errno: i32,
    },
    Read {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    MissingReadBuffer {
        fd: RawFd,
        offset_bytes: u64,
        errno: i32,
    },
    ReadLenTooLarge {
        fd: RawFd,
        offset_bytes: u64,
        len: u32,
        requested_len: u32,
        buffer_len_bytes: usize,
    },
    UnexpectedEof {
        part: &'static str,
        needed: usize,
        got: usize,
    },
    CheckpointMismatch {
        expected: CheckpointRef,
        got: CheckpointRef,
    },
    SectionOrdinalMismatch {
        expected: u32,
        got: u32,
    },
    SectionCountMismatchForPayloads {
        expected: u32,
        got: u32,
    },
    SectionKindMismatch {
        ordinal: u32,
        expected: CheckpointSectionKind,
        got: CheckpointSectionKind,
    },
    TrailingBytes {
        name: String,
        offset_bytes: u64,
    },
    Close {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: CheckpointImageLoadPhase,
        result: &'static str,
    },
}

impl fmt::Display for CheckpointImagePublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImagePublishError::Encode(error) => {
                write!(f, "encode checkpoint image failed: {error}")
            }
            CheckpointImagePublishError::ScratchNotEmpty { len } => {
                write!(f, "checkpoint image publish scratch completions not empty: {len}")
            }
            CheckpointImagePublishError::ZeroReapLimit => {
                write!(f, "checkpoint image publish reap limit must be nonzero")
            }
            CheckpointImagePublishError::SectionCountTooLarge { count, max_count } => {
                write!(f, "checkpoint image publish has {count} sections, above max {max_count}")
            }
            CheckpointImagePublishError::SectionCountMismatch { header, sections } => write!(
                f,
                "checkpoint image header declares {header} sections, publish received {sections}"
            ),
            CheckpointImagePublishError::SectionOrdinalMismatch { expected, got } => write!(
                f,
                "checkpoint image section ordinal {got} does not match expected {expected}"
            ),
            CheckpointImagePublishError::WriteBufferUnavailable => {
                write!(f, "checkpoint image publish could not lease a write buffer")
            }
            CheckpointImagePublishError::Backend { phase, source } => {
                write!(f, "checkpoint image publish backend failed during {phase}: {source}")
            }
            CheckpointImagePublishError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "checkpoint image publish {phase} expected {expected} completion(s), got {got}"
                )
            }
            CheckpointImagePublishError::UnexpectedToken { phase, expected, got } => write!(
                f,
                "checkpoint image publish {phase} got token {got:?}, expected {expected:?}"
            ),
            CheckpointImagePublishError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "checkpoint image publish {phase} saw no completion for {token:?} after {attempts} reap attempts"
            ),
            CheckpointImagePublishError::OpenTemp { name, errno } => {
                write!(f, "open checkpoint temp file {name:?} failed with errno {errno}")
            }
            CheckpointImagePublishError::WriteTemp { fd, offset_bytes, errno } => write!(
                f,
                "write checkpoint temp fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            CheckpointImagePublishError::MissingWriteBuffer { fd, offset_bytes, errno } => write!(
                f,
                "write checkpoint temp fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            CheckpointImagePublishError::SyncTemp { fd, errno } => {
                write!(f, "sync checkpoint temp fd {fd} failed with errno {errno}")
            }
            CheckpointImagePublishError::CloseTemp { fd, errno } => {
                write!(f, "close checkpoint temp fd {fd} failed with errno {errno}")
            }
            CheckpointImagePublishError::Rename { old_name, new_name, errno } => {
                write!(
                    f,
                    "rename checkpoint {old_name:?} to {new_name:?} failed with errno {errno}"
                )
            }
            CheckpointImagePublishError::UnlinkTemp { name, errno } => {
                write!(f, "unlink checkpoint temp file {name:?} failed with errno {errno}")
            }
            CheckpointImagePublishError::SyncDir { fd, errno } => {
                write!(f, "sync checkpoint directory fd {fd} failed with errno {errno}")
            }
            CheckpointImagePublishError::UnexpectedCompletionKind { phase, result } => write!(
                f,
                "checkpoint image publish {phase} got unexpected completion kind {result}"
            ),
        }
    }
}

impl fmt::Display for CheckpointImageLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImageLoadError::Decode(error) => {
                write!(f, "decode checkpoint image failed: {error}")
            }
            CheckpointImageLoadError::ScratchNotEmpty { len } => {
                write!(f, "checkpoint image load scratch completions not empty: {len}")
            }
            CheckpointImageLoadError::ZeroReapLimit => {
                write!(f, "checkpoint image load reap limit must be nonzero")
            }
            CheckpointImageLoadError::ReadBufferUnavailable => {
                write!(f, "checkpoint image load could not lease a read buffer")
            }
            CheckpointImageLoadError::Backend { phase, source } => {
                write!(f, "checkpoint image load backend failed during {phase}: {source}")
            }
            CheckpointImageLoadError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "checkpoint image load {phase} expected {expected} completion(s), got {got}"
                )
            }
            CheckpointImageLoadError::UnexpectedToken { phase, expected, got } => {
                write!(f, "checkpoint image load {phase} got token {got:?}, expected {expected:?}")
            }
            CheckpointImageLoadError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "checkpoint image load {phase} saw no completion for {token:?} after {attempts} reap attempts"
            ),
            CheckpointImageLoadError::Missing { name } => {
                write!(f, "checkpoint image {name:?} is missing")
            }
            CheckpointImageLoadError::Open { name, errno } => {
                write!(f, "open checkpoint image {name:?} failed with errno {errno}")
            }
            CheckpointImageLoadError::Read { fd, offset_bytes, errno } => write!(
                f,
                "read checkpoint image fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            CheckpointImageLoadError::MissingReadBuffer { fd, offset_bytes, errno } => write!(
                f,
                "read checkpoint image fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            CheckpointImageLoadError::ReadLenTooLarge {
                fd,
                offset_bytes,
                len,
                requested_len,
                buffer_len_bytes,
            } => write!(
                f,
                "read checkpoint image fd {fd} at offset {offset_bytes} returned {len} bytes, requested {requested_len}, buffer size {buffer_len_bytes}"
            ),
            CheckpointImageLoadError::UnexpectedEof { part, needed, got } => {
                write!(f, "checkpoint image ended during {part}: got {got} bytes, needed {needed}")
            }
            CheckpointImageLoadError::CheckpointMismatch { expected, got } => {
                write!(f, "checkpoint image identity {got:?} does not match requested {expected:?}")
            }
            CheckpointImageLoadError::SectionOrdinalMismatch { expected, got } => write!(
                f,
                "checkpoint image section ordinal {got} does not match expected {expected}"
            ),
            CheckpointImageLoadError::SectionCountMismatchForPayloads { expected, got } => {
                write!(f, "checkpoint image payload load expected {expected} sections, got {got}")
            }
            CheckpointImageLoadError::SectionKindMismatch { ordinal, expected, got } => write!(
                f,
                "checkpoint image section {ordinal} kind {got:?} does not match expected {expected:?}"
            ),
            CheckpointImageLoadError::TrailingBytes { name, offset_bytes } => write!(
                f,
                "checkpoint image {name:?} has trailing bytes after offset {offset_bytes}"
            ),
            CheckpointImageLoadError::Close { fd, errno } => {
                write!(f, "close checkpoint image fd {fd} failed with errno {errno}")
            }
            CheckpointImageLoadError::UnexpectedCompletionKind { phase, result } => {
                write!(f, "checkpoint image load {phase} got unexpected completion kind {result}")
            }
        }
    }
}

impl std::error::Error for CheckpointImagePublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointImagePublishError::Encode(error) => Some(error),
            CheckpointImagePublishError::Backend { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl std::error::Error for CheckpointImageLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointImageLoadError::Decode(error) => Some(error),
            CheckpointImageLoadError::Backend { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn publish_checkpoint_image<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    header: CheckpointHeader<'_>,
    sections: &[CheckpointSectionRef<'_>],
    config: CheckpointImagePublishConfig,
    completions: &mut Vec<Completion>,
) -> Result<PublishedCheckpointImage, CheckpointImagePublishError>
where
    D: BackendDriver,
{
    validate_publish_inputs(completions, config.max_reaps)?;
    validate_publish_sections(header, sections)?;

    let final_name = header.checkpoint().id().file_name();
    let temp_name = format!("{final_name}.tmp");
    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = PublishIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        dir: config.dir,
        offset_bytes: 0,
    };
    io.publish_image(header, sections, &temp_name, &final_name)
}

pub fn load_checkpoint_image<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    checkpoint: CheckpointRef,
    config: CheckpointImageLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<LoadedCheckpointImage, CheckpointImageLoadError>
where
    D: BackendDriver,
{
    validate_load_inputs(completions, config.max_reaps)?;

    let name = checkpoint.id().file_name();
    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = LoadIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        offset_bytes: 0,
    };
    io.load_from_dir(config.dir, checkpoint, &name)
}

pub fn load_checkpoint_image_payloads<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    checkpoint: CheckpointRef,
    config: CheckpointImageLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<LoadedCheckpointImagePayloads, CheckpointImageLoadError>
where
    D: BackendDriver,
{
    validate_load_inputs(completions, config.max_reaps)?;

    let name = checkpoint.id().file_name();
    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = LoadIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        offset_bytes: 0,
    };
    io.load_payloads_from_dir(config.dir, checkpoint, &name)
}

/// Encode one checkpoint records-section payload from a bounded store walk.
///
/// The payload is a sequence of ordinary length-prefixed mutation log records,
/// without a log frame header/trailer. The surrounding `.ick` section supplies
/// CRC coverage; recovery can decode each payload record through the normal
/// mutation record path. `out` is cleared before encoding and left empty on
/// error.
pub fn encode_checkpoint_records_section_payload(
    store: &CellStore,
    config: CheckpointRecordsSectionConfig,
    out: &mut Vec<u8>,
) -> Result<EncodedCheckpointRecordsSection, CheckpointRecordsSectionError> {
    out.clear();
    if config.max_payload_bytes == 0 {
        return Err(CheckpointRecordsSectionError::ZeroPayloadBudget);
    }
    if config.max_payload_bytes > MAX_CHECKPOINT_SECTION_PAYLOAD_LEN {
        return Err(CheckpointRecordsSectionError::PayloadBudgetTooLarge {
            max_payload_bytes: config.max_payload_bytes,
            max_allowed: MAX_CHECKPOINT_SECTION_PAYLOAD_LEN,
        });
    }

    let mut staging = LogStaging::with_capacity(config.max_payload_bytes)
        .map_err(MutationStageError::Log)
        .map_err(CheckpointRecordsSectionError::Stage)?;
    let mut stage_error = None;
    let walk = store
        .checkpoint_walk(config.cursor, config.walk_budget, config.now, |record| {
            if stage_error.is_some() {
                return;
            }
            if let Err(error) = stage_mutation_effect(
                &mut staging,
                config.namespace,
                checkpoint_store_record_effect(record),
            ) {
                stage_error = Some(error);
            }
        })
        .map_err(CheckpointRecordsSectionError::Walk)?;

    if let Some(error) = stage_error {
        out.clear();
        return Err(CheckpointRecordsSectionError::Stage(error));
    }

    out.extend_from_slice(staging.record_bytes());
    Ok(EncodedCheckpointRecordsSection { walk, payload_len: out.len() })
}

/// Encode complete keyspace checkpoint section payloads for a small/cold
/// snapshot composition.
///
/// This is not the final M2 checkpoint scheduler. It builds the namespace set,
/// namespace-catalog section payload, and records-section payload for the
/// currently materialized stores in deterministic namespace order. `records`
/// is bounded by `config.max_records_payload_bytes`; on any error all output
/// buffers are cleared so callers cannot accidentally publish a partial image.
pub fn encode_checkpoint_keyspace_snapshot_sections(
    keyspace: &Keyspace,
    config: CheckpointKeyspaceSnapshotConfig,
    namespaces: &mut Vec<NamespaceId>,
    catalog: &mut Vec<u8>,
    records: &mut Vec<u8>,
) -> Result<EncodedCheckpointKeyspaceSnapshot, CheckpointKeyspaceSnapshotError> {
    namespaces.clear();
    catalog.clear();
    records.clear();

    if let Err(error) = collect_checkpoint_namespaces(keyspace, namespaces) {
        catalog.clear();
        records.clear();
        return Err(error);
    }
    let catalog_snapshot = keyspace.ns_catalog_snapshot();
    if let Err(error) = encode_namespace_catalog(&catalog_snapshot, catalog) {
        namespaces.clear();
        catalog.clear();
        records.clear();
        return Err(CheckpointKeyspaceSnapshotError::Catalog(error));
    }

    let mut scratch = Vec::new();
    let mut default_stores_walked = 0usize;
    let mut named_stores_walked = 0usize;
    let mut records_emitted = 0usize;

    for (db, store) in keyspace.dbs() {
        let namespace = NamespaceId::new(db as u32);
        if let Err(error) = append_store_snapshot_records(
            store,
            namespace,
            config,
            records,
            &mut scratch,
            &mut records_emitted,
        ) {
            namespaces.clear();
            catalog.clear();
            records.clear();
            return Err(error);
        }
        default_stores_walked += 1;
    }
    for (id, store) in keyspace.named_dbs() {
        if !checkpoint_named_store_is_durable(keyspace, id) {
            continue;
        }
        let namespace = NamespaceId::new(id.get());
        if let Err(error) = append_store_snapshot_records(
            store,
            namespace,
            config,
            records,
            &mut scratch,
            &mut records_emitted,
        ) {
            namespaces.clear();
            catalog.clear();
            records.clear();
            return Err(error);
        }
        named_stores_walked += 1;
    }

    Ok(EncodedCheckpointKeyspaceSnapshot {
        namespace_count: namespaces.len(),
        default_stores_walked,
        named_stores_walked,
        records_emitted,
        catalog_payload_len: catalog.len(),
        records_payload_len: records.len(),
    })
}

pub fn encode_checkpoint_keyspace_snapshot_parts(
    keyspace: &Keyspace,
    config: CheckpointKeyspaceSnapshotConfig,
) -> Result<EncodedCheckpointKeyspaceSnapshotParts, CheckpointKeyspacePublishError> {
    let mut namespaces = Vec::new();
    let mut catalog = Vec::new();
    let mut records = Vec::new();
    let snapshot = encode_checkpoint_keyspace_snapshot_sections(
        keyspace,
        config,
        &mut namespaces,
        &mut catalog,
        &mut records,
    )
    .map_err(CheckpointKeyspacePublishError::Snapshot)?;
    Ok(EncodedCheckpointKeyspaceSnapshotParts { namespaces, catalog, records, snapshot })
}

pub fn encode_checkpoint_keyspace_snapshot_image_from_parts(
    cell: CellId,
    checkpoint: CheckpointRef,
    parts: &EncodedCheckpointKeyspaceSnapshotParts,
) -> Result<Vec<u8>, CheckpointKeyspacePublishError> {
    let header = CheckpointHeader::new(
        cell,
        checkpoint,
        CHECKPOINT_KEYSPACE_SNAPSHOT_SECTION_COUNT,
        &parts.namespaces,
    )
    .map_err(CheckpointKeyspacePublishError::Header)?;
    let sections = [
        CheckpointSectionRef::new(
            0,
            inf_log::CheckpointSectionKind::NamespaceCatalog,
            &parts.catalog,
        )
        .map_err(CheckpointKeyspacePublishError::Section)?,
        CheckpointSectionRef::new(1, inf_log::CheckpointSectionKind::Records, &parts.records)
            .map_err(CheckpointKeyspacePublishError::Section)?,
    ];

    let mut image = Vec::new();
    let mut header_bytes = Vec::new();
    encode_checkpoint_header(header, &mut header_bytes)
        .map_err(CheckpointKeyspacePublishError::Header)?;
    image.extend_from_slice(&header_bytes);

    let mut digest = header.digest();
    for section in sections {
        let frame = encode_checkpoint_section_frame_parts(section)
            .map_err(CheckpointKeyspacePublishError::Section)?;
        image.extend_from_slice(frame.header());
        image.extend_from_slice(section.payload());
        image.extend_from_slice(frame.trailer());
        digest.update_section(frame.meta());
    }

    let footer = CheckpointFooter::new(sections.len() as u32, digest);
    let mut footer_bytes = Vec::new();
    encode_checkpoint_footer(footer, &mut footer_bytes);
    image.extend_from_slice(&footer_bytes);
    Ok(image)
}

pub fn publish_checkpoint_keyspace_snapshot_image<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    cell: CellId,
    checkpoint: CheckpointRef,
    keyspace: &Keyspace,
    config: CheckpointKeyspacePublishConfig,
    completions: &mut Vec<Completion>,
) -> Result<PublishedCheckpointKeyspaceSnapshot, CheckpointKeyspacePublishError>
where
    D: BackendDriver,
{
    let mut namespaces = Vec::new();
    let mut catalog = Vec::new();
    let mut records = Vec::new();
    let snapshot = encode_checkpoint_keyspace_snapshot_sections(
        keyspace,
        config.snapshot,
        &mut namespaces,
        &mut catalog,
        &mut records,
    )
    .map_err(CheckpointKeyspacePublishError::Snapshot)?;
    let header = CheckpointHeader::new(cell, checkpoint, 2, &namespaces)
        .map_err(CheckpointKeyspacePublishError::Header)?;
    let sections = [
        CheckpointSectionRef::new(0, inf_log::CheckpointSectionKind::NamespaceCatalog, &catalog)
            .map_err(CheckpointKeyspacePublishError::Section)?,
        CheckpointSectionRef::new(1, inf_log::CheckpointSectionKind::Records, &records)
            .map_err(CheckpointKeyspacePublishError::Section)?,
    ];
    let image =
        publish_checkpoint_image(driver, pool, header, &sections, config.image, completions)
            .map_err(CheckpointKeyspacePublishError::Publish)?;
    Ok(PublishedCheckpointKeyspaceSnapshot { image, snapshot })
}

fn checkpoint_store_record_effect(record: CheckpointStoreRecord<'_>) -> MutationEffect<'_> {
    MutationEffect::StringPostImage {
        key: record.key,
        value: record.value,
        expire_at_ms: record.expire_at_ms,
        raw: record.raw,
    }
}

fn collect_checkpoint_namespaces(
    keyspace: &Keyspace,
    namespaces: &mut Vec<NamespaceId>,
) -> Result<(), CheckpointKeyspaceSnapshotError> {
    for (db, _) in keyspace.dbs() {
        namespaces.push(NamespaceId::new(db as u32));
    }
    for spec in keyspace.ns_catalog_snapshot().specs() {
        namespaces.push(NamespaceId::new(spec.id.get()));
    }
    namespaces.sort_by_key(|namespace| namespace.get());
    namespaces.dedup();
    if namespaces.len() > MAX_CHECKPOINT_HEADER_NAMESPACES {
        let count = namespaces.len();
        namespaces.clear();
        return Err(CheckpointKeyspaceSnapshotError::NamespaceSetTooLarge {
            count,
            max_count: MAX_CHECKPOINT_HEADER_NAMESPACES,
        });
    }
    Ok(())
}

fn checkpoint_named_store_is_durable(keyspace: &Keyspace, id: NsId) -> bool {
    keyspace.ns_get_by_id(id).is_some_and(|spec| spec.mode == NsMode::Durable)
}

fn append_store_snapshot_records(
    store: &CellStore,
    namespace: NamespaceId,
    config: CheckpointKeyspaceSnapshotConfig,
    records: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    records_emitted: &mut usize,
) -> Result<(), CheckpointKeyspaceSnapshotError> {
    if store.is_empty() {
        return Ok(());
    }
    let Some(remaining) = config.max_records_payload_bytes.checked_sub(records.len()) else {
        records.clear();
        scratch.clear();
        return Err(CheckpointKeyspaceSnapshotError::Records {
            namespace,
            source: CheckpointRecordsSectionError::PayloadBudgetTooLarge {
                max_payload_bytes: records.len(),
                max_allowed: config.max_records_payload_bytes,
            },
        });
    };
    let section_config = CheckpointRecordsSectionConfig::new(
        namespace,
        0,
        CheckpointWalkBudget::new(usize::MAX),
        config.now,
    )
    .with_max_payload_bytes(remaining);
    let encoded = encode_checkpoint_records_section_payload(store, section_config, scratch)
        .map_err(|source| CheckpointKeyspaceSnapshotError::Records { namespace, source })?;
    let walk = encoded.walk();
    if !walk.done {
        records.clear();
        scratch.clear();
        return Err(CheckpointKeyspaceSnapshotError::IncompleteStoreWalk {
            namespace,
            next_cursor: walk.next_cursor,
        });
    }
    records.extend_from_slice(scratch);
    *records_emitted += walk.records_emitted;
    Ok(())
}

fn validate_publish_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), CheckpointImagePublishError> {
    if !completions.is_empty() {
        return Err(CheckpointImagePublishError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(CheckpointImagePublishError::ZeroReapLimit);
    }
    Ok(())
}

fn validate_load_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), CheckpointImageLoadError> {
    if !completions.is_empty() {
        return Err(CheckpointImageLoadError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(CheckpointImageLoadError::ZeroReapLimit);
    }
    Ok(())
}

fn validate_publish_sections(
    header: CheckpointHeader<'_>,
    sections: &[CheckpointSectionRef<'_>],
) -> Result<(), CheckpointImagePublishError> {
    if sections.len() > MAX_CHECKPOINT_IMAGE_SECTIONS as usize {
        return Err(CheckpointImagePublishError::SectionCountTooLarge {
            count: sections.len(),
            max_count: MAX_CHECKPOINT_IMAGE_SECTIONS,
        });
    }
    if header.section_count() as usize != sections.len() {
        return Err(CheckpointImagePublishError::SectionCountMismatch {
            header: header.section_count(),
            sections: sections.len(),
        });
    }
    for (expected, section) in sections.iter().enumerate() {
        let expected = expected as u32;
        if section.ordinal() != expected {
            return Err(CheckpointImagePublishError::SectionOrdinalMismatch {
                expected,
                got: section.ordinal(),
            });
        }
    }
    Ok(())
}

#[derive(Copy, Clone, Debug)]
struct ReapParams {
    wait: Wait,
    max_reaps: u32,
}

struct PublishIo<'a, D> {
    driver: &'a mut D,
    pool: &'a mut BufferPool,
    completions: &'a mut Vec<Completion>,
    params: ReapParams,
    token: CompletionToken,
    dir: RawFd,
    offset_bytes: u64,
}

struct LoadIo<'a, D> {
    driver: &'a mut D,
    pool: &'a mut BufferPool,
    completions: &'a mut Vec<Completion>,
    params: ReapParams,
    token: CompletionToken,
    offset_bytes: u64,
}

struct LoadedCheckpointHeader {
    cell: CellId,
    checkpoint: CheckpointRef,
    namespaces: Vec<NamespaceId>,
    section_count: u32,
    digest: CheckpointDigest,
}

impl<D> PublishIo<'_, D>
where
    D: BackendDriver,
{
    fn publish_image(
        &mut self,
        header: CheckpointHeader<'_>,
        sections: &[CheckpointSectionRef<'_>],
        temp_name: &str,
        final_name: &str,
    ) -> Result<PublishedCheckpointImage, CheckpointImagePublishError> {
        let fd = self.open_temp(temp_name)?;
        let write_result = self.write_image(fd, header, sections);
        let close_result = self.close_temp(fd);
        let footer = match (write_result, close_result) {
            (Ok(footer), Ok(())) => footer,
            (Err(error), _) => {
                let _ = self.unlink_temp(temp_name);
                return Err(error);
            }
            (Ok(_), Err(error)) => {
                let _ = self.unlink_temp(temp_name);
                return Err(error);
            }
        };
        if let Err(error) = self.rename_temp(temp_name, final_name) {
            let _ = self.unlink_temp(temp_name);
            return Err(error);
        }
        self.sync_dir()?;
        Ok(PublishedCheckpointImage {
            checkpoint: header.checkpoint(),
            footer,
            bytes_written: self.offset_bytes,
        })
    }

    fn write_image(
        &mut self,
        fd: RawFd,
        header: CheckpointHeader<'_>,
        sections: &[CheckpointSectionRef<'_>],
    ) -> Result<CheckpointFooter, CheckpointImagePublishError> {
        let mut header_bytes = Vec::new();
        encode_checkpoint_header(header, &mut header_bytes)
            .map_err(CheckpointImagePublishError::Encode)?;
        self.write_bytes(fd, &header_bytes)?;

        let mut digest = header.digest();
        for section in sections {
            let parts = encode_checkpoint_section_frame_parts(*section)
                .map_err(CheckpointImagePublishError::Encode)?;
            self.write_bytes(fd, parts.header())?;
            self.write_bytes(fd, section.payload())?;
            self.write_bytes(fd, parts.trailer())?;
            digest.update_section(parts.meta());
        }

        let footer = CheckpointFooter::new(sections.len() as u32, digest);
        let mut footer_bytes = Vec::new();
        encode_checkpoint_footer(footer, &mut footer_bytes);
        self.write_bytes(fd, &footer_bytes)?;
        self.sync_temp(fd)?;
        Ok(footer)
    }

    fn open_temp(&mut self, temp_name: &str) -> Result<RawFd, CheckpointImagePublishError> {
        self.driver.push(IoOp::FileOpen {
            dir: self.dir,
            name: temp_name.to_string(),
            mode: FileOpenMode::ReadWriteCreateTruncate,
            token: self.token,
        });
        match self.reap(CheckpointImagePublishPhase::OpenTemp)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::OpenTemp { name: temp_name.to_string(), errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::OpenTemp,
                other,
            )),
        }
    }

    fn write_bytes(&mut self, fd: RawFd, bytes: &[u8]) -> Result<(), CheckpointImagePublishError> {
        let mut offset = 0usize;
        while offset < bytes.len() {
            let chunk_len = self.pool.buf_size().min(bytes.len() - offset);
            self.write_chunk(fd, &bytes[offset..offset + chunk_len])?;
            self.offset_bytes += chunk_len as u64;
            offset += chunk_len;
        }
        Ok(())
    }

    fn write_chunk(&mut self, fd: RawFd, chunk: &[u8]) -> Result<(), CheckpointImagePublishError> {
        let Some(buf) = self.pool.try_lease(LeaseKind::Send) else {
            return Err(CheckpointImagePublishError::WriteBufferUnavailable);
        };
        self.pool.bytes_mut(buf)[..chunk.len()].copy_from_slice(chunk);
        let offset_bytes = self.offset_bytes;
        self.driver.push(IoOp::FileWriteAt {
            fd,
            offset_bytes,
            buf,
            len: chunk.len() as u32,
            token: self.token,
        });
        match self.reap(CheckpointImagePublishPhase::WriteTemp)?.result {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf, "checkpoint write completion returned the wrong buffer");
                self.pool.release(got);
                Ok(())
            }
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "checkpoint write error returned the wrong buffer");
                self.pool.release(got);
                Err(CheckpointImagePublishError::WriteTemp { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::MissingWriteBuffer { fd, offset_bytes, errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::WriteTemp,
                other,
            )),
        }
    }

    fn sync_temp(&mut self, fd: RawFd) -> Result<(), CheckpointImagePublishError> {
        self.driver.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: self.token });
        match self.reap(CheckpointImagePublishPhase::SyncTemp)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::SyncTemp { fd, errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::SyncTemp,
                other,
            )),
        }
    }

    fn close_temp(&mut self, fd: RawFd) -> Result<(), CheckpointImagePublishError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(CheckpointImagePublishPhase::CloseTemp)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::CloseTemp { fd, errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::CloseTemp,
                other,
            )),
        }
    }

    fn rename_temp(
        &mut self,
        temp_name: &str,
        final_name: &str,
    ) -> Result<(), CheckpointImagePublishError> {
        self.driver.push(IoOp::FileRename {
            old_dir: self.dir,
            old_name: temp_name.to_string(),
            new_dir: self.dir,
            new_name: final_name.to_string(),
            token: self.token,
        });
        match self.reap(CheckpointImagePublishPhase::Rename)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::Rename {
                    old_name: temp_name.to_string(),
                    new_name: final_name.to_string(),
                    errno,
                })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::Rename,
                other,
            )),
        }
    }

    fn unlink_temp(&mut self, temp_name: &str) -> Result<(), CheckpointImagePublishError> {
        self.driver.push(IoOp::FileUnlink {
            dir: self.dir,
            name: temp_name.to_string(),
            token: self.token,
        });
        match self.reap(CheckpointImagePublishPhase::UnlinkTemp)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::UnlinkTemp { name: temp_name.to_string(), errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::UnlinkTemp,
                other,
            )),
        }
    }

    fn sync_dir(&mut self) -> Result<(), CheckpointImagePublishError> {
        self.driver.push(IoOp::FileSync {
            fd: self.dir,
            mode: FileSyncMode::Full,
            token: self.token,
        });
        match self.reap(CheckpointImagePublishPhase::SyncDir)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImagePublishError::SyncDir { fd: self.dir, errno })
            }
            other => Err(unexpected_publish_completion_kind(
                self.pool,
                CheckpointImagePublishPhase::SyncDir,
                other,
            )),
        }
    }

    fn reap(
        &mut self,
        phase: CheckpointImagePublishPhase,
    ) -> Result<Completion, CheckpointImagePublishError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| CheckpointImagePublishError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(CheckpointImagePublishError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                release_result_buffer(self.pool, &completion.result);
                return Err(CheckpointImagePublishError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(CheckpointImagePublishError::ReapLimitExceeded {
            phase,
            token: self.token,
            attempts: self.params.max_reaps,
        })
    }
}

impl<D> LoadIo<'_, D>
where
    D: BackendDriver,
{
    fn load_from_dir(
        &mut self,
        dir: RawFd,
        checkpoint: CheckpointRef,
        name: &str,
    ) -> Result<LoadedCheckpointImage, CheckpointImageLoadError> {
        let fd = self.open_checkpoint(dir, name)?;
        let read = self.read_summary(fd, checkpoint, name);
        let close = self.close_checkpoint(fd);
        match (read, close) {
            (Ok(image), Ok(())) => Ok(image),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn load_payloads_from_dir(
        &mut self,
        dir: RawFd,
        checkpoint: CheckpointRef,
        name: &str,
    ) -> Result<LoadedCheckpointImagePayloads, CheckpointImageLoadError> {
        let fd = self.open_checkpoint(dir, name)?;
        let read = self.read_payloads(fd, checkpoint, name);
        let close = self.close_checkpoint(fd);
        match (read, close) {
            (Ok(image), Ok(())) => Ok(image),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn open_checkpoint(
        &mut self,
        dir: RawFd,
        name: &str,
    ) -> Result<RawFd, CheckpointImageLoadError> {
        self.driver.push(IoOp::FileOpen {
            dir,
            name: name.to_string(),
            mode: FileOpenMode::ReadOnly,
            token: self.token,
        });
        match self.reap(CheckpointImageLoadPhase::Open)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno: ENOENT_ERRNO, buf: None } => {
                Err(CheckpointImageLoadError::Missing { name: name.to_string() })
            }
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImageLoadError::Open { name: name.to_string(), errno })
            }
            other => Err(unexpected_load_completion_kind(
                self.pool,
                CheckpointImageLoadPhase::Open,
                other,
            )),
        }
    }

    fn read_summary(
        &mut self,
        fd: RawFd,
        expected_checkpoint: CheckpointRef,
        name: &str,
    ) -> Result<LoadedCheckpointImage, CheckpointImageLoadError> {
        let header = self.read_header(fd, expected_checkpoint)?;
        let mut digest = header.digest;
        let mut sections = Vec::with_capacity(header.section_count as usize);
        for expected in 0..header.section_count {
            let section = self.read_section(fd, expected)?;
            digest.update_section(section);
            sections.push(section);
        }
        let footer = self.read_footer(fd, name, header.section_count, digest)?;

        Ok(LoadedCheckpointImage {
            cell: header.cell,
            checkpoint: header.checkpoint,
            namespaces: header.namespaces,
            sections,
            footer,
            bytes_read: self.offset_bytes,
        })
    }

    fn read_payloads(
        &mut self,
        fd: RawFd,
        expected_checkpoint: CheckpointRef,
        name: &str,
    ) -> Result<LoadedCheckpointImagePayloads, CheckpointImageLoadError> {
        let header = self.read_header(fd, expected_checkpoint)?;
        let mut digest = header.digest;
        let mut sections = Vec::with_capacity(header.section_count as usize);
        let mut payloads = Vec::with_capacity(header.section_count as usize);
        for expected in 0..header.section_count {
            let (section, payload) = self.read_section_payload(fd, expected)?;
            digest.update_section(section);
            sections.push(section);
            payloads.push(payload);
        }
        let footer = self.read_footer(fd, name, header.section_count, digest)?;
        if sections.len() != CHECKPOINT_KEYSPACE_SNAPSHOT_SECTION_COUNT as usize {
            return Err(CheckpointImageLoadError::SectionCountMismatchForPayloads {
                expected: CHECKPOINT_KEYSPACE_SNAPSHOT_SECTION_COUNT,
                got: sections.len() as u32,
            });
        }
        validate_loaded_section_kind(&sections, 0, CheckpointSectionKind::NamespaceCatalog)?;
        validate_loaded_section_kind(&sections, 1, CheckpointSectionKind::Records)?;

        let mut payloads = payloads.into_iter();
        let namespace_catalog = payloads.next().expect("validated namespace-catalog payload");
        let records = payloads.next().expect("validated records payload");
        Ok(LoadedCheckpointImagePayloads {
            image: LoadedCheckpointImage {
                cell: header.cell,
                checkpoint: header.checkpoint,
                namespaces: header.namespaces,
                sections,
                footer,
                bytes_read: self.offset_bytes,
            },
            namespace_catalog,
            records,
        })
    }

    fn read_header(
        &mut self,
        fd: RawFd,
        expected_checkpoint: CheckpointRef,
    ) -> Result<LoadedCheckpointHeader, CheckpointImageLoadError> {
        let prefix = self.read_exact(fd, CHECKPOINT_IMAGE_HEADER_FIXED_LEN, "header-prefix")?;
        let header_len =
            checkpoint_header_len_from_prefix(&prefix).map_err(CheckpointImageLoadError::Decode)?;
        let mut header_bytes = prefix;
        if header_len > CHECKPOINT_IMAGE_HEADER_FIXED_LEN {
            let rest =
                self.read_exact(fd, header_len - CHECKPOINT_IMAGE_HEADER_FIXED_LEN, "header")?;
            header_bytes.extend_from_slice(&rest);
        }
        let header =
            decode_checkpoint_header(&header_bytes).map_err(CheckpointImageLoadError::Decode)?;
        if header.checkpoint() != expected_checkpoint {
            return Err(CheckpointImageLoadError::CheckpointMismatch {
                expected: expected_checkpoint,
                got: header.checkpoint(),
            });
        }

        Ok(LoadedCheckpointHeader {
            cell: header.cell(),
            checkpoint: header.checkpoint(),
            namespaces: header.namespaces().collect(),
            section_count: header.section_count(),
            digest: header.digest(),
        })
    }

    fn read_footer(
        &mut self,
        fd: RawFd,
        name: &str,
        section_count: u32,
        digest: CheckpointDigest,
    ) -> Result<CheckpointFooter, CheckpointImageLoadError> {
        let footer_bytes = self.read_exact(fd, CHECKPOINT_FOOTER_LEN, "footer")?;
        let footer =
            decode_checkpoint_footer(&footer_bytes).map_err(CheckpointImageLoadError::Decode)?;
        validate_checkpoint_footer(footer, section_count, digest)
            .map_err(CheckpointImageLoadError::Decode)?;
        if self.read_probe_byte(fd)? {
            return Err(CheckpointImageLoadError::TrailingBytes {
                name: name.to_string(),
                offset_bytes: self.offset_bytes,
            });
        }
        Ok(footer)
    }

    fn read_section(
        &mut self,
        fd: RawFd,
        expected_ordinal: u32,
    ) -> Result<CheckpointSectionMeta, CheckpointImageLoadError> {
        let section = self.read_section_bytes(fd, expected_ordinal)?;
        decode_checkpoint_section(&section)
            .map(|section| section.meta())
            .map_err(CheckpointImageLoadError::Decode)
    }

    fn read_section_payload(
        &mut self,
        fd: RawFd,
        expected_ordinal: u32,
    ) -> Result<(CheckpointSectionMeta, Vec<u8>), CheckpointImageLoadError> {
        let section = self.read_section_bytes(fd, expected_ordinal)?;
        decode_checkpoint_section(&section)
            .map(|section| (section.meta(), section.payload().to_vec()))
            .map_err(CheckpointImageLoadError::Decode)
    }

    fn read_section_bytes(
        &mut self,
        fd: RawFd,
        expected_ordinal: u32,
    ) -> Result<Vec<u8>, CheckpointImageLoadError> {
        let mut section = self.read_exact(fd, CHECKPOINT_SECTION_HEADER_LEN, "section-header")?;
        let header = decode_checkpoint_section_frame_header(&section)
            .map_err(CheckpointImageLoadError::Decode)?;
        if header.ordinal() != expected_ordinal {
            return Err(CheckpointImageLoadError::SectionOrdinalMismatch {
                expected: expected_ordinal,
                got: header.ordinal(),
            });
        }
        let rest_len = header.payload_len() as usize + CHECKPOINT_SECTION_TRAILER_LEN;
        let rest = self.read_exact(fd, rest_len, "section-body")?;
        section.extend_from_slice(&rest);
        Ok(section)
    }

    fn read_exact(
        &mut self,
        fd: RawFd,
        len: usize,
        part: &'static str,
    ) -> Result<Vec<u8>, CheckpointImageLoadError> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let remaining = len - out.len();
            let chunk_len = self.pool.buf_size().min(remaining);
            let chunk = self.read_chunk(fd, chunk_len as u32)?;
            if chunk.is_empty() {
                return Err(CheckpointImageLoadError::UnexpectedEof {
                    part,
                    needed: len,
                    got: out.len(),
                });
            }
            self.offset_bytes += chunk.len() as u64;
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    fn read_probe_byte(&mut self, fd: RawFd) -> Result<bool, CheckpointImageLoadError> {
        let chunk = self.read_chunk(fd, 1)?;
        if chunk.is_empty() {
            Ok(false)
        } else {
            self.offset_bytes += chunk.len() as u64;
            Ok(true)
        }
    }

    fn read_chunk(&mut self, fd: RawFd, len: u32) -> Result<Vec<u8>, CheckpointImageLoadError> {
        debug_assert!(len > 0);
        let Some(buf) = self.pool.try_lease(LeaseKind::Recv) else {
            return Err(CheckpointImageLoadError::ReadBufferUnavailable);
        };
        let offset_bytes = self.offset_bytes;
        self.driver.push(IoOp::FileReadAt { fd, offset_bytes, buf, len, token: self.token });
        match self.reap(CheckpointImageLoadPhase::Read)?.result {
            CompletionResult::FileRead { buf: got, len: got_len } => {
                self.handle_read_completion(fd, offset_bytes, len, buf, got, got_len)
            }
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "checkpoint read error returned the wrong buffer");
                self.pool.release(got);
                Err(CheckpointImageLoadError::Read { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                self.pool.release(buf);
                Err(CheckpointImageLoadError::MissingReadBuffer { fd, offset_bytes, errno })
            }
            other => {
                self.pool.release(buf);
                Err(unexpected_load_completion_kind(
                    self.pool,
                    CheckpointImageLoadPhase::Read,
                    other,
                ))
            }
        }
    }

    fn handle_read_completion(
        &mut self,
        fd: RawFd,
        offset_bytes: u64,
        requested_len: u32,
        expected: BufferId,
        got: BufferId,
        len: u32,
    ) -> Result<Vec<u8>, CheckpointImageLoadError> {
        assert_eq!(got, expected, "checkpoint read completion returned the wrong buffer");
        if len > requested_len || len as usize > self.pool.buf_size() {
            self.pool.release(got);
            return Err(CheckpointImageLoadError::ReadLenTooLarge {
                fd,
                offset_bytes,
                len,
                requested_len,
                buffer_len_bytes: self.pool.buf_size(),
            });
        }
        let bytes = self.pool.bytes(got)[..len as usize].to_vec();
        self.pool.release(got);
        Ok(bytes)
    }

    fn close_checkpoint(&mut self, fd: RawFd) -> Result<(), CheckpointImageLoadError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(CheckpointImageLoadPhase::Close)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(CheckpointImageLoadError::Close { fd, errno })
            }
            other => Err(unexpected_load_completion_kind(
                self.pool,
                CheckpointImageLoadPhase::Close,
                other,
            )),
        }
    }

    fn reap(
        &mut self,
        phase: CheckpointImageLoadPhase,
    ) -> Result<Completion, CheckpointImageLoadError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| CheckpointImageLoadError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(CheckpointImageLoadError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                release_result_buffer(self.pool, &completion.result);
                return Err(CheckpointImageLoadError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(CheckpointImageLoadError::ReapLimitExceeded {
            phase,
            token: self.token,
            attempts: self.params.max_reaps,
        })
    }
}

fn release_result_buffer(pool: &mut BufferPool, result: &CompletionResult) {
    match result {
        CompletionResult::Recv { buf, .. }
        | CompletionResult::Sent { buf }
        | CompletionResult::FileRead { buf, .. }
        | CompletionResult::FileWritten { buf }
        | CompletionResult::Error { buf: Some(buf), .. } => pool.release(*buf),
        CompletionResult::Accepted { .. }
        | CompletionResult::RecvDropped
        | CompletionResult::Closed
        | CompletionResult::FileOpened { .. }
        | CompletionResult::FileDone
        | CompletionResult::FileClosed
        | CompletionResult::Error { buf: None, .. } => {}
    }
}

fn release_live_checkpoint_result_buffer(pool: &mut BufferPool, result: &CompletionResult) {
    release_result_buffer(pool, result);
}

fn unexpected_live_checkpoint_completion(
    pool: &mut BufferPool,
    phase: LiveCheckpointPublishPhase,
    result: CompletionResult,
) -> LiveCheckpointPublishError {
    let name = result_name(&result);
    release_result_buffer(pool, &result);
    LiveCheckpointPublishError::UnexpectedCompletion { phase, result: name }
}

fn unexpected_publish_completion_kind(
    pool: &mut BufferPool,
    phase: CheckpointImagePublishPhase,
    result: CompletionResult,
) -> CheckpointImagePublishError {
    let name = result_name(&result);
    release_result_buffer(pool, &result);
    CheckpointImagePublishError::UnexpectedCompletionKind { phase, result: name }
}

fn unexpected_load_completion_kind(
    pool: &mut BufferPool,
    phase: CheckpointImageLoadPhase,
    result: CompletionResult,
) -> CheckpointImageLoadError {
    let name = result_name(&result);
    release_result_buffer(pool, &result);
    CheckpointImageLoadError::UnexpectedCompletionKind { phase, result: name }
}

fn validate_loaded_section_kind(
    sections: &[CheckpointSectionMeta],
    ordinal: u32,
    expected: CheckpointSectionKind,
) -> Result<(), CheckpointImageLoadError> {
    let section = sections[ordinal as usize];
    if section.kind() != expected {
        return Err(CheckpointImageLoadError::SectionKindMismatch {
            ordinal,
            expected,
            got: section.kind(),
        });
    }
    Ok(())
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

    use crate::durability::decode_mutation_record;
    use inf_foundation::time::Nanos;
    use inf_log::{
        CheckpointId, CheckpointSectionKind, LogStagingError, Lsn, RecordKind,
        decode_checkpoint_footer, decode_checkpoint_header, decode_checkpoint_section,
        decode_checkpoint_section_frame_header, decode_record_sequence, encode_checkpoint_section,
    };
    use inf_runtime::{Capabilities, SubmitStats};
    use inf_store::{
        CellStore, Keyspace, MutationEffect, NsCreateSpec, NsFsyncPolicy, NsMode, SetExpire,
        SetOptions, StoreConfig, decode_namespace_catalog,
    };

    const DIR_FD: RawFd = 40;
    const TEMP_FD: RawFd = 77;
    const CHECKPOINT_FD: RawFd = 78;
    const TOKEN_SLOT: u32 = 17;
    const TOKEN_GEN: u32 = 3;
    const TEST_EIO: i32 = 5;
    const NOW: Nanos = Nanos(1_000_000);

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum ObservedOp {
        Open {
            dir: RawFd,
            name: String,
            mode: FileOpenMode,
            token: CompletionToken,
        },
        WriteAt {
            fd: RawFd,
            offset_bytes: u64,
            len: u32,
            token: CompletionToken,
        },
        ReadAt {
            fd: RawFd,
            offset_bytes: u64,
            len: u32,
            token: CompletionToken,
        },
        Sync {
            fd: RawFd,
            mode: FileSyncMode,
            token: CompletionToken,
        },
        Close {
            fd: RawFd,
            token: CompletionToken,
        },
        Unlink {
            dir: RawFd,
            name: String,
            token: CompletionToken,
        },
        Rename {
            old_dir: RawFd,
            old_name: String,
            new_dir: RawFd,
            new_name: String,
            token: CompletionToken,
        },
    }

    #[derive(Debug, Default)]
    struct TestDriver {
        ops: Vec<IoOp>,
        observed: Vec<ObservedOp>,
        written: Vec<u8>,
        file_bytes: Vec<u8>,
        open_errno: Option<i32>,
        read_errno: Option<i32>,
        write_errno: Option<i32>,
        unlink_errno: Option<i32>,
        close_errno: Option<i32>,
        stats: SubmitStats,
    }

    impl BackendDriver for TestDriver {
        fn push(&mut self, op: IoOp) {
            self.ops.push(op);
        }

        fn submit_and_reap(
            &mut self,
            pool: &mut BufferPool,
            _wait: Wait,
            out: &mut Vec<Completion>,
        ) -> io::Result<usize> {
            let submitted = self.ops.len() as u64;
            let before = out.len();
            for op in core::mem::take(&mut self.ops) {
                match op {
                    IoOp::FileOpen { dir, name, mode, token } => {
                        self.observed.push(ObservedOp::Open { dir, name, mode, token });
                        let result = match self.open_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => {
                                let fd = if mode == FileOpenMode::ReadOnly {
                                    CHECKPOINT_FD
                                } else {
                                    TEMP_FD
                                };
                                CompletionResult::FileOpened { fd }
                            }
                        };
                        out.push(Completion { token, result });
                    }
                    IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                        self.observed.push(ObservedOp::WriteAt { fd, offset_bytes, len, token });
                        let result = match self.write_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: Some(buf) },
                            None => {
                                let start = offset_bytes as usize;
                                let end = start + len as usize;
                                if end > self.written.len() {
                                    self.written.resize(end, 0);
                                }
                                self.written[start..end]
                                    .copy_from_slice(&pool.bytes(buf)[..len as usize]);
                                CompletionResult::FileWritten { buf }
                            }
                        };
                        out.push(Completion { token, result });
                    }
                    IoOp::FileReadAt { fd, offset_bytes, buf, len, token } => {
                        self.observed.push(ObservedOp::ReadAt { fd, offset_bytes, len, token });
                        let result = match self.read_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: Some(buf) },
                            None => {
                                let start = offset_bytes as usize;
                                let available = self.file_bytes.get(start..).unwrap_or(&[]);
                                let read_len = available.len().min(len as usize);
                                pool.bytes_mut(buf)[..read_len]
                                    .copy_from_slice(&available[..read_len]);
                                CompletionResult::FileRead { buf, len: read_len as u32 }
                            }
                        };
                        out.push(Completion { token, result });
                    }
                    IoOp::FileSync { fd, mode, token } => {
                        self.observed.push(ObservedOp::Sync { fd, mode, token });
                        out.push(Completion { token, result: CompletionResult::FileDone });
                    }
                    IoOp::FileClose { fd, token } => {
                        self.observed.push(ObservedOp::Close { fd, token });
                        let result = match self.close_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileClosed,
                        };
                        out.push(Completion { token, result });
                    }
                    IoOp::FileUnlink { dir, name, token } => {
                        self.observed.push(ObservedOp::Unlink { dir, name, token });
                        let result = match self.unlink_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token, result });
                    }
                    IoOp::FileRename { old_dir, old_name, new_dir, new_name, token } => {
                        self.observed.push(ObservedOp::Rename {
                            old_dir,
                            old_name,
                            new_dir,
                            new_name,
                            token,
                        });
                        out.push(Completion { token, result: CompletionResult::FileDone });
                    }
                    other => panic!("unexpected op {other:?}"),
                }
            }
            let produced = out.len() - before;
            self.stats = SubmitStats { syscalls: 1, sqes: submitted, cqes: produced as u64 };
            Ok(produced)
        }

        fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
            Ok(())
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                backend: "test",
                multishot_accept: false,
                multishot_recv: false,
                provided_buffers: false,
                fixed_buffers: false,
                single_issuer: false,
                defer_taskrun: false,
                performance_tier: false,
            }
        }

        fn submit_stats(&self) -> SubmitStats {
            self.stats
        }
    }

    fn checkpoint() -> CheckpointRef {
        CheckpointRef::new(CheckpointId::new(9).unwrap(), Lsn::new(3, 128))
    }

    fn token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, TOKEN_SLOT, TOKEN_GEN)
    }

    fn publish_config() -> CheckpointImagePublishConfig {
        CheckpointImagePublishConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn load_config() -> CheckpointImageLoadConfig {
        CheckpointImageLoadConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn namespaces() -> [NamespaceId; 2] {
        [NamespaceId::new(1), NamespaceId::new(9)]
    }

    fn sections<'a>(catalog: &'a [u8], records: &'a [u8]) -> [CheckpointSectionRef<'a>; 2] {
        [
            CheckpointSectionRef::new(0, CheckpointSectionKind::NamespaceCatalog, catalog).unwrap(),
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, records).unwrap(),
        ]
    }

    fn expected_image(
        header: CheckpointHeader<'_>,
        sections: &[CheckpointSectionRef<'_>],
    ) -> (Vec<u8>, CheckpointFooter, Vec<CheckpointSectionMeta>) {
        let mut image = Vec::new();
        encode_checkpoint_header(header, &mut image).unwrap();
        let mut digest = header.digest();
        let mut metas = Vec::new();
        for section in sections {
            let mut bytes = Vec::new();
            let meta = encode_checkpoint_section(*section, &mut bytes).unwrap();
            digest.update_section(meta);
            metas.push(meta);
            image.extend_from_slice(&bytes);
        }
        let footer = CheckpointFooter::new(sections.len() as u32, digest);
        let mut bytes = Vec::new();
        encode_checkpoint_footer(footer, &mut bytes);
        image.extend_from_slice(&bytes);
        (image, footer, metas)
    }

    #[test]
    fn records_section_payload_encodes_live_store_records_as_mutation_records() {
        let mut store = CellStore::new(StoreConfig::default());
        store.set(b"a", b"one", SetOptions::default(), NOW).unwrap();
        store
            .set(
                b"ttl",
                b"two",
                SetOptions { expire: SetExpire::At(Nanos(5_000_000)), ..Default::default() },
                NOW,
            )
            .unwrap();
        let namespace = NamespaceId::new(42);
        let config = CheckpointRecordsSectionConfig::new(
            namespace,
            0,
            CheckpointWalkBudget::new(usize::MAX),
            NOW,
        );
        let mut payload = vec![0xff];

        let encoded =
            encode_checkpoint_records_section_payload(&store, config, &mut payload).unwrap();

        assert!(encoded.walk().done);
        assert_eq!(encoded.walk().records_emitted, 2);
        assert_eq!(encoded.payload_len(), payload.len());
        let section =
            CheckpointSectionRef::new(0, CheckpointSectionKind::Records, &payload).unwrap();
        let mut section_bytes = Vec::new();
        encode_checkpoint_section(section, &mut section_bytes).unwrap();
        let decoded_section = decode_checkpoint_section(&section_bytes).unwrap();
        assert_eq!(decoded_section.payload(), payload.as_slice());

        let mut effects = Vec::new();
        let record_count = decode_record_sequence(&payload, Lsn::new(0, 0), |decoded| {
            let record = decoded.record();
            assert_eq!(record.kind(), RecordKind::StringPostImage);
            assert_eq!(record.namespace(), namespace);
            match decode_mutation_record(record).unwrap() {
                MutationEffect::StringPostImage { key, value, expire_at_ms, raw } => {
                    effects.push((key.to_vec(), value.to_vec(), expire_at_ms, raw))
                }
                other => panic!("unexpected checkpoint record effect: {other:?}"),
            }
        })
        .unwrap();
        assert_eq!(record_count, 2);
        effects.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
        assert_eq!(
            effects,
            [
                (b"a".to_vec(), b"one".to_vec(), None, false),
                (b"ttl".to_vec(), b"two".to_vec(), Some(5), false)
            ]
        );
    }

    #[test]
    fn records_section_payload_fails_closed_when_budget_fills() {
        let mut store = CellStore::new(StoreConfig::default());
        store.set(b"a", b"one", SetOptions::default(), NOW).unwrap();
        let config = CheckpointRecordsSectionConfig::new(
            NamespaceId::new(42),
            0,
            CheckpointWalkBudget::new(usize::MAX),
            NOW,
        )
        .with_max_payload_bytes(1);
        let mut payload = vec![1, 2, 3];

        let error =
            encode_checkpoint_records_section_payload(&store, config, &mut payload).unwrap_err();

        assert!(matches!(
            error,
            CheckpointRecordsSectionError::Stage(MutationStageError::Log(
                LogStagingError::Full { .. }
            ))
        ));
        assert!(payload.is_empty());
    }

    #[test]
    fn records_section_payload_rejects_invalid_budget_before_walk() {
        let store = CellStore::new(StoreConfig::default());
        let config = CheckpointRecordsSectionConfig::new(
            NamespaceId::new(42),
            0,
            CheckpointWalkBudget::new(1),
            NOW,
        )
        .with_max_payload_bytes(0);
        let mut payload = vec![1, 2, 3];

        assert_eq!(
            encode_checkpoint_records_section_payload(&store, config, &mut payload),
            Err(CheckpointRecordsSectionError::ZeroPayloadBudget)
        );
        assert!(payload.is_empty());
    }

    #[test]
    fn keyspace_snapshot_sections_encode_catalog_records_and_header_namespaces() {
        let mut keyspace = Keyspace::new(StoreConfig::default());
        keyspace.db_mut(0).set(b"d0", b"zero", SetOptions::default(), NOW).unwrap();
        keyspace.db_mut(2).set(b"d2", b"two", SetOptions::default(), NOW).unwrap();
        let ledger = keyspace
            .ns_create(NsCreateSpec {
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: Some(NsFsyncPolicy::Always),
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        keyspace
            .named_db_mut(ledger)
            .unwrap()
            .set(b"ln", b"named", SetOptions::default(), NOW)
            .unwrap();
        let cache = keyspace
            .ns_create(NsCreateSpec {
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        keyspace
            .named_db_mut(cache)
            .unwrap()
            .set(b"volatile", b"skip", SetOptions::default(), NOW)
            .unwrap();
        let mut namespaces = vec![NamespaceId::new(99)];
        let mut catalog = vec![1];
        let mut records = vec![2];

        let encoded = encode_checkpoint_keyspace_snapshot_sections(
            &keyspace,
            CheckpointKeyspaceSnapshotConfig::new(NOW),
            &mut namespaces,
            &mut catalog,
            &mut records,
        )
        .unwrap();

        assert_eq!(
            namespaces,
            [
                NamespaceId::new(0),
                NamespaceId::new(2),
                NamespaceId::new(ledger.get()),
                NamespaceId::new(cache.get())
            ]
        );
        assert_eq!(encoded.namespace_count(), 4);
        assert_eq!(encoded.default_stores_walked(), 2);
        assert_eq!(encoded.named_stores_walked(), 1);
        assert_eq!(encoded.records_emitted(), 3);
        assert_eq!(encoded.catalog_payload_len(), catalog.len());
        assert_eq!(encoded.records_payload_len(), records.len());

        let decoded_catalog = decode_namespace_catalog(&catalog).unwrap();
        assert_eq!(decoded_catalog.specs()[0].id, ledger);
        assert_eq!(decoded_catalog.specs()[0].name, b"ledger");
        assert_eq!(decoded_catalog.specs()[1].id, cache);
        assert_eq!(decoded_catalog.specs()[1].name, b"cache");

        let mut decoded = Vec::new();
        let record_count = decode_record_sequence(&records, Lsn::new(0, 0), |entry| {
            let record = entry.record();
            let effect = decode_mutation_record(record).unwrap();
            match effect {
                MutationEffect::StringPostImage { key, value, .. } => {
                    decoded.push((record.namespace(), key.to_vec(), value.to_vec()))
                }
                other => panic!("unexpected checkpoint snapshot effect: {other:?}"),
            }
        })
        .unwrap();
        assert_eq!(record_count, 3);
        decoded.sort_by(|lhs, rhs| lhs.1.cmp(&rhs.1));
        assert_eq!(
            decoded,
            [
                (NamespaceId::new(0), b"d0".to_vec(), b"zero".to_vec()),
                (NamespaceId::new(2), b"d2".to_vec(), b"two".to_vec()),
                (NamespaceId::new(ledger.get()), b"ln".to_vec(), b"named".to_vec())
            ]
        );

        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let sections = sections(&catalog, &records);
        let (image, footer, _) = expected_image(header, &sections);
        assert_eq!(
            decode_checkpoint_footer(&image[image.len() - CHECKPOINT_FOOTER_LEN..]),
            Ok(footer)
        );
    }

    #[test]
    fn keyspace_snapshot_sections_fail_closed_on_records_budget() {
        let mut keyspace = Keyspace::new(StoreConfig::default());
        keyspace.db_mut(0).set(b"d0", b"zero", SetOptions::default(), NOW).unwrap();
        let mut namespaces = vec![NamespaceId::new(99)];
        let mut catalog = vec![1];
        let mut records = vec![2];

        let error = encode_checkpoint_keyspace_snapshot_sections(
            &keyspace,
            CheckpointKeyspaceSnapshotConfig::new(NOW).with_max_records_payload_bytes(1),
            &mut namespaces,
            &mut catalog,
            &mut records,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointKeyspaceSnapshotError::Records {
                namespace,
                source: CheckpointRecordsSectionError::Stage(MutationStageError::Log(
                    LogStagingError::Full { .. }
                ))
            } if namespace == NamespaceId::new(0)
        ));
        assert!(namespaces.is_empty());
        assert!(catalog.is_empty());
        assert!(records.is_empty());
    }

    #[test]
    fn publish_keyspace_snapshot_image_writes_composed_checkpoint() {
        let mut keyspace = Keyspace::new(StoreConfig::default());
        keyspace.db_mut(0).set(b"d0", b"zero", SetOptions::default(), NOW).unwrap();
        keyspace.db_mut(2).set(b"d2", b"two", SetOptions::default(), NOW).unwrap();
        let ledger = keyspace
            .ns_create(NsCreateSpec {
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: Some(NsFsyncPolicy::Always),
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        keyspace
            .named_db_mut(ledger)
            .unwrap()
            .set(b"ln", b"named", SetOptions::default(), NOW)
            .unwrap();
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 13);
        let mut completions = Vec::new();

        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(4),
            checkpoint(),
            &keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(NOW),
                publish_config(),
            ),
            &mut completions,
        )
        .unwrap();

        assert_eq!(published.image().checkpoint(), checkpoint());
        assert_eq!(published.image().bytes_written(), driver.written.len() as u64);
        assert_eq!(published.snapshot().records_emitted(), 3);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());

        let image = &driver.written;
        let header_len = checkpoint_header_len_from_prefix(image).unwrap();
        let header = decode_checkpoint_header(&image[..header_len]).unwrap();
        assert_eq!(header.cell(), CellId(4));
        assert_eq!(header.checkpoint(), checkpoint());
        assert_eq!(header.section_count(), 2);
        assert_eq!(
            header.namespaces().collect::<Vec<_>>(),
            [NamespaceId::new(0), NamespaceId::new(2), NamespaceId::new(ledger.get())]
        );

        let mut at = header_len;
        let catalog_header =
            decode_checkpoint_section_frame_header(&image[at..at + CHECKPOINT_SECTION_HEADER_LEN])
                .unwrap();
        let catalog_len = CHECKPOINT_SECTION_HEADER_LEN
            + catalog_header.payload_len() as usize
            + CHECKPOINT_SECTION_TRAILER_LEN;
        let catalog_section = decode_checkpoint_section(&image[at..at + catalog_len]).unwrap();
        at += catalog_len;
        assert_eq!(catalog_section.kind(), CheckpointSectionKind::NamespaceCatalog);
        let decoded_catalog = decode_namespace_catalog(catalog_section.payload()).unwrap();
        assert_eq!(decoded_catalog.specs()[0].id, ledger);

        let records_header =
            decode_checkpoint_section_frame_header(&image[at..at + CHECKPOINT_SECTION_HEADER_LEN])
                .unwrap();
        let records_len = CHECKPOINT_SECTION_HEADER_LEN
            + records_header.payload_len() as usize
            + CHECKPOINT_SECTION_TRAILER_LEN;
        let records_section = decode_checkpoint_section(&image[at..at + records_len]).unwrap();
        at += records_len;
        assert_eq!(records_section.kind(), CheckpointSectionKind::Records);
        let mut record_namespaces = Vec::new();
        let record_count =
            decode_record_sequence(records_section.payload(), Lsn::new(0, 0), |entry| {
                let record = entry.record();
                decode_mutation_record(record).unwrap();
                record_namespaces.push(record.namespace());
            })
            .unwrap();
        record_namespaces.sort_by_key(|namespace| namespace.get());
        assert_eq!(record_count, 3);
        assert_eq!(
            record_namespaces,
            [NamespaceId::new(0), NamespaceId::new(2), NamespaceId::new(ledger.get())]
        );

        let footer = decode_checkpoint_footer(&image[at..at + CHECKPOINT_FOOTER_LEN]).unwrap();
        assert_eq!(footer, published.image().footer());
        at += CHECKPOINT_FOOTER_LEN;
        assert_eq!(at, image.len());
    }

    #[test]
    fn publish_writes_syncs_closes_renames_and_dirsyncs_checkpoint_image() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 8);
        let mut completions = Vec::new();
        let namespaces = namespaces();
        let sections = sections(b"catalog", b"records-0123456789");
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let (expected, expected_footer, _) = expected_image(header, &sections);

        let published = publish_checkpoint_image(
            &mut driver,
            &mut pool,
            header,
            &sections,
            publish_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(published.checkpoint(), checkpoint());
        assert_eq!(published.footer(), expected_footer);
        assert_eq!(published.bytes_written(), expected.len() as u64);
        assert_eq!(driver.written, expected);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert_eq!(
            driver.observed.first(),
            Some(&ObservedOp::Open {
                dir: DIR_FD,
                name: "ckpt-000009.ick.tmp".to_string(),
                mode: FileOpenMode::ReadWriteCreateTruncate,
                token: token(),
            })
        );
        assert!(driver.observed.contains(&ObservedOp::Sync {
            fd: TEMP_FD,
            mode: FileSyncMode::DataOnly,
            token: token(),
        }));
        assert!(driver.observed.contains(&ObservedOp::Close { fd: TEMP_FD, token: token() }));
        assert!(driver.observed.contains(&ObservedOp::Rename {
            old_dir: DIR_FD,
            old_name: "ckpt-000009.ick.tmp".to_string(),
            new_dir: DIR_FD,
            new_name: "ckpt-000009.ick".to_string(),
            token: token(),
        }));
        assert_eq!(
            driver.observed.last(),
            Some(&ObservedOp::Sync { fd: DIR_FD, mode: FileSyncMode::Full, token: token() })
        );
    }

    #[test]
    fn load_checkpoint_image_returns_validated_summary_and_closes_file() {
        let namespaces = namespaces();
        let sections = sections(b"catalog", b"records-0123456789");
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let (image, footer, metas) = expected_image(header, &sections);
        let mut driver = TestDriver { file_bytes: image.clone(), ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 9);
        let mut completions = Vec::new();

        let loaded = load_checkpoint_image(
            &mut driver,
            &mut pool,
            checkpoint(),
            load_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded.cell(), CellId(4));
        assert_eq!(loaded.checkpoint(), checkpoint());
        assert_eq!(loaded.namespaces(), namespaces.as_slice());
        assert_eq!(loaded.sections(), metas.as_slice());
        assert_eq!(loaded.footer(), footer);
        assert_eq!(loaded.bytes_read(), image.len() as u64);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert!(driver.observed.contains(&ObservedOp::Close { fd: CHECKPOINT_FD, token: token() }));
        assert_eq!(
            decode_checkpoint_header(&image[..checkpoint_header_len_from_prefix(&image).unwrap()])
                .unwrap()
                .section_count(),
            2
        );
        let section_offset = checkpoint_header_len_from_prefix(&image).unwrap();
        let first_section =
            decode_checkpoint_section(&image[section_offset..section_offset + 28 + 7 + 4]).unwrap();
        assert_eq!(first_section.meta(), metas[0]);
        assert_eq!(
            decode_checkpoint_footer(&image[image.len() - CHECKPOINT_FOOTER_LEN..]).unwrap(),
            footer
        );
    }

    #[test]
    fn load_checkpoint_image_payloads_returns_validated_payloads_and_closes_file() {
        let namespaces = namespaces();
        let sections = sections(b"catalog", b"records-0123456789");
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let (image, footer, metas) = expected_image(header, &sections);
        let mut driver = TestDriver { file_bytes: image.clone(), ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 9);
        let mut completions = Vec::new();

        let loaded = load_checkpoint_image_payloads(
            &mut driver,
            &mut pool,
            checkpoint(),
            load_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded.image().cell(), CellId(4));
        assert_eq!(loaded.image().checkpoint(), checkpoint());
        assert_eq!(loaded.image().namespaces(), namespaces.as_slice());
        assert_eq!(loaded.image().sections(), metas.as_slice());
        assert_eq!(loaded.image().footer(), footer);
        assert_eq!(loaded.image().bytes_read(), image.len() as u64);
        assert_eq!(loaded.namespace_catalog(), b"catalog");
        assert_eq!(loaded.records(), b"records-0123456789");
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert!(driver.observed.contains(&ObservedOp::Close { fd: CHECKPOINT_FD, token: token() }));
    }

    #[test]
    fn load_checkpoint_image_payloads_rejects_unexpected_section_count_after_validation() {
        let namespaces = namespaces();
        let sections =
            [CheckpointSectionRef::new(0, CheckpointSectionKind::NamespaceCatalog, b"catalog")
                .unwrap()];
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 1, &namespaces).unwrap();
        let (image, _, _) = expected_image(header, &sections);
        let mut driver = TestDriver { file_bytes: image, ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 64);
        let mut completions = Vec::new();

        let error = load_checkpoint_image_payloads(
            &mut driver,
            &mut pool,
            checkpoint(),
            load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointImageLoadError::SectionCountMismatchForPayloads { expected: 2, got: 1 }
        ));
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(driver.observed.contains(&ObservedOp::Close { fd: CHECKPOINT_FD, token: token() }));
    }

    #[test]
    fn load_checkpoint_image_payloads_rejects_unexpected_section_kind_after_validation() {
        let namespaces = namespaces();
        let sections = [
            CheckpointSectionRef::new(0, CheckpointSectionKind::Records, b"catalog").unwrap(),
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, b"records").unwrap(),
        ];
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let (image, _, _) = expected_image(header, &sections);
        let mut driver = TestDriver { file_bytes: image, ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 64);
        let mut completions = Vec::new();

        let error = load_checkpoint_image_payloads(
            &mut driver,
            &mut pool,
            checkpoint(),
            load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointImageLoadError::SectionKindMismatch {
                ordinal: 0,
                expected: CheckpointSectionKind::NamespaceCatalog,
                got: CheckpointSectionKind::Records
            }
        ));
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(driver.observed.contains(&ObservedOp::Close { fd: CHECKPOINT_FD, token: token() }));
    }

    #[test]
    fn load_checkpoint_image_fails_closed_on_trailing_bytes_after_close() {
        let namespaces = namespaces();
        let sections = sections(b"catalog", b"records");
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let (mut image, _, _) = expected_image(header, &sections);
        image.push(0xff);
        let mut driver = TestDriver { file_bytes: image, ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 64);
        let mut completions = Vec::new();

        let error = load_checkpoint_image(
            &mut driver,
            &mut pool,
            checkpoint(),
            load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, CheckpointImageLoadError::TrailingBytes { .. }));
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(driver.observed.contains(&ObservedOp::Close { fd: CHECKPOINT_FD, token: token() }));
    }

    #[test]
    fn publish_closes_and_unlinks_temp_on_write_error_before_rename() {
        let namespaces = namespaces();
        let sections = sections(b"catalog", b"records");
        let header = CheckpointHeader::new(CellId(4), checkpoint(), 2, &namespaces).unwrap();
        let mut driver = TestDriver { write_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 64);
        let mut completions = Vec::new();

        let error = publish_checkpoint_image(
            &mut driver,
            &mut pool,
            header,
            &sections,
            publish_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, CheckpointImagePublishError::WriteTemp { .. }));
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(driver.observed.contains(&ObservedOp::Close { fd: TEMP_FD, token: token() }));
        assert!(driver.observed.contains(&ObservedOp::Unlink {
            dir: DIR_FD,
            name: "ckpt-000009.ick.tmp".to_string(),
            token: token(),
        }));
        assert!(!driver.observed.iter().any(|op| matches!(op, ObservedOp::Rename { .. })));
    }
}
