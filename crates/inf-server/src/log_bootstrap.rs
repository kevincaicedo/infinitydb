use core::convert::Infallible;
use core::fmt;
use std::io;

use inf_alloc::BufferPool;
use inf_foundation::CellId;
use inf_log::{
    LogCodecError, RecoveryManifest, SegmentCatalog, SegmentConfig, SegmentFrame,
    SegmentFrameError, SegmentFrameSink, SegmentId, SegmentLifecycleError, SegmentReadConfig,
    SegmentReadConfigError, SegmentReadError, SegmentReadTerminal, SegmentTailPolicy,
    scan_segment_names,
};
use inf_runtime::{
    BackendDriver, Completion, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode, IoOp,
    RawFd, TokenClass, Wait,
};
use inf_store::Keyspace;

use crate::checkpoint::CheckpointImageLoadConfig;
use crate::log_writer::LogWriteIo;
use crate::manifest::{
    RecoveryManifestLoadConfig, RecoveryManifestLoadError, load_recovery_manifest,
};
use crate::recovery::{
    AppliedCheckpointImage, CheckpointImageApplyError, KeyspaceReplayError, KeyspaceReplaySink,
    KeyspaceReplayStats, SegmentReadIo, SegmentReadIoError, apply_checkpoint_image_to_keyspace,
};

const AT_FDCWD_FD: RawFd = -100;
const CKPT_DIR_NAME: &str = "ckpt";
const EEXIST_ERRNO: i32 = 17;
const ENOENT_ERRNO: i32 = 2;
const LOG_DIR_NAME: &str = "log";
const LOG_LAYOUT_DIR_MODE: u32 = 0o700;

pub const DEFAULT_LOG_BOOTSTRAP_REAP_LIMIT: u32 = 64;
pub use inf_log::{
    CheckpointId as LogCheckpointId, CheckpointRef as LogCheckpointRef, FrameMeta as LogFrameMeta,
    NamespaceId as LogNamespaceId, RecoveryManifest as LogRecoveryManifest,
    SegmentCatalog as LogSegmentCatalog, SegmentId as LogSegmentId,
    scan_segment_names as scan_log_segment_names,
};

/// Catalog for the first active segment created by [`open_first_boot_log_writer`].
///
/// This keeps simulator and boot callers behind the server bootstrap seam
/// instead of requiring them to construct `inf-log` segment catalogs directly.
pub fn first_boot_segment_catalog() -> SegmentCatalog {
    scan_segment_names([SegmentId::ZERO.file_name().as_str()])
        .expect("segment zero is a valid singleton segment catalog")
}

/// Cold boot configuration for opening the active log segment.
///
/// This adapter belongs in `inf-server`: `inf-log` owns segment naming and
/// placement, `inf-runtime` owns the backend file-op seam, and the server
/// composes those pieces into one cell-local writer.
#[derive(Copy, Clone, Debug)]
pub struct LogBootstrapConfig {
    pub dir: RawFd,
    pub segment: SegmentId,
    pub segment_config: SegmentConfig,
    pub bootstrap_token_slot: u32,
    pub bootstrap_token_generation: u32,
    pub writer_token_slot: u32,
    pub writer_token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl LogBootstrapConfig {
    pub const fn first_boot(
        dir: RawFd,
        segment_config: SegmentConfig,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> LogBootstrapConfig {
        LogBootstrapConfig {
            dir,
            segment: SegmentId::ZERO,
            segment_config,
            bootstrap_token_slot,
            bootstrap_token_generation: 0,
            writer_token_slot,
            writer_token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_LOG_BOOTSTRAP_REAP_LIMIT,
        }
    }

    pub fn first_boot_sized(
        dir: RawFd,
        segment_size_bytes: u32,
        frame_size_max_bytes: u32,
        preallocate_threshold_bytes: u32,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> Result<LogBootstrapConfig, SegmentLifecycleError> {
        let segment_config = SegmentConfig::new(
            segment_size_bytes,
            frame_size_max_bytes,
            preallocate_threshold_bytes,
        )?;
        Ok(LogBootstrapConfig::first_boot(
            dir,
            segment_config,
            bootstrap_token_slot,
            writer_token_slot,
        ))
    }

    pub const fn with_generations(
        mut self,
        bootstrap_token_generation: u32,
        writer_token_generation: u32,
    ) -> LogBootstrapConfig {
        self.bootstrap_token_generation = bootstrap_token_generation;
        self.writer_token_generation = writer_token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> LogBootstrapConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> LogBootstrapConfig {
        self.max_reaps = max_reaps;
        self
    }
}

/// Cold first-boot layout configuration for one cell.
///
/// This is deliberately only directory topology and the active-segment writer:
/// it does not scan existing segment sets, recover active offsets, or claim a
/// namespace durability policy. Production boot wires those later pieces on
/// top of the same backend seam.
#[derive(Copy, Clone, Debug)]
pub struct LogLayoutConfig {
    pub root_dir: RawFd,
    pub cell: CellId,
    pub segment_config: SegmentConfig,
    pub bootstrap_token_slot: u32,
    pub bootstrap_token_generation: u32,
    pub writer_token_slot: u32,
    pub writer_token_generation: u32,
    pub wait: Wait,
    pub max_reaps: u32,
}

impl LogLayoutConfig {
    pub const fn first_boot(
        root_dir: RawFd,
        cell: CellId,
        segment_config: SegmentConfig,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> LogLayoutConfig {
        LogLayoutConfig {
            root_dir,
            cell,
            segment_config,
            bootstrap_token_slot,
            bootstrap_token_generation: 0,
            writer_token_slot,
            writer_token_generation: 0,
            wait: Wait::Poll,
            max_reaps: DEFAULT_LOG_BOOTSTRAP_REAP_LIMIT,
        }
    }

    pub fn first_boot_sized(
        root_dir: RawFd,
        cell: CellId,
        segment_size_bytes: u32,
        frame_size_max_bytes: u32,
        preallocate_threshold_bytes: u32,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> Result<LogLayoutConfig, SegmentLifecycleError> {
        let segment_config = SegmentConfig::new(
            segment_size_bytes,
            frame_size_max_bytes,
            preallocate_threshold_bytes,
        )?;
        Ok(LogLayoutConfig::first_boot(
            root_dir,
            cell,
            segment_config,
            bootstrap_token_slot,
            writer_token_slot,
        ))
    }

    pub const fn with_generations(
        mut self,
        bootstrap_token_generation: u32,
        writer_token_generation: u32,
    ) -> LogLayoutConfig {
        self.bootstrap_token_generation = bootstrap_token_generation;
        self.writer_token_generation = writer_token_generation;
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> LogLayoutConfig {
        self.wait = wait;
        self
    }

    pub const fn with_max_reaps(mut self, max_reaps: u32) -> LogLayoutConfig {
        self.max_reaps = max_reaps;
        self
    }

    const fn with_root_dir(mut self, root_dir: RawFd) -> LogLayoutConfig {
        self.root_dir = root_dir;
        self
    }

    fn segment_bootstrap(self, log_dir: RawFd) -> LogBootstrapConfig {
        LogBootstrapConfig {
            dir: log_dir,
            segment: SegmentId::ZERO,
            segment_config: self.segment_config,
            bootstrap_token_slot: self.bootstrap_token_slot,
            bootstrap_token_generation: self.bootstrap_token_generation,
            writer_token_slot: self.writer_token_slot,
            writer_token_generation: self.writer_token_generation,
            wait: self.wait,
            max_reaps: self.max_reaps,
        }
    }
}

/// Cold first-boot configuration for a named data root under a parent
/// directory.
///
/// `root_name` is a single directory entry, not a recursive path. That keeps
/// parent fsync semantics explicit: this helper creates one entry under
/// `root_parent_dir`, fsyncs that parent after opening the root, then delegates
/// the per-cell layout to [`open_first_boot_log_writer`].
#[derive(Clone, Debug)]
pub struct LogDataRootConfig {
    pub root_parent_dir: RawFd,
    pub root_name: String,
    pub layout: LogLayoutConfig,
}

impl LogDataRootConfig {
    pub fn first_boot(
        root_name: String,
        cell: CellId,
        segment_config: SegmentConfig,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> LogDataRootConfig {
        LogDataRootConfig {
            root_parent_dir: AT_FDCWD_FD,
            root_name,
            layout: LogLayoutConfig::first_boot(
                AT_FDCWD_FD,
                cell,
                segment_config,
                bootstrap_token_slot,
                writer_token_slot,
            ),
        }
    }

    pub fn first_boot_sized(
        root_name: String,
        cell: CellId,
        segment_size_bytes: u32,
        frame_size_max_bytes: u32,
        preallocate_threshold_bytes: u32,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> Result<LogDataRootConfig, SegmentLifecycleError> {
        let segment_config = SegmentConfig::new(
            segment_size_bytes,
            frame_size_max_bytes,
            preallocate_threshold_bytes,
        )?;
        Ok(LogDataRootConfig::first_boot(
            root_name,
            cell,
            segment_config,
            bootstrap_token_slot,
            writer_token_slot,
        ))
    }

    pub fn first_boot_default(
        root_name: String,
        cell: CellId,
        bootstrap_token_slot: u32,
        writer_token_slot: u32,
    ) -> LogDataRootConfig {
        LogDataRootConfig::first_boot(
            root_name,
            cell,
            SegmentConfig::DEFAULT,
            bootstrap_token_slot,
            writer_token_slot,
        )
    }

    pub fn with_generations(
        mut self,
        bootstrap_token_generation: u32,
        writer_token_generation: u32,
    ) -> LogDataRootConfig {
        self.layout =
            self.layout.with_generations(bootstrap_token_generation, writer_token_generation);
        self
    }

    pub fn with_wait(mut self, wait: Wait) -> LogDataRootConfig {
        self.layout = self.layout.with_wait(wait);
        self
    }

    pub fn with_max_reaps(mut self, max_reaps: u32) -> LogDataRootConfig {
        self.layout = self.layout.with_max_reaps(max_reaps);
        self
    }
}

/// Create/open a named data root, then create/open one cell's log layout
/// below it and install the active segment writer.
pub fn open_first_boot_log_writer_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    completions: &mut Vec<Completion>,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    io.create_directory(parent.fd, &config.root_name, LogBootstrapPhase::CreateDataRootDir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    io.sync_directory(parent.fd, LogBootstrapPhase::SyncDataRootParentDir)?;
    let root = LayoutRoot { fd: root_dir, close_after_bootstrap: true };
    let result = open_first_boot_log_writer_in_open_root(
        &mut io,
        config.layout.with_root_dir(root_dir),
        root,
    );
    if result.is_ok() && parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    result
}

/// Create/open `shard-N/{log,ckpt}`, fsync created parent directories, open
/// and preallocate the first active segment, then close directory fds.
///
/// `root_dir` may be an already-open directory fd, or `AT_FDCWD` to mean the
/// current directory. In the latter case this adapter opens `.` first so
/// created shard entries can be fsynced through a real fd.
pub fn open_first_boot_log_writer<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: LogLayoutConfig,
    completions: &mut Vec<Completion>,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_bootstrap_inputs(
        completions,
        config.max_reaps,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
        config.writer_token_slot,
        config.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
    );
    let mut io =
        BootstrapIo { driver, pool, completions, params: ReapParams::from_layout(config), token };
    let root = io.open_layout_root(config.root_dir)?;
    open_first_boot_log_writer_in_open_root(&mut io, config, root)
}

/// Open the existing per-cell log directory and return its fd to serving code.
///
/// Callers use this after boot/recovery has created or validated the layout.
/// The returned fd is intentionally long-lived: `ServerPlane` segment
/// maintenance uses it to create and directory-sync prepared successor
/// segments through the normal runtime backend.
pub fn open_log_directory<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: LogLayoutConfig,
    completions: &mut Vec<Completion>,
) -> Result<RawFd, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_bootstrap_inputs(
        completions,
        config.max_reaps,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
        config.writer_token_slot,
        config.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
    );
    let mut io =
        BootstrapIo { driver, pool, completions, params: ReapParams::from_layout(config), token };
    let root = io.open_layout_root(config.root_dir)?;
    open_log_directory_in_open_root(&mut io, config, root)
}

pub fn open_log_directory_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    completions: &mut Vec<Completion>,
) -> Result<RawFd, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    let root = LayoutRoot { fd: root_dir, close_after_bootstrap: true };
    let result =
        open_log_directory_in_open_root(&mut io, config.layout.with_root_dir(root_dir), root);
    if result.is_ok() && parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    result
}

pub fn open_checkpoint_directory_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    completions: &mut Vec<Completion>,
) -> Result<RawFd, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    let shard_name = format!("shard-{}", config.layout.cell.0);
    let shard_dir = io.open_directory(root_dir, &shard_name, LogBootstrapPhase::OpenShardDir)?;
    let ckpt_dir =
        io.open_directory(shard_dir, CKPT_DIR_NAME, LogBootstrapPhase::OpenCheckpointDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    io.close_directory(root_dir, LogBootstrapPhase::CloseRootDir)?;
    if parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    Ok(ckpt_dir)
}

/// Open an existing active segment, recover its active tail offset, then
/// construct a [`LogWriteIo`] positioned after the last complete frame.
pub fn open_recovered_log_writer_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    catalog: &SegmentCatalog,
    completions: &mut Vec<Completion>,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    io.create_directory(parent.fd, &config.root_name, LogBootstrapPhase::CreateDataRootDir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    io.sync_directory(parent.fd, LogBootstrapPhase::SyncDataRootParentDir)?;
    let root = LayoutRoot { fd: root_dir, close_after_bootstrap: true };
    let result = open_recovered_log_writer_in_open_root(
        &mut io,
        config.layout.with_root_dir(root_dir),
        root,
        catalog,
    );
    if result.is_ok() && parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    result
}

/// Open an existing per-cell log, replay all complete user-mutation frames
/// into `keyspace`, and return a writer positioned after the active tail's
/// last complete frame.
pub fn open_recovered_log_writer_replaying_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    catalog: &SegmentCatalog,
    keyspace: &mut Keyspace,
    completions: &mut Vec<Completion>,
) -> Result<(LogWriteIo, KeyspaceReplayStats), LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    io.create_directory(parent.fd, &config.root_name, LogBootstrapPhase::CreateDataRootDir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    io.sync_directory(parent.fd, LogBootstrapPhase::SyncDataRootParentDir)?;
    let root = LayoutRoot { fd: root_dir, close_after_bootstrap: true };
    let result = open_recovered_log_writer_replaying_in_open_root(
        &mut io,
        config.layout.with_root_dir(root_dir),
        root,
        catalog,
        keyspace,
    );
    if result.is_ok() && parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    result
}

/// Apply the checkpoint named by `manifest`, replay its tail segment set into
/// `keyspace`, and return a writer positioned after the active tail.
pub fn open_recovered_log_writer_replaying_manifest_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    manifest: &RecoveryManifest,
    keyspace: &mut Keyspace,
    completions: &mut Vec<Completion>,
) -> Result<(LogWriteIo, AppliedCheckpointImage, KeyspaceReplayStats), LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    io.create_directory(parent.fd, &config.root_name, LogBootstrapPhase::CreateDataRootDir)?;
    let root_dir =
        io.open_directory(parent.fd, &config.root_name, LogBootstrapPhase::OpenDataRootDir)?;
    io.sync_directory(parent.fd, LogBootstrapPhase::SyncDataRootParentDir)?;
    let root = LayoutRoot { fd: root_dir, close_after_bootstrap: true };
    let result = open_recovered_log_writer_replaying_manifest_in_open_root(
        &mut io,
        config.layout.with_root_dir(root_dir),
        root,
        manifest,
        keyspace,
    );
    if result.is_ok() && parent.close_after_bootstrap {
        io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir)?;
    }
    result
}

