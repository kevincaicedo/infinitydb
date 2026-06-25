use core::fmt;
use std::io;

use inf_alloc::{BufferPool, LeaseKind};
use inf_log::{
    MAX_RECOVERY_MANIFEST_BYTES, RECOVERY_MANIFEST_FILE, RECOVERY_MANIFEST_TEMP_FILE,
    RecoveryManifest, RecoveryManifestError, decode_recovery_manifest, encode_recovery_manifest,
};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode, IoOp,
    RawFd, TokenClass, Wait,
};

pub const DEFAULT_RECOVERY_MANIFEST_REAP_LIMIT: u32 = 32;
const ENOENT_ERRNO: i32 = 2;
const MAX_RECOVERY_MANIFEST_READ_BYTES: usize = MAX_RECOVERY_MANIFEST_BYTES + 1;

#[derive(Copy, Clone, Debug)]
pub struct RecoveryManifestPublishConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl RecoveryManifestPublishConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> RecoveryManifestPublishConfig {
        RecoveryManifestPublishConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_RECOVERY_MANIFEST_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> RecoveryManifestPublishConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> RecoveryManifestPublishConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> RecoveryManifestPublishConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Copy, Clone, Debug)]
pub struct RecoveryManifestLoadConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl RecoveryManifestLoadConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> RecoveryManifestLoadConfig {
        RecoveryManifestLoadConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_RECOVERY_MANIFEST_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> RecoveryManifestLoadConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> RecoveryManifestLoadConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> RecoveryManifestLoadConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecoveryManifestPublishPhase {
    OpenTemp,
    WriteTemp,
    SyncTemp,
    CloseTemp,
    Rename,
    SyncDir,
}

impl fmt::Display for RecoveryManifestPublishPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryManifestPublishPhase::OpenTemp => write!(f, "open-temp"),
            RecoveryManifestPublishPhase::WriteTemp => write!(f, "write-temp"),
            RecoveryManifestPublishPhase::SyncTemp => write!(f, "sync-temp"),
            RecoveryManifestPublishPhase::CloseTemp => write!(f, "close-temp"),
            RecoveryManifestPublishPhase::Rename => write!(f, "rename"),
            RecoveryManifestPublishPhase::SyncDir => write!(f, "sync-dir"),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecoveryManifestLoadPhase {
    Open,
    Read,
    Close,
}

impl fmt::Display for RecoveryManifestLoadPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryManifestLoadPhase::Open => write!(f, "open"),
            RecoveryManifestLoadPhase::Read => write!(f, "read"),
            RecoveryManifestLoadPhase::Close => write!(f, "close"),
        }
    }
}

