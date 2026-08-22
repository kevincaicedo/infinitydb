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

pub mod blob;
pub mod ckpt;
mod commit;
mod effect;
pub mod fault;
pub mod flush;
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

pub use blob::{
    BLOB_CHUNK_BYTES, BLOB_HEADER_BYTES, ExtentHeaderV1, ExtentId, ExtentReader, ExtentSummary,
    ExtentWriteFailure, ExtentWriter, SealedExtent, extent_file_name, extent_frame_offset,
    inspect_extent_bytes, list_extent_ids, open_extent, parse_extent_file_name,
    parse_extent_header, probe_extent_file, unlink_extent_file,
};
pub use ckpt::{
    BlobRefEntry, CkptConfig, IckBlobRefSection, IckIdxSidecarSection, IckIdxSidecarStep,
    IckLiveSetSection, IckRefSection, IckStream, IckSummary, IdxSidecarMeta, LiveSetFileEntry,
    SectionLease, SyncIckWriter, read_ick, read_ick_hybrid,
};
pub use commit::{
    CommitStats, FrameId, FramePlan, FsyncClass, FsyncTicket, GroupCommit, REORDER_WINDOW_FRAMES,
    SyncReason,
};
pub use effect::MutationEffect;
pub use flush::{
    PendingSealView, TIER_FILE_CAPACITY_DEFAULT, TierDrive, TierFileMeta, TierFlush,
    TierFlushConfig, TierFlushError,
};
pub use frame::{
    DEFAULT_MAX_FRAME_LEN, FRAME_ALIGN, FRAME_HEADER_LEN, FRAME_HEADER_LEN_V1, FRAME_MAGIC,
    FRAME_MAGIC_V1, FRAME_MAGIC_V3, FRAME_TRAILER_LEN, FrameBuilder, FrameDecodeError, FrameIter,
    FrameLayout, FrameRecordError, FrameRef, FrameStamp, MIN_FRAME_LEN, MIN_FRAME_LEN_V1,
    RecordIter, align_up_frame, decode_frame, frame_header_len,
};
pub use fs::{SegmentIoMode, TierIoMode};
pub use lsn::{Lsn, SegmentId};
pub use manifest::{
    Manifest, ManifestDecodeError, TierFileRange, TierNsManifest, manifest_envelope, read_manifest,
    write_manifest,
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
    DEFAULT_FUA_MAX_FRAME_BYTES, DEFAULT_SEGMENT_BYTES, DeferredBegin, FrameSlot, FsyncFailed,
    LogError, MaintainReport, RotorStats, SealHandoff, SegmentConfig, SegmentRotor,
    ZERO_FILL_HEAD_START, ZERO_FILL_SLICE_BYTES, ZeroSlice, parse_segment_file_name,
    segment_file_name,
};
pub use staging::{
    DEFAULT_STAGING_BYTES, FrameLease, MAX_FRAMES_IN_FLIGHT, StagedAt, StagingConfig, StagingFull,
    StagingRing, StagingStats,
};
pub use tail::{LogCorruption, RegionEvidence, RegionScan, scan_region, scan_region_evidence};
pub use tier::{
    RoundEffect, SealOutcome, SealReason, TIER_FOOTER_BYTES, TIER_FRAME_BYTES, TIER_FRAME_DATA,
    TIER_HEADER_BYTES, TierCorruption, TierDecodeError, TierFooterV1, TierHeaderV1, TierIdentity,
    TierOpView, TierSummary, TierWriteFailure, TierWriter, inspect_tier_bytes,
    parse_tier_file_name, parse_tier_footer, parse_tier_header, probe_tier_file, tier_extract,
    tier_file_name, tier_frame_offset, tier_frame_span,
};
