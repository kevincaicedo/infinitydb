//! Recovery helpers that compose `inf-log` readers with `BackendDriver` I/O.
//!
//! This module is deliberately below the serving plane and above `inf-log`:
//! it may name both `BackendDriver` operations and log-frame readers, while
//! keeping syscall scheduling out of the log-format crate.

use core::fmt;
use std::io;
use std::path::{Path, PathBuf};

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use inf_log::{
    CheckpointRef, LogCodecError, Lsn, NamespaceId, RecordKind, SegmentCatalog, SegmentFileReader,
    SegmentFrame, SegmentFrameSink, SegmentId, SegmentReadConfig, SegmentReadError,
    SegmentReadTerminal, SegmentScanError, SegmentTailPolicy, decode_record_sequence,
    scan_segment_names,
};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, RawFd, TokenClass,
};
use inf_store::{
    CellStore, DEFAULT_DBS, Keyspace, NsCatalog, NsCatalogError, NsId, NsMode, OpError,
    decode_namespace_catalog,
};

use crate::checkpoint::{
    CheckpointImageLoadConfig, CheckpointImageLoadError, LoadedCheckpointImage,
    load_checkpoint_image_payloads,
};
use crate::durability::{
    CheckpointBeginRecordDecodeError, MutationRecordDecodeError, decode_checkpoint_begin_record,
    decode_mutation_record,
};

pub type SegmentReadQueueError = SegmentReadIoError<core::convert::Infallible>;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct KeyspaceReplayStats {
    pub frames: u64,
    pub records: u64,
    pub last_frame_end: Option<Lsn>,
}

/// Applies decoded M2 user-mutation records to one cell's owned keyspace.
///
/// This sink routes `NamespaceId(0..15)` into Redis default databases and
/// `NamespaceId >= 16` only when the recovered catalog maps the id to a live
/// durable named namespace. Unknown, dropped, memory-mode, and topic-mode ids
/// fail closed so recovery never silently invents a storage home.
pub struct KeyspaceReplaySink<'a> {
    keyspace: &'a mut Keyspace,
    min_lsn: Option<Lsn>,
    stats: KeyspaceReplayStats,
}

