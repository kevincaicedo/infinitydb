use core::fmt;
use std::io;

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode, IoOp,
    RawFd, TokenClass, Wait,
};
use inf_store::{
    MAX_NAMESPACE_CATALOG_BYTES, NsCatalog, NsCatalogError, decode_namespace_catalog,
    encode_namespace_catalog,
};

pub const NAMESPACE_CATALOG_FILE: &str = "META";
pub const NAMESPACE_CATALOG_TEMP_FILE: &str = "META.tmp";
pub const DEFAULT_NAMESPACE_CATALOG_REAP_LIMIT: u32 = 32;
const AT_FDCWD_FD: RawFd = -100;
const ENOENT_ERRNO: i32 = 2;
const MAX_NAMESPACE_CATALOG_READ_BYTES: usize = MAX_NAMESPACE_CATALOG_BYTES + 1;

#[derive(Copy, Clone, Debug)]
pub struct NamespaceCatalogPublishConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl NamespaceCatalogPublishConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> NamespaceCatalogPublishConfig {
        NamespaceCatalogPublishConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_NAMESPACE_CATALOG_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> NamespaceCatalogPublishConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> NamespaceCatalogPublishConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> NamespaceCatalogPublishConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Copy, Clone, Debug)]
pub struct NamespaceCatalogLoadConfig {
    pub dir: RawFd,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl NamespaceCatalogLoadConfig {
    pub const fn new(dir: RawFd, token_slot: u32) -> NamespaceCatalogLoadConfig {
        NamespaceCatalogLoadConfig {
            dir,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_NAMESPACE_CATALOG_REAP_LIMIT,
        }
    }

    pub const fn with_generation(mut self, token_generation: u32) -> NamespaceCatalogLoadConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> NamespaceCatalogLoadConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> NamespaceCatalogLoadConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Clone, Debug)]
pub struct NamespaceCatalogDataRootLoadConfig {
    pub root_parent_dir: RawFd,
    pub root_name: String,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl NamespaceCatalogDataRootLoadConfig {
    pub fn new(root_name: String, token_slot: u32) -> NamespaceCatalogDataRootLoadConfig {
        NamespaceCatalogDataRootLoadConfig {
            root_parent_dir: AT_FDCWD_FD,
            root_name,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_NAMESPACE_CATALOG_REAP_LIMIT,
        }
    }

    pub fn with_root_parent_dir(
        mut self,
        root_parent_dir: RawFd,
    ) -> NamespaceCatalogDataRootLoadConfig {
        self.root_parent_dir = root_parent_dir;
        self
    }

    pub fn with_generation(mut self, token_generation: u32) -> NamespaceCatalogDataRootLoadConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> NamespaceCatalogDataRootLoadConfig {
        self.wait = wait;
        self
    }

    pub fn with_max_reaps(mut self, max_reaps: u32) -> NamespaceCatalogDataRootLoadConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Clone, Debug)]
