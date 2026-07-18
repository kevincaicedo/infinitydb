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

pub mod ckpt;
mod commit;
mod effect;
pub mod fault;
mod frame;
pub mod fs;
mod lsn;
pub mod manifest;
pub mod meta;
mod reader;
mod record;
mod scan;
mod segment;
mod staging;
mod tail;
pub mod tier;

pub use ckpt::{CkptConfig, IckStream, IckSummary, SectionLease};
pub use commit::{CommitStats, FsyncClass, FsyncTicket, GroupCommit, SyncReason};
pub use effect::MutationEffect;
pub use frame::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FRAME_HEADER_LEN_V1, FRAME_MAGIC, FRAME_MAGIC_V1,
    FRAME_TRAILER_LEN, FrameBuilder, FrameDecodeError, FrameIter, FrameRecordError, FrameRef,
    FrameStamp, MIN_FRAME_LEN, MIN_FRAME_LEN_V1, RecordIter, decode_frame, frame_header_len,
};
pub use lsn::{Lsn, SegmentId};
pub use manifest::{
    Manifest, ManifestDecodeError, manifest_envelope, read_manifest, write_manifest,
};
pub use reader::{ApplyError, DEFAULT_READ_CHUNK, ReadEnd, ReadError, ReaderConfig, SegmentReader};
pub use record::{
    DOC_VERSION_MASK, DocLineage, NsId, RecordDecodeError, RecordType, RecordView, decode_record,
};
pub use scan::{
    CellDirs, ScanError, ScanOutcome, SegmentScan, create_cell_dirs, create_cell_dirs_deferred,
    scan_log_dir, scan_log_dir_from,
};
pub use segment::{
    DEFAULT_SEGMENT_BYTES, DeferredBegin, FrameSlot, FsyncFailed, LogError, MaintainReport,
    RotorStats, SealHandoff, SegmentConfig, SegmentRotor, parse_segment_file_name,
    segment_file_name,
};
pub use staging::{
    DEFAULT_STAGING_BYTES, FrameLease, StagedAt, StagingConfig, StagingFull, StagingRing,
    StagingStats,
};
pub use tail::{LogCorruption, RegionEvidence, RegionScan, scan_region, scan_region_evidence};
pub use tier::{
    TIER_FRAME_BYTES, TIER_FRAME_DATA, TIER_HEADER_BYTES, TierCorruption, TierWriter, tier_extract,
    tier_file_name, tier_frame_offset, tier_frame_span,
};