#[derive(Debug)]
pub enum RecoveryManifestPublishError {
    Encode(RecoveryManifestError),
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    WriteBufferUnavailable,
    Backend {
        phase: RecoveryManifestPublishPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: RecoveryManifestPublishPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: RecoveryManifestPublishPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    ReapLimitExceeded {
        phase: RecoveryManifestPublishPhase,
        token: CompletionToken,
        attempts: u32,
    },
    OpenTemp {
        name: &'static str,
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
        old_name: &'static str,
        new_name: &'static str,
        errno: i32,
    },
    SyncDir {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: RecoveryManifestPublishPhase,
        result: &'static str,
    },
}

#[derive(Debug)]
pub enum RecoveryManifestLoadError {
    Decode(RecoveryManifestError),
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    ManifestTooLarge {
        max_len_bytes: usize,
    },
    ReadBufferUnavailable,
    Backend {
        phase: RecoveryManifestLoadPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: RecoveryManifestLoadPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: RecoveryManifestLoadPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    ReapLimitExceeded {
        phase: RecoveryManifestLoadPhase,
        token: CompletionToken,
        attempts: u32,
    },
    Open {
        name: &'static str,
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
        buffer_len_bytes: usize,
    },
    Close {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: RecoveryManifestLoadPhase,
        result: &'static str,
    },
}

impl fmt::Display for RecoveryManifestPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryManifestPublishError::Encode(error) => {
                write!(f, "encode recovery manifest failed: {error}")
            }
            RecoveryManifestPublishError::ScratchNotEmpty { len } => {
                write!(f, "recovery manifest publish scratch completions not empty: {len}")
            }
            RecoveryManifestPublishError::ZeroReapLimit => {
                write!(f, "recovery manifest publish reap limit must be nonzero")
            }
            RecoveryManifestPublishError::WriteBufferUnavailable => {
                write!(f, "recovery manifest publish could not lease a write buffer")
            }
            RecoveryManifestPublishError::Backend { phase, source } => {
                write!(f, "recovery manifest publish backend failed during {phase}: {source}")
            }
            RecoveryManifestPublishError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "recovery manifest publish {phase} expected {expected} completion(s), got {got}"
                )
            }
            RecoveryManifestPublishError::UnexpectedToken { phase, expected, got } => {
                write!(
                    f,
                    "recovery manifest publish {phase} got token {got:?}, expected {expected:?}"
                )
            }
            RecoveryManifestPublishError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "recovery manifest publish {phase} saw no completion for {token:?} after {attempts} reap attempts"
            ),
            RecoveryManifestPublishError::OpenTemp { name, errno } => {
                write!(f, "open recovery manifest temp file {name:?} failed with errno {errno}")
            }
            RecoveryManifestPublishError::WriteTemp { fd, offset_bytes, errno } => write!(
                f,
                "write recovery manifest temp fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            RecoveryManifestPublishError::MissingWriteBuffer { fd, offset_bytes, errno } => write!(
                f,
                "write recovery manifest temp fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            RecoveryManifestPublishError::SyncTemp { fd, errno } => {
                write!(f, "sync recovery manifest temp fd {fd} failed with errno {errno}")
            }
            RecoveryManifestPublishError::CloseTemp { fd, errno } => {
                write!(f, "close recovery manifest temp fd {fd} failed with errno {errno}")
            }
            RecoveryManifestPublishError::Rename { old_name, new_name, errno } => write!(
                f,
                "rename recovery manifest {old_name:?} to {new_name:?} failed with errno {errno}"
            ),
            RecoveryManifestPublishError::SyncDir { fd, errno } => {
                write!(f, "sync recovery manifest directory fd {fd} failed with errno {errno}")
            }
            RecoveryManifestPublishError::UnexpectedCompletionKind { phase, result } => write!(
                f,
                "recovery manifest publish {phase} got unexpected completion kind {result}"
            ),
        }
    }
}

impl fmt::Display for RecoveryManifestLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryManifestLoadError::Decode(error) => {
                write!(f, "decode recovery manifest failed: {error}")
            }
            RecoveryManifestLoadError::ScratchNotEmpty { len } => {
                write!(f, "recovery manifest load scratch completions not empty: {len}")
            }
            RecoveryManifestLoadError::ZeroReapLimit => {
                write!(f, "recovery manifest load reap limit must be nonzero")
            }
            RecoveryManifestLoadError::ManifestTooLarge { max_len_bytes } => {
                write!(f, "recovery manifest exceeds maximum length {max_len_bytes} bytes")
            }
            RecoveryManifestLoadError::ReadBufferUnavailable => {
                write!(f, "recovery manifest load could not lease a read buffer")
            }
            RecoveryManifestLoadError::Backend { phase, source } => {
                write!(f, "recovery manifest load backend failed during {phase}: {source}")
            }
            RecoveryManifestLoadError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "recovery manifest load {phase} expected {expected} completion(s), got {got}"
                )
            }
            RecoveryManifestLoadError::UnexpectedToken { phase, expected, got } => {
                write!(f, "recovery manifest load {phase} got token {got:?}, expected {expected:?}")
            }
            RecoveryManifestLoadError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "recovery manifest load {phase} saw no completion for {token:?} after {attempts} reap attempts"
            ),
            RecoveryManifestLoadError::Open { name, errno } => {
                write!(f, "open recovery manifest {name:?} failed with errno {errno}")
            }
            RecoveryManifestLoadError::Read { fd, offset_bytes, errno } => write!(
                f,
                "read recovery manifest fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            RecoveryManifestLoadError::MissingReadBuffer { fd, offset_bytes, errno } => write!(
                f,
                "read recovery manifest fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            RecoveryManifestLoadError::ReadLenTooLarge {
                fd,
                offset_bytes,
                len,
                buffer_len_bytes,
            } => write!(
                f,
                "read recovery manifest fd {fd} at offset {offset_bytes} returned {len} bytes, larger than buffer size {buffer_len_bytes}"
            ),
            RecoveryManifestLoadError::Close { fd, errno } => {
                write!(f, "close recovery manifest fd {fd} failed with errno {errno}")
            }
            RecoveryManifestLoadError::UnexpectedCompletionKind { phase, result } => {
                write!(f, "recovery manifest load {phase} got unexpected completion kind {result}")
            }
        }
    }
}