pub struct NamespaceCatalogDataRootPublishConfig {
    pub root_parent_dir: RawFd,
    pub root_name: String,
    pub token_slot: u32,
    pub token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl NamespaceCatalogDataRootPublishConfig {
    pub fn new(root_name: String, token_slot: u32) -> NamespaceCatalogDataRootPublishConfig {
        NamespaceCatalogDataRootPublishConfig {
            root_parent_dir: AT_FDCWD_FD,
            root_name,
            token_slot,
            token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_NAMESPACE_CATALOG_REAP_LIMIT,
        }
    }

    pub fn with_root_parent_dir(
        mut self,
        root_parent_dir: RawFd,
    ) -> NamespaceCatalogDataRootPublishConfig {
        self.root_parent_dir = root_parent_dir;
        self
    }

    pub fn with_generation(
        mut self,
        token_generation: u32,
    ) -> NamespaceCatalogDataRootPublishConfig {
        self.token_generation = token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> NamespaceCatalogDataRootPublishConfig {
        self.wait = wait;
        self
    }

    pub fn with_max_reaps(mut self, max_reaps: u32) -> NamespaceCatalogDataRootPublishConfig {
        self.max_reaps = max_reaps;
        self
    }
}

#[derive(Clone, Debug)]
pub struct NamespaceCatalogLivePublishConfig {
    pub root_parent_dir: RawFd,
    pub root_name: String,
    pub token_slot: u32,
    pub token_generation: u32,
}

impl NamespaceCatalogLivePublishConfig {
    pub fn new(root_name: String, token_slot: u32) -> NamespaceCatalogLivePublishConfig {
        NamespaceCatalogLivePublishConfig {
            root_parent_dir: AT_FDCWD_FD,
            root_name,
            token_slot,
            token_generation: 0,
        }
    }

    pub fn with_root_parent_dir(
        mut self,
        root_parent_dir: RawFd,
    ) -> NamespaceCatalogLivePublishConfig {
        self.root_parent_dir = root_parent_dir;
        self
    }

    pub fn with_generation(mut self, token_generation: u32) -> NamespaceCatalogLivePublishConfig {
        self.token_generation = token_generation;
        self
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NamespaceCatalogLivePublishEvent {
    Idle,
    Pending,
    Completed,
}

pub struct NamespaceCatalogLivePublisher {
    root_parent_dir: RawFd,
    root_name: String,
    token: CompletionToken,
    state: LivePublishState,
}

impl NamespaceCatalogLivePublisher {
    pub fn new(
        config: NamespaceCatalogLivePublishConfig,
    ) -> Result<NamespaceCatalogLivePublisher, NamespaceCatalogPublishError> {
        validate_data_root_publish_name(&config.root_name)?;
        Ok(NamespaceCatalogLivePublisher {
            root_parent_dir: config.root_parent_dir,
            root_name: config.root_name,
            token: CompletionToken::new(
                TokenClass::File,
                config.token_slot,
                config.token_generation,
            ),
            state: LivePublishState::Idle,
        })
    }

    pub const fn token(&self) -> CompletionToken {
        self.token
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, LivePublishState::Idle)
    }

    pub fn start(&mut self, snapshot: NsCatalog) -> Result<(), NamespaceCatalogPublishError> {
        assert!(self.is_idle(), "namespace catalog live publisher already active");
        let mut catalog = Vec::new();
        encode_namespace_catalog(&snapshot, &mut catalog)
            .map_err(NamespaceCatalogPublishError::Encode)?;
        assert!(catalog.len() <= MAX_NAMESPACE_CATALOG_BYTES);
        self.state = LivePublishState::OpenDataRoot { catalog };
        Ok(())
    }

    pub fn drive(
        &mut self,
        pool: &mut BufferPool,
        ops: &mut Vec<IoOp>,
    ) -> Result<NamespaceCatalogLivePublishEvent, NamespaceCatalogPublishError> {
        let state = core::mem::replace(&mut self.state, LivePublishState::Idle);
        match state {
            LivePublishState::Idle => {
                self.state = LivePublishState::Idle;
                Ok(NamespaceCatalogLivePublishEvent::Idle)
            }
            LivePublishState::OpenDataRoot { catalog } => {
                ops.push(IoOp::FileOpen {
                    dir: self.root_parent_dir,
                    name: self.root_name.clone(),
                    mode: FileOpenMode::Directory,
                    token: self.token,
                });
                self.state = LivePublishState::OpeningDataRoot { catalog };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::OpenTemp { root, catalog } => {
                ops.push(IoOp::FileOpen {
                    dir: root,
                    name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: self.token,
                });
                self.state = LivePublishState::OpeningTemp { root, catalog };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::WriteTemp { root, temp, catalog, offset } => {
                if offset >= catalog.len() {
                    self.state = LivePublishState::SyncTemp { root, temp };
                    return self.drive(pool, ops);
                }
                let chunk_len = pool.buf_size().min(catalog.len() - offset);
                let Some(buf) = pool.try_lease(LeaseKind::Send) else {
                    self.state = LivePublishState::WriteTemp { root, temp, catalog, offset };
                    return Ok(NamespaceCatalogLivePublishEvent::Pending);
                };
                pool.bytes_mut(buf)[..chunk_len]
                    .copy_from_slice(&catalog[offset..offset + chunk_len]);
                let offset_bytes = offset as u64;
                ops.push(IoOp::FileWriteAt {
                    fd: temp,
                    offset_bytes,
                    buf,
                    len: chunk_len as u32,
                    token: self.token,
                });
                self.state =
                    LivePublishState::WritingTemp { root, temp, catalog, offset, chunk_len, buf };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::SyncTemp { root, temp } => {
                ops.push(IoOp::FileSync {
                    fd: temp,
                    mode: FileSyncMode::DataOnly,
                    token: self.token,
                });
                self.state = LivePublishState::SyncingTemp { root, temp };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::CloseTemp { root, temp } => {
                ops.push(IoOp::FileClose { fd: temp, token: self.token });
                self.state = LivePublishState::ClosingTemp { root, temp };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::Rename { root } => {
                ops.push(IoOp::FileRename {
                    old_dir: root,
                    old_name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    new_dir: root,
                    new_name: NAMESPACE_CATALOG_FILE.to_string(),
                    token: self.token,
                });
                self.state = LivePublishState::Renaming { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::SyncDir { root } => {
                ops.push(IoOp::FileSync { fd: root, mode: FileSyncMode::Full, token: self.token });
                self.state = LivePublishState::SyncingDir { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            LivePublishState::CloseRoot { root } => {
                ops.push(IoOp::FileClose { fd: root, token: self.token });
                self.state = LivePublishState::ClosingRoot { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            inflight => {
                self.state = inflight;
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
        }
    }

    pub fn on_completion(
        &mut self,
        pool: &mut BufferPool,
        completion: Completion,
    ) -> Result<NamespaceCatalogLivePublishEvent, NamespaceCatalogPublishError> {
        let phase = self.state.phase().expect("namespace catalog completion with no active phase");
        if completion.token != self.token {
            release_result_buffer(pool, &completion.result);
            return Err(NamespaceCatalogPublishError::UnexpectedToken {
                phase,
                expected: self.token,
                got: completion.token,
            });
        }

        let state = core::mem::replace(&mut self.state, LivePublishState::Idle);
        match (state, completion.result) {
            (
                LivePublishState::OpeningDataRoot { catalog },
                CompletionResult::FileOpened { fd },
            ) => {
                self.state = LivePublishState::OpenTemp { root: fd, catalog };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::OpeningDataRoot { .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::OpenDataRoot {
                name: self.root_name.clone(),
                errno,
            }),
            (
                LivePublishState::OpeningTemp { root, catalog },
                CompletionResult::FileOpened { fd },
            ) => {
                self.state = LivePublishState::WriteTemp { root, temp: fd, catalog, offset: 0 };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::OpeningTemp { .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::OpenTemp {
                name: NAMESPACE_CATALOG_TEMP_FILE,
                errno,
            }),
            (
                LivePublishState::WritingTemp { root, temp, catalog, offset, chunk_len, buf },
                CompletionResult::FileWritten { buf: got },
            ) => {
                assert_eq!(got, buf, "catalog write completion returned the wrong buffer");
                pool.release(got);
                self.state =
                    LivePublishState::WriteTemp { root, temp, catalog, offset: offset + chunk_len };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::WritingTemp { temp, offset, buf, .. },
                CompletionResult::Error { errno, buf: Some(got) },
            ) => {
                assert_eq!(got, buf, "catalog write error returned the wrong buffer");
                pool.release(got);
                Err(NamespaceCatalogPublishError::WriteTemp {
                    fd: temp,
                    offset_bytes: offset as u64,
                    errno,
                })
            }
            (
                LivePublishState::WritingTemp { temp, offset, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::MissingWriteBuffer {
                fd: temp,
                offset_bytes: offset as u64,
                errno,
            }),
            (LivePublishState::SyncingTemp { root, temp }, CompletionResult::FileDone) => {
                self.state = LivePublishState::CloseTemp { root, temp };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::SyncingTemp { temp, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::SyncTemp { fd: temp, errno }),
            (LivePublishState::ClosingTemp { root, .. }, CompletionResult::FileClosed) => {
                self.state = LivePublishState::Rename { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::ClosingTemp { temp, .. },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::CloseTemp { fd: temp, errno }),
            (LivePublishState::Renaming { root }, CompletionResult::FileDone) => {
                self.state = LivePublishState::SyncDir { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (LivePublishState::Renaming { .. }, CompletionResult::Error { errno, buf: None }) => {
                Err(NamespaceCatalogPublishError::Rename {
                    old_name: NAMESPACE_CATALOG_TEMP_FILE,
                    new_name: NAMESPACE_CATALOG_FILE,
                    errno,
                })
            }
            (LivePublishState::SyncingDir { root }, CompletionResult::FileDone) => {
                self.state = LivePublishState::CloseRoot { root };
                Ok(NamespaceCatalogLivePublishEvent::Pending)
            }
            (
                LivePublishState::SyncingDir { root },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::SyncDir { fd: root, errno }),
            (LivePublishState::ClosingRoot { .. }, CompletionResult::FileClosed) => {
                self.state = LivePublishState::Idle;
                Ok(NamespaceCatalogLivePublishEvent::Completed)
            }
            (
                LivePublishState::ClosingRoot { root },
                CompletionResult::Error { errno, buf: None },
            ) => Err(NamespaceCatalogPublishError::CloseDataRoot { fd: root, errno }),
            (state, result) => {
                self.state = state;
                Err(unexpected_publish_completion(pool, phase, result))
            }
        }
    }
}

enum LivePublishState {
    Idle,
    OpenDataRoot {
        catalog: Vec<u8>,
    },
    OpeningDataRoot {
        catalog: Vec<u8>,
    },
    OpenTemp {
        root: RawFd,
        catalog: Vec<u8>,
    },
    OpeningTemp {
        root: RawFd,
        catalog: Vec<u8>,
    },
    WriteTemp {
        root: RawFd,
        temp: RawFd,
        catalog: Vec<u8>,
        offset: usize,
    },
    WritingTemp {
        root: RawFd,
        temp: RawFd,
        catalog: Vec<u8>,
        offset: usize,
        chunk_len: usize,
        buf: BufferId,
    },
    SyncTemp {
        root: RawFd,
        temp: RawFd,
    },
    SyncingTemp {
        root: RawFd,
        temp: RawFd,
    },
    CloseTemp {
        root: RawFd,
        temp: RawFd,
    },
    ClosingTemp {
        root: RawFd,
        temp: RawFd,
    },
    Rename {
        root: RawFd,
    },
    Renaming {
        root: RawFd,
    },
    SyncDir {
        root: RawFd,
    },
    SyncingDir {
        root: RawFd,
    },
    CloseRoot {
        root: RawFd,
    },
    ClosingRoot {
        root: RawFd,
    },
}

impl LivePublishState {
    fn phase(&self) -> Option<NamespaceCatalogPublishPhase> {
        match self {
            LivePublishState::Idle => None,
            LivePublishState::OpenDataRoot { .. } | LivePublishState::OpeningDataRoot { .. } => {
                Some(NamespaceCatalogPublishPhase::OpenDataRoot)
            }
            LivePublishState::OpenTemp { .. } | LivePublishState::OpeningTemp { .. } => {
                Some(NamespaceCatalogPublishPhase::OpenTemp)
            }
            LivePublishState::WriteTemp { .. } | LivePublishState::WritingTemp { .. } => {
                Some(NamespaceCatalogPublishPhase::WriteTemp)
            }
            LivePublishState::SyncTemp { .. } | LivePublishState::SyncingTemp { .. } => {
                Some(NamespaceCatalogPublishPhase::SyncTemp)
            }
            LivePublishState::CloseTemp { .. } | LivePublishState::ClosingTemp { .. } => {
                Some(NamespaceCatalogPublishPhase::CloseTemp)
            }
            LivePublishState::Rename { .. } | LivePublishState::Renaming { .. } => {
                Some(NamespaceCatalogPublishPhase::Rename)
            }
            LivePublishState::SyncDir { .. } | LivePublishState::SyncingDir { .. } => {
                Some(NamespaceCatalogPublishPhase::SyncDir)
            }
            LivePublishState::CloseRoot { .. } | LivePublishState::ClosingRoot { .. } => {
                Some(NamespaceCatalogPublishPhase::CloseDataRoot)
            }
        }
    }
}

/// Publish the node namespace catalog as one restart-visible META image.
///
/// This is a cold/control-plane primitive for M2-S08. It does not activate
/// durable namespaces by itself; it only gives namespace DDL a crash protocol:
/// write an exact-length `META.tmp`, sync the file, close it, rename to
/// `META`, then fsync the parent directory.
pub fn publish_namespace_catalog<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    snapshot: &NsCatalog,
    config: NamespaceCatalogPublishConfig,
    completions: &mut Vec<Completion>,
) -> Result<(), NamespaceCatalogPublishError>
where
    D: BackendDriver,
{
    validate_publish_inputs(completions, config.max_reaps)?;

    let mut catalog = Vec::new();
    encode_namespace_catalog(snapshot, &mut catalog)
        .map_err(NamespaceCatalogPublishError::Encode)?;
    assert!(catalog.len() <= MAX_NAMESPACE_CATALOG_BYTES);

    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = PublishIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        dir: config.dir,
    };

    io.publish_catalog_bytes(&catalog)
}

/// Load and validate the node namespace catalog from `META`.
///
/// Missing `META` is first boot and returns an empty catalog. Present but
/// malformed bytes are fatal to recovery and surface as a typed decode error.
pub fn load_namespace_catalog<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: NamespaceCatalogLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<NsCatalog, NamespaceCatalogLoadError>
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

/// Publish `META` into a named node data root.
///
/// This is the production/control-plane counterpart to
/// [`load_namespace_catalog_in_data_root`]. The root directory is opened
/// through `BackendDriver`, the same exact-length `META.tmp` publish protocol
/// runs against that fd, and the root fd is closed before returning.
pub fn publish_namespace_catalog_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    snapshot: &NsCatalog,
    config: &NamespaceCatalogDataRootPublishConfig,
    completions: &mut Vec<Completion>,
) -> Result<(), NamespaceCatalogPublishError>
where
    D: BackendDriver,
{
    validate_data_root_publish_name(&config.root_name)?;
    validate_publish_inputs(completions, config.max_reaps)?;

    let mut catalog = Vec::new();
    encode_namespace_catalog(snapshot, &mut catalog)
        .map_err(NamespaceCatalogPublishError::Encode)?;
    assert!(catalog.len() <= MAX_NAMESPACE_CATALOG_BYTES);

    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = PublishIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
        dir: config.root_parent_dir,
    };

    let root = io.open_data_root(config.root_parent_dir, &config.root_name)?;
    io.dir = root;
    let published = io.publish_catalog_bytes(&catalog);
    let close = io.close_data_root(root);
    match (published, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

/// Load `META` from a named node data root.
///
/// This is the production boot helper for S08. The root directory is opened
/// through `BackendDriver`, then the same bounded `META` loader runs against
/// that fd. Missing `META` remains first-boot empty state.
pub fn load_namespace_catalog_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &NamespaceCatalogDataRootLoadConfig,
    completions: &mut Vec<Completion>,
) -> Result<NsCatalog, NamespaceCatalogLoadError>
where
    D: BackendDriver,
{
    validate_data_root_load_name(&config.root_name)?;
    validate_load_inputs(completions, config.max_reaps)?;

    let token = CompletionToken::new(TokenClass::File, config.token_slot, config.token_generation);
    let mut io = LoadIo {
        driver,
        pool,
        completions,
        params: ReapParams { wait: config.wait, max_reaps: config.max_reaps },
        token,
    };

    let root = io.open_data_root(config.root_parent_dir, &config.root_name)?;
    let loaded = io.load_from_dir(root);
    let close = io.close_data_root(root);
    match (loaded, close) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn validate_publish_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), NamespaceCatalogPublishError> {
    if !completions.is_empty() {
        return Err(NamespaceCatalogPublishError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(NamespaceCatalogPublishError::ZeroReapLimit);
    }
    Ok(())
}

fn validate_data_root_publish_name(name: &str) -> Result<(), NamespaceCatalogPublishError> {
    if !data_root_name_is_valid(name) {
        return Err(NamespaceCatalogPublishError::InvalidDataRootName { name: name.to_string() });
    }
    Ok(())
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

fn unexpected_publish_completion(
    pool: &mut BufferPool,
    phase: NamespaceCatalogPublishPhase,
    result: CompletionResult,
) -> NamespaceCatalogPublishError {
    let name = result_name(&result);
    release_result_buffer(pool, &result);
    NamespaceCatalogPublishError::UnexpectedCompletionKind { phase, result: name }
}

fn validate_load_inputs(
    completions: &[Completion],
    max_reaps: u32,
) -> Result<(), NamespaceCatalogLoadError> {
    if !completions.is_empty() {
        return Err(NamespaceCatalogLoadError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(NamespaceCatalogLoadError::ZeroReapLimit);
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

impl<D> PublishIo<'_, D>
where
    D: BackendDriver,
{
    fn publish_catalog_bytes(
        &mut self,
        catalog: &[u8],
    ) -> Result<(), NamespaceCatalogPublishError> {
        let fd = self.open_temp()?;
        self.write_temp(fd, catalog)?;
        self.sync_temp(fd)?;
        self.close_temp(fd)?;
        self.rename_temp()?;
        self.sync_dir()?;
        Ok(())
    }

    fn open_data_root(
        &mut self,
        parent_dir: RawFd,
        name: &str,
    ) -> Result<RawFd, NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileOpen {
            dir: parent_dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: self.token,
        });
        match self.reap(NamespaceCatalogPublishPhase::OpenDataRoot)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::OpenDataRoot { name: name.to_string(), errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::OpenDataRoot,
                result: result_name(&other),
            }),
        }
    }

    fn close_data_root(&mut self, fd: RawFd) -> Result<(), NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(NamespaceCatalogPublishPhase::CloseDataRoot)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::CloseDataRoot { fd, errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::CloseDataRoot,
                result: result_name(&other),
            }),
        }
    }

    fn open_temp(&mut self) -> Result<RawFd, NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileOpen {
            dir: self.dir,
            name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
            mode: FileOpenMode::ReadWriteCreateTruncate,
            token: self.token,
        });
        match self.reap(NamespaceCatalogPublishPhase::OpenTemp)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::OpenTemp {
                    name: NAMESPACE_CATALOG_TEMP_FILE,
                    errno,
                })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::OpenTemp,
                result: result_name(&other),
            }),
        }
    }

    fn write_temp(
        &mut self,
        fd: RawFd,
        catalog: &[u8],
    ) -> Result<(), NamespaceCatalogPublishError> {
        let mut offset = 0usize;
        while offset < catalog.len() {
            let chunk_len = self.pool.buf_size().min(catalog.len() - offset);
            self.write_temp_chunk(fd, offset, &catalog[offset..offset + chunk_len])?;
            offset += chunk_len;
        }
        Ok(())
    }

    fn write_temp_chunk(
        &mut self,
        fd: RawFd,
        offset: usize,
        chunk: &[u8],
    ) -> Result<(), NamespaceCatalogPublishError> {
        debug_assert!(!chunk.is_empty());
        let Some(buf) = self.pool.try_lease(LeaseKind::Send) else {
            return Err(NamespaceCatalogPublishError::WriteBufferUnavailable);
        };
        self.pool.bytes_mut(buf)[..chunk.len()].copy_from_slice(chunk);
        let offset_bytes = offset as u64;
        let len = chunk.len() as u32;
        self.driver.push(IoOp::FileWriteAt { fd, offset_bytes, buf, len, token: self.token });
        match self.reap(NamespaceCatalogPublishPhase::WriteTemp)?.result {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf, "catalog write completion returned the wrong buffer");
                self.pool.release(got);
                Ok(())
            }
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "catalog write error returned the wrong buffer");
                self.pool.release(got);
                Err(NamespaceCatalogPublishError::WriteTemp { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::MissingWriteBuffer { fd, offset_bytes, errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::WriteTemp,
                result: result_name(&other),
            }),
        }
    }

    fn sync_temp(&mut self, fd: RawFd) -> Result<(), NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: self.token });
        match self.reap(NamespaceCatalogPublishPhase::SyncTemp)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::SyncTemp { fd, errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::SyncTemp,
                result: result_name(&other),
            }),
        }
    }

    fn close_temp(&mut self, fd: RawFd) -> Result<(), NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(NamespaceCatalogPublishPhase::CloseTemp)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::CloseTemp { fd, errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::CloseTemp,
                result: result_name(&other),
            }),
        }
    }

    fn rename_temp(&mut self) -> Result<(), NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileRename {
            old_dir: self.dir,
            old_name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
            new_dir: self.dir,
            new_name: NAMESPACE_CATALOG_FILE.to_string(),
            token: self.token,
        });
        match self.reap(NamespaceCatalogPublishPhase::Rename)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::Rename {
                    old_name: NAMESPACE_CATALOG_TEMP_FILE,
                    new_name: NAMESPACE_CATALOG_FILE,
                    errno,
                })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::Rename,
                result: result_name(&other),
            }),
        }
    }

    fn sync_dir(&mut self) -> Result<(), NamespaceCatalogPublishError> {
        self.driver.push(IoOp::FileSync {
            fd: self.dir,
            mode: FileSyncMode::Full,
            token: self.token,
        });
        match self.reap(NamespaceCatalogPublishPhase::SyncDir)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogPublishError::SyncDir { fd: self.dir, errno })
            }
            other => Err(NamespaceCatalogPublishError::UnexpectedCompletionKind {
                phase: NamespaceCatalogPublishPhase::SyncDir,
                result: result_name(&other),
            }),
        }
    }

    fn reap(
        &mut self,
        phase: NamespaceCatalogPublishPhase,
    ) -> Result<Completion, NamespaceCatalogPublishError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| NamespaceCatalogPublishError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(NamespaceCatalogPublishError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                return Err(NamespaceCatalogPublishError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(NamespaceCatalogPublishError::ReapLimitExceeded {
            phase,
            token: self.token,
            attempts: self.params.max_reaps,
        })
    }
}