/// Load the optional per-cell recovery MANIFEST from `shard-N/ckpt` under a
/// named data root.
///
/// Missing data root, shard directory, checkpoint directory, or MANIFEST is
/// first-boot/no-checkpoint state and returns `Ok(None)`. Present but corrupt
/// MANIFEST bytes fail closed through [`LogBootstrapError::RecoveryManifestLoad`].
pub fn load_recovery_manifest_in_data_root<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: &LogDataRootConfig,
    completions: &mut Vec<Completion>,
) -> Result<Option<RecoveryManifest>, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_data_root_name(&config.root_name)?;
    validate_bootstrap_inputs(
        completions,
        config.layout.max_reaps,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
        config.layout.writer_token_slot,
        config.layout.writer_token_generation,
    )?;

    let token = CompletionToken::new(
        TokenClass::File,
        config.layout.bootstrap_token_slot,
        config.layout.bootstrap_token_generation,
    );
    let mut io = BootstrapIo {
        driver,
        pool,
        completions,
        params: ReapParams::from_layout(config.layout),
        token,
    };
    let parent = io.open_layout_root(config.root_parent_dir)?;
    let loaded =
        io.load_recovery_manifest_from_data_root(parent.fd, &config.root_name, config.layout);
    let close_parent = if parent.close_after_bootstrap {
        Some(io.close_directory(parent.fd, LogBootstrapPhase::CloseDataRootParentDir))
    } else {
        None
    };
    match (loaded, close_parent) {
        (Ok(manifest), None | Some(Ok(()))) => Ok(manifest),
        (Err(error), _) => Err(error),
        (Ok(_), Some(Err(error))) => Err(error),
    }
}

