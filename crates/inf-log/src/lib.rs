//! `inf-log` — the log spine (master plan §8, L2): per-cell record framing,
//! segment files, group commit, checkpoints, MANIFEST, and recovery.
//!
//! Milestone state (M2-E1): record format v1 + batch frames + CRC32C
//! (M2-S01), the segment lifecycle (M2-S02), the mutation-effect staging
//! ring (M2-S03), and the sequential frame read path (M2-S04) are
//! implemented. Group commit / fsync policies (S05+) follow in this
//! milestone; S05 moves the hot-path write/fsync and the reader's
//! sequential reads onto `BackendDriver`.
//!
//! Boundaries (M2 §3.3): this crate knows records, frames, files, and LSNs
//! — never sockets, RESP, or keyspace semantics. `inf-store` hands it typed
//! mutation effects; it hands back LSNs and the durability watermark. All
//! file effects it performs directly go through the injected
//! [`fs::SegmentFs`] seam so the deterministic-simulation tier can fault
//! every one of them (L7); the per-iteration hot-path write/fsync rides
//! `BackendDriver` from M2-S05.
#![forbid(unsafe_code)]

mod commit;
mod effect;
mod frame;
pub mod fs;
mod lsn;
pub mod meta;
mod reader;
mod record;
mod scan;
mod segment;
mod staging;

pub use commit::{CommitStats, FsyncClass, FsyncTicket, GroupCommit, SyncReason};
pub use effect::MutationEffect;
pub use frame::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FRAME_MAGIC, FRAME_TRAILER_LEN, FrameBuilder,
    FrameDecodeError, FrameIter, FrameRecordError, FrameRef, MIN_FRAME_LEN, RecordIter,
    decode_frame,
};
pub use lsn::{Lsn, SegmentId};
pub use reader::{ApplyError, DEFAULT_READ_CHUNK, ReadEnd, ReadError, ReaderConfig, SegmentReader};
pub use record::{NsId, RecordDecodeError, RecordType, RecordView, decode_record};
pub use scan::{CellDirs, ScanError, SegmentScan, create_cell_dirs, scan_log_dir};
pub use segment::{
    DEFAULT_SEGMENT_BYTES, DeferredBegin, FrameSlot, FsyncFailed, LogError, MaintainReport,
    RotorStats, SealHandoff, SegmentConfig, SegmentRotor, parse_segment_file_name,
    segment_file_name,
};
pub use staging::{
    DEFAULT_STAGING_BYTES, FrameLease, StagedAt, StagingConfig, StagingFull, StagingRing,
    StagingStats,
};