struct LoadIo<'a, D> {
    driver: &'a mut D,
    pool: &'a mut BufferPool,
    completions: &'a mut Vec<Completion>,
    params: ReapParams,
    token: CompletionToken,
}

impl<D> LoadIo<'_, D>
where
    D: BackendDriver,
{
    fn load_from_dir(&mut self, dir: RawFd) -> Result<NsCatalog, NamespaceCatalogLoadError> {
        let Some(fd) = self.open_meta(dir)? else {
            return Ok(NsCatalog::empty());
        };
        let read = self.read_meta(fd);
        let close = self.close_meta(fd);
        match (read, close) {
            (Ok(image), Ok(())) => {
                decode_namespace_catalog(&image).map_err(NamespaceCatalogLoadError::Decode)
            }
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn open_data_root(
        &mut self,
        parent_dir: RawFd,
        name: &str,
    ) -> Result<RawFd, NamespaceCatalogLoadError> {
        self.driver.push(IoOp::FileOpen {
            dir: parent_dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: self.token,
        });
        match self.reap(NamespaceCatalogLoadPhase::OpenDataRoot)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogLoadError::OpenDataRoot { name: name.to_string(), errno })
            }
            other => Err(NamespaceCatalogLoadError::UnexpectedCompletionKind {
                phase: NamespaceCatalogLoadPhase::OpenDataRoot,
                result: result_name(&other),
            }),
        }
    }

    fn close_data_root(&mut self, fd: RawFd) -> Result<(), NamespaceCatalogLoadError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(NamespaceCatalogLoadPhase::CloseDataRoot)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogLoadError::CloseDataRoot { fd, errno })
            }
            other => Err(NamespaceCatalogLoadError::UnexpectedCompletionKind {
                phase: NamespaceCatalogLoadPhase::CloseDataRoot,
                result: result_name(&other),
            }),
        }
    }

    fn open_meta(&mut self, dir: RawFd) -> Result<Option<RawFd>, NamespaceCatalogLoadError> {
        self.driver.push(IoOp::FileOpen {
            dir,
            name: NAMESPACE_CATALOG_FILE.to_string(),
            mode: FileOpenMode::ReadOnly,
            token: self.token,
        });
        match self.reap(NamespaceCatalogLoadPhase::Open)?.result {
            CompletionResult::FileOpened { fd } => Ok(Some(fd)),
            CompletionResult::Error { errno: ENOENT_ERRNO, buf: None } => Ok(None),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogLoadError::Open { name: NAMESPACE_CATALOG_FILE, errno })
            }
            other => Err(NamespaceCatalogLoadError::UnexpectedCompletionKind {
                phase: NamespaceCatalogLoadPhase::Open,
                result: result_name(&other),
            }),
        }
    }

    fn read_meta(&mut self, fd: RawFd) -> Result<Vec<u8>, NamespaceCatalogLoadError> {
        let mut image = Vec::new();
        let mut offset_bytes = 0u64;
        loop {
            let remaining = MAX_NAMESPACE_CATALOG_READ_BYTES - image.len();
            let chunk_len = self.pool.buf_size().min(remaining);
            let read = self.read_meta_chunk(fd, offset_bytes, chunk_len as u32)?;
            if read.is_empty() {
                return Ok(image);
            }
            image.extend_from_slice(&read);
            if image.len() > MAX_NAMESPACE_CATALOG_BYTES {
                return Err(NamespaceCatalogLoadError::CatalogTooLarge {
                    max_len_bytes: MAX_NAMESPACE_CATALOG_BYTES,
                });
            }
            offset_bytes += read.len() as u64;
        }
    }

    fn read_meta_chunk(
        &mut self,
        fd: RawFd,
        offset_bytes: u64,
        len: u32,
    ) -> Result<Vec<u8>, NamespaceCatalogLoadError> {
        debug_assert!(len > 0);
        let Some(buf) = self.pool.try_lease(LeaseKind::Recv) else {
            return Err(NamespaceCatalogLoadError::ReadBufferUnavailable);
        };
        self.driver.push(IoOp::FileReadAt { fd, offset_bytes, buf, len, token: self.token });
        match self.reap(NamespaceCatalogLoadPhase::Read)?.result {
            CompletionResult::FileRead { buf: got, len } => {
                assert_eq!(got, buf, "catalog read completion returned the wrong buffer");
                if len as usize > self.pool.buf_size() {
                    self.pool.release(got);
                    return Err(NamespaceCatalogLoadError::ReadLenTooLarge {
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
            CompletionResult::Error { errno, buf: Some(got) } => {
                assert_eq!(got, buf, "catalog read error returned the wrong buffer");
                self.pool.release(got);
                Err(NamespaceCatalogLoadError::Read { fd, offset_bytes, errno })
            }
            CompletionResult::Error { errno, buf: None } => {
                self.pool.release(buf);
                Err(NamespaceCatalogLoadError::MissingReadBuffer { fd, offset_bytes, errno })
            }
            other => {
                self.pool.release(buf);
                Err(NamespaceCatalogLoadError::UnexpectedCompletionKind {
                    phase: NamespaceCatalogLoadPhase::Read,
                    result: result_name(&other),
                })
            }
        }
    }

    fn close_meta(&mut self, fd: RawFd) -> Result<(), NamespaceCatalogLoadError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(NamespaceCatalogLoadPhase::Close)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(NamespaceCatalogLoadError::Close { fd, errno })
            }
            other => Err(NamespaceCatalogLoadError::UnexpectedCompletionKind {
                phase: NamespaceCatalogLoadPhase::Close,
                result: result_name(&other),
            }),
        }
    }

    fn reap(
        &mut self,
        phase: NamespaceCatalogLoadPhase,
    ) -> Result<Completion, NamespaceCatalogLoadError> {
        for _ in 0..self.params.max_reaps {
            let before = self.completions.len();
            self.driver
                .submit_and_reap(self.pool, self.params.wait, self.completions)
                .map_err(|source| NamespaceCatalogLoadError::Backend { phase, source })?;
            let produced = self.completions.len() - before;
            if produced == 0 {
                continue;
            }
            if produced != 1 {
                return Err(NamespaceCatalogLoadError::UnexpectedCompletionCount {
                    phase,
                    expected: 1,
                    got: produced,
                });
            }
            let completion = self.completions.pop().expect("one produced completion");
            if completion.token != self.token {
                return Err(NamespaceCatalogLoadError::UnexpectedToken {
                    phase,
                    expected: self.token,
                    got: completion.token,
                });
            }
            return Ok(completion);
        }
        Err(NamespaceCatalogLoadError::ReapLimitExceeded {
            phase,
            token: self.token,
            attempts: self.params.max_reaps,
        })
    }
}