fn open_first_boot_log_writer_in_open_root<D>(
    io: &mut BootstrapIo<'_, D>,
    config: LogLayoutConfig,
    root: LayoutRoot,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    let shard_name = format!("shard-{}", config.cell.0);
    let shard_dir = io.create_open_directory(root.fd, &shard_name, DirectoryPhases::shard())?;
    let log_dir = io.create_open_directory(shard_dir, LOG_DIR_NAME, DirectoryPhases::log())?;
    let ckpt_dir =
        io.create_open_directory(shard_dir, CKPT_DIR_NAME, DirectoryPhases::checkpoint())?;

    io.close_directory(ckpt_dir, LogBootstrapPhase::CloseCheckpointDir)?;
    let writer = io.open_segment_writer(config.segment_bootstrap(log_dir))?;
    io.sync_directory(log_dir, LogBootstrapPhase::SyncLogDir)?;
    io.close_directory(log_dir, LogBootstrapPhase::CloseLogDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    if root.close_after_bootstrap {
        io.close_directory(root.fd, LogBootstrapPhase::CloseRootDir)?;
    }

    Ok(writer)
}

fn open_recovered_log_writer_in_open_root<D>(
    io: &mut BootstrapIo<'_, D>,
    config: LogLayoutConfig,
    root: LayoutRoot,
    catalog: &SegmentCatalog,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    let active = catalog.last().ok_or(LogBootstrapError::EmptySegmentCatalog)?;
    let shard_name = format!("shard-{}", config.cell.0);
    let shard_dir = io.create_open_directory(root.fd, &shard_name, DirectoryPhases::shard())?;
    let log_dir = io.create_open_directory(shard_dir, LOG_DIR_NAME, DirectoryPhases::log())?;
    let ckpt_dir =
        io.create_open_directory(shard_dir, CKPT_DIR_NAME, DirectoryPhases::checkpoint())?;

    io.close_directory(ckpt_dir, LogBootstrapPhase::CloseCheckpointDir)?;
    let active_fd = io.open_existing_segment(log_dir, active)?;
    let active_offset = io.recover_active_tail(active_fd, active)?.offset_bytes();
    io.close_directory(log_dir, LogBootstrapPhase::CloseLogDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    if root.close_after_bootstrap {
        io.close_directory(root.fd, LogBootstrapPhase::CloseRootDir)?;
    }

    LogWriteIo::open(
        active_fd,
        active,
        active_offset,
        config.segment_config,
        config.writer_token_slot,
        config.writer_token_generation,
    )
    .map_err(LogBootstrapError::Segment)
}

fn open_log_directory_in_open_root<D>(
    io: &mut BootstrapIo<'_, D>,
    config: LogLayoutConfig,
    root: LayoutRoot,
) -> Result<RawFd, LogBootstrapError>
where
    D: BackendDriver,
{
    let shard_name = format!("shard-{}", config.cell.0);
    let shard_dir = io.open_directory(root.fd, &shard_name, LogBootstrapPhase::OpenShardDir)?;
    let log_dir = io.open_directory(shard_dir, LOG_DIR_NAME, LogBootstrapPhase::OpenLogDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    if root.close_after_bootstrap {
        io.close_directory(root.fd, LogBootstrapPhase::CloseRootDir)?;
    }
    Ok(log_dir)
}

fn open_recovered_log_writer_replaying_in_open_root<D>(
    io: &mut BootstrapIo<'_, D>,
    config: LogLayoutConfig,
    root: LayoutRoot,
    catalog: &SegmentCatalog,
    keyspace: &mut Keyspace,
) -> Result<(LogWriteIo, KeyspaceReplayStats), LogBootstrapError>
where
    D: BackendDriver,
{
    let active = catalog.last().ok_or(LogBootstrapError::EmptySegmentCatalog)?;
    let shard_name = format!("shard-{}", config.cell.0);
    let shard_dir = io.create_open_directory(root.fd, &shard_name, DirectoryPhases::shard())?;
    let log_dir = io.create_open_directory(shard_dir, LOG_DIR_NAME, DirectoryPhases::log())?;
    let ckpt_dir =
        io.create_open_directory(shard_dir, CKPT_DIR_NAME, DirectoryPhases::checkpoint())?;

    io.close_directory(ckpt_dir, LogBootstrapPhase::CloseCheckpointDir)?;
    let mut replay = KeyspaceReplaySink::new(keyspace);
    let mut active_fd = None;
    let mut active_offset = 0;
    for segment in catalog.iter() {
        let fd = io.open_existing_segment(log_dir, segment)?;
        if segment == active {
            let tail = io.recover_active_tail_replaying(fd, active, &mut replay)?;
            active_offset = tail.offset_bytes();
            active_fd = Some(fd);
        } else {
            io.replay_sealed_segment(fd, segment, &mut replay)?;
            io.close_file(fd, LogBootstrapPhase::CloseRecoveredSegment)?;
        }
    }

    io.close_directory(log_dir, LogBootstrapPhase::CloseLogDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    if root.close_after_bootstrap {
        io.close_directory(root.fd, LogBootstrapPhase::CloseRootDir)?;
    }

    let writer = LogWriteIo::open(
        active_fd.expect("catalog last segment was opened above"),
        active,
        active_offset,
        config.segment_config,
        config.writer_token_slot,
        config.writer_token_generation,
    )
    .map_err(LogBootstrapError::Segment)?;
    Ok((writer, replay.stats()))
}

fn open_recovered_log_writer_replaying_manifest_in_open_root<D>(
    io: &mut BootstrapIo<'_, D>,
    config: LogLayoutConfig,
    root: LayoutRoot,
    manifest: &RecoveryManifest,
    keyspace: &mut Keyspace,
) -> Result<(LogWriteIo, AppliedCheckpointImage, KeyspaceReplayStats), LogBootstrapError>
where
    D: BackendDriver,
{
    let catalog = manifest.segments();
    let active = catalog.last().ok_or(LogBootstrapError::EmptySegmentCatalog)?;
    let begin_segment = SegmentId::new(manifest.begin_lsn().segment())
        .expect("recovery manifest validates begin LSN segment id");
    let shard_name = format!("shard-{}", config.cell.0);
    let shard_dir = io.create_open_directory(root.fd, &shard_name, DirectoryPhases::shard())?;
    let log_dir = io.create_open_directory(shard_dir, LOG_DIR_NAME, DirectoryPhases::log())?;
    let ckpt_dir =
        io.create_open_directory(shard_dir, CKPT_DIR_NAME, DirectoryPhases::checkpoint())?;

    let applied = io.apply_checkpoint_image(ckpt_dir, manifest, keyspace)?;
    io.close_directory(ckpt_dir, LogBootstrapPhase::CloseCheckpointDir)?;
    let mut replay = KeyspaceReplaySink::from_lsn(keyspace, manifest.begin_lsn());
    let mut active_fd = None;
    let mut active_offset = 0;
    for segment in catalog.iter() {
        if segment < begin_segment {
            continue;
        }
        let fd = io.open_existing_segment(log_dir, segment)?;
        if segment == active {
            let tail = io.recover_active_tail_replaying(fd, active, &mut replay)?;
            active_offset = tail.offset_bytes();
            active_fd = Some(fd);
        } else {
            io.replay_sealed_segment(fd, segment, &mut replay)?;
            io.close_file(fd, LogBootstrapPhase::CloseRecoveredSegment)?;
        }
    }

    io.close_directory(log_dir, LogBootstrapPhase::CloseLogDir)?;
    io.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir)?;
    if root.close_after_bootstrap {
        io.close_directory(root.fd, LogBootstrapPhase::CloseRootDir)?;
    }

    let writer = LogWriteIo::open(
        active_fd.expect("manifest active segment was opened above"),
        active,
        active_offset,
        config.segment_config,
        config.writer_token_slot,
        config.writer_token_generation,
    )
    .map_err(LogBootstrapError::Segment)?;
    Ok((writer, applied, replay.stats()))
}

/// Open and preallocate the active segment, then construct a [`LogWriteIo`].
///
/// The caller supplies an empty completion scratch vector so this boot path
/// never drops completions owned by another subsystem. It is intended for
/// startup/setup before the normal cell loop begins.
pub fn open_preallocated_log_writer<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    config: LogBootstrapConfig,
    completions: &mut Vec<Completion>,
) -> Result<LogWriteIo, LogBootstrapError>
where
    D: BackendDriver,
{
    validate_bootstrap_inputs(
        completions,
        config.max_reaps,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
        config.writer_token_slot,
        config.writer_token_generation,
    )?;

    let bootstrap_token = CompletionToken::new(
        TokenClass::File,
        config.bootstrap_token_slot,
        config.bootstrap_token_generation,
    );

    driver.push(IoOp::FileOpen {
        dir: config.dir,
        name: config.segment.file_name(),
        mode: FileOpenMode::ReadWriteCreate,
        token: bootstrap_token,
    });
    let fd = match reap_bootstrap_completion(
        driver,
        pool,
        completions,
        ReapParams::from_segment(config),
        LogBootstrapPhase::Open,
        bootstrap_token,
    )?
    .result
    {
        CompletionResult::FileOpened { fd } => fd,
        CompletionResult::Error { errno, buf: None } => {
            return Err(LogBootstrapError::Open { segment: config.segment, errno });
        }
        other => {
            return Err(LogBootstrapError::UnexpectedCompletionKind {
                phase: LogBootstrapPhase::Open,
                result: result_name(&other),
            });
        }
    };

    driver.push(IoOp::FilePreallocate {
        fd,
        len_bytes: u64::from(config.segment_config.segment_size_bytes()),
        token: bootstrap_token,
    });
    match reap_bootstrap_completion(
        driver,
        pool,
        completions,
        ReapParams::from_segment(config),
        LogBootstrapPhase::Preallocate,
        bootstrap_token,
    )?
    .result
    {
        CompletionResult::FileDone => {}
        CompletionResult::Error { errno, buf: None } => {
            return Err(LogBootstrapError::Preallocate { segment: config.segment, fd, errno });
        }
        other => {
            return Err(LogBootstrapError::UnexpectedCompletionKind {
                phase: LogBootstrapPhase::Preallocate,
                result: result_name(&other),
            });
        }
    }

    LogWriteIo::open(
        fd,
        config.segment,
        0,
        config.segment_config,
        config.writer_token_slot,
        config.writer_token_generation,
    )
    .map_err(LogBootstrapError::Segment)
}

fn validate_bootstrap_inputs(
    completions: &[Completion],
    max_reaps: u32,
    bootstrap_token_slot: u32,
    bootstrap_token_generation: u32,
    writer_token_slot: u32,
    writer_token_generation: u32,
) -> Result<(), LogBootstrapError> {
    if !completions.is_empty() {
        return Err(LogBootstrapError::ScratchNotEmpty { len: completions.len() });
    }
    if max_reaps == 0 {
        return Err(LogBootstrapError::ZeroReapLimit);
    }

    let bootstrap_token =
        CompletionToken::new(TokenClass::File, bootstrap_token_slot, bootstrap_token_generation);
    let writer_token =
        CompletionToken::new(TokenClass::File, writer_token_slot, writer_token_generation);
    if bootstrap_token == writer_token {
        return Err(LogBootstrapError::TokenCollision { token: bootstrap_token });
    }
    Ok(())
}

fn validate_data_root_name(name: &str) -> Result<(), LogBootstrapError> {
    let reason = if name.is_empty() {
        Some(DataRootNameError::Empty)
    } else if matches!(name, "." | "..") {
        Some(DataRootNameError::SpecialEntry)
    } else if name.as_bytes().contains(&0) {
        Some(DataRootNameError::ContainsNul)
    } else if name.as_bytes().contains(&b'/') {
        Some(DataRootNameError::ContainsSlash)
    } else {
        None
    };
    match reason {
        Some(reason) => {
            Err(LogBootstrapError::InvalidDataRootName { name: name.to_string(), reason })
        }
        None => Ok(()),
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DataRootNameError {
    Empty,
    SpecialEntry,
    ContainsNul,
    ContainsSlash,
}

impl fmt::Display for DataRootNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataRootNameError::Empty => write!(f, "empty"),
            DataRootNameError::SpecialEntry => write!(f, "special directory entry"),
            DataRootNameError::ContainsNul => write!(f, "contains NUL"),
            DataRootNameError::ContainsSlash => write!(f, "contains slash"),
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct ReapParams {
    wait: Wait,
    max_reaps: u32,
}

impl ReapParams {
    fn from_layout(config: LogLayoutConfig) -> ReapParams {
        ReapParams { wait: config.wait, max_reaps: config.max_reaps }
    }

    fn from_segment(config: LogBootstrapConfig) -> ReapParams {
        ReapParams { wait: config.wait, max_reaps: config.max_reaps }
    }
}

#[derive(Copy, Clone, Debug)]
struct LayoutRoot {
    fd: RawFd,
    close_after_bootstrap: bool,
}

#[derive(Copy, Clone, Debug)]
struct DirectoryPhases {
    create: LogBootstrapPhase,
    open: LogBootstrapPhase,
    sync_parent: LogBootstrapPhase,
}

impl DirectoryPhases {
    const fn shard() -> DirectoryPhases {
        DirectoryPhases {
            create: LogBootstrapPhase::CreateShardDir,
            open: LogBootstrapPhase::OpenShardDir,
            sync_parent: LogBootstrapPhase::SyncRootDir,
        }
    }

    const fn log() -> DirectoryPhases {
        DirectoryPhases {
            create: LogBootstrapPhase::CreateLogDir,
            open: LogBootstrapPhase::OpenLogDir,
            sync_parent: LogBootstrapPhase::SyncShardDir,
        }
    }

    const fn checkpoint() -> DirectoryPhases {
        DirectoryPhases {
            create: LogBootstrapPhase::CreateCheckpointDir,
            open: LogBootstrapPhase::OpenCheckpointDir,
            sync_parent: LogBootstrapPhase::SyncShardDir,
        }
    }
}

struct BootstrapIo<'a, D> {
    driver: &'a mut D,
    pool: &'a mut BufferPool,
    completions: &'a mut Vec<Completion>,
    params: ReapParams,
    token: CompletionToken,
}

impl<D> BootstrapIo<'_, D>
where
    D: BackendDriver,
{
    fn open_layout_root(&mut self, root_dir: RawFd) -> Result<LayoutRoot, LogBootstrapError> {
        if root_dir >= 0 {
            return Ok(LayoutRoot { fd: root_dir, close_after_bootstrap: false });
        }
        if root_dir != AT_FDCWD_FD {
            return Err(LogBootstrapError::InvalidRootDir { fd: root_dir });
        }
        let fd = self.open_directory(AT_FDCWD_FD, ".", LogBootstrapPhase::OpenRootDir)?;
        Ok(LayoutRoot { fd, close_after_bootstrap: true })
    }

    fn create_open_directory(
        &mut self,
        parent_dir: RawFd,
        name: &str,
        phases: DirectoryPhases,
    ) -> Result<RawFd, LogBootstrapError> {
        let created = self.create_directory(parent_dir, name, phases.create)?;
        let fd = self.open_directory(parent_dir, name, phases.open)?;
        if created {
            self.sync_directory(parent_dir, phases.sync_parent)?;
        }
        Ok(fd)
    }

    fn create_directory(
        &mut self,
        parent_dir: RawFd,
        name: &str,
        phase: LogBootstrapPhase,
    ) -> Result<bool, LogBootstrapError> {
        self.driver.push(IoOp::FileCreateDir {
            dir: parent_dir,
            name: name.to_string(),
            mode: LOG_LAYOUT_DIR_MODE,
            token: self.token,
        });
        match self.reap(phase)?.result {
            CompletionResult::FileDone => Ok(true),
            CompletionResult::Error { errno: EEXIST_ERRNO, buf: None } => Ok(false),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::CreateDir { phase, name: name.to_string(), errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn open_directory(
        &mut self,
        parent_dir: RawFd,
        name: &str,
        phase: LogBootstrapPhase,
    ) -> Result<RawFd, LogBootstrapError> {
        self.driver.push(IoOp::FileOpen {
            dir: parent_dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: self.token,
        });
        match self.reap(phase)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::OpenDir { phase, name: name.to_string(), errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn open_optional_directory(
        &mut self,
        parent_dir: RawFd,
        name: &str,
        phase: LogBootstrapPhase,
    ) -> Result<Option<RawFd>, LogBootstrapError> {
        self.driver.push(IoOp::FileOpen {
            dir: parent_dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: self.token,
        });
        match self.reap(phase)?.result {
            CompletionResult::FileOpened { fd } => Ok(Some(fd)),
            CompletionResult::Error { errno: ENOENT_ERRNO, buf: None } => Ok(None),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::OpenDir { phase, name: name.to_string(), errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn sync_directory(
        &mut self,
        fd: RawFd,
        phase: LogBootstrapPhase,
    ) -> Result<(), LogBootstrapError> {
        self.driver.push(IoOp::FileSync { fd, mode: FileSyncMode::Full, token: self.token });
        match self.reap(phase)?.result {
            CompletionResult::FileDone => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::SyncDir { phase, fd, errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn close_directory(
        &mut self,
        fd: RawFd,
        phase: LogBootstrapPhase,
    ) -> Result<(), LogBootstrapError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(phase)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::CloseDir { phase, fd, errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn close_file(&mut self, fd: RawFd, phase: LogBootstrapPhase) -> Result<(), LogBootstrapError> {
        self.driver.push(IoOp::FileClose { fd, token: self.token });
        match self.reap(phase)?.result {
            CompletionResult::FileClosed => Ok(()),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::CloseFile { phase, fd, errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase,
                result: result_name(&other),
            }),
        }
    }

    fn open_segment_writer(
        &mut self,
        config: LogBootstrapConfig,
    ) -> Result<LogWriteIo, LogBootstrapError> {
        open_preallocated_log_writer(self.driver, self.pool, config, self.completions)
    }

    fn apply_checkpoint_image(
        &mut self,
        ckpt_dir: RawFd,
        manifest: &RecoveryManifest,
        keyspace: &mut Keyspace,
    ) -> Result<AppliedCheckpointImage, LogBootstrapError> {
        let config = CheckpointImageLoadConfig::new(ckpt_dir, self.token.slot())
            .with_generation(self.token.generation())
            .with_wait(self.params.wait)
            .with_max_reaps(self.params.max_reaps);
        apply_checkpoint_image_to_keyspace(
            self.driver,
            self.pool,
            keyspace,
            manifest.checkpoint(),
            config,
            self.completions,
        )
        .map_err(LogBootstrapError::CheckpointImageApply)
    }

    fn load_recovery_manifest_from_data_root(
        &mut self,
        parent_dir: RawFd,
        root_name: &str,
        config: LogLayoutConfig,
    ) -> Result<Option<RecoveryManifest>, LogBootstrapError> {
        let Some(root_dir) = self.open_optional_directory(
            parent_dir,
            root_name,
            LogBootstrapPhase::OpenDataRootDir,
        )?
        else {
            return Ok(None);
        };
        let loaded = self.load_recovery_manifest_from_root(root_dir, config);
        let close = self.close_directory(root_dir, LogBootstrapPhase::CloseRootDir);
        match (loaded, close) {
            (Ok(manifest), Ok(())) => Ok(manifest),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn load_recovery_manifest_from_root(
        &mut self,
        root_dir: RawFd,
        config: LogLayoutConfig,
    ) -> Result<Option<RecoveryManifest>, LogBootstrapError> {
        let shard_name = format!("shard-{}", config.cell.0);
        let Some(shard_dir) =
            self.open_optional_directory(root_dir, &shard_name, LogBootstrapPhase::OpenShardDir)?
        else {
            return Ok(None);
        };
        let loaded = self.load_recovery_manifest_from_shard(shard_dir);
        let close = self.close_directory(shard_dir, LogBootstrapPhase::CloseShardDir);
        match (loaded, close) {
            (Ok(manifest), Ok(())) => Ok(manifest),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn load_recovery_manifest_from_shard(
        &mut self,
        shard_dir: RawFd,
    ) -> Result<Option<RecoveryManifest>, LogBootstrapError> {
        let Some(ckpt_dir) = self.open_optional_directory(
            shard_dir,
            CKPT_DIR_NAME,
            LogBootstrapPhase::OpenCheckpointDir,
        )?
        else {
            return Ok(None);
        };
        let loaded = self.load_recovery_manifest_from_checkpoint_dir(ckpt_dir);
        let close = self.close_directory(ckpt_dir, LogBootstrapPhase::CloseCheckpointDir);
        match (loaded, close) {
            (Ok(manifest), Ok(())) => Ok(manifest),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn load_recovery_manifest_from_checkpoint_dir(
        &mut self,
        ckpt_dir: RawFd,
    ) -> Result<Option<RecoveryManifest>, LogBootstrapError> {
        let config = RecoveryManifestLoadConfig::new(ckpt_dir, self.token.slot())
            .with_generation(self.token.generation())
            .with_wait(self.params.wait)
            .with_max_reaps(self.params.max_reaps);
        load_recovery_manifest(self.driver, self.pool, config, self.completions)
            .map_err(LogBootstrapError::RecoveryManifestLoad)
    }

    fn open_existing_segment(
        &mut self,
        log_dir: RawFd,
        segment: SegmentId,
    ) -> Result<RawFd, LogBootstrapError> {
        self.driver.push(IoOp::FileOpen {
            dir: log_dir,
            name: segment.file_name(),
            mode: FileOpenMode::ReadWrite,
            token: self.token,
        });
        match self.reap(LogBootstrapPhase::OpenRecoveredSegment)?.result {
            CompletionResult::FileOpened { fd } => Ok(fd),
            CompletionResult::Error { errno, buf: None } => {
                Err(LogBootstrapError::OpenRecoveredSegment { segment, errno })
            }
            other => Err(LogBootstrapError::UnexpectedCompletionKind {
                phase: LogBootstrapPhase::OpenRecoveredSegment,
                result: result_name(&other),
            }),
        }
    }

    fn recover_active_tail(
        &mut self,
        active_fd: RawFd,
        active: SegmentId,
    ) -> Result<RecoveredActiveTail, LogBootstrapError> {
        let chunk_bytes = u32::try_from(self.pool.buf_size()).map_err(|_| {
            LogBootstrapError::ReadBufferTooLarge { buffer_len_bytes: self.pool.buf_size() }
        })?;
        let read_config =
            SegmentReadConfig::new(chunk_bytes).map_err(LogBootstrapError::ReadConfig)?;
        let mut read = SegmentReadIo::new(
            active_fd,
            self.token.slot(),
            self.token.generation(),
            active,
            SegmentTailPolicy::ActiveTail,
            read_config,
        );
        let mut sink = ActiveTailOffsetSink::new(active);
        let mut queued = Vec::new();
        loop {
            queued.clear();
            let queued_read =
                read.queue_next(self.pool, &mut queued).map_err(LogBootstrapError::ActiveRead)?;
            if !queued_read {
                let terminal = read
                    .terminal()
                    .ok_or(LogBootstrapError::MissingActiveTailTerminal { segment: active })?;
                return Ok(RecoveredActiveTail::from_terminal(sink.offset_bytes(), terminal));
            }
            assert_eq!(queued.len(), 1, "segment reader queues one op at a time");
            self.driver.push(queued.pop().expect("one queued read"));
            let completion = self.reap(LogBootstrapPhase::ReadRecoveredActiveSegment)?;
            let done = match read.on_completion(completion, self.pool, &mut sink) {
                Ok(done) => done,
                Err(error) => {
                    return self.recover_active_tail_error(
                        active_fd,
                        active,
                        sink.offset_bytes(),
                        error,
                    );
                }
            };
            if done {
                let terminal = read
                    .terminal()
                    .ok_or(LogBootstrapError::MissingActiveTailTerminal { segment: active })?;
                return Ok(RecoveredActiveTail::from_terminal(sink.offset_bytes(), terminal));
            }
        }
    }

    fn replay_sealed_segment(
        &mut self,
        fd: RawFd,
        segment: SegmentId,
        sink: &mut KeyspaceReplaySink<'_>,
    ) -> Result<(), LogBootstrapError> {
        let chunk_bytes = u32::try_from(self.pool.buf_size()).map_err(|_| {
            LogBootstrapError::ReadBufferTooLarge { buffer_len_bytes: self.pool.buf_size() }
        })?;
        let read_config =
            SegmentReadConfig::new(chunk_bytes).map_err(LogBootstrapError::ReadConfig)?;
        let mut read = SegmentReadIo::new(
            fd,
            self.token.slot(),
            self.token.generation(),
            segment,
            SegmentTailPolicy::Sealed,
            read_config,
        );
        let mut sink = OffsetReplaySink::new(segment, sink);
        let mut queued = Vec::new();
        loop {
            queued.clear();
            let queued_read = read
                .queue_next(self.pool, &mut queued)
                .map_err(LogBootstrapError::ReplayQueueRead)?;
            if !queued_read {
                read.terminal().ok_or(LogBootstrapError::MissingActiveTailTerminal { segment })?;
                return Ok(());
            }
            assert_eq!(queued.len(), 1, "segment reader queues one op at a time");
            self.driver.push(queued.pop().expect("one queued read"));
            let completion = self.reap(LogBootstrapPhase::ReadRecoveredActiveSegment)?;
            let done = read
                .on_completion(completion, self.pool, &mut sink)
                .map_err(LogBootstrapError::ReplayRead)?;
            if done {
                read.terminal().ok_or(LogBootstrapError::MissingActiveTailTerminal { segment })?;
                return Ok(());
            }
        }
    }

    fn recover_active_tail_replaying(
        &mut self,
        active_fd: RawFd,
        active: SegmentId,
        sink: &mut KeyspaceReplaySink<'_>,
    ) -> Result<RecoveredActiveTail, LogBootstrapError> {
        let chunk_bytes = u32::try_from(self.pool.buf_size()).map_err(|_| {
            LogBootstrapError::ReadBufferTooLarge { buffer_len_bytes: self.pool.buf_size() }
        })?;
        let read_config =
            SegmentReadConfig::new(chunk_bytes).map_err(LogBootstrapError::ReadConfig)?;
        let mut read = SegmentReadIo::new(
            active_fd,
            self.token.slot(),
            self.token.generation(),
            active,
            SegmentTailPolicy::ActiveTail,
            read_config,
        );
        let mut sink = OffsetReplaySink::new(active, sink);
        let mut queued = Vec::new();
        loop {
            queued.clear();
            let queued_read = read
                .queue_next(self.pool, &mut queued)
                .map_err(LogBootstrapError::ReplayQueueRead)?;
            if !queued_read {
                let terminal = read
                    .terminal()
                    .ok_or(LogBootstrapError::MissingActiveTailTerminal { segment: active })?;
                return Ok(RecoveredActiveTail::from_terminal(sink.offset_bytes(), terminal));
            }
            assert_eq!(queued.len(), 1, "segment reader queues one op at a time");
            self.driver.push(queued.pop().expect("one queued read"));
            let completion = self.reap(LogBootstrapPhase::ReadRecoveredActiveSegment)?;
            let done = match read.on_completion(completion, self.pool, &mut sink) {
                Ok(done) => done,
                Err(error) => {
                    return self.recover_active_tail_replay_error(
                        active_fd,
                        active,
                        sink.offset_bytes(),
                        error,
                    );
                }
            };
            if done {
                let terminal = read
                    .terminal()
                    .ok_or(LogBootstrapError::MissingActiveTailTerminal { segment: active })?;
                return Ok(RecoveredActiveTail::from_terminal(sink.offset_bytes(), terminal));
            }
        }
    }

    fn recover_active_tail_replay_error(
        &mut self,
        _active_fd: RawFd,
        active: SegmentId,
        truncated_at: u32,
        error: SegmentReadIoError<KeyspaceReplayError>,
    ) -> Result<RecoveredActiveTail, LogBootstrapError> {
        let Some(disposition) = classify_active_frame_error(&error) else {
            return Err(LogBootstrapError::ReplayRead(error));
        };
        match disposition {
            ActiveFrameErrorDisposition::CandidateTorn { offset } => {
                if offset < truncated_at {
                    return Err(LogBootstrapError::LogCorruption { segment: active, offset });
                }
                Ok(RecoveredActiveTail::torn_tail(truncated_at, offset))
            }
            ActiveFrameErrorDisposition::FatalCorruption { offset } => {
                Err(LogBootstrapError::LogCorruption { segment: active, offset })
            }
        }
    }

    fn recover_active_tail_error(
        &mut self,
        _active_fd: RawFd,
        active: SegmentId,
        truncated_at: u32,
        error: SegmentReadIoError<Infallible>,
    ) -> Result<RecoveredActiveTail, LogBootstrapError> {
        let Some(disposition) = classify_active_frame_error(&error) else {
            return Err(LogBootstrapError::ActiveRead(error));
        };
        match disposition {
            ActiveFrameErrorDisposition::CandidateTorn { offset } => {
                if offset < truncated_at {
                    return Err(LogBootstrapError::LogCorruption { segment: active, offset });
                }
                Ok(RecoveredActiveTail::torn_tail(truncated_at, offset))
            }
            ActiveFrameErrorDisposition::FatalCorruption { offset } => {
                Err(LogBootstrapError::LogCorruption { segment: active, offset })
            }
        }
    }

    fn reap(&mut self, phase: LogBootstrapPhase) -> Result<Completion, LogBootstrapError> {
        reap_bootstrap_completion(
            self.driver,
            self.pool,
            self.completions,
            self.params,
            phase,
            self.token,
        )
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ActiveFrameErrorDisposition {
    CandidateTorn { offset: u32 },
    FatalCorruption { offset: u32 },
}

fn classify_active_frame_error<E>(
    error: &SegmentReadIoError<E>,
) -> Option<ActiveFrameErrorDisposition> {
    let SegmentReadIoError::Reader(SegmentReadError::Frame(error)) = error else {
        return None;
    };
    Some(classify_active_frame_error_inner(error))
}

fn classify_active_frame_error_inner(error: &SegmentFrameError) -> ActiveFrameErrorDisposition {
    match error {
        SegmentFrameError::FrameLengthTooSmall { offset, .. }
        | SegmentFrameError::FrameLengthTooLarge { offset, .. } => {
            ActiveFrameErrorDisposition::CandidateTorn { offset: *offset }
        }
        SegmentFrameError::Codec { offset, source, .. } if is_torn_tail_candidate(source) => {
            ActiveFrameErrorDisposition::CandidateTorn { offset: *offset }
        }
        SegmentFrameError::ZeroTailInSealedSegment { offset, .. }
        | SegmentFrameError::PartialFrame { offset, .. }
        | SegmentFrameError::FrameLsnMismatch { offset, .. }
        | SegmentFrameError::Codec { offset, .. } => {
            ActiveFrameErrorDisposition::FatalCorruption { offset: *offset }
        }
        SegmentFrameError::OffsetTooLarge { .. } => {
            ActiveFrameErrorDisposition::FatalCorruption { offset: u32::MAX }
        }
    }
}

fn is_torn_tail_candidate(source: &LogCodecError) -> bool {
    matches!(
        source,
        LogCodecError::BadMagic { .. }
            | LogCodecError::LengthMismatch { .. }
            | LogCodecError::CrcMismatch { .. }
    )
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ActiveTailDecision {
    Clean,
    TornTail { truncated_at: u32, observed_at: u32 },
}

#[derive(Copy, Clone, Debug)]
struct RecoveredActiveTail {
    offset_bytes: u32,
    decision: ActiveTailDecision,
}

impl RecoveredActiveTail {
    fn from_terminal(offset_bytes: u32, terminal: SegmentReadTerminal) -> RecoveredActiveTail {
        debug_assert_eq!(terminal.offset_bytes(), offset_bytes);
        let decision = match terminal {
            SegmentReadTerminal::CompleteEof { .. } => ActiveTailDecision::Clean,
            SegmentReadTerminal::ActiveZeroTail { offset, .. }
            | SegmentReadTerminal::ActivePartialFrame { offset, .. } => {
                ActiveTailDecision::TornTail { truncated_at: offset_bytes, observed_at: offset }
            }
        };
        RecoveredActiveTail { offset_bytes, decision }
    }

    fn torn_tail(offset_bytes: u32, observed_at: u32) -> RecoveredActiveTail {
        RecoveredActiveTail {
            offset_bytes,
            decision: ActiveTailDecision::TornTail { truncated_at: offset_bytes, observed_at },
        }
    }

    fn offset_bytes(self) -> u32 {
        if let ActiveTailDecision::TornTail { truncated_at, observed_at } = self.decision {
            debug_assert_eq!(truncated_at, self.offset_bytes);
            debug_assert!(observed_at >= truncated_at);
        }
        self.offset_bytes
    }
}

struct ActiveTailOffsetSink {
    segment: SegmentId,
    offset_bytes: u32,
}

impl ActiveTailOffsetSink {
    const fn new(segment: SegmentId) -> ActiveTailOffsetSink {
        ActiveTailOffsetSink { segment, offset_bytes: 0 }
    }

    const fn offset_bytes(&self) -> u32 {
        self.offset_bytes
    }
}

impl SegmentFrameSink for ActiveTailOffsetSink {
    type Error = Infallible;

    fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error> {
        debug_assert_eq!(frame.frame_start().segment(), self.segment.get());
        debug_assert_eq!(frame.frame_end().segment(), self.segment.get());
        self.offset_bytes = frame.frame_end().offset();
        Ok(())
    }
}

struct OffsetReplaySink<'a, S> {
    segment: SegmentId,
    offset_bytes: u32,
    inner: &'a mut S,
}

impl<'a, S> OffsetReplaySink<'a, S> {
    const fn new(segment: SegmentId, inner: &'a mut S) -> OffsetReplaySink<'a, S> {
        OffsetReplaySink { segment, offset_bytes: 0, inner }
    }

    const fn offset_bytes(&self) -> u32 {
        self.offset_bytes
    }
}

impl<S> SegmentFrameSink for OffsetReplaySink<'_, S>
where
    S: SegmentFrameSink,
{
    type Error = S::Error;

    fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error> {
        debug_assert_eq!(frame.frame_start().segment(), self.segment.get());
        debug_assert_eq!(frame.frame_end().segment(), self.segment.get());
        self.inner.push_frame(frame)?;
        self.offset_bytes = frame.frame_end().offset();
        Ok(())
    }
}

fn reap_bootstrap_completion<D>(
    driver: &mut D,
    pool: &mut BufferPool,
    completions: &mut Vec<Completion>,
    params: ReapParams,
    phase: LogBootstrapPhase,
    expected: CompletionToken,
) -> Result<Completion, LogBootstrapError>
where
    D: BackendDriver,
{
    for _ in 0..params.max_reaps {
        let before = completions.len();
        driver
            .submit_and_reap(pool, params.wait, completions)
            .map_err(|source| LogBootstrapError::Backend { phase, source })?;
        let produced = completions.len() - before;
        if produced == 0 {
            continue;
        }
        if produced != 1 {
            return Err(LogBootstrapError::UnexpectedCompletionCount {
                phase,
                expected: 1,
                got: produced,
            });
        }
        let completion = completions.pop().expect("one produced completion");
        if completion.token != expected {
            return Err(LogBootstrapError::UnexpectedToken {
                phase,
                expected,
                got: completion.token,
            });
        }
        return Ok(completion);
    }

    Err(LogBootstrapError::ReapLimitExceeded { phase, token: expected, attempts: params.max_reaps })
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LogBootstrapPhase {
    OpenRootDir,
    CreateDataRootDir,
    OpenDataRootDir,
    SyncDataRootParentDir,
    CreateShardDir,
    OpenShardDir,
    SyncRootDir,
    CreateLogDir,
    OpenLogDir,
    CreateCheckpointDir,
    OpenCheckpointDir,
    SyncShardDir,
    CloseCheckpointDir,
    Open,
    OpenRecoveredSegment,
    ReadRecoveredActiveSegment,
    CloseRecoveredSegment,
    Preallocate,
    SyncLogDir,
    CloseLogDir,
    CloseShardDir,
    CloseRootDir,
    CloseDataRootParentDir,
}

impl fmt::Display for LogBootstrapPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogBootstrapPhase::OpenRootDir => write!(f, "open root directory"),
            LogBootstrapPhase::CreateDataRootDir => write!(f, "create data-root directory"),
            LogBootstrapPhase::OpenDataRootDir => write!(f, "open data-root directory"),
            LogBootstrapPhase::SyncDataRootParentDir => {
                write!(f, "sync data-root parent directory")
            }
            LogBootstrapPhase::CreateShardDir => write!(f, "create shard directory"),
            LogBootstrapPhase::OpenShardDir => write!(f, "open shard directory"),
            LogBootstrapPhase::SyncRootDir => write!(f, "sync root directory"),
            LogBootstrapPhase::CreateLogDir => write!(f, "create log directory"),
            LogBootstrapPhase::OpenLogDir => write!(f, "open log directory"),
            LogBootstrapPhase::CreateCheckpointDir => write!(f, "create checkpoint directory"),
            LogBootstrapPhase::OpenCheckpointDir => write!(f, "open checkpoint directory"),
            LogBootstrapPhase::SyncShardDir => write!(f, "sync shard directory"),
            LogBootstrapPhase::CloseCheckpointDir => write!(f, "close checkpoint directory"),
            LogBootstrapPhase::Open => write!(f, "open"),
            LogBootstrapPhase::OpenRecoveredSegment => write!(f, "open recovered segment"),
            LogBootstrapPhase::ReadRecoveredActiveSegment => {
                write!(f, "read recovered active segment")
            }
            LogBootstrapPhase::CloseRecoveredSegment => write!(f, "close recovered segment"),
            LogBootstrapPhase::Preallocate => write!(f, "preallocate"),
            LogBootstrapPhase::SyncLogDir => write!(f, "sync log directory"),
            LogBootstrapPhase::CloseLogDir => write!(f, "close log directory"),
            LogBootstrapPhase::CloseShardDir => write!(f, "close shard directory"),
            LogBootstrapPhase::CloseRootDir => write!(f, "close root directory"),
            LogBootstrapPhase::CloseDataRootParentDir => {
                write!(f, "close data-root parent directory")
            }
        }
    }
}

#[derive(Debug)]
pub enum LogBootstrapError {
    ScratchNotEmpty { len: usize },
    ZeroReapLimit,
    TokenCollision { token: CompletionToken },
    InvalidRootDir { fd: RawFd },
    InvalidDataRootName { name: String, reason: DataRootNameError },
    EmptySegmentCatalog,
    MissingActiveTailTerminal { segment: SegmentId },
    ActiveTailScanOffsetTooLarge { segment: SegmentId, offset_bytes: u64 },
    LogCorruption { segment: SegmentId, offset: u32 },
    ReadBufferTooLarge { buffer_len_bytes: usize },
    ReadConfig(SegmentReadConfigError),
    ReapLimitExceeded { phase: LogBootstrapPhase, token: CompletionToken, attempts: u32 },
    Backend { phase: LogBootstrapPhase, source: io::Error },
    UnexpectedCompletionCount { phase: LogBootstrapPhase, expected: usize, got: usize },
    UnexpectedToken { phase: LogBootstrapPhase, expected: CompletionToken, got: CompletionToken },
    CreateDir { phase: LogBootstrapPhase, name: String, errno: i32 },
    OpenDir { phase: LogBootstrapPhase, name: String, errno: i32 },
    SyncDir { phase: LogBootstrapPhase, fd: RawFd, errno: i32 },
    CloseDir { phase: LogBootstrapPhase, fd: RawFd, errno: i32 },
    CloseFile { phase: LogBootstrapPhase, fd: RawFd, errno: i32 },
    Open { segment: SegmentId, errno: i32 },
    OpenRecoveredSegment { segment: SegmentId, errno: i32 },
    ActiveRead(SegmentReadIoError<Infallible>),
    ReplayQueueRead(SegmentReadIoError<Infallible>),
    ReplayRead(SegmentReadIoError<KeyspaceReplayError>),
    CheckpointImageApply(CheckpointImageApplyError),
    RecoveryManifestLoad(RecoveryManifestLoadError),
    Preallocate { segment: SegmentId, fd: RawFd, errno: i32 },
    UnexpectedCompletionKind { phase: LogBootstrapPhase, result: &'static str },
    Segment(SegmentLifecycleError),
}

impl fmt::Display for LogBootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogBootstrapError::ScratchNotEmpty { len } => {
                write!(f, "log bootstrap completion scratch is not empty ({len} completions)")
            }
            LogBootstrapError::ZeroReapLimit => {
                write!(f, "log bootstrap max_reaps must be nonzero")
            }
            LogBootstrapError::TokenCollision { token } => {
                write!(f, "log bootstrap token collides with writer token {token:?}")
            }
            LogBootstrapError::InvalidRootDir { fd } => {
                write!(f, "log layout root fd must be open or AT_FDCWD, got {fd}")
            }
            LogBootstrapError::InvalidDataRootName { name, reason } => {
                write!(f, "log data-root name {name:?} is invalid: {reason}")
            }
            LogBootstrapError::EmptySegmentCatalog => {
                write!(f, "log recovery requires at least one catalogued segment")
            }
            LogBootstrapError::MissingActiveTailTerminal { segment } => write!(
                f,
                "log recovery finished active segment {} without a terminal tail state",
                segment.file_name()
            ),
            LogBootstrapError::ActiveTailScanOffsetTooLarge { segment, offset_bytes } => write!(
                f,
                "log recovery active-tail scan for {} reached offset {offset_bytes}, above the v1 u32 LSN range",
                segment.file_name()
            ),
            LogBootstrapError::LogCorruption { segment, offset } => {
                write!(f, "log corruption in segment {} at offset {offset}", segment.file_name())
            }
            LogBootstrapError::ReadBufferTooLarge { buffer_len_bytes } => {
                write!(f, "log recovery read buffer size {buffer_len_bytes} exceeds u32::MAX")
            }
            LogBootstrapError::ReadConfig(error) => error.fmt(f),
            LogBootstrapError::ReapLimitExceeded { phase, token, attempts } => write!(
                f,
                "log bootstrap {phase} did not complete for token {token:?} after {attempts} reaps"
            ),
            LogBootstrapError::Backend { phase, source } => {
                write!(f, "log bootstrap {phase} backend failure: {source}")
            }
            LogBootstrapError::UnexpectedCompletionCount { phase, expected, got } => {
                write!(f, "log bootstrap {phase} expected {expected} completion, got {got}")
            }
            LogBootstrapError::UnexpectedToken { phase, expected, got } => {
                write!(f, "log bootstrap {phase} got token {got:?}, expected {expected:?}")
            }
            LogBootstrapError::CreateDir { phase, name, errno } => {
                write!(f, "log bootstrap {phase} {name:?} failed with errno {errno}")
            }
            LogBootstrapError::OpenDir { phase, name, errno } => {
                write!(f, "log bootstrap {phase} {name:?} failed with errno {errno}")
            }
            LogBootstrapError::SyncDir { phase, fd, errno } => {
                write!(f, "log bootstrap {phase} fd {fd} failed with errno {errno}")
            }
            LogBootstrapError::CloseDir { phase, fd, errno } => {
                write!(f, "log bootstrap {phase} fd {fd} failed with errno {errno}")
            }
            LogBootstrapError::CloseFile { phase, fd, errno } => {
                write!(f, "log bootstrap {phase} fd {fd} failed with errno {errno}")
            }
            LogBootstrapError::Open { segment, errno } => {
                write!(f, "open segment {} failed with errno {errno}", segment.file_name())
            }
            LogBootstrapError::OpenRecoveredSegment { segment, errno } => write!(
                f,
                "open recovered segment {} failed with errno {errno}",
                segment.file_name()
            ),
            LogBootstrapError::ActiveRead(error) => error.fmt(f),
            LogBootstrapError::ReplayQueueRead(error) => error.fmt(f),
            LogBootstrapError::ReplayRead(error) => error.fmt(f),
            LogBootstrapError::CheckpointImageApply(error) => error.fmt(f),
            LogBootstrapError::RecoveryManifestLoad(error) => error.fmt(f),
            LogBootstrapError::Preallocate { segment, fd, errno } => write!(
                f,
                "preallocate segment {} on fd {fd} failed with errno {errno}",
                segment.file_name()
            ),
            LogBootstrapError::UnexpectedCompletionKind { phase, result } => {
                write!(f, "log bootstrap {phase} got unexpected completion kind {result}")
            }
            LogBootstrapError::Segment(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LogBootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LogBootstrapError::Backend { source, .. } => Some(source),
            LogBootstrapError::Segment(source) => Some(source),
            LogBootstrapError::ReadConfig(source) => Some(source),
            LogBootstrapError::ActiveRead(source) => Some(source),
            LogBootstrapError::ReplayQueueRead(source) => Some(source),
            LogBootstrapError::ReplayRead(source) => Some(source),
            LogBootstrapError::CheckpointImageApply(source) => Some(source),
            LogBootstrapError::RecoveryManifestLoad(source) => Some(source),
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
    use inf_alloc::LeaseKind;
    use inf_foundation::CellId;
    use inf_foundation::fault::{FaultTrigger, FaultTriggerState};
    use inf_foundation::time::Nanos;
    use inf_log::{
        CheckpointFooter, CheckpointHeader, CheckpointId, CheckpointRef, CheckpointSectionKind,
        CheckpointSectionRef, FRAME_HEADER_LEN, Lsn, NamespaceId, RECOVERY_MANIFEST_FILE,
        RecordKind, RecordRef, RecoveryManifest, encode_batch_frame, encode_checkpoint_footer,
        encode_checkpoint_header, encode_checkpoint_section, encode_recovery_manifest,
        fault::{DIR_FSYNC_FAIL, TORN_FRAME},
    };
    use inf_runtime::{Capabilities, SubmitStats};
    use inf_store::{Keyspace, MutationEffect, SetOptions, StoreConfig};
    use std::collections::VecDeque;

    use crate::checkpoint::{
        CheckpointKeyspaceSnapshotConfig, encode_checkpoint_keyspace_snapshot_sections,
    };
    use crate::durability::DurabilityCell;

    const DIR_FD: RawFd = 41;
    const FIRST_DIR_FD: RawFd = 100;
    const OPENED_FD: RawFd = 77;
    const BOOTSTRAP_SLOT: u32 = 10;
    const WRITER_SLOT: u32 = 11;
    const BOOTSTRAP_GEN: u32 = 2;
    const WRITER_GEN: u32 = 3;
    const TEST_ENOENT: i32 = 2;
    const TEST_EIO: i32 = 5;
    const TEST_ENOSPC: i32 = 28;

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum ObservedOp {
        CreateDir { dir: RawFd, name: String, mode: u32, token: CompletionToken },
        Open { dir: RawFd, name: String, mode: FileOpenMode, token: CompletionToken },
        Preallocate { fd: RawFd, len_bytes: u64, token: CompletionToken },
        ReadAt { fd: RawFd, offset_bytes: u64, len: u32, token: CompletionToken },
        Sync { fd: RawFd, mode: FileSyncMode, token: CompletionToken },
        Close { fd: RawFd, token: CompletionToken },
    }

    #[derive(Debug)]
    struct TestDriver {
        ops: Vec<IoOp>,
        observed: Vec<ObservedOp>,
        create_dir_errnos: VecDeque<i32>,
        open_errno: Option<i32>,
        open_errnos: VecDeque<Option<i32>>,
        read_errno: Option<i32>,
        file_bytes: Vec<u8>,
        file_by_name: Vec<(String, Vec<u8>)>,
        fd_bytes: Vec<(RawFd, Vec<u8>)>,
        next_file_fd: RawFd,
        preallocate_errno: Option<i32>,
        sync_errno: Option<i32>,
        close_errno: Option<i32>,
        complete: bool,
        wrong_token: Option<CompletionToken>,
        extra_completion: bool,
        backend_error: bool,
        next_dir_fd: RawFd,
        stats: SubmitStats,
    }

    impl Default for TestDriver {
        fn default() -> TestDriver {
            TestDriver {
                ops: Vec::new(),
                observed: Vec::new(),
                create_dir_errnos: VecDeque::new(),
                open_errno: None,
                open_errnos: VecDeque::new(),
                read_errno: None,
                file_bytes: Vec::new(),
                file_by_name: Vec::new(),
                fd_bytes: Vec::new(),
                next_file_fd: OPENED_FD,
                preallocate_errno: None,
                sync_errno: None,
                close_errno: None,
                complete: true,
                wrong_token: None,
                extra_completion: false,
                backend_error: false,
                next_dir_fd: FIRST_DIR_FD,
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
                    IoOp::FileCreateDir { dir, name, mode, token } => {
                        self.observed.push(ObservedOp::CreateDir { dir, name, mode, token });
                        let result = match self.create_dir_errnos.pop_front() {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FileOpen { dir, name, mode, token } => {
                        self.observed.push(ObservedOp::Open {
                            dir,
                            name: name.clone(),
                            mode,
                            token,
                        });
                        let open_errno = self.open_errnos.pop_front().flatten().or(self.open_errno);
                        let result = match open_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None if mode == FileOpenMode::Directory => {
                                let fd = self.next_dir_fd;
                                self.next_dir_fd += 1;
                                CompletionResult::FileOpened { fd }
                            }
                            None => {
                                let fd = if self.file_by_name.is_empty() {
                                    OPENED_FD
                                } else {
                                    let fd = self.next_file_fd;
                                    self.next_file_fd += 1;
                                    fd
                                };
                                if let Some((_, bytes)) =
                                    self.file_by_name.iter().find(|(path, _)| path == &name)
                                {
                                    self.fd_bytes.push((fd, bytes.clone()));
                                }
                                CompletionResult::FileOpened { fd }
                            }
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FilePreallocate { fd, len_bytes, token } => {
                        self.observed.push(ObservedOp::Preallocate { fd, len_bytes, token });
                        let result = match self.preallocate_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FileReadAt { fd, offset_bytes, buf, len, token } => {
                        self.observed.push(ObservedOp::ReadAt { fd, offset_bytes, len, token });
                        let result = match self.read_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: Some(buf) },
                            None => {
                                let start = offset_bytes as usize;
                                let source = self
                                    .fd_bytes
                                    .iter()
                                    .find(|(open_fd, _)| *open_fd == fd)
                                    .map(|(_, bytes)| bytes.as_slice())
                                    .unwrap_or(&self.file_bytes);
                                let available = source.get(start..).unwrap_or(&[]);
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
                        let result = match self.sync_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileDone,
                        };
                        out.push(Completion { token: self.wrong_token.unwrap_or(token), result });
                    }
                    IoOp::FileClose { fd, token } => {
                        self.observed.push(ObservedOp::Close { fd, token });
                        let result = match self.close_errno {
                            Some(errno) => CompletionResult::Error { errno, buf: None },
                            None => CompletionResult::FileClosed,
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
                single_issuer: true,
                defer_taskrun: false,
                performance_tier: false,
            }
        }

        fn submit_stats(&self) -> SubmitStats {
            self.stats
        }
    }

    fn small_segment_config() -> SegmentConfig {
        SegmentConfig::new(4096, 1024, 1024).unwrap()
    }

    fn bootstrap_config() -> LogBootstrapConfig {
        LogBootstrapConfig::first_boot(DIR_FD, small_segment_config(), BOOTSTRAP_SLOT, WRITER_SLOT)
            .with_generations(BOOTSTRAP_GEN, WRITER_GEN)
            .with_max_reaps(2)
    }

    fn layout_config() -> LogLayoutConfig {
        LogLayoutConfig::first_boot(
            AT_FDCWD_FD,
            CellId(0),
            small_segment_config(),
            BOOTSTRAP_SLOT,
            WRITER_SLOT,
        )
        .with_generations(BOOTSTRAP_GEN, WRITER_GEN)
        .with_max_reaps(2)
    }

    fn data_root_config() -> LogDataRootConfig {
        LogDataRootConfig::first_boot(
            "infinity-data".to_string(),
            CellId(0),
            small_segment_config(),
            BOOTSTRAP_SLOT,
            WRITER_SLOT,
        )
        .with_generations(BOOTSTRAP_GEN, WRITER_GEN)
        .with_max_reaps(2)
    }

    fn single_segment_catalog() -> SegmentCatalog {
        scan_catalog([SegmentId::ZERO.file_name()])
    }

    fn scan_catalog<const N: usize>(names: [String; N]) -> SegmentCatalog {
        inf_log::scan_segment_names(names.iter().map(String::as_str)).unwrap()
    }

    fn record(payload: &[u8]) -> RecordRef<'_> {
        RecordRef::new(RecordKind::StringPostImage, NamespaceId::new(1), 0, payload).unwrap()
    }

    fn append_frame(out: &mut Vec<u8>, segment: SegmentId, payloads: &[&[u8]]) {
        let offset = out.len() as u32;
        let refs: Vec<_> = payloads.iter().map(|payload| record(payload)).collect();
        encode_batch_frame(Lsn::new(segment.get(), offset), &refs, out).unwrap();
    }

    fn append_mutation_frame(
        out: &mut Vec<u8>,
        segment: SegmentId,
        namespace: NamespaceId,
        effects: &[MutationEffect<'_>],
    ) {
        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        for effect in effects {
            durability.stage_mutation_effect(namespace, *effect).unwrap();
        }
        durability.drain_frame(Lsn::new(segment.get(), out.len() as u32), out).unwrap().unwrap();
    }

    fn append_checkpoint_begin_frame(
        out: &mut Vec<u8>,
        segment: SegmentId,
        checkpoint: CheckpointId,
    ) -> Lsn {
        let first_lsn = Lsn::new(segment.get(), out.len() as u32);
        let mut durability = DurabilityCell::with_capacity(64).unwrap();
        durability.stage_checkpoint_begin(checkpoint).unwrap();
        durability.drain_frame(first_lsn, out).unwrap().unwrap();
        first_lsn
    }

    fn checkpoint_image(keyspace: &Keyspace, checkpoint: CheckpointRef, now: Nanos) -> Vec<u8> {
        let mut namespaces = Vec::new();
        let mut catalog = Vec::new();
        let mut records = Vec::new();
        encode_checkpoint_keyspace_snapshot_sections(
            keyspace,
            CheckpointKeyspaceSnapshotConfig::new(now),
            &mut namespaces,
            &mut catalog,
            &mut records,
        )
        .unwrap();
        let header = CheckpointHeader::new(CellId(0), checkpoint, 2, &namespaces).unwrap();
        let sections = [
            CheckpointSectionRef::new(0, CheckpointSectionKind::NamespaceCatalog, &catalog)
                .unwrap(),
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, &records).unwrap(),
        ];
        let mut image = Vec::new();
        encode_checkpoint_header(header, &mut image).unwrap();
        let mut digest = header.digest();
        for section in sections {
            let mut bytes = Vec::new();
            let meta = encode_checkpoint_section(section, &mut bytes).unwrap();
            digest.update_section(meta);
            image.extend_from_slice(&bytes);
        }
        let footer = CheckpointFooter::new(2, digest);
        let mut footer_bytes = Vec::new();
        encode_checkpoint_footer(footer, &mut footer_bytes);
        image.extend_from_slice(&footer_bytes);
        image
    }

    fn corrupt_frame_body(bytes: &mut [u8], frame_offset: u32) {
        let body_offset = frame_offset as usize + FRAME_HEADER_LEN;
        bytes[body_offset] ^= 0x80;
    }

    fn bootstrap_token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, BOOTSTRAP_SLOT, BOOTSTRAP_GEN)
    }

    fn writer_token() -> CompletionToken {
        CompletionToken::new(TokenClass::File, WRITER_SLOT, WRITER_GEN)
    }

    #[test]
    fn data_root_bootstrap_creates_root_then_cell_layout() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(
            &driver.observed[..4],
            &[
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: ".".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD,
                    name: "infinity-data".to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
            ]
        );
        assert_eq!(
            driver.observed.last(),
            Some(&ObservedOp::Close { fd: FIRST_DIR_FD, token: bootstrap_token() })
        );
        assert_eq!(writer.token(), writer_token());
        assert_eq!(writer.active_segment(), SegmentId::ZERO);
    }

    #[test]
    fn data_root_bootstrap_syncs_parent_when_root_already_exists() {
        let mut driver =
            TestDriver { create_dir_errnos: VecDeque::from([EEXIST_ERRNO]), ..Default::default() };
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(
            &driver.observed[..4],
            &[
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: ".".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD,
                    name: "infinity-data".to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
            ]
        );
        assert_eq!(writer.token(), writer_token());
    }

    #[test]
    fn data_root_bootstrap_rejects_recursive_root_name() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();
        let mut config = data_root_config();
        config.root_name = "parent/infinity-data".to_string();

        let error = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &config,
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::InvalidDataRootName { reason: DataRootNameError::ContainsSlash, .. }
        ));
        assert!(driver.observed.is_empty());
    }

    #[test]
    fn data_root_manifest_load_returns_none_when_root_is_missing() {
        let mut driver = TestDriver {
            open_errnos: VecDeque::from([None, Some(TEST_ENOENT)]),
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(2, 128);
        let mut completions = Vec::new();

        let loaded = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded, None);
        assert!(!driver.observed.iter().any(|op| {
            matches!(
                op,
                ObservedOp::Open {
                    name,
                    ..
                } if name == "shard-0"
            )
        }));
        assert_eq!(
            driver.observed.last(),
            Some(&ObservedOp::Close { fd: FIRST_DIR_FD, token: bootstrap_token() })
        );
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn data_root_manifest_load_returns_none_when_manifest_is_missing() {
        let mut driver = TestDriver {
            open_errnos: VecDeque::from([None, None, None, None, Some(TEST_ENOENT)]),
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(2, 128);
        let mut completions = Vec::new();

        let loaded = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded, None);
        assert!(driver.observed.iter().any(|op| {
            matches!(
                op,
                ObservedOp::Open {
                    name,
                    mode: FileOpenMode::ReadOnly,
                    ..
                } if name == RECOVERY_MANIFEST_FILE
            )
        }));
        assert!(
            driver.observed.iter().any(|op| {
                matches!(op, ObservedOp::Close { fd, .. } if *fd == FIRST_DIR_FD + 3)
            })
        );
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn data_root_manifest_load_decodes_manifest_from_checkpoint_dir() {
        let checkpoint =
            CheckpointRef::new(CheckpointId::new(9).unwrap(), Lsn::new(SegmentId::ZERO.get(), 0));
        let manifest = RecoveryManifest::new(checkpoint, single_segment_catalog()).unwrap();
        let mut manifest_bytes = Vec::new();
        encode_recovery_manifest(&manifest, &mut manifest_bytes).unwrap();
        let mut driver = TestDriver {
            file_by_name: vec![(RECOVERY_MANIFEST_FILE.to_string(), manifest_bytes)],
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(4, 32);
        let mut completions = Vec::new();

        let loaded = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(loaded, Some(manifest));
        assert!(driver.observed.iter().any(|op| {
            matches!(
                op,
                ObservedOp::Open {
                    name,
                    mode: FileOpenMode::ReadOnly,
                    ..
                } if name == RECOVERY_MANIFEST_FILE
            )
        }));
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn data_root_log_directory_open_returns_log_fd_and_closes_parents() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(4, 64);
        let mut completions = Vec::new();

        let log_dir = open_log_directory_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(log_dir, FIRST_DIR_FD + 3);
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: ".".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD,
                    name: "infinity-data".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD + 1,
                    name: "shard-0".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD + 2,
                    name: LOG_DIR_NAME.to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Close { fd: FIRST_DIR_FD + 2, token: bootstrap_token() },
                ObservedOp::Close { fd: FIRST_DIR_FD + 1, token: bootstrap_token() },
                ObservedOp::Close { fd: FIRST_DIR_FD, token: bootstrap_token() },
            ]
        );
        assert!(completions.is_empty());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_reads_active_tail_offset() {
        let mut file_bytes = Vec::new();
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"one"]);
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"two", b"three"]);
        let active_offset = file_bytes.len() as u32;
        file_bytes.resize(1024, 0);
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();

        let writer = open_recovered_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), active_offset);
        assert_eq!(writer.token(), writer_token());
        assert!(driver.observed.iter().any(|op| {
            matches!(
                op,
                ObservedOp::Open {
                    name,
                    mode: FileOpenMode::ReadWrite,
                    ..
                } if name == &SegmentId::ZERO.file_name()
            )
        }));
        assert!(!driver.observed.iter().any(|op| matches!(op, ObservedOp::Preallocate { .. })));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_replays_active_tail_records() {
        let mut file_bytes = Vec::new();
        append_mutation_frame(
            &mut file_bytes,
            SegmentId::ZERO,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"k",
                value: b"value",
                expire_at_ms: Some(50),
                raw: false,
            }],
        );
        append_mutation_frame(
            &mut file_bytes,
            SegmentId::ZERO,
            NamespaceId::new(0),
            &[MutationEffect::ExpireAt { key: b"k", expire_at_ms: None }],
        );
        let active_offset = file_bytes.len() as u32;
        file_bytes.resize(1024, 0);
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();
        let mut keyspace = Keyspace::new(StoreConfig::default());

        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut keyspace,
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), active_offset);
        assert_eq!(replay.frames, 2);
        assert_eq!(replay.records, 2);
        assert_eq!(keyspace.db_mut(0).get(b"k", Nanos(100_000_000)), Some(b"value".as_slice()));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_manifest_bootstrap_applies_checkpoint_then_tail() {
        let now = Nanos(1_000_000);
        let checkpoint_id = CheckpointId::new(9).unwrap();
        let segment = SegmentId::ZERO;
        let mut log_bytes = Vec::new();
        append_mutation_frame(
            &mut log_bytes,
            segment,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"before",
                value: b"skip",
                expire_at_ms: None,
                raw: false,
            }],
        );
        let begin_lsn = append_checkpoint_begin_frame(&mut log_bytes, segment, checkpoint_id);
        append_mutation_frame(
            &mut log_bytes,
            segment,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"after",
                value: b"tail",
                expire_at_ms: None,
                raw: false,
            }],
        );
        let checkpoint = CheckpointRef::new(checkpoint_id, begin_lsn);
        let manifest = RecoveryManifest::new(checkpoint, single_segment_catalog()).unwrap();
        let mut checkpoint_keyspace = Keyspace::new(StoreConfig::default());
        checkpoint_keyspace
            .db_mut(0)
            .set(b"snapshot", b"image", SetOptions::default(), now)
            .unwrap();
        let image = checkpoint_image(&checkpoint_keyspace, checkpoint, now);
        let mut driver = TestDriver {
            file_by_name: vec![
                (checkpoint.id().file_name(), image),
                (segment.file_name(), log_bytes.clone()),
            ],
            ..TestDriver::default()
        };
        let mut pool = BufferPool::new(8, 64);
        let mut completions = Vec::new();
        let mut target = Keyspace::new(StoreConfig::default());

        let (writer, applied, replay) = open_recovered_log_writer_replaying_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &manifest,
            &mut target,
            &mut completions,
        )
        .unwrap();

        assert_eq!(applied.image().checkpoint(), checkpoint);
        assert_eq!(applied.records().records, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(writer.active_segment(), segment);
        assert_eq!(writer.active_offset_bytes(), log_bytes.len() as u32);
        assert_eq!(target.db_mut(0).get(b"snapshot", now), Some(b"image".as_slice()));
        assert_eq!(target.db_mut(0).get(b"after", now), Some(b"tail".as_slice()));
        assert_eq!(target.db_mut(0).get(b"before", now), None);
        assert_eq!(pool.reconcile(), Ok(()));
        assert!(completions.is_empty());
    }

    #[test]
    fn recovered_data_root_bootstrap_positions_writer_before_partial_tail() {
        let mut file_bytes = Vec::new();
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"one"]);
        let active_offset = file_bytes.len() as u32;
        file_bytes.extend_from_slice(b"ILG1");
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();

        let writer = open_recovered_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), active_offset);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_torn_frame_fault_truncates_bad_crc_final_frame() {
        let mut file_bytes = Vec::new();
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"one"]);
        let active_offset = file_bytes.len() as u32;
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"torn-final"]);
        let mut fault = FaultTriggerState::new(FaultTrigger::nth(1).unwrap());
        if fault.should_fire(TORN_FRAME) {
            corrupt_frame_body(&mut file_bytes, active_offset);
        }
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();

        let writer = open_recovered_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), active_offset);
        assert_eq!(fault.occurrences(), 1);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_truncates_active_tail_before_later_magic() {
        let mut file_bytes = Vec::new();
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"one"]);
        let corrupt_offset = file_bytes.len() as u32;
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"corrupt"]);
        let corrupt_end = file_bytes.len();
        file_bytes[corrupt_end - 1] ^= 0x80;
        append_frame(&mut file_bytes, SegmentId::ZERO, &[b"valid-after-corruption"]);
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();

        let writer = open_recovered_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), corrupt_offset);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_replay_drops_active_tail_after_corrupt_frame() {
        let mut file_bytes = Vec::new();
        append_mutation_frame(
            &mut file_bytes,
            SegmentId::ZERO,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"stable",
                value: b"ok",
                expire_at_ms: None,
                raw: false,
            }],
        );
        let corrupt_offset = file_bytes.len() as u32;
        append_mutation_frame(
            &mut file_bytes,
            SegmentId::ZERO,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"corrupt",
                value: b"bad",
                expire_at_ms: None,
                raw: false,
            }],
        );
        let corrupt_end = file_bytes.len();
        file_bytes[corrupt_end - 1] ^= 0x80;
        append_mutation_frame(
            &mut file_bytes,
            SegmentId::ZERO,
            NamespaceId::new(0),
            &[MutationEffect::StringPostImage {
                key: b"after",
                value: b"later",
                expire_at_ms: None,
                raw: false,
            }],
        );
        let mut driver = TestDriver { file_bytes, ..Default::default() };
        let mut pool = BufferPool::new(4, 256);
        let mut completions = Vec::new();
        let mut target = Keyspace::new(StoreConfig::default());

        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &single_segment_catalog(),
            &mut target,
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), corrupt_offset);
        assert_eq!(replay.frames, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(target.db_mut(0).get(b"stable", Nanos(0)), Some(b"ok".as_slice()));
        assert_eq!(target.db_mut(0).get(b"corrupt", Nanos(0)), None);
        assert_eq!(target.db_mut(0).get(b"after", Nanos(0)), None);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovered_data_root_bootstrap_requires_catalogued_segment() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let error = open_recovered_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &data_root_config(),
            &SegmentCatalog::empty(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, LogBootstrapError::EmptySegmentCatalog));
    }

    #[test]
    fn first_boot_layout_creates_syncs_and_closes_directories() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let writer =
            open_first_boot_log_writer(&mut driver, &mut pool, layout_config(), &mut completions)
                .unwrap();

        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: ".".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD,
                    name: "shard-0".to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD,
                    name: "shard-0".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD + 1,
                    name: LOG_DIR_NAME.to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD + 1,
                    name: LOG_DIR_NAME.to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD + 1,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD + 1,
                    name: CKPT_DIR_NAME.to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD + 1,
                    name: CKPT_DIR_NAME.to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD + 1,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
                ObservedOp::Close { fd: FIRST_DIR_FD + 3, token: bootstrap_token() },
                ObservedOp::Open {
                    dir: FIRST_DIR_FD + 2,
                    name: SegmentId::ZERO.file_name(),
                    mode: FileOpenMode::ReadWriteCreate,
                    token: bootstrap_token(),
                },
                ObservedOp::Preallocate {
                    fd: OPENED_FD,
                    len_bytes: u64::from(small_segment_config().segment_size_bytes()),
                    token: bootstrap_token(),
                },
                ObservedOp::Sync {
                    fd: FIRST_DIR_FD + 2,
                    mode: FileSyncMode::Full,
                    token: bootstrap_token(),
                },
                ObservedOp::Close { fd: FIRST_DIR_FD + 2, token: bootstrap_token() },
                ObservedOp::Close { fd: FIRST_DIR_FD + 1, token: bootstrap_token() },
                ObservedOp::Close { fd: FIRST_DIR_FD, token: bootstrap_token() },
            ]
        );
        assert_eq!(writer.token(), writer_token());
        assert_eq!(writer.active_segment(), SegmentId::ZERO);
    }

    #[test]
    fn first_boot_layout_create_dir_error_stops_before_child_open() {
        let mut driver =
            TestDriver { create_dir_errnos: VecDeque::from([TEST_ENOSPC]), ..Default::default() };
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let error =
            open_first_boot_log_writer(&mut driver, &mut pool, layout_config(), &mut completions)
                .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::CreateDir {
                phase: LogBootstrapPhase::CreateShardDir,
                errno: TEST_ENOSPC,
                ..
            }
        ));
        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: AT_FDCWD_FD,
                    name: ".".to_string(),
                    mode: FileOpenMode::Directory,
                    token: bootstrap_token(),
                },
                ObservedOp::CreateDir {
                    dir: FIRST_DIR_FD,
                    name: "shard-0".to_string(),
                    mode: LOG_LAYOUT_DIR_MODE,
                    token: bootstrap_token(),
                },
            ]
        );
    }

    #[test]
    fn first_boot_layout_dir_fsync_fault_is_typed_error() {
        let mut fault = FaultTriggerState::new(FaultTrigger::nth(1).unwrap());
        let mut driver = TestDriver::default();
        if fault.should_fire(DIR_FSYNC_FAIL) {
            driver.sync_errno = Some(TEST_EIO);
        }
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let error =
            open_first_boot_log_writer(&mut driver, &mut pool, layout_config(), &mut completions)
                .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::SyncDir {
                phase: LogBootstrapPhase::SyncRootDir,
                fd: FIRST_DIR_FD,
                errno: TEST_EIO,
            }
        ));
        assert_eq!(fault.occurrences(), 1);
        assert!(driver.observed.iter().any(|op| {
            matches!(op, ObservedOp::Sync { fd: FIRST_DIR_FD, mode: FileSyncMode::Full, .. })
        }));
    }

    #[test]
    fn opens_preallocates_and_returns_writer_on_open_fd() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(2, 1024);
        let mut completions = Vec::new();

        let mut writer = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(
            driver.observed,
            vec![
                ObservedOp::Open {
                    dir: DIR_FD,
                    name: SegmentId::ZERO.file_name(),
                    mode: FileOpenMode::ReadWriteCreate,
                    token: bootstrap_token(),
                },
                ObservedOp::Preallocate {
                    fd: OPENED_FD,
                    len_bytes: u64::from(small_segment_config().segment_size_bytes()),
                    token: bootstrap_token(),
                },
            ]
        );
        assert_eq!(writer.token(), writer_token());
        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(writer.active_offset_bytes(), 0);

        let mut durability = DurabilityCell::with_capacity(256).unwrap();
        durability
            .stage_mutation_effect(NamespaceId::new(1), MutationEffect::Delete { key: b"k" })
            .unwrap();
        let mut ops = Vec::new();
        writer.queue_frame(&mut durability, &mut pool, &mut ops).unwrap().unwrap();
        match ops[0] {
            IoOp::FileWriteAt { fd, token, .. } => {
                assert_eq!(fd, OPENED_FD);
                assert_eq!(token, writer_token());
            }
            ref other => panic!("unexpected op {other:?}"),
        }
    }

    #[test]
    fn open_error_stops_before_preallocate() {
        let mut driver = TestDriver { open_errno: Some(TEST_ENOENT), ..Default::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::Open { segment: SegmentId::ZERO, errno: TEST_ENOENT }
        ));
        assert_eq!(driver.observed.len(), 1);
    }

    #[test]
    fn preallocate_error_reports_the_open_fd() {
        let mut driver = TestDriver { preallocate_errno: Some(TEST_ENOSPC), ..Default::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::Preallocate {
                segment: SegmentId::ZERO,
                fd: OPENED_FD,
                errno: TEST_ENOSPC,
            }
        ));
        assert_eq!(driver.observed.len(), 2);
    }

    #[test]
    fn completion_scratch_must_be_empty() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 1024);
        let mut completions =
            vec![Completion { token: bootstrap_token(), result: CompletionResult::FileDone }];

        let error = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(error, LogBootstrapError::ScratchNotEmpty { len: 1 }));
    }

    #[test]
    fn reap_limit_bounds_bootstrap_waiting() {
        let mut driver = TestDriver { complete: false, ..Default::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LogBootstrapError::ReapLimitExceeded {
                phase: LogBootstrapPhase::Open,
                attempts: 2,
                ..
            }
        ));
    }

    #[test]
    fn stale_or_ambiguous_completions_are_rejected() {
        let wrong = CompletionToken::new(TokenClass::File, 123, 0);
        let mut wrong_driver = TestDriver { wrong_token: Some(wrong), ..Default::default() };
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();

        let error = open_preallocated_log_writer(
            &mut wrong_driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LogBootstrapError::UnexpectedToken {
                phase: LogBootstrapPhase::Open,
                expected,
                got,
            } if expected == bootstrap_token() && got == wrong
        ));

        let mut extra_driver = TestDriver { extra_completion: true, ..Default::default() };
        let mut extra_pool = BufferPool::new(1, 1024);
        let mut extra_completions = Vec::new();
        let error = open_preallocated_log_writer(
            &mut extra_driver,
            &mut extra_pool,
            bootstrap_config(),
            &mut extra_completions,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LogBootstrapError::UnexpectedCompletionCount {
                phase: LogBootstrapPhase::Open,
                expected: 1,
                got: 2,
            }
        ));
    }

    #[test]
    fn writer_token_must_not_reuse_the_bootstrap_token() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 1024);
        let mut completions = Vec::new();
        let config = LogBootstrapConfig::first_boot(
            DIR_FD,
            small_segment_config(),
            BOOTSTRAP_SLOT,
            BOOTSTRAP_SLOT,
        )
        .with_generations(BOOTSTRAP_GEN, BOOTSTRAP_GEN);

        let error = open_preallocated_log_writer(&mut driver, &mut pool, config, &mut completions)
            .unwrap_err();

        assert!(
            matches!(error, LogBootstrapError::TokenCollision { token } if token == bootstrap_token())
        );
    }

    #[test]
    fn preexisting_leases_are_unaffected_by_bootstrap() {
        let mut driver = TestDriver::default();
        let mut pool = BufferPool::new(1, 1024);
        let leased = pool.try_lease(LeaseKind::Send).unwrap();
        let mut completions = Vec::new();

        let writer = open_preallocated_log_writer(
            &mut driver,
            &mut pool,
            bootstrap_config(),
            &mut completions,
        )
        .unwrap();

        assert_eq!(writer.active_segment(), SegmentId::ZERO);
        assert_eq!(pool.leased(), 1);
        pool.release(leased);
        assert_eq!(pool.reconcile(), Ok(()));
    }
}