impl<'a> KeyspaceReplaySink<'a> {
    pub fn new(keyspace: &'a mut Keyspace) -> KeyspaceReplaySink<'a> {
        KeyspaceReplaySink { keyspace, min_lsn: None, stats: KeyspaceReplayStats::default() }
    }

    pub fn from_lsn(keyspace: &'a mut Keyspace, min_lsn: Lsn) -> KeyspaceReplaySink<'a> {
        KeyspaceReplaySink {
            keyspace,
            min_lsn: Some(min_lsn),
            stats: KeyspaceReplayStats::default(),
        }
    }

    #[inline]
    pub const fn stats(&self) -> KeyspaceReplayStats {
        self.stats
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct CheckpointRecordsApplyStats {
    pub records: u64,
    pub last_record_lsn: Option<Lsn>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AppliedCheckpointImage {
    image: LoadedCheckpointImage,
    records: CheckpointRecordsApplyStats,
}

impl AppliedCheckpointImage {
    #[inline]
    pub const fn image(&self) -> &LoadedCheckpointImage {
        &self.image
    }

    #[inline]
    pub const fn records(&self) -> CheckpointRecordsApplyStats {
        self.records
    }
}

pub fn apply_checkpoint_image_to_keyspace<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    keyspace: &mut Keyspace,
    checkpoint: CheckpointRef,
    config: CheckpointImageLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<AppliedCheckpointImage, CheckpointImageApplyError>
where
    D: BackendDriver,
{
    validate_checkpoint_image_apply_target(keyspace)?;
    let loaded = load_checkpoint_image_payloads(driver, pool, checkpoint, config, completions)
        .map_err(CheckpointImageApplyError::Load)?;
    let catalog = decode_namespace_catalog(loaded.namespace_catalog())
        .map_err(CheckpointImageApplyError::CatalogDecode)?;
    let first_lsn = loaded.image().checkpoint().begin_lsn();
    validate_checkpoint_records_payload_against_catalog(&catalog, loaded.records(), first_lsn)
        .map_err(CheckpointImageApplyError::Records)?;
    keyspace
        .ns_replace_with_recovered_catalog(catalog)
        .map_err(CheckpointImageApplyError::CatalogInstall)?;
    let records = apply_checkpoint_records_payload(keyspace, loaded.records(), first_lsn)
        .map_err(CheckpointImageApplyError::Records)?;
    Ok(AppliedCheckpointImage { image: loaded.image().clone(), records })
}

pub fn apply_checkpoint_records_payload(
    keyspace: &mut Keyspace,
    payload: &[u8],
    first_lsn: Lsn,
) -> Result<CheckpointRecordsApplyStats, CheckpointRecordsApplyError> {
    let stats = validate_checkpoint_records_payload(keyspace, payload, first_lsn)?;
    let mut apply_error = None;
    let mut applied = 0u64;
    decode_record_sequence(payload, first_lsn, |decoded| {
        if apply_error.is_some() {
            return;
        }
        let lsn = decoded.lsn();
        let record = decoded.record();
        let effect = decode_mutation_record(record)
            .expect("checkpoint records payload was validated before apply");
        let store = namespace_store_mut(keyspace, record.namespace())
            .expect("checkpoint namespace was validated before apply");
        if let Err(source) = store.replay_mutation_effect(effect) {
            apply_error = Some(CheckpointRecordsApplyError::Store { lsn, source });
            return;
        }
        applied += 1;
    })
    .map_err(|source| CheckpointRecordsApplyError::RecordSequence { source })?;
    if let Some(error) = apply_error {
        return Err(error);
    }
    debug_assert_eq!(applied, stats.records);
    keyspace.refresh_pressure();
    Ok(stats)
}

fn validate_checkpoint_records_payload(
    keyspace: &Keyspace,
    payload: &[u8],
    first_lsn: Lsn,
) -> Result<CheckpointRecordsApplyStats, CheckpointRecordsApplyError> {
    validate_checkpoint_records_payload_with(payload, first_lsn, |namespace| {
        checkpoint_namespace_supported(keyspace, namespace)
    })
}

fn validate_checkpoint_records_payload_against_catalog(
    catalog: &NsCatalog,
    payload: &[u8],
    first_lsn: Lsn,
) -> Result<CheckpointRecordsApplyStats, CheckpointRecordsApplyError> {
    validate_checkpoint_records_payload_with(payload, first_lsn, |namespace| {
        checkpoint_catalog_namespace_supported(catalog, namespace)
    })
}

fn validate_checkpoint_records_payload_with<F>(
    payload: &[u8],
    first_lsn: Lsn,
    mut namespace_supported: F,
) -> Result<CheckpointRecordsApplyStats, CheckpointRecordsApplyError>
where
    F: FnMut(NamespaceId) -> bool,
{
    let mut stats = CheckpointRecordsApplyStats::default();
    let mut semantic_error = None;
    decode_record_sequence(payload, first_lsn, |decoded| {
        if semantic_error.is_some() {
            return;
        }
        let lsn = decoded.lsn();
        let record = decoded.record();
        if let Err(source) = decode_mutation_record(record) {
            semantic_error = Some(CheckpointRecordsApplyError::Decode { lsn, source });
            return;
        }
        if !namespace_supported(record.namespace()) {
            semantic_error = Some(CheckpointRecordsApplyError::UnsupportedNamespace {
                lsn,
                namespace: record.namespace(),
            });
            return;
        }
        stats.records += 1;
        stats.last_record_lsn = Some(lsn);
    })
    .map_err(|source| CheckpointRecordsApplyError::RecordSequence { source })?;
    if let Some(error) = semantic_error {
        return Err(error);
    }
    Ok(stats)
}

fn validate_checkpoint_image_apply_target(
    keyspace: &Keyspace,
) -> Result<(), CheckpointImageApplyError> {
    let live_records = keyspace.dbs().map(|(_, store)| store.len()).sum::<usize>()
        + keyspace.named_dbs().map(|(_, store)| store.len()).sum::<usize>();
    let named_namespaces = keyspace.ns_iter().count();
    if live_records != 0 || named_namespaces != 0 {
        return Err(CheckpointImageApplyError::TargetNotEmpty { live_records, named_namespaces });
    }
    Ok(())
}

fn checkpoint_namespace_supported(keyspace: &Keyspace, namespace: NamespaceId) -> bool {
    if default_namespace_db(namespace).is_some() {
        return true;
    }
    keyspace
        .ns_get_by_id(NsId::new(namespace.get()))
        .is_some_and(|spec| spec.mode == NsMode::Durable)
}

fn checkpoint_catalog_namespace_supported(catalog: &NsCatalog, namespace: NamespaceId) -> bool {
    if default_namespace_db(namespace).is_some() {
        return true;
    }
    catalog
        .specs()
        .iter()
        .any(|spec| spec.id == NsId::new(namespace.get()) && spec.mode == NsMode::Durable)
}

impl SegmentFrameSink for KeyspaceReplaySink<'_> {
    type Error = KeyspaceReplayError;

    fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error> {
        if self.min_lsn.is_some_and(|min_lsn| frame.frame_end() <= min_lsn) {
            return Ok(());
        }
        let mut boundary_seen = self.min_lsn.is_none_or(|min_lsn| frame.frame_start() >= min_lsn);
        for decoded in frame.frame().records() {
            let lsn = decoded.lsn();
            if let Some(min_lsn) = self.min_lsn {
                if lsn < min_lsn {
                    continue;
                }
                if !boundary_seen {
                    if lsn != min_lsn {
                        return Err(KeyspaceReplayError::BeginLsnInsideFrame {
                            begin: min_lsn,
                            frame_start: frame.frame_start(),
                            frame_end: frame.frame_end(),
                        });
                    }
                    boundary_seen = true;
                }
            }
            let record = decoded.record();
            if record.kind() == RecordKind::CheckpointBegin {
                decode_checkpoint_begin_record(record)
                    .map_err(|source| KeyspaceReplayError::CheckpointBegin { lsn, source })?;
                continue;
            }
            let effect = decode_mutation_record(record)
                .map_err(|source| KeyspaceReplayError::Decode { lsn, source })?;
            namespace_store_mut(self.keyspace, record.namespace())
                .ok_or(KeyspaceReplayError::UnsupportedNamespace {
                    lsn,
                    namespace: record.namespace(),
                })?
                .replay_mutation_effect(effect)
                .map_err(|source| KeyspaceReplayError::Store { lsn, source })?;
            self.stats.records += 1;
        }
        if !boundary_seen {
            return Err(KeyspaceReplayError::BeginLsnInsideFrame {
                begin: self.min_lsn.expect("boundary was required"),
                frame_start: frame.frame_start(),
                frame_end: frame.frame_end(),
            });
        }
        self.keyspace.refresh_pressure();
        self.stats.frames += 1;
        self.stats.last_frame_end = Some(frame.frame_end());
        Ok(())
    }
}

fn default_namespace_db(namespace: NamespaceId) -> Option<usize> {
    let db = usize::try_from(namespace.get()).ok()?;
    (db < DEFAULT_DBS).then_some(db)
}

fn namespace_store_mut(keyspace: &mut Keyspace, namespace: NamespaceId) -> Option<&mut CellStore> {
    if let Some(db) = default_namespace_db(namespace) {
        return Some(keyspace.db_mut(db));
    }
    keyspace.durable_named_db_mut(NsId::new(namespace.get()))
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeyspaceReplayError {
    UnsupportedNamespace { lsn: Lsn, namespace: NamespaceId },
    BeginLsnInsideFrame { begin: Lsn, frame_start: Lsn, frame_end: Lsn },
    CheckpointBegin { lsn: Lsn, source: CheckpointBeginRecordDecodeError },
    Decode { lsn: Lsn, source: MutationRecordDecodeError },
    Store { lsn: Lsn, source: OpError },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointRecordsApplyError {
    RecordSequence { source: LogCodecError },
    UnsupportedNamespace { lsn: Lsn, namespace: NamespaceId },
    Decode { lsn: Lsn, source: MutationRecordDecodeError },
    Store { lsn: Lsn, source: OpError },
}

#[derive(Debug)]
pub enum CheckpointImageApplyError {
    TargetNotEmpty { live_records: usize, named_namespaces: usize },
    Load(CheckpointImageLoadError),
    CatalogDecode(NsCatalogError),
    CatalogInstall(NsCatalogError),
    Records(CheckpointRecordsApplyError),
}

impl fmt::Display for KeyspaceReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyspaceReplayError::UnsupportedNamespace { lsn, namespace } => write!(
                f,
                "log record {lsn} names unsupported namespace id {} during replay",
                namespace.get()
            ),
            KeyspaceReplayError::BeginLsnInsideFrame { begin, frame_start, frame_end } => write!(
                f,
                "tail replay begin LSN {begin} falls inside frame {frame_start}..{frame_end}"
            ),
            KeyspaceReplayError::CheckpointBegin { lsn, source } => {
                write!(f, "log record {lsn} failed checkpoint-begin decode: {source}")
            }
            KeyspaceReplayError::Decode { lsn, source } => {
                write!(f, "log record {lsn} failed mutation decode: {source}")
            }
            KeyspaceReplayError::Store { lsn, source } => {
                write!(f, "log record {lsn} failed store replay: {source:?}")
            }
        }
    }
}

impl fmt::Display for CheckpointRecordsApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointRecordsApplyError::RecordSequence { source } => {
                write!(f, "checkpoint records payload failed record decode: {source}")
            }
            CheckpointRecordsApplyError::UnsupportedNamespace { lsn, namespace } => write!(
                f,
                "checkpoint record {lsn} names unsupported namespace id {}",
                namespace.get()
            ),
            CheckpointRecordsApplyError::Decode { lsn, source } => {
                write!(f, "checkpoint record {lsn} failed mutation decode: {source}")
            }
            CheckpointRecordsApplyError::Store { lsn, source } => {
                write!(f, "checkpoint record {lsn} failed store replay: {source:?}")
            }
        }
    }
}

impl fmt::Display for CheckpointImageApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointImageApplyError::TargetNotEmpty { live_records, named_namespaces } => write!(
                f,
                "checkpoint image apply requires an empty keyspace, found {live_records} live record(s) and {named_namespaces} named namespace(s)"
            ),
            CheckpointImageApplyError::Load(error) => {
                write!(f, "checkpoint image load failed during recovery: {error}")
            }
            CheckpointImageApplyError::CatalogDecode(error) => {
                write!(f, "checkpoint namespace catalog decode failed: {error:?}")
            }
            CheckpointImageApplyError::CatalogInstall(error) => {
                write!(f, "checkpoint namespace catalog install failed: {error:?}")
            }
            CheckpointImageApplyError::Records(error) => {
                write!(f, "checkpoint records payload apply failed: {error}")
            }
        }
    }
}

impl std::error::Error for KeyspaceReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KeyspaceReplayError::CheckpointBegin { source, .. } => Some(source),
            KeyspaceReplayError::Decode { source, .. } => Some(source),
            KeyspaceReplayError::UnsupportedNamespace { .. }
            | KeyspaceReplayError::BeginLsnInsideFrame { .. }
            | KeyspaceReplayError::Store { .. } => None,
        }
    }
}