fn validate_data_root_load_name(name: &str) -> Result<(), NamespaceCatalogLoadError> {
    if !data_root_name_is_valid(name) {
        return Err(NamespaceCatalogLoadError::InvalidDataRootName { name: name.to_string() });
    }
    Ok(())
}

fn data_root_name_is_valid(name: &str) -> bool {
    !(name.is_empty()
        || matches!(name, "." | "..")
        || name.as_bytes().contains(&0)
        || name.as_bytes().contains(&b'/'))
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NamespaceCatalogPublishPhase {
    OpenDataRoot,
    OpenTemp,
    WriteTemp,
    SyncTemp,
    CloseTemp,
    Rename,
    SyncDir,
    CloseDataRoot,
}

impl fmt::Display for NamespaceCatalogPublishPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceCatalogPublishPhase::OpenDataRoot => {
                write!(f, "open namespace catalog data root")
            }
            NamespaceCatalogPublishPhase::OpenTemp => write!(f, "open namespace catalog temp"),
            NamespaceCatalogPublishPhase::WriteTemp => write!(f, "write namespace catalog temp"),
            NamespaceCatalogPublishPhase::SyncTemp => write!(f, "sync namespace catalog temp"),
            NamespaceCatalogPublishPhase::CloseTemp => write!(f, "close namespace catalog temp"),
            NamespaceCatalogPublishPhase::Rename => write!(f, "rename namespace catalog"),
            NamespaceCatalogPublishPhase::SyncDir => write!(f, "sync namespace catalog directory"),
            NamespaceCatalogPublishPhase::CloseDataRoot => {
                write!(f, "close namespace catalog data root")
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NamespaceCatalogLoadPhase {
    OpenDataRoot,
    Open,
    Read,
    Close,
    CloseDataRoot,
}

impl fmt::Display for NamespaceCatalogLoadPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceCatalogLoadPhase::OpenDataRoot => {
                write!(f, "open namespace catalog data root")
            }
            NamespaceCatalogLoadPhase::Open => write!(f, "open namespace catalog"),
            NamespaceCatalogLoadPhase::Read => write!(f, "read namespace catalog"),
            NamespaceCatalogLoadPhase::Close => write!(f, "close namespace catalog"),
            NamespaceCatalogLoadPhase::CloseDataRoot => {
                write!(f, "close namespace catalog data root")
            }
        }
    }
}