impl std::error::Error for RecoveryManifestPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecoveryManifestPublishError::Encode(error) => Some(error),
            RecoveryManifestPublishError::Backend { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl std::error::Error for RecoveryManifestLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecoveryManifestLoadError::Decode(error) => Some(error),
            RecoveryManifestLoadError::Backend { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Publish the per-cell recovery `MANIFEST` image with the M2 atomic swap.
///
/// The cold recovery root protocol is exact-length `MANIFEST.tmp` write,
/// temp-file fdatasync, temp close, atomic rename to `MANIFEST`, then parent
/// directory fsync. The full checkpoint writer/load path remains separate.
pub fn publish_recovery_manifest<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    manifest: &RecoveryManifest,
    config: RecoveryManifestPublishConfig,
    completions: &mut Vec<Completion>,
) -> Result<(), RecoveryManifestPublishError>
where
    D: BackendDriver,
{
    validate_publish_inputs(completions, config.max_reaps)?;

    let mut bytes = Vec::new();
    encode_recovery_manifest(manifest, &mut bytes).map_err(RecoveryManifestPublishError::Encode)?;

    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = PublishIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        dir: config.dir,
    };

    io.publish_manifest_bytes(&bytes)
}

/// Load the optional per-cell recovery `MANIFEST` image from a shard directory.
///
/// Missing `MANIFEST` is first-boot or no-checkpoint state and returns
/// `Ok(None)`. Present but malformed bytes fail closed as a typed decode error.
pub fn load_recovery_manifest<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: RecoveryManifestLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<Option<RecoveryManifest>, RecoveryManifestLoadError>
where
    D: BackendDriver,
{
    validate_load_inputs(completions, config.max_reaps)?;

    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = LoadIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
    };

    io.load_from_dir(config.dir)
}