impl std::error::Error for CheckpointRecordsApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointRecordsApplyError::RecordSequence { source } => Some(source),
            CheckpointRecordsApplyError::Decode { source, .. } => Some(source),
            CheckpointRecordsApplyError::UnsupportedNamespace { .. }
            | CheckpointRecordsApplyError::Store { .. } => None,
        }
    }
}

impl std::error::Error for CheckpointImageApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckpointImageApplyError::Load(error) => Some(error),
            CheckpointImageApplyError::Records(error) => Some(error),
            CheckpointImageApplyError::TargetNotEmpty { .. }
            | CheckpointImageApplyError::CatalogDecode(_)
            | CheckpointImageApplyError::CatalogInstall(_) => None,
        }
    }
}

/// Strict host directory enumeration for cold production boot.
///
/// This is intentionally only the directory walk. Segment-name validation
/// stays in `inf-log`, and segment file reads still flow through
/// `BackendDriver` so active-tail recovery remains faultable in tests.
pub fn scan_host_segment_directory(
    path: impl AsRef<Path>,
    scratch: &mut Vec<String>,
) -> Result<Option<SegmentCatalog>, HostSegmentScanError> {
    let path = path.as_ref();
    scratch.clear();
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(HostSegmentScanError::Read { path: path.to_path_buf(), source });
        }
    };

    for entry in entries {
        let entry = entry
            .map_err(|source| HostSegmentScanError::Read { path: path.to_path_buf(), source })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(HostSegmentScanError::NonUtf8Entry { path: entry.path() });
        };
        scratch.push(name.to_string());
    }

    scan_segment_names(scratch.iter().map(String::as_str))
        .map(Some)
        .map_err(HostSegmentScanError::Scan)
}

#[derive(Debug)]
pub enum HostSegmentScanError {
    Read { path: PathBuf, source: io::Error },
    NonUtf8Entry { path: PathBuf },
    Scan(SegmentScanError),
}