#[derive(Debug)]
pub enum NamespaceCatalogPublishError {
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    InvalidDataRootName {
        name: String,
    },
    Encode(NsCatalogError),
    CatalogTooLarge {
        catalog_len_bytes: usize,
        buffer_len_bytes: usize,
    },
    WriteBufferUnavailable,
    ReapLimitExceeded {
        phase: NamespaceCatalogPublishPhase,
        token: CompletionToken,
        attempts: u32,
    },
    Backend {
        phase: NamespaceCatalogPublishPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: NamespaceCatalogPublishPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: NamespaceCatalogPublishPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    OpenTemp {
        name: &'static str,
        errno: i32,
    },
    OpenDataRoot {
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
        old_name: &'static str,
        new_name: &'static str,
        errno: i32,
    },
    SyncDir {
        fd: RawFd,
        errno: i32,
    },
    CloseDataRoot {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: NamespaceCatalogPublishPhase,
        result: &'static str,
    },
}

#[derive(Debug)]
pub enum NamespaceCatalogLoadError {
    ScratchNotEmpty {
        len: usize,
    },
    ZeroReapLimit,
    InvalidDataRootName {
        name: String,
    },
    Decode(NsCatalogError),
    CatalogTooLarge {
        max_len_bytes: usize,
    },
    ReadBufferUnavailable,
    ReapLimitExceeded {
        phase: NamespaceCatalogLoadPhase,
        token: CompletionToken,
        attempts: u32,
    },
    Backend {
        phase: NamespaceCatalogLoadPhase,
        source: io::Error,
    },
    UnexpectedCompletionCount {
        phase: NamespaceCatalogLoadPhase,
        expected: usize,
        got: usize,
    },
    UnexpectedToken {
        phase: NamespaceCatalogLoadPhase,
        expected: CompletionToken,
        got: CompletionToken,
    },
    Open {
        name: &'static str,
        errno: i32,
    },
    OpenDataRoot {
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
        buffer_len_bytes: usize,
    },
    Close {
        fd: RawFd,
        errno: i32,
    },
    CloseDataRoot {
        fd: RawFd,
        errno: i32,
    },
    UnexpectedCompletionKind {
        phase: NamespaceCatalogLoadPhase,
        result: &'static str,
    },
}

impl fmt::Display for NamespaceCatalogPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceCatalogPublishError::ScratchNotEmpty { len } => {
                write!(f, "namespace catalog completion scratch is not empty ({len} completions)")
            }
            NamespaceCatalogPublishError::ZeroReapLimit => {
                write!(f, "namespace catalog max_reaps must be nonzero")
            }
            NamespaceCatalogPublishError::InvalidDataRootName { name } => {
                write!(f, "invalid namespace catalog data root name {name:?}")
            }
            NamespaceCatalogPublishError::Encode(error) => {
                write!(f, "namespace catalog encode failed: {error:?}")
            }
            NamespaceCatalogPublishError::CatalogTooLarge {
                catalog_len_bytes,
                buffer_len_bytes,
            } => write!(
                f,
                "namespace catalog image is {catalog_len_bytes} bytes, larger than buffer size {buffer_len_bytes}"
            ),
            NamespaceCatalogPublishError::WriteBufferUnavailable => {
                write!(f, "namespace catalog write buffer unavailable")
            }
            NamespaceCatalogPublishError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "namespace catalog publish {phase} did not complete for token {token:?} after {attempts} reaps"
            ),
            NamespaceCatalogPublishError::Backend { phase, source } => {
                write!(f, "namespace catalog publish {phase} backend failure: {source}")
            }
            NamespaceCatalogPublishError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "namespace catalog publish {phase} expected {expected} completion, got {got}"
                )
            }
            NamespaceCatalogPublishError::UnexpectedToken { phase, expected, got } => write!(
                f,
                "namespace catalog publish {phase} got token {got:?}, expected {expected:?}"
            ),
            NamespaceCatalogPublishError::OpenTemp { name, errno } => {
                write!(f, "open namespace catalog temp {name:?} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::OpenDataRoot { name, errno } => {
                write!(f, "open namespace catalog data root {name:?} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::WriteTemp { fd, offset_bytes, errno } => {
                write!(
                    f,
                    "write namespace catalog temp fd {fd} at offset {offset_bytes} failed with errno {errno}"
                )
            }
            NamespaceCatalogPublishError::MissingWriteBuffer { fd, offset_bytes, errno } => write!(
                f,
                "write namespace catalog temp fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            NamespaceCatalogPublishError::SyncTemp { fd, errno } => {
                write!(f, "sync namespace catalog temp fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::CloseTemp { fd, errno } => {
                write!(f, "close namespace catalog temp fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::Rename { old_name, new_name, errno } => write!(
                f,
                "rename namespace catalog {old_name:?} to {new_name:?} failed with errno {errno}"
            ),
            NamespaceCatalogPublishError::SyncDir { fd, errno } => {
                write!(f, "sync namespace catalog directory fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::CloseDataRoot { fd, errno } => {
                write!(f, "close namespace catalog data root fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogPublishError::UnexpectedCompletionKind { phase, result } => write!(
                f,
                "namespace catalog publish {phase} got unexpected completion kind {result}"
            ),
        }
    }
}

impl fmt::Display for NamespaceCatalogLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceCatalogLoadError::ScratchNotEmpty { len } => {
                write!(f, "namespace catalog load scratch is not empty ({len} completions)")
            }
            NamespaceCatalogLoadError::ZeroReapLimit => {
                write!(f, "namespace catalog load max_reaps must be nonzero")
            }
            NamespaceCatalogLoadError::InvalidDataRootName { name } => {
                write!(f, "invalid namespace catalog data root name {name:?}")
            }
            NamespaceCatalogLoadError::Decode(error) => {
                write!(f, "namespace catalog decode failed: {error:?}")
            }
            NamespaceCatalogLoadError::CatalogTooLarge { max_len_bytes } => {
                write!(f, "namespace catalog exceeds maximum length {max_len_bytes} bytes")
            }
            NamespaceCatalogLoadError::ReadBufferUnavailable => {
                write!(f, "namespace catalog read buffer unavailable")
            }
            NamespaceCatalogLoadError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "namespace catalog load {phase} did not complete for token {token:?} after {attempts} reaps"
            ),
            NamespaceCatalogLoadError::Backend { phase, source } => {
                write!(f, "namespace catalog load {phase} backend failure: {source}")
            }
            NamespaceCatalogLoadError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(
                    f,
                    "namespace catalog load {phase} expected {expected} completion, got {got}"
                )
            }
            NamespaceCatalogLoadError::UnexpectedToken { phase, expected, got } => {
                write!(f, "namespace catalog load {phase} got token {got:?}, expected {expected:?}")
            }
            NamespaceCatalogLoadError::Open { name, errno } => {
                write!(f, "open namespace catalog {name:?} failed with errno {errno}")
            }
            NamespaceCatalogLoadError::OpenDataRoot { name, errno } => {
                write!(f, "open namespace catalog data root {name:?} failed with errno {errno}")
            }
            NamespaceCatalogLoadError::Read { fd, offset_bytes, errno } => write!(
                f,
                "read namespace catalog fd {fd} at offset {offset_bytes} failed with errno {errno}"
            ),
            NamespaceCatalogLoadError::MissingReadBuffer { fd, offset_bytes, errno } => write!(
                f,
                "read namespace catalog fd {fd} at offset {offset_bytes} failed with errno {errno} without returning the buffer"
            ),
            NamespaceCatalogLoadError::ReadLenTooLarge {
                fd,
                offset_bytes,
                len,
                buffer_len_bytes,
            } => write!(
                f,
                "read namespace catalog fd {fd} at offset {offset_bytes} returned {len} bytes, larger than buffer size {buffer_len_bytes}"
            ),
            NamespaceCatalogLoadError::Close { fd, errno } => {
                write!(f, "close namespace catalog fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogLoadError::CloseDataRoot { fd, errno } => {
                write!(f, "close namespace catalog data root fd {fd} failed with errno {errno}")
            }
            NamespaceCatalogLoadError::UnexpectedCompletionKind { phase, result } => {
                write!(f, "namespace catalog load {phase} got unexpected completion kind {result}")
            }
        }
    }
}