fn validate_publish_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), RecoveryManifestPublishError> {
    if !completions.is_empty() {
        return Err(RecoveryManifestPublishError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(RecoveryManifestPublishError::ZeroReapLimit);
    }
    Ok(())
}

fn validate_load_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), RecoveryManifestLoadError> {
    if !completions.is_empty() {
        return Err(RecoveryManifestLoadError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(RecoveryManifestLoadError::ZeroReapLimit);
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
}

struct LoadIo<'a, D> {
    driver: &'a mut D,
    pool: &'a mut BufferPool,
    completions: &'a mut Vec<Completion>,
    params: ReapParams,
    token: CompletionToken,
}

impl<D> PublishIo<'_, D>
where
    D: BackendDriver,
{
    fn publish_manifest_bytes(
        &mut self,
        manifest: &[u8],
    ) -> Result<(), RecoveryManifestPublishError> {
        let fd = self.open_temp()?;
        self.write_temp(fd, manifest)?;
        self.sync_temp(fd)?;
        self.close_temp(fd)?;
        self.rename_temp()?;
        self.sync_dir()?;
        Ok(())
    }

    fn open_temp(&mut self) -> Result<RawFd, RecoveryManifestPublishError> {
        self.driver.push(IoOp::FileOpen {
            dir: self.dir,
            name: RECOVERY_MANIFEST_TEMP_FILE.to_string(),
            mode: FileOpenMode::ReadWriteCreateTruncate,
            token: self.token,
        });
        match self.reap(RecoveryManifestPublishPhase::OpenTemp)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::OpenTemp {
                    name: RECOVERY_MANIFEST_TEMP_FILE,
                    errno,
                })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::OpenTemp,
                other,
            )),
        }
    }

    fn write_temp(
        &mut self,
        fd: RawFd,
        manifest: &[u8],
    ) -> Result<(), RecoveryManifestPublishError> {
        let mut offset = 0usize;
        while offset < manifest.len() {
            let chunk_len = self.pool.buf_size().min(manifest.len() - offset);
            self.write_temp_chunk(fd, offset, &manifest[offset..offset + chunk_len])?;
            offset += chunk_len;
        }
        Ok(())
    }

    fn write_temp_chunk(
        &mut self,
        fd: RawFd,
        offset: usize,
        chunk: &[u8],
    ) -> Result<(), RecoveryManifestPublishError> {
        let Some(buf) = self.pool.try_lease(LeaseKind::Send) else {
            return Err(RecoveryManifestPublishError::WriteBufferUnavailable);
        };
        self.pool.bytes_mut(buf)[..chunk.len()].copy_from_slice(chunk);
        let offset_bytes = offset as u64;
        self.driver.push(IoOp::FileWriteAt {
            fd,
            offset_bytes,
            buf,
            len: chunk.len() as u32,
            token: self.token,
        });
        match self.reap(RecoveryManifestPublishPhase::WriteTemp)?.result {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf, "manifest write completion returned the wrong buffer");
                self.pool.release(got);
                Ok(())
            }
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "manifest write error returned the wrong buffer");
                self.pool.release(got);
                Err(RecoveryManifestPublishError::WriteTemp { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::MissingWriteBuffer { fd, offset_bytes, errno })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::WriteTemp,
                other,
            )),
        }
    }

    fn sync_temp(&mut self, fd: RawFd) -> Result<(), RecoveryManifestPublishError> {
        self.driver.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: self.token });
        match self.reap(RecoveryManifestPublishPhase::SyncTemp)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::SyncTemp { fd, errno })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::SyncTemp,
                other,
            )),
        }
    }

    fn close_temp(&mut self, fd: RawFd) -> Result<(), RecoveryManifestPublishError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(RecoveryManifestPublishPhase::CloseTemp)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::CloseTemp { fd, errno })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::CloseTemp,
                other,
            )),
        }
    }

    fn rename_temp(&mut self) -> Result<(), RecoveryManifestPublishError> {
        self.driver.push(IoOp::FileRename {
            old_dir: self.dir,
            old_name: RECOVERY_MANIFEST_TEMP_FILE.to_string(),
            new_dir: self.dir,
            new_name: RECOVERY_MANIFEST_FILE.to_string(),
            token: self.token,
        });
        match self.reap(RecoveryManifestPublishPhase::Rename)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::Rename {
                    old_name: RECOVERY_MANIFEST_TEMP_FILE,
                    new_name: RECOVERY_MANIFEST_FILE,
                    errno,
                })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::Rename,
                other,
            )),
        }
    }

    fn sync_dir(&mut self) -> Result<(), RecoveryManifestPublishError> {
        self.driver.push(IoOp::FileSync {
            fd: self.dir,
            mode: FileSyncMode::Full,
            token: self.token,
        });
        match self.reap(RecoveryManifestPublishPhase::SyncDir)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestPublishError::SyncDir { fd: self.dir, errno })
            }
            other => Err(unexpected_completion_kind(
                self.pool,
                RecoveryManifestPublishPhase::SyncDir,
                other,
            )),
        }
    }

    fn reap(
        &mut self,
        phase: RecoveryManifestPublishPhase,
    ) -> Result<Completion, RecoveryManifestPublishError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| RecoveryManifestPublishError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(RecoveryManifestPublishError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                release_result_buffer(self.pool, &completion.result);
                return Err(RecoveryManifestPublishError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(RecoveryManifestPublishError::ReapLimitExceeded {
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
    ) -> Result<Option<RecoveryManifest>, RecoveryManifestLoadError> {
        let Some(fd) = self.open_manifest(dir)? else {
            return Ok(None);
        };
        let read = self.read_manifest(fd);
        let close = self.close_manifest(fd);
        match (read, close) {
            (Ok(image), Ok(())) => decode_recovery_manifest(&image)
                .map(Some)
                .map_err(RecoveryManifestLoadError::Decode),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn open_manifest(&mut self, dir: RawFd) -> Result<Option<RawFd>, RecoveryManifestLoadError> {
        self.driver.push(IoOp::FileOpen {
            dir,
            name: RECOVERY_MANIFEST_FILE.to_string(),
            mode: FileOpenMode::ReadOnly,
            token: self.token,
        });
        match self.reap(RecoveryManifestLoadPhase::Open)?.result {
            CompletionResult::FileOpened { fd } => Ok(Some(fd)),
            CompletionResult::Error { errno: ENOENT_ERRNO, buf: None } => Ok(None),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestLoadError::Open { name: RECOVERY_MANIFEST_FILE, errno })
            }
            other => Err(RecoveryManifestLoadError::UnexpectedCompletionKind {
                phase: RecoveryManifestLoadPhase::Open,
                result: result_name(&other),
            }),
        }
    }

    fn read_manifest(&mut self, fd: RawFd) -> Result<Vec<u8>, RecoveryManifestLoadError> {
        let mut image = Vec::new();
        let mut offset_bytes = 0u64;
        loop {
            let remaining = MAX_RECOVERY_MANIFEST_READ_BYTES - image.len();
            let chunk_len = self.pool.buf_size().min(remaining);
            let read = self.read_manifest_chunk(fd, offset_bytes, chunk_len as u32)?;
            if read.is_empty() {
                return Ok(image);
            }
            image.extend_from_slice(&read);
            if image.len() > MAX_RECOVERY_MANIFEST_BYTES {
                return Err(RecoveryManifestLoadError::ManifestTooLarge {
                    max_len_bytes: MAX_RECOVERY_MANIFEST_BYTES,
                });
            }
            offset_bytes += read.len() as u64;
        }
    }

    fn read_manifest_chunk(
        &mut self,
        fd: RawFd,
        offset_bytes: u64,
        len: u32,
    ) -> Result<Vec<u8>, RecoveryManifestLoadError> {
        debug_assert!(len > 0);
        let Some(buf) = self.pool.try_lease(LeaseKind::Recv) else {
            return Err(RecoveryManifestLoadError::ReadBufferUnavailable);
        };
        self.driver.push(IoOp::FileReadAt { fd, offset_bytes, buf, len, token: self.token });
        match self.reap(RecoveryManifestLoadPhase::Read)?.result {
            CompletionResult::FileRead { buf: got, len } => {
                self.handle_read_completion(fd, offset_bytes, buf, got, len)
            }
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "manifest read error returned the wrong buffer");
                self.pool.release(got);
                Err(RecoveryManifestLoadError::Read { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                self.pool.release(buf);
                Err(RecoveryManifestLoadError::MissingReadBuffer { fd, offset_bytes, errno })
            }
            other => {
                self.pool.release(buf);
                Err(RecoveryManifestLoadError::UnexpectedCompletionKind {
                    phase: RecoveryManifestLoadPhase::Read,
                    result: result_name(&other),
                })
            }
        }
    }

    fn handle_read_completion(
        &mut self,
        fd: RawFd,
        offset_bytes: u64,
        expected: inf_alloc::BufferId,
        got: inf_alloc::BufferId,
        len: u32,
    ) -> Result<Vec<u8>, RecoveryManifestLoadError> {
        assert_eq!(got, expected, "manifest read completion returned the wrong buffer");
        if len as usize > self.pool.buf_size() {
            self.pool.release(got);
            return Err(RecoveryManifestLoadError::ReadLenTooLarge {
                fd,
                offset_bytes,
                len,
                buffer_len_bytes: self.pool.buf_size(),
            });
        }
        let bytes = self.pool.bytes(got)[..len as usize].to_vec();
        self.pool.release(got);
        Ok(bytes)
    }

    fn close_manifest(&mut self, fd: RawFd) -> Result<(), RecoveryManifestLoadError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(RecoveryManifestLoadPhase::Close)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(RecoveryManifestLoadError::Close { fd, errno })
            }
            other => Err(RecoveryManifestLoadError::UnexpectedCompletionKind {
                phase: RecoveryManifestLoadPhase::Close,
                result: result_name(&other),
            }),
        }
    }

    fn reap(
        &mut self,
        phase: RecoveryManifestLoadPhase,
    ) -> Result<Completion, RecoveryManifestLoadError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| RecoveryManifestLoadError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(RecoveryManifestLoadError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                release_result_buffer(self.pool, &completion.result);
                return Err(RecoveryManifestLoadError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(RecoveryManifestLoadError::ReapLimitExceeded {
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

fn unexpected_completion_kind(
    pool: &mut BufferPool,
    phase: RecoveryManifestPublishPhase,
    result: CompletionResult,
) -> RecoveryManifestPublishError {
    let name = result_name(&result);
    release_result_buffer(pool, &result);
    RecoveryManifestPublishError::UnexpectedCompletionKind { phase, result: name }
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

    use inf_log::{
        CheckpointId, CheckpointRef, Lsn, MAX_RECOVERY_MANIFEST_BYTES, RecoveryManifestError,
        SegmentId, decode_recovery_manifest, encode_recovery_manifest, fault::MANIFEST_RENAME_FAIL,
        scan_segment_names,
    };
    use inf_runtime::{Capabilities, SubmitStats};

    const DIR_FD: RawFd = 40;
    const TEMP_FD: RawFd = 77;
    const MANIFEST_FD: RawFd = 78;
    const TOKEN_SLOT: u32 = 13;
    const TOKEN_GEN: u32 = 5;
    const TEST_ENOENT: i32 = 2;
    const TEST_EIO: i32 = 5;

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
        close_errno: Option<i32>,
        rename_errno: Option<i32>,
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
                                    MANIFEST_FD
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
                    IoOp::FileRename { old_dir, old_name, new_dir, new_name, token } => {
                        self.observed.push(ObservedOp::Rename {
                            old_dir,
                            old_name,
                            new_dir,
                            new_name,
                            token,
                        });
                        let result = match self.rename_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token, result });
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

    fn token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, TOKEN_SLOT, TOKEN_GEN)
    }

    fn config() -> RecoveryManifestPublishConfig {
        RecoveryManifestPublishConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn load_config() -> RecoveryManifestLoadConfig {
        RecoveryManifestLoadConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn manifest() -> RecoveryManifest {
        let names: Vec<String> =
            (2..=4).map(|raw| SegmentId::new(raw).unwrap().file_name()).collect();
        let segments = scan_segment_names(names.iter().map(String::as_str)).unwrap();
        RecoveryManifest::new(
            CheckpointRef::new(CheckpointId::new(8).unwrap(), Lsn::new(2, 64)),
            segments,
        )
        .unwrap()
    }

    #[test]
    fn publish_writes_syncs_renames_and_dirsyncs_manifest_image() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();
        let manifest = manifest();
        let mut expected = Vec::new();
        encode_recovery_manifest(&manifest, &mut expected).unwrap();

        publish_recovery_manifest(&mut driver, &mut pool, &manifest, config(), &mut completions)
            .unwrap();

        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert_eq!(driver.written, expected);
        assert_eq!(decode_recovery_manifest(&driver.written).unwrap(), manifest);
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: DIR_FD,
                    name: RECOVERY_MANIFEST_TEMP_FILE.to_string(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: token(),
                },
                ObservedOp::WriteAt {
                    fd: TEMP_FD,
                    offset_bytes: 0,
                    len: expected.len() as u32,
                    token: token(),
                },
                ObservedOp::Sync { fd: TEMP_FD, mode: FileSyncMode::DataOnly, token: token() },
                ObservedOp::Close { fd: TEMP_FD, token: token() },
                ObservedOp::Rename {
                    old_dir: DIR_FD,
                    old_name: RECOVERY_MANIFEST_TEMP_FILE.to_string(),
                    new_dir: DIR_FD,
                    new_name: RECOVERY_MANIFEST_FILE.to_string(),
                    token: token(),
                },
                ObservedOp::Sync { fd: DIR_FD, mode: FileSyncMode::Full, token: token() },
            ]
        );
    }

    #[test]
    fn publish_chunks_manifest_when_buffer_is_smaller_than_image() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 8);
        let mut completions = Vec::new();
        let manifest = manifest();
        let mut expected = Vec::new();
        encode_recovery_manifest(&manifest, &mut expected).unwrap();

        publish_recovery_manifest(&mut driver, &mut pool, &manifest, config(), &mut completions)
            .unwrap();

        assert_eq!(driver.written, expected);
        let writes: Vec<_> = driver
            .observed
            .iter()
            .filter_map(|op| match op {
                ObservedOp::WriteAt { offset_bytes, len, .. } => Some((*offset_bytes, *len)),
                _ => None,
            })
            .collect();
        assert!(writes.len() > 1);
        let mut next = 0u64;
        for (offset, len) in writes {
            assert_eq!(offset, next);
            next += u64::from(len);
        }
        assert_eq!(next as usize, expected.len());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn manifest_rename_fail_fault_is_typed_before_directory_sync() {
        assert_eq!(MANIFEST_RENAME_FAIL.name(), "manifest_rename_fail");
        let mut driver = TestDriver { rename_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let error = publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest(),
            config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(matches!(
            error,
            RecoveryManifestPublishError::Rename {
                old_name: RECOVERY_MANIFEST_TEMP_FILE,
                new_name: RECOVERY_MANIFEST_FILE,
                errno: TEST_EIO,
            }
        ));
        assert!(driver.observed.iter().any(|op| matches!(op, ObservedOp::Rename { .. })));
        assert!(
            !driver.observed.iter().any(|op| matches!(op, ObservedOp::Sync { fd: DIR_FD, .. }))
        );
    }

    #[test]
    fn load_missing_manifest_returns_none() {
        let mut driver = TestDriver { open_errno: Some(TEST_ENOENT), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();

        let loaded =
            load_recovery_manifest(&mut driver, &mut pool, load_config(), &mut completions)
                .unwrap();

        assert_eq!(loaded, None);
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
        assert_eq!(
            driver.observed,
            vec![ObservedOp::Open {
                dir: DIR_FD,
                name: RECOVERY_MANIFEST_FILE.to_string(),
                mode: FileOpenMode::ReadOnly,
                token: token(),
            }]
        );
    }

    #[test]
    fn load_reads_chunks_closes_and_decodes_manifest_image() {
        let manifest = manifest();
        let mut expected = Vec::new();
        encode_recovery_manifest(&manifest, &mut expected).unwrap();
        let mut driver = TestDriver { file_bytes: expected.clone(), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 9);
        let mut completions = Vec::new();

        let loaded =
            load_recovery_manifest(&mut driver, &mut pool, load_config(), &mut completions)
                .unwrap();

        assert_eq!(loaded, Some(manifest));
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
        let reads: Vec<_> = driver
            .observed
            .iter()
            .filter_map(|op| match op {
                ObservedOp::ReadAt { offset_bytes, len, .. } => Some((*offset_bytes, *len)),
                _ => None,
            })
            .collect();
        assert!(reads.len() > 1);
        let mut next = 0u64;
        for (offset, len) in reads {
            assert_eq!(offset, next);
            let remaining = expected.len() as u64 - next;
            next += remaining.min(u64::from(len));
        }
        assert_eq!(next as usize, expected.len());
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: MANIFEST_FD, .. })));
    }

    #[test]
    fn load_rejects_oversized_manifest_and_closes_fd() {
        let mut driver = TestDriver {
            file_bytes: vec![0; MAX_RECOVERY_MANIFEST_BYTES + 1],
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = load_recovery_manifest(&mut driver, &mut pool, load_config(), &mut completions)
            .unwrap_err();

        assert!(matches!(
            error,
            RecoveryManifestLoadError::ManifestTooLarge {
                max_len_bytes: MAX_RECOVERY_MANIFEST_BYTES
            }
        ));
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: MANIFEST_FD, .. })));
    }

    #[test]
    fn load_corrupt_manifest_returns_decode_error_after_close() {
        let mut bytes = Vec::new();
        encode_recovery_manifest(&manifest(), &mut bytes).unwrap();
        bytes[12] ^= 0x80;
        let mut driver = TestDriver { file_bytes: bytes, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();

        let error = load_recovery_manifest(&mut driver, &mut pool, load_config(), &mut completions)
            .unwrap_err();

        assert!(matches!(
            error,
            RecoveryManifestLoadError::Decode(RecoveryManifestError::BadCrc { .. })
        ));
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: MANIFEST_FD, .. })));
    }
}