impl fmt::Display for HostSegmentScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostSegmentScanError::Read { path, source } => {
                write!(f, "read log segment directory {} failed: {source}", path.display())
            }
            HostSegmentScanError::NonUtf8Entry { path } => {
                write!(f, "log segment directory entry {} is not UTF-8", path.display())
            }
            HostSegmentScanError::Scan(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for HostSegmentScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HostSegmentScanError::Read { source, .. } => Some(source),
            HostSegmentScanError::Scan(source) => Some(source),
            HostSegmentScanError::NonUtf8Entry { .. } => None,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct InFlightRead {
    offset_bytes: u64,
    buf: BufferId,
}

/// Recovery-side adapter from [`SegmentFileReader`] requests to file I/O ops.
///
/// Exactly one read may be in flight. This keeps buffer custody explicit and
/// makes the recovery runner's next action inspectable in deterministic tests.
#[derive(Debug)]
pub struct SegmentReadIo {
    fd: RawFd,
    token: CompletionToken,
    reader: SegmentFileReader,
    in_flight: Option<InFlightRead>,
}

impl SegmentReadIo {
    pub fn new(
        fd: RawFd,
        token_slot: u32,
        token_generation: u32,
        segment: SegmentId,
        tail_policy: SegmentTailPolicy,
        config: SegmentReadConfig,
    ) -> SegmentReadIo {
        SegmentReadIo {
            fd,
            token: CompletionToken::new(TokenClass::File, token_slot, token_generation),
            reader: SegmentFileReader::new(segment, tail_policy, config),
            in_flight: None,
        }
    }

    #[inline]
    pub const fn segment(&self) -> SegmentId {
        self.reader.segment()
    }

    #[inline]
    pub const fn is_finished(&self) -> bool {
        self.reader.is_finished()
    }

    #[inline]
    pub const fn terminal(&self) -> Option<SegmentReadTerminal> {
        self.reader.terminal()
    }

    #[inline]
    pub const fn frames_read(&self) -> u64 {
        self.reader.frames_read()
    }

    #[inline]
    pub const fn records_read(&self) -> u64 {
        self.reader.records_read()
    }

    #[inline]
    pub const fn token(&self) -> CompletionToken {
        self.token
    }

    #[inline]
    pub const fn read_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Queue the next positioned file read, if the segment is not complete.
    ///
    /// Returns `Ok(true)` when an op was queued, `Ok(false)` when the reader
    /// is already finished. The caller submits the returned op through the
    /// normal reactor batch, preserving the one-backend-entry-per-iteration
    /// law.
    pub fn queue_next(
        &mut self,
        pool: &mut BufferPool,
        out: &mut Vec<IoOp>,
    ) -> Result<bool, SegmentReadQueueError> {
        if self.reader.is_finished() {
            return Ok(false);
        }
        if let Some(read) = self.in_flight {
            return Err(SegmentReadIoError::ReadAlreadyInFlight {
                segment: self.segment(),
                offset_bytes: read.offset_bytes,
            });
        }

        let request = self.reader.next_request().expect("unfinished reader has a request");
        if request.len_bytes() as usize > pool.buf_size() {
            return Err(SegmentReadIoError::ReadBufferTooSmall {
                segment: self.segment(),
                request_len_bytes: request.len_bytes(),
                buffer_len_bytes: pool.buf_size(),
            });
        }

        let Some(buf) = pool.try_lease(LeaseKind::Recv) else {
            return Err(SegmentReadIoError::ReadBufferUnavailable { segment: self.segment() });
        };
        out.push(IoOp::FileReadAt {
            fd: self.fd,
            offset_bytes: request.offset_bytes(),
            buf,
            len: request.len_bytes(),
            token: self.token,
        });
        self.in_flight = Some(InFlightRead { offset_bytes: request.offset_bytes(), buf });
        Ok(true)
    }

    /// Consume one file completion and feed its bytes to the segment reader.
    ///
    /// Returns `Ok(true)` when the segment reader reached EOF or an accepted
    /// active tail, `Ok(false)` when another read is needed.
    pub fn on_completion<S>(
        &mut self,
        completion: Completion,
        pool: &mut BufferPool,
        sink: &mut S,
    ) -> Result<bool, SegmentReadIoError<S::Error>>
    where
        S: SegmentFrameSink,
    {
        if completion.token != self.token {
            return Err(SegmentReadIoError::UnexpectedToken {
                segment: self.segment(),
                expected: self.token,
                got: completion.token,
            });
        }
        let Some(read) = self.in_flight.take() else {
            return Err(SegmentReadIoError::UnexpectedCompletion { segment: self.segment() });
        };

        match completion.result {
            CompletionResult::FileRead { buf, len } => {
                assert_eq!(buf, read.buf, "file read completion returned the wrong buffer");
                self.handle_file_read(read.offset_bytes, buf, len, pool, sink)?;
            }
            CompletionResult::Error { errno, buf } => {
                self.release_error_buffer(read.buf, buf, pool)?;
                return Err(SegmentReadIoError::FileRead {
                    segment: self.segment(),
                    offset_bytes: read.offset_bytes,
                    errno,
                });
            }
            other => {
                self.in_flight = Some(read);
                return Err(SegmentReadIoError::UnexpectedCompletionKind {
                    segment: self.segment(),
                    result: completion_kind(&other),
                });
            }
        }

        Ok(self.reader.is_finished())
    }

    fn handle_file_read<S>(
        &mut self,
        offset_bytes: u64,
        buf: BufferId,
        len: u32,
        pool: &mut BufferPool,
        sink: &mut S,
    ) -> Result<(), SegmentReadIoError<S::Error>>
    where
        S: SegmentFrameSink,
    {
        if len as usize > pool.buf_size() {
            pool.release(buf);
            return Err(SegmentReadIoError::ReadLenTooLarge {
                segment: self.segment(),
                len,
                buffer_len_bytes: pool.buf_size(),
            });
        }
        let result = {
            let bytes = &pool.bytes(buf)[..len as usize];
            self.reader.push_read(offset_bytes, bytes, sink)
        };
        pool.release(buf);
        result.map_err(SegmentReadIoError::Reader)
    }

    fn release_error_buffer<E>(
        &self,
        expected: BufferId,
        actual: Option<BufferId>,
        pool: &mut BufferPool,
    ) -> Result<(), SegmentReadIoError<E>> {
        let Some(actual) = actual else {
            pool.release(expected);
            return Err(SegmentReadIoError::MissingErrorBuffer { segment: self.segment() });
        };
        assert_eq!(actual, expected, "file error completion returned the wrong buffer");
        pool.release(actual);
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentReadIoError<E> {
    ReadAlreadyInFlight { segment: SegmentId, offset_bytes: u64 },
    ReadBufferTooSmall { segment: SegmentId, request_len_bytes: u32, buffer_len_bytes: usize },
    ReadBufferUnavailable { segment: SegmentId },
    UnexpectedToken { segment: SegmentId, expected: CompletionToken, got: CompletionToken },
    UnexpectedCompletion { segment: SegmentId },
    UnexpectedCompletionKind { segment: SegmentId, result: &'static str },
    MissingErrorBuffer { segment: SegmentId },
    ReadLenTooLarge { segment: SegmentId, len: u32, buffer_len_bytes: usize },
    FileRead { segment: SegmentId, offset_bytes: u64, errno: i32 },
    Reader(SegmentReadError<E>),
}

impl<E: fmt::Display> fmt::Display for SegmentReadIoError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentReadIoError::ReadAlreadyInFlight { segment, offset_bytes } => write!(
                f,
                "segment {} already has a read in flight at offset {offset_bytes}",
                segment.file_name()
            ),
            SegmentReadIoError::ReadBufferTooSmall {
                segment,
                request_len_bytes,
                buffer_len_bytes,
            } => write!(
                f,
                "segment {} read request {request_len_bytes} exceeds buffer size {buffer_len_bytes}",
                segment.file_name()
            ),
            SegmentReadIoError::ReadBufferUnavailable { segment } => {
                write!(f, "segment {} could not lease a read buffer", segment.file_name())
            }
            SegmentReadIoError::UnexpectedToken { segment, expected, got } => write!(
                f,
                "segment {} read token mismatch: expected {expected:?}, got {got:?}",
                segment.file_name()
            ),
            SegmentReadIoError::UnexpectedCompletion { segment } => write!(
                f,
                "segment {} received a read completion with no read in flight",
                segment.file_name()
            ),
            SegmentReadIoError::UnexpectedCompletionKind { segment, result } => write!(
                f,
                "segment {} received unexpected file completion {result}",
                segment.file_name()
            ),
            SegmentReadIoError::MissingErrorBuffer { segment } => write!(
                f,
                "segment {} file error did not return the leased buffer",
                segment.file_name()
            ),
            SegmentReadIoError::ReadLenTooLarge { segment, len, buffer_len_bytes } => write!(
                f,
                "segment {} read completion length {len} exceeds buffer size {buffer_len_bytes}",
                segment.file_name()
            ),
            SegmentReadIoError::FileRead { segment, offset_bytes, errno } => write!(
                f,
                "segment {} read at offset {offset_bytes} failed with errno {errno}",
                segment.file_name()
            ),
            SegmentReadIoError::Reader(error) => error.fmt(f),
        }
    }
}

impl<E> std::error::Error for SegmentReadIoError<E> where E: std::error::Error + 'static {}

fn completion_kind(result: &CompletionResult) -> &'static str {
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
    use crate::checkpoint::{
        CheckpointImagePublishConfig, CheckpointKeyspacePublishConfig,
        CheckpointKeyspaceSnapshotConfig, encode_checkpoint_keyspace_snapshot_sections,
        publish_checkpoint_image, publish_checkpoint_keyspace_snapshot_image,
    };
    use crate::durability::{stage_checkpoint_begin, stage_mutation_effect};
    use core::convert::Infallible;
    use inf_foundation::{
        CellId,
        rng::{Entropy, SplitMix64},
        time::Nanos,
    };
    use inf_log::{
        CheckpointHeader, CheckpointId, CheckpointRef, CheckpointSectionKind, CheckpointSectionRef,
        LogStaging, Lsn, NamespaceId, RecordKind, RecordRef, SegmentFrame, SegmentFrameError,
        encode_batch_frame, iter_segment_frames,
    };
    use inf_runtime::{Capabilities, FileOpenMode, SubmitStats, Wait};
    use inf_store::{
        Keyspace, MutationEffect, NsCatalog, NsCreateSpec, NsFsyncPolicy, NsId, NsMode, NsSpec,
        SetOptions, StoreConfig, decode_namespace_catalog, encode_namespace_catalog,
    };
    use std::fs;
    use std::path::PathBuf;

    const DIR_FD: RawFd = 40;
    const TEMP_FD: RawFd = 77;
    const CHECKPOINT_FD: RawFd = 78;
    const TOKEN_SLOT: u32 = 17;
    const TOKEN_GEN: u32 = 3;
    const TEST_EIO: i32 = 5;

    #[derive(Debug, Default)]
    struct TestDriver {
        ops: Vec<IoOp>,
        written: Vec<u8>,
        file_bytes: Vec<u8>,
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
                    IoOp::FileOpen { mode, token, .. } => {
                        let fd =
                            if mode == FileOpenMode::ReadOnly { CHECKPOINT_FD } else { TEMP_FD };
                        out.push(Completion { token, result: CompletionResult::FileOpened { fd } });
                    }
                    IoOp::FileWriteAt { offset_bytes, buf, len, token, .. } => {
                        let start = offset_bytes as usize;
                        let end = start + len as usize;
                        if end > self.written.len() {
                            self.written.resize(end, 0);
                        }
                        self.written[start..end].copy_from_slice(&pool.bytes(buf)[..len as usize]);
                        out.push(Completion {
                            token,
                            result: CompletionResult::FileWritten { buf },
                        });
                    }
                    IoOp::FileReadAt { offset_bytes, buf, len, token, .. } => {
                        let start = offset_bytes as usize;
                        let available = self.file_bytes.get(start..).unwrap_or(&[]);
                        let read_len = available.len().min(len as usize);
                        pool.bytes_mut(buf)[..read_len].copy_from_slice(&available[..read_len]);
                        out.push(Completion {
                            token,
                            result: CompletionResult::FileRead { buf, len: read_len as u32 },
                        });
                    }
                    IoOp::FileSync { token, .. } | IoOp::FileRename { token, .. } => {
                        out.push(Completion { token, result: CompletionResult::FileDone });
                    }
                    IoOp::FileClose { token, .. } => {
                        out.push(Completion { token, result: CompletionResult::FileClosed });
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

    #[derive(Debug, Default)]
    struct PayloadSink {
        payloads: Vec<Vec<u8>>,
    }

    impl SegmentFrameSink for PayloadSink {
        type Error = Infallible;

        fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error> {
            self.payloads
                .extend(frame.frame().records().map(|record| record.record().payload().to_vec()));
            Ok(())
        }
    }

    fn record(payload: &[u8]) -> RecordRef<'_> {
        RecordRef::new(RecordKind::StringPostImage, NamespaceId::new(1), 0, payload).unwrap()
    }

    fn append_frame(out: &mut Vec<u8>, segment: SegmentId, payloads: &[Vec<u8>]) {
        let offset = out.len() as u32;
        let refs: Vec<_> = payloads.iter().map(|payload| record(payload)).collect();
        encode_batch_frame(Lsn::new(segment.get(), offset), &refs, out).unwrap();
    }

    fn mutation_frame(
        segment: SegmentId,
        effects: &[(NamespaceId, MutationEffect<'_>)],
    ) -> Vec<u8> {
        let mut staging = LogStaging::with_capacity(1024).unwrap();
        for (namespace, effect) in effects {
            stage_mutation_effect(&mut staging, *namespace, *effect).unwrap();
        }
        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(segment.get(), 0), &mut frame).unwrap().unwrap();
        frame
    }

    fn checkpoint_begin_frame(segment: SegmentId, checkpoint: CheckpointId) -> Vec<u8> {
        let mut staging = LogStaging::with_capacity(64).unwrap();
        stage_checkpoint_begin(&mut staging, checkpoint).unwrap();
        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(segment.get(), 0), &mut frame).unwrap().unwrap();
        frame
    }

    fn append_staged_frame(out: &mut Vec<u8>, segment: SegmentId, staging: &mut LogStaging) -> Lsn {
        let first_lsn = Lsn::new(segment.get(), out.len() as u32);
        staging.drain_frame(first_lsn, out).unwrap().unwrap();
        first_lsn
    }

    fn checkpoint_ref() -> CheckpointRef {
        CheckpointRef::new(CheckpointId::new(9).unwrap(), Lsn::new(3, 128))
    }

    fn publish_config() -> CheckpointImagePublishConfig {
        CheckpointImagePublishConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn load_config() -> CheckpointImageLoadConfig {
        CheckpointImageLoadConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("inf-server-recovery-{name}-{}", std::process::id()))
    }

    #[test]
    fn keyspace_replay_sink_applies_default_namespace_mutations() {
        let segment = SegmentId::ZERO;
        let bytes = mutation_frame(
            segment,
            &[
                (
                    NamespaceId::new(0),
                    MutationEffect::StringPostImage {
                        key: b"k",
                        value: b"value",
                        expire_at_ms: Some(50),
                        raw: false,
                    },
                ),
                (NamespaceId::new(0), MutationEffect::ExpireAt { key: b"k", expire_at_ms: None }),
                (
                    NamespaceId::new(1),
                    MutationEffect::StringPostImage {
                        key: b"other",
                        value: b"db1",
                        expire_at_ms: None,
                        raw: true,
                    },
                ),
            ],
        );
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());

        {
            let mut sink = KeyspaceReplaySink::new(&mut keyspace);
            let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed);
            for frame in &mut iter {
                sink.push_frame(frame.unwrap()).unwrap();
            }
            assert_eq!(sink.stats().frames, 1);
            assert_eq!(sink.stats().records, 3);
            assert!(sink.stats().last_frame_end.is_some());
        }

        assert_eq!(keyspace.db_mut(0).get(b"k", Nanos(100_000_000)), Some(b"value".as_slice()));
        assert_eq!(keyspace.db_mut(1).get(b"other", Nanos(0)), Some(b"db1".as_slice()));
        assert_eq!(
            keyspace.db_mut(1).object_encoding(b"other", Nanos(0)),
            Some((inf_store::Encoding::Raw, None))
        );
    }

    #[test]
    fn checkpoint_records_payload_apply_restores_default_and_durable_named_state() {
        let now = Nanos(1_000_000);
        let mut source = Keyspace::new(StoreConfig::default());
        source.db_mut(0).set(b"d0", b"zero", SetOptions::default(), now).unwrap();
        let ledger = source
            .ns_create(NsCreateSpec {
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: Some(NsFsyncPolicy::Always),
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        source
            .named_db_mut(ledger)
            .unwrap()
            .set(b"ln", b"named", SetOptions::default(), now)
            .unwrap();
        let cache = source
            .ns_create(NsCreateSpec {
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        source
            .named_db_mut(cache)
            .unwrap()
            .set(b"volatile", b"skip", SetOptions::default(), now)
            .unwrap();
        let mut namespaces = Vec::new();
        let mut catalog = Vec::new();
        let mut records = Vec::new();
        encode_checkpoint_keyspace_snapshot_sections(
            &source,
            CheckpointKeyspaceSnapshotConfig::new(now),
            &mut namespaces,
            &mut catalog,
            &mut records,
        )
        .unwrap();
        let recovered_catalog = decode_namespace_catalog(&catalog).unwrap();
        let mut target = Keyspace::new(StoreConfig::default());
        target.ns_replace_with_recovered_catalog(recovered_catalog).unwrap();

        let stats =
            apply_checkpoint_records_payload(&mut target, &records, Lsn::new(0, 0)).unwrap();

        assert_eq!(stats.records, 2);
        assert!(stats.last_record_lsn.is_some());
        assert_eq!(target.db_mut(0).get(b"d0", now), Some(b"zero".as_slice()));
        assert_eq!(
            target.durable_named_db_mut(ledger).unwrap().get(b"ln", now),
            Some(b"named".as_slice())
        );
        assert!(target.ns_get_by_id(cache).is_some(), "memory namespace definition is cataloged");
        assert!(target.named_db(cache).is_none(), "memory namespace data is not checkpointed");
    }

    #[test]
    fn checkpoint_records_payload_apply_rejects_named_memory_records() {
        let mut target = Keyspace::new(StoreConfig::default());
        let cache = target
            .ns_create(NsCreateSpec {
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        let mut staging = LogStaging::with_capacity(128).unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(cache.get()),
            MutationEffect::StringPostImage {
                key: b"volatile",
                value: b"skip",
                expire_at_ms: None,
                raw: false,
            },
        )
        .unwrap();

        let error =
            apply_checkpoint_records_payload(&mut target, staging.record_bytes(), Lsn::new(0, 0))
                .unwrap_err();

        assert!(matches!(
            error,
            CheckpointRecordsApplyError::UnsupportedNamespace { namespace, .. }
                if namespace == NamespaceId::new(cache.get())
        ));
        assert!(target.named_db(cache).is_none());
    }

    #[test]
    fn checkpoint_image_apply_loads_catalog_and_records_into_cold_keyspace() {
        let now = Nanos(1_000_000);
        let mut source = Keyspace::new(StoreConfig::default());
        source.db_mut(0).set(b"d0", b"zero", SetOptions::default(), now).unwrap();
        let ledger = source
            .ns_create(NsCreateSpec {
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: Some(NsFsyncPolicy::Always),
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        source
            .named_db_mut(ledger)
            .unwrap()
            .set(b"ln", b"named", SetOptions::default(), now)
            .unwrap();
        let cache = source
            .ns_create(NsCreateSpec {
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        source
            .named_db_mut(cache)
            .unwrap()
            .set(b"volatile", b"skip", SetOptions::default(), now)
            .unwrap();
        let mut publish_driver = TestDriver::default();
        let mut pool = BufferPool::new(4, 13);
        let mut completions = Vec::new();

        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut publish_driver,
            &mut pool,
            CellId(4),
            checkpoint_ref(),
            &source,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                publish_config(),
            ),
            &mut completions,
        )
        .unwrap();
        assert_eq!(published.snapshot().records_emitted(), 2);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());

        let mut load_driver =
            TestDriver { file_bytes: publish_driver.written.clone(), ..TestDriver::default() };
        let mut target = Keyspace::new(StoreConfig::default());
        let applied = apply_checkpoint_image_to_keyspace(
            &mut load_driver,
            &mut pool,
            &mut target,
            checkpoint_ref(),
            load_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(applied.image().checkpoint(), checkpoint_ref());
        assert_eq!(applied.records().records, 2);
        assert_eq!(target.db_mut(0).get(b"d0", now), Some(b"zero".as_slice()));
        assert_eq!(
            target.durable_named_db_mut(ledger).unwrap().get(b"ln", now),
            Some(b"named".as_slice())
        );
        assert!(target.ns_get_by_id(cache).is_some(), "memory namespace definition is cataloged");
        assert!(target.named_db(cache).is_none(), "memory namespace data is not checkpointed");
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
    }

    #[test]
    fn checkpoint_image_apply_round_trips_seeded_keyspaces() {
        for seed in 0..16 {
            let now = Nanos(2_000_000 + seed);
            let mut rng = SplitMix64::new(0xC1C1_0063 ^ seed);
            let mut source = Keyspace::new(StoreConfig::default());
            let ledger = source
                .ns_create(NsCreateSpec {
                    name: format!("ledger-{seed}").into_bytes(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Always),
                    policy: None,
                    maxmemory: None,
                })
                .unwrap();
            let cache = source
                .ns_create(NsCreateSpec {
                    name: format!("cache-{seed}").into_bytes(),
                    mode: NsMode::Memory,
                    fsync: None,
                    policy: None,
                    maxmemory: None,
                })
                .unwrap();
            let mut expected_default = Vec::new();
            let mut expected_named = Vec::new();
            seed_default_checkpoint_records(
                &mut source,
                &mut rng,
                now,
                seed,
                &mut expected_default,
            );
            seed_named_checkpoint_records(
                &mut source,
                &mut rng,
                now,
                ledger,
                seed,
                &mut expected_named,
            );
            source
                .named_db_mut(cache)
                .unwrap()
                .set(b"volatile", b"skip", SetOptions::default(), now)
                .unwrap();

            let checkpoint =
                CheckpointRef::new(CheckpointId::new(20 + seed as u32).unwrap(), Lsn::new(3, 128));
            let mut publish_driver = TestDriver::default();
            let mut pool = BufferPool::new(4, 4096);
            let mut completions = Vec::new();
            let published = publish_checkpoint_keyspace_snapshot_image(
                &mut publish_driver,
                &mut pool,
                CellId(4),
                checkpoint,
                &source,
                CheckpointKeyspacePublishConfig::new(
                    CheckpointKeyspaceSnapshotConfig::new(now),
                    publish_config(),
                ),
                &mut completions,
            )
            .unwrap();
            let expected_records = expected_default.len() + expected_named.len();
            assert_eq!(published.snapshot().records_emitted(), expected_records);
            assert!(completions.is_empty());

            let mut load_driver =
                TestDriver { file_bytes: publish_driver.written.clone(), ..TestDriver::default() };
            let mut target = Keyspace::new(StoreConfig::default());
            let applied = apply_checkpoint_image_to_keyspace(
                &mut load_driver,
                &mut pool,
                &mut target,
                checkpoint,
                load_config(),
                &mut completions,
            )
            .unwrap();

            assert_eq!(applied.records().records as usize, expected_records);
            for (db, key, value) in &expected_default {
                assert_eq!(target.db_mut(*db).get(key, now), Some(value.as_slice()));
            }
            for (key, value) in &expected_named {
                assert_eq!(
                    target.durable_named_db_mut(ledger).unwrap().get(key, now),
                    Some(value.as_slice())
                );
            }
            assert!(target.ns_get_by_id(cache).is_some());
            assert!(target.named_db(cache).is_none());
            assert_eq!(pool.reconcile(), Ok(()));
            assert!(completions.is_empty());
        }
    }

    fn seed_default_checkpoint_records(
        source: &mut Keyspace,
        rng: &mut SplitMix64,
        now: Nanos,
        seed: u64,
        expected: &mut Vec<(usize, Vec<u8>, Vec<u8>)>,
    ) {
        let count = 1 + rng.next_below(8) as usize;
        for idx in 0..count {
            let db = rng.next_below(4) as usize;
            let key = format!("d{seed}-{idx}-{db}").into_bytes();
            let value = format!("v{:016x}", rng.next_u64()).into_bytes();
            source.db_mut(db).set(&key, &value, SetOptions::default(), now).unwrap();
            expected.push((db, key, value));
        }
    }

    fn seed_named_checkpoint_records(
        source: &mut Keyspace,
        rng: &mut SplitMix64,
        now: Nanos,
        ledger: NsId,
        seed: u64,
        expected: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) {
        let count = 1 + rng.next_below(6) as usize;
        for idx in 0..count {
            let key = format!("n{seed}-{idx}").into_bytes();
            let value = format!("nv{:016x}", rng.next_u64()).into_bytes();
            source
                .durable_named_db_mut(ledger)
                .unwrap()
                .set(&key, &value, SetOptions::default(), now)
                .unwrap();
            expected.push((key, value));
        }
    }

    #[test]
    fn checkpoint_image_apply_rejects_nonempty_target_before_io() {
        let now = Nanos(1_000_000);
        let mut target = Keyspace::new(StoreConfig::default());
        target.db_mut(0).set(b"existing", b"value", SetOptions::default(), now).unwrap();
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 64);
        let mut completions = Vec::new();

        let error = apply_checkpoint_image_to_keyspace(
            &mut driver,
            &mut pool,
            &mut target,
            checkpoint_ref(),
            load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointImageApplyError::TargetNotEmpty { live_records: 1, named_namespaces: 0 }
        ));
        assert!(driver.ops.is_empty());
        assert!(driver.written.is_empty());
        assert!(completions.is_empty());
    }

    #[test]
    fn checkpoint_image_apply_rejects_named_memory_records_before_catalog_install() {
        let mut source = Keyspace::new(StoreConfig::default());
        let cache = source
            .ns_create(NsCreateSpec {
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            })
            .unwrap();
        let mut catalog = Vec::new();
        encode_namespace_catalog(&source.ns_catalog_snapshot(), &mut catalog).unwrap();
        let mut staging = LogStaging::with_capacity(128).unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(cache.get()),
            MutationEffect::StringPostImage {
                key: b"volatile",
                value: b"skip",
                expire_at_ms: None,
                raw: false,
            },
        )
        .unwrap();
        let records = staging.record_bytes().to_vec();
        let namespaces = [NamespaceId::new(cache.get())];
        let header = CheckpointHeader::new(CellId(4), checkpoint_ref(), 2, &namespaces).unwrap();
        let sections = [
            CheckpointSectionRef::new(0, CheckpointSectionKind::NamespaceCatalog, &catalog)
                .unwrap(),
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, &records).unwrap(),
        ];
        let mut publish_driver = TestDriver::default();
        let mut pool = BufferPool::new(4, 64);
        let mut completions = Vec::new();
        publish_checkpoint_image(
            &mut publish_driver,
            &mut pool,
            header,
            &sections,
            publish_config(),
            &mut completions,
        )
        .unwrap();
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());

        let mut load_driver =
            TestDriver { file_bytes: publish_driver.written, ..TestDriver::default() };
        let mut target = Keyspace::new(StoreConfig::default());
        let error = apply_checkpoint_image_to_keyspace(
            &mut load_driver,
            &mut pool,
            &mut target,
            checkpoint_ref(),
            load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointImageApplyError::Records(
                CheckpointRecordsApplyError::UnsupportedNamespace { namespace, .. }
            ) if namespace == NamespaceId::new(cache.get())
        ));
        assert!(target.ns_iter().next().is_none(), "catalog is installed only after validation");
        assert!(target.named_dbs().next().is_none());
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
    }

    #[test]
    fn keyspace_replay_sink_validates_and_ignores_checkpoint_begin_records() {
        let segment = SegmentId::ZERO;
        let bytes = checkpoint_begin_frame(segment, CheckpointId::new(7).unwrap());
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        let mut sink = KeyspaceReplaySink::new(&mut keyspace);
        let frame = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap();

        sink.push_frame(frame).unwrap();

        assert_eq!(sink.stats().frames, 1);
        assert_eq!(sink.stats().records, 0);
    }

    #[test]
    fn keyspace_replay_sink_from_lsn_skips_pre_checkpoint_frames() {
        let segment = SegmentId::ZERO;
        let now = Nanos(1_000_000);
        let mut bytes = Vec::new();
        let mut before = LogStaging::with_capacity(256).unwrap();
        stage_mutation_effect(
            &mut before,
            NamespaceId::new(0),
            MutationEffect::StringPostImage {
                key: b"before",
                value: b"skip",
                expire_at_ms: None,
                raw: false,
            },
        )
        .unwrap();
        append_staged_frame(&mut bytes, segment, &mut before);
        let mut begin = LogStaging::with_capacity(64).unwrap();
        stage_checkpoint_begin(&mut begin, CheckpointId::new(7).unwrap()).unwrap();
        let begin_lsn = append_staged_frame(&mut bytes, segment, &mut begin);
        let mut after = LogStaging::with_capacity(256).unwrap();
        stage_mutation_effect(
            &mut after,
            NamespaceId::new(0),
            MutationEffect::StringPostImage {
                key: b"after",
                value: b"keep",
                expire_at_ms: None,
                raw: false,
            },
        )
        .unwrap();
        append_staged_frame(&mut bytes, segment, &mut after);
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());

        {
            let mut sink = KeyspaceReplaySink::from_lsn(&mut keyspace, begin_lsn);
            for frame in iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed) {
                sink.push_frame(frame.unwrap()).unwrap();
            }
            assert_eq!(sink.stats().frames, 2);
            assert_eq!(sink.stats().records, 1);
        }

        assert_eq!(keyspace.db_mut(0).get(b"before", now), None);
        assert_eq!(keyspace.db_mut(0).get(b"after", now), Some(b"keep".as_slice()));
    }

    #[test]
    fn keyspace_replay_sink_from_lsn_rejects_mid_frame_begin() {
        let segment = SegmentId::ZERO;
        let bytes = mutation_frame(
            segment,
            &[(
                NamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"k",
                    value: b"v",
                    expire_at_ms: None,
                    raw: false,
                },
            )],
        );
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        let mut sink = KeyspaceReplaySink::from_lsn(&mut keyspace, Lsn::new(segment.get(), 1));
        let frame = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap();

        let error = sink.push_frame(frame).unwrap_err();

        assert!(matches!(error, KeyspaceReplayError::BeginLsnInsideFrame { .. }));
        assert_eq!(keyspace.db_mut(0).get(b"k", Nanos(0)), None);
    }

    #[test]
    fn keyspace_replay_sink_rejects_malformed_checkpoint_begin_records() {
        let segment = SegmentId::ZERO;
        let checkpoint = CheckpointId::new(7).unwrap();
        let bytes = checkpoint.get().to_le_bytes();
        let bad_record =
            RecordRef::new(RecordKind::CheckpointBegin, NamespaceId::new(0), 0, &bytes).unwrap();
        let mut image = Vec::new();
        encode_batch_frame(Lsn::new(segment.get(), 0), &[bad_record], &mut image).unwrap();
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        let mut sink = KeyspaceReplaySink::new(&mut keyspace);
        let frame = iter_segment_frames(segment, &image, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap();

        let error = sink.push_frame(frame).unwrap_err();

        assert!(matches!(
            error,
            KeyspaceReplayError::CheckpointBegin {
                source: CheckpointBeginRecordDecodeError::InvalidNamespace { namespace },
                ..
            } if namespace == NamespaceId::new(0)
        ));
    }

    #[test]
    fn keyspace_replay_sink_applies_named_durable_namespace_mutations() {
        let segment = SegmentId::ZERO;
        let bytes = mutation_frame(
            segment,
            &[(
                NamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"order:1",
                    value: b"paid",
                    expire_at_ms: None,
                    raw: false,
                },
            )],
        );
        let catalog = NsCatalog::new(
            NsId::new(17),
            vec![NsSpec {
                id: NsId::new(16),
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: Some(NsFsyncPolicy::Always),
                policy: None,
                maxmemory: None,
            }],
        )
        .expect("catalog");
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        keyspace.ns_replace_with_recovered_catalog(catalog).expect("recover catalog");

        {
            let mut sink = KeyspaceReplaySink::new(&mut keyspace);
            let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed);
            for frame in &mut iter {
                sink.push_frame(frame.unwrap()).unwrap();
            }
            assert_eq!(sink.stats().frames, 1);
            assert_eq!(sink.stats().records, 1);
        }

        assert_eq!(
            keyspace
                .durable_named_db_mut(NsId::new(16))
                .expect("named durable store")
                .get(b"order:1", Nanos(0)),
            Some(b"paid".as_slice())
        );
    }

    #[test]
    fn keyspace_replay_sink_rejects_unroutable_namespace_ids() {
        let segment = SegmentId::ZERO;
        let bytes = mutation_frame(
            segment,
            &[(NamespaceId::new(16), MutationEffect::Delete { key: b"k" })],
        );
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        let mut sink = KeyspaceReplaySink::new(&mut keyspace);
        let frame = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap();

        let error = sink.push_frame(frame).unwrap_err();

        assert!(matches!(
            error,
            KeyspaceReplayError::UnsupportedNamespace { namespace, .. }
                if namespace == NamespaceId::new(16)
        ));
    }

    #[test]
    fn keyspace_replay_sink_rejects_named_memory_namespace_records() {
        let segment = SegmentId::ZERO;
        let bytes = mutation_frame(
            segment,
            &[(NamespaceId::new(16), MutationEffect::Delete { key: b"k" })],
        );
        let catalog = NsCatalog::new(
            NsId::new(17),
            vec![NsSpec {
                id: NsId::new(16),
                name: b"cache".to_vec(),
                mode: NsMode::Memory,
                fsync: None,
                policy: None,
                maxmemory: None,
            }],
        )
        .expect("catalog");
        let mut keyspace = inf_store::Keyspace::new(StoreConfig::default());
        keyspace.ns_replace_with_recovered_catalog(catalog).expect("recover catalog");
        let mut sink = KeyspaceReplaySink::new(&mut keyspace);
        let frame = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap();

        let error = sink.push_frame(frame).unwrap_err();

        assert!(matches!(
            error,
            KeyspaceReplayError::UnsupportedNamespace { namespace, .. }
                if namespace == NamespaceId::new(16)
        ));
    }

    #[test]
    fn host_segment_scan_distinguishes_missing_directory_from_empty_catalog() {
        let missing = temp_dir("missing");
        let _ = fs::remove_dir_all(&missing);
        let mut scratch = Vec::new();

        assert!(scan_host_segment_directory(&missing, &mut scratch).unwrap().is_none());

        let empty = temp_dir("empty");
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir(&empty).unwrap();
        let catalog = scan_host_segment_directory(&empty, &mut scratch).unwrap().unwrap();

        assert!(catalog.is_empty());
        fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn host_segment_scan_fails_closed_for_bad_entries() {
        let dir = temp_dir("bad-entry");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("notes.txt"), b"not a segment").unwrap();
        let mut scratch = Vec::new();

        let error = scan_host_segment_directory(&dir, &mut scratch).unwrap_err();

        assert!(matches!(error, HostSegmentScanError::Scan(_)));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn job(segment: SegmentId, chunk_bytes: u32) -> SegmentReadIo {
        SegmentReadIo::new(
            77,
            4,
            9,
            segment,
            SegmentTailPolicy::Sealed,
            SegmentReadConfig::new(chunk_bytes).unwrap(),
        )
    }

    fn active_tail_job(segment: SegmentId, chunk_bytes: u32) -> SegmentReadIo {
        SegmentReadIo::new(
            77,
            4,
            9,
            segment,
            SegmentTailPolicy::ActiveTail,
            SegmentReadConfig::new(chunk_bytes).unwrap(),
        )
    }

    fn complete_next(
        read: &mut SegmentReadIo,
        pool: &mut BufferPool,
        ops: &mut Vec<IoOp>,
        bytes: &[u8],
        sink: &mut PayloadSink,
    ) -> Result<bool, SegmentReadIoError<Infallible>> {
        assert!(read.queue_next(pool, ops)?);
        let op = ops.pop().unwrap();
        let (offset_bytes, buf, token, len) = match op {
            IoOp::FileReadAt { offset_bytes, buf, token, len, .. } => {
                (offset_bytes, buf, token, len)
            }
            other => panic!("unexpected op {other:?}"),
        };
        assert!(bytes.len() <= len as usize);
        pool.bytes_mut(buf)[..bytes.len()].copy_from_slice(bytes);
        read.on_completion(
            Completion {
                token,
                result: CompletionResult::FileRead { buf, len: bytes.len() as u32 },
            },
            pool,
            sink,
        )
        .inspect(|_| assert_eq!(read.next_offset_for_test(), offset_bytes + bytes.len() as u64))
    }

    impl SegmentReadIo {
        fn next_offset_for_test(&self) -> u64 {
            self.reader.next_read_offset_bytes()
        }
    }

    #[test]
    fn segment_read_io_maps_requests_to_file_read_at_and_releases_buffers() {
        let segment = SegmentId::new(3).unwrap();
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"a".to_vec(), b"bb".to_vec()]);
        append_frame(&mut bytes, segment, &[b"ccc".to_vec()]);

        let mut read = job(segment, 7);
        let mut pool = BufferPool::new(2, 7);
        let mut ops = Vec::new();
        let mut sink = PayloadSink::default();
        let mut cursor = 0usize;

        while !read.is_finished() {
            if cursor == bytes.len() {
                assert!(complete_next(&mut read, &mut pool, &mut ops, &[], &mut sink).unwrap());
                break;
            }
            let end = (cursor + 7).min(bytes.len());
            let finished =
                complete_next(&mut read, &mut pool, &mut ops, &bytes[cursor..end], &mut sink)
                    .unwrap();
            cursor = end;
            assert!(!finished);
        }

        assert_eq!(sink.payloads, vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]);
        assert_eq!(read.frames_read(), 2);
        assert_eq!(read.records_read(), 3);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn segment_read_io_rejects_second_read_until_completion() {
        let segment = SegmentId::ZERO;
        let mut read = job(segment, 16);
        let mut pool = BufferPool::new(2, 16);
        let mut ops = Vec::new();

        assert_eq!(read.queue_next(&mut pool, &mut ops), Ok(true));
        assert_eq!(
            read.queue_next(&mut pool, &mut ops),
            Err(SegmentReadIoError::ReadAlreadyInFlight { segment, offset_bytes: 0 })
        );
    }

    #[test]
    fn segment_read_io_reports_small_buffer_before_leasing() {
        let segment = SegmentId::ZERO;
        let mut read = job(segment, 32);
        let mut pool = BufferPool::new(1, 16);
        let mut ops = Vec::new();

        assert_eq!(
            read.queue_next(&mut pool, &mut ops),
            Err(SegmentReadIoError::ReadBufferTooSmall {
                segment,
                request_len_bytes: 32,
                buffer_len_bytes: 16,
            })
        );
        assert_eq!(pool.leased(), 0);
        assert!(ops.is_empty());
    }

    #[test]
    fn segment_read_io_surfaces_file_errors_and_returns_buffer() {
        let segment = SegmentId::ZERO;
        let mut read = job(segment, 16);
        let mut pool = BufferPool::new(1, 16);
        let mut ops = Vec::new();
        let mut sink = PayloadSink::default();

        assert_eq!(read.queue_next(&mut pool, &mut ops), Ok(true));
        let op = ops.pop().unwrap();
        let (buf, token) = match op {
            IoOp::FileReadAt { buf, token, .. } => (buf, token),
            other => panic!("unexpected op {other:?}"),
        };
        let error = read
            .on_completion(
                Completion {
                    token,
                    result: CompletionResult::Error { errno: TEST_EIO, buf: Some(buf) },
                },
                &mut pool,
                &mut sink,
            )
            .unwrap_err();

        assert_eq!(
            error,
            SegmentReadIoError::FileRead { segment, offset_bytes: 0, errno: TEST_EIO }
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn segment_read_io_preserves_inflight_on_wrong_token() {
        let segment = SegmentId::ZERO;
        let mut read = job(segment, 16);
        let mut pool = BufferPool::new(1, 16);
        let mut ops = Vec::new();
        let mut sink = PayloadSink::default();

        assert_eq!(read.queue_next(&mut pool, &mut ops), Ok(true));
        let wrong = CompletionToken::new(TokenClass::File, 8, 9);
        let error = read
            .on_completion(
                Completion { token: wrong, result: CompletionResult::FileDone },
                &mut pool,
                &mut sink,
            )
            .unwrap_err();

        assert_eq!(
            error,
            SegmentReadIoError::UnexpectedToken { segment, expected: read.token(), got: wrong }
        );
        assert!(read.read_in_flight());

        let op = ops.pop().unwrap();
        let (buf, token) = match op {
            IoOp::FileReadAt { buf, token, .. } => (buf, token),
            other => panic!("unexpected op {other:?}"),
        };
        assert!(
            read.on_completion(
                Completion { token, result: CompletionResult::FileRead { buf, len: 0 } },
                &mut pool,
                &mut sink,
            )
            .unwrap()
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn segment_read_io_surfaces_reader_tail_errors() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"sealed".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let mut read = job(segment, 1024);
        let mut pool = BufferPool::new(1, 1024);
        let mut ops = Vec::new();
        let mut sink = PayloadSink::default();

        assert!(!complete_next(&mut read, &mut pool, &mut ops, &bytes, &mut sink).unwrap());
        let error = complete_next(&mut read, &mut pool, &mut ops, &[], &mut sink).unwrap_err();

        assert_eq!(
            error,
            SegmentReadIoError::Reader(SegmentReadError::Frame(SegmentFrameError::PartialFrame {
                segment,
                offset: tail_offset,
                needed: 24,
                available: 4,
            }))
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn segment_read_io_exposes_active_partial_tail_terminal() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"active".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let mut read = active_tail_job(segment, 1024);
        let mut pool = BufferPool::new(1, 1024);
        let mut ops = Vec::new();
        let mut sink = PayloadSink::default();

        assert!(!complete_next(&mut read, &mut pool, &mut ops, &bytes, &mut sink).unwrap());
        assert!(complete_next(&mut read, &mut pool, &mut ops, &[], &mut sink).unwrap());

        assert_eq!(
            read.terminal(),
            Some(SegmentReadTerminal::ActivePartialFrame {
                segment,
                offset: tail_offset,
                needed: 24,
                available: 4,
            })
        );
        assert_eq!(sink.payloads, vec![b"active".to_vec()]);
        assert_eq!(pool.reconcile(), Ok(()));
    }
}