impl std::error::Error for NamespaceCatalogPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NamespaceCatalogPublishError::Backend { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl std::error::Error for NamespaceCatalogLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NamespaceCatalogLoadError::Backend { source, .. } => Some(source),
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
    use std::collections::VecDeque;

    use inf_runtime::{Capabilities, SubmitStats};
    use inf_store::{NsFsyncPolicy, NsMode, NsSpec};

    const DIR_FD: RawFd = 40;
    const ROOT_FD: RawFd = 41;
    const TEMP_FD: RawFd = 77;
    const TOKEN_SLOT: u32 = 12;
    const TOKEN_GEN: u32 = 3;
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

    #[derive(Debug)]
    struct TestDriver {
        ops: Vec<IoOp>,
        observed: Vec<ObservedOp>,
        written: Vec<u8>,
        file_bytes: Vec<u8>,
        open_results: VecDeque<Result<RawFd, i32>>,
        open_errno: Option<i32>,
        read_errno: Option<i32>,
        write_errno: Option<i32>,
        sync_results: VecDeque<Result<(), i32>>,
        close_results: VecDeque<Result<(), i32>>,
        close_errno: Option<i32>,
        rename_errno: Option<i32>,
        wrong_token: Option<CompletionToken>,
        extra_completion: bool,
        backend_error: bool,
        complete: bool,
        stats: SubmitStats,
    }

    impl Default for TestDriver {
        fn default() -> TestDriver {
            TestDriver {
                ops: Vec::new(),
                observed: Vec::new(),
                written: Vec::new(),
                file_bytes: Vec::new(),
                open_results: VecDeque::new(),
                open_errno: None,
                read_errno: None,
                write_errno: None,
                sync_results: VecDeque::new(),
                close_results: VecDeque::new(),
                close_errno: None,
                rename_errno: None,
                wrong_token: None,
                extra_completion: false,
                backend_error: false,
                complete: true,
                stats: SubmitStats::default(),
            }
        }
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
            if self.backend_error {
                return Err(io::Error::other("test backend failure"));
            }
            let submitted = self.ops.len() as u64;
            if !self.complete {
                self.stats = SubmitStats { syscalls: 1, sqes: submitted, cqes: 0 };
                return Ok(0);
            }

            let before = out.len();
            for op in core::mem::take(&mut self.ops) {
                match op {
                    IoOp::FileOpen { dir, name, mode, token } => {
                        self.observed.push(ObservedOp::Open { dir, name, mode, token });
                        let result = match self.open_results.pop_front() {
                            Some(Ok(fd)) => CompletionResult::FileOpened { fd },
                            Some(Err(errno)) => CompletionResult::Error { errno, buf: None },
                            None => match self.open_errno {
                                Some(errno) => CompletionResult::Error { errno, buf: None },
                                None => CompletionResult::FileOpened { fd: TEMP_FD },
                            },
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
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
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
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
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FileSync { fd, mode, token } => {
                        self.observed.push(ObservedOp::Sync { fd, mode, token });
                        let result = match self.sync_results.pop_front() {
                            Some(Err(errno)) => CompletionResult::Error { errno, buf: None },
                            Some(Ok(())) | None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FileClose { fd, token } => {
                        self.observed.push(ObservedOp::Close { fd, token });
                        let result = match self.close_results.pop_front() {
                            Some(Err(errno)) => CompletionResult::Error { errno, buf: None },
                            Some(Ok(())) => CompletionResult::FileClosed,
                            None => match self.close_errno {
                                Some(errno) => CompletionResult::Error { errno, buf: None },
                                None => CompletionResult::FileClosed,
                            },
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
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
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    other => panic!("unexpected op {other:?}"),
                }
            }
            if self.extra_completion {
                out.push(Completion {
                    token: CompletionToken::new(TokenClass::File, 99, 0),
                    result: CompletionResult::FileDone,
                });
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

    fn config() -> NamespaceCatalogPublishConfig {
        NamespaceCatalogPublishConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn load_config() -> NamespaceCatalogLoadConfig {
        NamespaceCatalogLoadConfig::new(DIR_FD, TOKEN_SLOT).with_generation(TOKEN_GEN)
    }

    fn data_root_load_config() -> NamespaceCatalogDataRootLoadConfig {
        NamespaceCatalogDataRootLoadConfig::new("infinity-data".to_string(), TOKEN_SLOT)
            .with_generation(TOKEN_GEN)
    }

    fn data_root_publish_config() -> NamespaceCatalogDataRootPublishConfig {
        NamespaceCatalogDataRootPublishConfig::new("infinity-data".to_string(), TOKEN_SLOT)
            .with_generation(TOKEN_GEN)
    }

    fn live_publish_config() -> NamespaceCatalogLivePublishConfig {
        NamespaceCatalogLivePublishConfig::new("infinity-data".to_string(), TOKEN_SLOT)
            .with_generation(TOKEN_GEN)
    }

    fn catalog() -> NsCatalog {
        NsCatalog::new(
            inf_store::NsId::new(18),
            vec![
                NsSpec {
                    id: inf_store::NsId::new(16),
                    name: b"cache".to_vec(),
                    mode: NsMode::Memory,
                    fsync: None,
                    policy: None,
                    maxmemory: Some(1024),
                },
                NsSpec {
                    id: inf_store::NsId::new(17),
                    name: b"orders".to_vec(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Always),
                    policy: None,
                    maxmemory: None,
                },
            ],
        )
        .expect("test catalog")
    }

    #[test]
    fn publish_writes_syncs_renames_and_dirsyncs_meta_image() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();
        let snapshot = catalog();
        let mut expected = Vec::new();
        encode_namespace_catalog(&snapshot, &mut expected).unwrap();

        publish_namespace_catalog(&mut driver, &mut pool, &snapshot, config(), &mut completions)
            .unwrap();

        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert_eq!(decode_namespace_catalog(&driver.written).unwrap(), snapshot);
        assert_eq!(driver.written, expected);
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: DIR_FD,
                    name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: token()
                },
                ObservedOp::WriteAt {
                    fd: TEMP_FD,
                    offset_bytes: 0,
                    len: expected.len() as u32,
                    token: token()
                },
                ObservedOp::Sync { fd: TEMP_FD, mode: FileSyncMode::DataOnly, token: token() },
                ObservedOp::Close { fd: TEMP_FD, token: token() },
                ObservedOp::Rename {
                    old_dir: DIR_FD,
                    old_name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    new_dir: DIR_FD,
                    new_name: NAMESPACE_CATALOG_FILE.to_string(),
                    token: token()
                },
                ObservedOp::Sync { fd: DIR_FD, mode: FileSyncMode::Full, token: token() },
            ]
        );
    }

    #[test]
    fn publish_chunks_catalog_when_buffer_is_smaller_than_image() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 16);
        let mut completions = Vec::new();
        let snapshot = catalog();
        let mut expected = Vec::new();
        encode_namespace_catalog(&snapshot, &mut expected).unwrap();

        publish_namespace_catalog(&mut driver, &mut pool, &snapshot, config(), &mut completions)
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
        assert!(writes.iter().all(|(_, len)| *len <= 16));
        let mut next = 0u64;
        for (offset, len) in writes {
            assert_eq!(offset, next);
            next += u64::from(len);
        }
        assert_eq!(next as usize, expected.len());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn publish_to_data_root_opens_root_publishes_meta_and_closes_root() {
        let snapshot = catalog();
        let mut expected = Vec::new();
        encode_namespace_catalog(&snapshot, &mut expected).unwrap();
        let mut open_results = VecDeque::new();
        open_results.push_back(Ok(ROOT_FD));
        open_results.push_back(Ok(TEMP_FD));
        let mut driver = TestDriver { open_results, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 4096);
        let mut completions = Vec::new();

        publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &snapshot,
            &data_root_publish_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(decode_namespace_catalog(&driver.written).unwrap(), snapshot);
        assert_eq!(driver.written, expected);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: token(),
                },
                ObservedOp::Open {
                    dir: ROOT_FD,
                    name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
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
                    old_dir: ROOT_FD,
                    old_name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    new_dir: ROOT_FD,
                    new_name: NAMESPACE_CATALOG_FILE.to_string(),
                    token: token(),
                },
                ObservedOp::Sync { fd: ROOT_FD, mode: FileSyncMode::Full, token: token() },
                ObservedOp::Close { fd: ROOT_FD, token: token() },
            ]
        );
    }

    #[test]
    fn live_publish_drives_data_root_meta_protocol_without_blocking() {
        let snapshot = catalog();
        let mut expected = Vec::new();
        encode_namespace_catalog(&snapshot, &mut expected).unwrap();
        let mut publisher = NamespaceCatalogLivePublisher::new(live_publish_config()).unwrap();
        publisher.start(snapshot.clone()).unwrap();
        let mut pool = BufferPool::new(1, 16);
        let mut ops = Vec::new();
        let mut observed = Vec::new();
        let mut written = Vec::new();
        let mut completed = false;

        while !completed {
            assert_eq!(
                publisher.drive(&mut pool, &mut ops).unwrap(),
                NamespaceCatalogLivePublishEvent::Pending
            );
            assert_eq!(ops.len(), 1, "live publisher must emit one op at a time");
            let op = ops.pop().unwrap();
            let result = match op {
                IoOp::FileOpen { dir, name, mode, token } => {
                    observed.push(ObservedOp::Open { dir, name, mode, token });
                    let fd = if mode == FileOpenMode::Directory { ROOT_FD } else { TEMP_FD };
                    CompletionResult::FileOpened { fd }
                }
                IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                    observed.push(ObservedOp::WriteAt { fd, offset_bytes, len, token });
                    let start = offset_bytes as usize;
                    let end = start + len as usize;
                    if end > written.len() {
                        written.resize(end, 0);
                    }
                    written[start..end].copy_from_slice(&pool.bytes(buf)[..len as usize]);
                    CompletionResult::FileWritten { buf }
                }
                IoOp::FileSync { fd, mode, token } => {
                    observed.push(ObservedOp::Sync { fd, mode, token });
                    CompletionResult::FileDone
                }
                IoOp::FileClose { fd, token } => {
                    observed.push(ObservedOp::Close { fd, token });
                    CompletionResult::FileClosed
                }
                IoOp::FileRename { old_dir, old_name, new_dir, new_name, token } => {
                    observed.push(ObservedOp::Rename {
                        old_dir,
                        old_name,
                        new_dir,
                        new_name,
                        token,
                    });
                    CompletionResult::FileDone
                }
                other => panic!("unexpected live publish op {other:?}"),
            };
            completed =
                publisher.on_completion(&mut pool, Completion { token: token(), result }).unwrap()
                    == NamespaceCatalogLivePublishEvent::Completed;
        }

        assert!(publisher.is_idle());
        assert_eq!(written, expected);
        assert_eq!(decode_namespace_catalog(&written).unwrap(), snapshot);
        let non_writes: Vec<_> = observed
            .iter()
            .filter(|op| !matches!(op, ObservedOp::WriteAt { .. }))
            .cloned()
            .collect();
        assert_eq!(
            non_writes,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: token(),
                },
                ObservedOp::Open {
                    dir: ROOT_FD,
                    name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: token(),
                },
                ObservedOp::Sync { fd: TEMP_FD, mode: FileSyncMode::DataOnly, token: token() },
                ObservedOp::Close { fd: TEMP_FD, token: token() },
                ObservedOp::Rename {
                    old_dir: ROOT_FD,
                    old_name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    new_dir: ROOT_FD,
                    new_name: NAMESPACE_CATALOG_FILE.to_string(),
                    token: token(),
                },
                ObservedOp::Sync { fd: ROOT_FD, mode: FileSyncMode::Full, token: token() },
                ObservedOp::Close { fd: ROOT_FD, token: token() },
            ]
        );
        let writes: Vec<_> = observed
            .iter()
            .filter_map(|op| match op {
                ObservedOp::WriteAt { fd, offset_bytes, len, token } => {
                    Some((*fd, *offset_bytes, *len, *token))
                }
                _ => None,
            })
            .collect();
        assert!(writes.len() > 1);
        let mut next = 0u64;
        for (fd, offset, len, got_token) in writes {
            assert_eq!(fd, TEMP_FD);
            assert_eq!(offset, next);
            assert!(len <= 16);
            assert_eq!(got_token, token());
            next += u64::from(len);
        }
        assert_eq!(next as usize, expected.len());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn live_publish_retries_write_when_buffer_pool_is_dry() {
        let mut publisher = NamespaceCatalogLivePublisher::new(live_publish_config()).unwrap();
        publisher.start(catalog()).unwrap();
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        let IoOp::FileOpen { .. } = ops.pop().unwrap() else { panic!("expected root open") };
        publisher
            .on_completion(
                &mut pool,
                Completion { token: token(), result: CompletionResult::FileOpened { fd: ROOT_FD } },
            )
            .unwrap();
        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        let IoOp::FileOpen { .. } = ops.pop().unwrap() else { panic!("expected temp open") };
        publisher
            .on_completion(
                &mut pool,
                Completion { token: token(), result: CompletionResult::FileOpened { fd: TEMP_FD } },
            )
            .unwrap();

        let held = pool.try_lease(LeaseKind::Send).unwrap();
        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        assert!(ops.is_empty(), "dry pool should retry without queuing an op");
        pool.release(held);
        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        let Some(IoOp::FileWriteAt { buf, .. }) = ops.pop() else { panic!("expected write op") };
        publisher
            .on_completion(
                &mut pool,
                Completion { token: token(), result: CompletionResult::FileWritten { buf } },
            )
            .unwrap();
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn live_publish_write_error_returns_buffer() {
        let mut publisher = NamespaceCatalogLivePublisher::new(live_publish_config()).unwrap();
        publisher.start(catalog()).unwrap();
        let mut pool = BufferPool::new(1, 128);
        let mut ops = Vec::new();

        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        ops.clear();
        publisher
            .on_completion(
                &mut pool,
                Completion { token: token(), result: CompletionResult::FileOpened { fd: ROOT_FD } },
            )
            .unwrap();
        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        ops.clear();
        publisher
            .on_completion(
                &mut pool,
                Completion { token: token(), result: CompletionResult::FileOpened { fd: TEMP_FD } },
            )
            .unwrap();
        assert_eq!(
            publisher.drive(&mut pool, &mut ops).unwrap(),
            NamespaceCatalogLivePublishEvent::Pending
        );
        let Some(IoOp::FileWriteAt { buf, .. }) = ops.pop() else { panic!("expected write op") };

        let error = publisher
            .on_completion(
                &mut pool,
                Completion {
                    token: token(),
                    result: CompletionResult::Error { errno: TEST_EIO, buf: Some(buf) },
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::WriteTemp {
                fd: TEMP_FD,
                offset_bytes: 0,
                errno: TEST_EIO
            }
        ));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn publish_to_data_root_rejects_bad_root_name_before_io() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();
        let config = NamespaceCatalogDataRootPublishConfig::new("../bad".to_string(), TOKEN_SLOT);

        let error = publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog(),
            &config,
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, NamespaceCatalogPublishError::InvalidDataRootName { .. }));
        assert!(driver.observed.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn publish_to_data_root_open_error_is_typed() {
        let mut driver = TestDriver { open_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog(),
            &data_root_publish_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::OpenDataRoot { errno: TEST_EIO, .. }
        ));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn publish_to_data_root_close_error_is_typed_after_successful_publish() {
        let mut open_results = VecDeque::new();
        open_results.push_back(Ok(ROOT_FD));
        open_results.push_back(Ok(TEMP_FD));
        let mut close_results = VecDeque::new();
        close_results.push_back(Ok(()));
        close_results.push_back(Err(TEST_EIO));
        let mut driver = TestDriver { open_results, close_results, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 4096);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog(),
            &data_root_publish_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::CloseDataRoot { fd: ROOT_FD, errno: TEST_EIO }
        ));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: ROOT_FD, .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn write_error_returns_buffer_and_stops_before_rename() {
        let mut driver = TestDriver { write_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog(
            &mut driver,
            &mut pool,
            &catalog(),
            config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::WriteTemp {
                fd: TEMP_FD,
                offset_bytes: 0,
                errno: TEST_EIO
            }
        ));
        assert_eq!(pool.reconcile(), Ok(()));
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: DIR_FD,
                    name: NAMESPACE_CATALOG_TEMP_FILE.to_string(),
                    mode: FileOpenMode::ReadWriteCreateTruncate,
                    token: token()
                },
                ObservedOp::WriteAt {
                    fd: TEMP_FD,
                    offset_bytes: 0,
                    len: driver.observed_write_len(),
                    token: token()
                },
            ]
        );
    }

    #[test]
    fn rename_failure_is_typed_before_directory_sync() {
        let mut driver = TestDriver { rename_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog(
            &mut driver,
            &mut pool,
            &catalog(),
            config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::Rename {
                old_name: NAMESPACE_CATALOG_TEMP_FILE,
                new_name: NAMESPACE_CATALOG_FILE,
                errno: TEST_EIO
            }
        ));
        assert!(
            !driver.observed.iter().any(|op| matches!(op, ObservedOp::Sync { fd: DIR_FD, .. }))
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn directory_sync_failure_is_typed_after_rename() {
        let mut sync_results = VecDeque::new();
        sync_results.push_back(Ok(()));
        sync_results.push_back(Err(TEST_EIO));
        let mut driver = TestDriver { sync_results, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog(
            &mut driver,
            &mut pool,
            &catalog(),
            config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::SyncDir { fd: DIR_FD, errno: TEST_EIO }
        ));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Sync { fd: DIR_FD, .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn wrong_token_is_reported() {
        let wrong = CompletionToken::new(TokenClass::File, TOKEN_SLOT + 1, TOKEN_GEN);
        let mut driver = TestDriver { wrong_token: Some(wrong), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = publish_namespace_catalog(
            &mut driver,
            &mut pool,
            &catalog(),
            config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogPublishError::UnexpectedToken {
                phase: NamespaceCatalogPublishPhase::OpenTemp,
                expected,
                got
            } if expected == token() && got == wrong
        ));
    }

    #[test]
    fn load_missing_meta_is_first_boot_empty_catalog() {
        let mut driver = TestDriver { open_errno: Some(TEST_ENOENT), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 16);
        let mut completions = Vec::new();

        let loaded =
            load_namespace_catalog(&mut driver, &mut pool, load_config(), &mut completions)
                .unwrap();

        assert_eq!(loaded, NsCatalog::empty());
        assert_eq!(
            driver.observed,
            vec![ObservedOp::Open {
                dir: DIR_FD,
                name: NAMESPACE_CATALOG_FILE.to_string(),
                mode: FileOpenMode::ReadOnly,
                token: token()
            }]
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_reads_chunks_closes_and_decodes_catalog() {
        let snapshot = catalog();
        let mut bytes = Vec::new();
        encode_namespace_catalog(&snapshot, &mut bytes).unwrap();
        let mut driver = TestDriver { file_bytes: bytes.clone(), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 13);
        let mut completions = Vec::new();

        let loaded =
            load_namespace_catalog(&mut driver, &mut pool, load_config(), &mut completions)
                .unwrap();

        assert_eq!(loaded, snapshot);
        assert!(matches!(
            driver.observed.first(),
            Some(ObservedOp::Open {
                dir: DIR_FD,
                name,
                mode: FileOpenMode::ReadOnly,
                token: got_token
            }) if name == NAMESPACE_CATALOG_FILE && *got_token == token()
        ));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: TEMP_FD, .. })));
        let reads: Vec<_> = driver
            .observed
            .iter()
            .filter_map(|op| match op {
                ObservedOp::ReadAt { offset_bytes, len, .. } => Some((*offset_bytes, *len)),
                _ => None,
            })
            .collect();
        assert!(reads.len() > 1);
        assert!(reads.iter().all(|(_, len)| *len <= 13));
        assert_eq!(reads.last().copied(), Some((bytes.len() as u64, 13)));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_from_data_root_opens_root_loads_meta_and_closes_root() {
        let snapshot = catalog();
        let mut bytes = Vec::new();
        encode_namespace_catalog(&snapshot, &mut bytes).unwrap();
        let mut open_results = VecDeque::new();
        open_results.push_back(Ok(ROOT_FD));
        open_results.push_back(Ok(TEMP_FD));
        let mut driver =
            TestDriver { file_bytes: bytes.clone(), open_results, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 4096);
        let mut completions = Vec::new();

        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_load_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded, snapshot);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: token(),
                },
                ObservedOp::Open {
                    dir: ROOT_FD,
                    name: NAMESPACE_CATALOG_FILE.to_string(),
                    mode: FileOpenMode::ReadOnly,
                    token: token(),
                },
                ObservedOp::ReadAt { fd: TEMP_FD, offset_bytes: 0, len: 4096, token: token() },
                ObservedOp::ReadAt {
                    fd: TEMP_FD,
                    offset_bytes: bytes.len() as u64,
                    len: 4096,
                    token: token(),
                },
                ObservedOp::Close { fd: TEMP_FD, token: token() },
                ObservedOp::Close { fd: ROOT_FD, token: token() },
            ]
        );
    }

    #[test]
    fn load_from_data_root_treats_missing_meta_as_empty_after_root_open() {
        let mut open_results = VecDeque::new();
        open_results.push_back(Ok(ROOT_FD));
        open_results.push_back(Err(TEST_ENOENT));
        let mut driver = TestDriver { open_results, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();

        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_load_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded, NsCatalog::empty());
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: ROOT_FD, .. })));
    }

    #[test]
    fn load_from_data_root_rejects_bad_root_name_before_io() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();
        let config = NamespaceCatalogDataRootLoadConfig::new("../bad".to_string(), TOKEN_SLOT);

        let error =
            load_namespace_catalog_in_data_root(&mut driver, &mut pool, &config, &mut completions)
                .unwrap_err();

        assert!(matches!(error, NamespaceCatalogLoadError::InvalidDataRootName { .. }));
        assert!(driver.observed.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_from_data_root_open_error_is_typed() {
        let mut driver = TestDriver { open_errno: Some(TEST_EIO), ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 128);
        let mut completions = Vec::new();

        let error = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_load_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, NamespaceCatalogLoadError::OpenDataRoot { errno: TEST_EIO, .. }));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_read_error_returns_buffer() {
        let mut driver = TestDriver {
            file_bytes: vec![1, 2, 3],
            read_errno: Some(TEST_EIO),
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(1, 16);
        let mut completions = Vec::new();

        let error = load_namespace_catalog(&mut driver, &mut pool, load_config(), &mut completions)
            .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogLoadError::Read { fd: TEMP_FD, offset_bytes: 0, errno: TEST_EIO }
        ));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: TEMP_FD, .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_rejects_oversized_catalog_before_decode() {
        let mut driver = TestDriver {
            file_bytes: vec![0; MAX_NAMESPACE_CATALOG_BYTES + 1],
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(1, 4096);
        let mut completions = Vec::new();

        let error = load_namespace_catalog(&mut driver, &mut pool, load_config(), &mut completions)
            .unwrap_err();

        assert!(matches!(
            error,
            NamespaceCatalogLoadError::CatalogTooLarge {
                max_len_bytes: MAX_NAMESPACE_CATALOG_BYTES
            }
        ));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: TEMP_FD, .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn load_corrupt_catalog_fails_closed_after_close() {
        let mut bytes = Vec::new();
        encode_namespace_catalog(&catalog(), &mut bytes).unwrap();
        bytes[0] ^= 0xFF;
        let mut driver = TestDriver { file_bytes: bytes, ..TestDriver::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = load_namespace_catalog(&mut driver, &mut pool, load_config(), &mut completions)
            .unwrap_err();

        assert!(matches!(error, NamespaceCatalogLoadError::Decode(_)));
        assert!(matches!(driver.observed.last(), Some(ObservedOp::Close { fd: TEMP_FD, .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    impl TestDriver {
        fn observed_write_len(&self) -> u32 {
            self.observed
                .iter()
                .find_map(|op| match op {
                    ObservedOp::WriteAt { len, .. } => Some(*len),
                    _ => None,
                })
                .unwrap_or(0)
        }
    }
}
