//! Blob extents — out-of-line storage for values above the inline bound
//! (M4-S17, ADR-0061). One extent is exactly one `blob-NNNNNN.iblob`
//! file in the per-cell `cold/` directory: a 4 KiB header block, then
//! CRC32C frames under the ADR-0056 discipline (4092 payload + 4 CRC —
//! the `tier_frame_span`/`tier_extract` arithmetic is reused, not
//! duplicated). There is **no footer**: an extent is written once,
//! fdatasync'd once, then referenced — the referencing WAL frame's
//! group-commit ack is the extent's commit record, and the ordering
//! "extent durable before referencing ack" is structural: [`finish`]
//! (ExtentWriter::finish) is the only constructor of [`SealedExtent`],
//! and reference position requires the token (ADR-0061 D3).
//!
//! Fsync failure here is a **typed abort, not fail-stop** — the
//! ADR-0061 D3 narrower behavior §8.4 permits: at barrier time the
//! extent is referenced by nothing durable and nothing acked, the file
//! is abandoned (id quarantined, reclaimed by the orphan sweep), and
//! the fsyncgate poison — retrying the fsync and trusting a later
//! success — is structurally absent. WAL and tier-file fsync fatality
//! are untouched.

use std::io;
use std::path::{Path, PathBuf};

use inf_simd::crc32c;

use crate::fs::{SegmentFile, SegmentFs, TierIoMode};
use crate::record::NsId;
use crate::tier::{
    FrameStaging, TIER_BATCH_FRAMES, TIER_FRAME_BYTES, TIER_FRAME_DATA, TierCorruption,
    TierDecodeError, tier_frame_span,
};

/// One extent-file header block (same block discipline as tier files).
pub const BLOB_HEADER_BYTES: usize = 4096;
/// Header bytes covered by the header CRC.
pub const BLOB_HEADER_CRC_COVER: usize = 32;
/// The chunk budget: one staged batch reaches the device as one write
/// (L3), and the S17 staging bound is stated against this number —
/// a 1 GiB round trip holds peak staging ≤ 2× this (plan §4.1).
pub const BLOB_CHUNK_BYTES: usize = TIER_BATCH_FRAMES * TIER_FRAME_BYTES;

const BLOB_MAGIC: &[u8; 4] = b"IBX0";

/// Allocate-once extent identity — monotonic per cell per namespace,
/// never reused within or across boot lives (ADR-0061 D1; the S18 ABA
/// pitfall is unrepresentable because the cursor only grows).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ExtentId(pub u64);

impl core::fmt::Display for ExtentId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// `blob-NNNNNN.iblob` for extent `id` (zero-padded to at least six
/// digits, wider ids print in full — the tier-file naming rule).
#[must_use]
pub fn extent_file_name(id: ExtentId) -> String {
    format!("blob-{:06}.iblob", id.0)
}

/// Parses an extent file name back to its id; `None` for foreign names
/// (the boot listing must never guess — a name either parses or the
/// file is not an extent).
#[must_use]
pub fn parse_extent_file_name(name: &str) -> Option<ExtentId> {
    let digits = name.strip_prefix("blob-")?.strip_suffix(".iblob")?;
    if digits.len() < 6 || digits.len() > 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().map(ExtentId)
}

/// Parsed v1 extent header — the ground truth the orphan sweep and every
/// read verifies against (ADR-0061 D1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ExtentHeaderV1 {
    /// Owning cell.
    pub cell: u32,
    /// Owning namespace.
    pub ns: NsId,
    /// The allocate-once extent id (must match the file name).
    pub extent_id: ExtentId,
    /// Exact value bytes in this extent — known at create, so the file
    /// is self-describing from block 0 (no footer; see module doc).
    pub data_len: u64,
}

/// Typed write failure on an extent (ADR-0061 D3). `Fsync` is a typed
/// **abort** — the extent is abandoned and never referenced; callers
/// surface the failure to the writer as an operating error. This is the
/// audited, ADR-defined narrower behavior of the §8.4 rule (see
/// `scripts/check-fsync-fail-stop.sh`); the file is never retried.
#[derive(Debug)]
pub enum ExtentWriteFailure {
    /// A device write failed (short writes included) — the append fails
    /// whole and the extent is abandoned.
    Write(io::Error),
    /// The durability barrier failed — the extent is abandoned; nothing
    /// durable references it, so no promised byte is lost.
    Fsync(io::Error),
}

impl core::fmt::Display for ExtentWriteFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExtentWriteFailure::Write(e) => write!(f, "extent write failed: {e}"),
            ExtentWriteFailure::Fsync(e) => write!(f, "extent fsync failed (typed abort): {e}"),
        }
    }
}

impl std::error::Error for ExtentWriteFailure {}

impl ExtentWriteFailure {
    /// True when the write failed for space (M4-S21, ADR-0063 D4) — the
    /// caller surfaces `DISKFULL` instead of a generic write failure.
    /// Per-op by design: there is no blob latch (a latch would refuse
    /// the very attempts that are its only recovery probe).
    #[must_use]
    pub fn is_storage_full(&self) -> bool {
        match self {
            ExtentWriteFailure::Write(e) => {
                e.kind() == io::ErrorKind::StorageFull || e.raw_os_error() == Some(28)
            }
            ExtentWriteFailure::Fsync(_) => false,
        }
    }
}

/// Proof that an extent's bytes are durable (ADR-0061 D3): constructed
/// **only** by [`ExtentWriter::finish`] after its fdatasync returned.
/// Reference position — `TieredTable::insert_extent` and the
/// `StringSetExtent` effect — requires this token, so an unfsynced
/// extent is unrepresentable in a staged frame. The token is consumed
/// at stage time; it never outlives the mutation that references it.
#[derive(Debug)]
pub struct SealedExtent {
    ns: NsId,
    extent_id: ExtentId,
    data_len: u64,
    device_bytes: u64,
}

impl SealedExtent {
    /// Owning namespace.
    #[must_use]
    pub fn ns(&self) -> NsId {
        self.ns
    }

    /// The durable extent's id.
    #[must_use]
    pub fn extent_id(&self) -> ExtentId {
        self.extent_id
    }

    /// Exact value bytes the extent holds.
    #[must_use]
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Bytes the writer handed the device over the extent's whole life
    /// (header + frames) — the `blob_bytes` accounting input (ADR-0061
    /// D8; the writer is consumed by `finish`, so the figure rides the
    /// token).
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }
}

/// Chunked extent writer over the injected fs seam (the `TierWriter`
/// pattern — blocking writes driven from the plane, never from command
/// futures; the reactor-tier drive reuses the staged intents via
/// `IoOp::LogWrite`/`Fdatasync` plus the coverage-neutral commit-ledger
/// barrier named in ADR-0061 D3). Staging is bounded by construction:
/// one [`BLOB_CHUNK_BYTES`] batch window plus one frame-sized tail — the
/// value is never fully resident (L5; the S17 budget row asserts it).
pub struct ExtentWriter<F: SegmentFs> {
    file: F::File,
    path: PathBuf,
    ns: NsId,
    extent_id: ExtentId,
    /// Declared value length (the header wrote it; `finish` asserts it
    /// was reached exactly — a mismatch is a programmer error).
    data_len: u64,
    /// Value bytes appended so far.
    written: u64,
    /// Partial tail-frame payload (zero-padded on write).
    tail: Box<[u8]>,
    tail_fill: usize,
    /// Single-block window (header + partial tail frame).
    staging: FrameStaging,
    /// Multi-frame append batch (one device write per full window — L3).
    batch: FrameStaging,
    batch_frames: usize,
    batch_first_frame: u64,
    /// Bytes handed to the device (header + every frame write) — the
    /// `blob_bytes` accounting leg (ADR-0061 D8).
    device_bytes: u64,
}

impl<F: SegmentFs> ExtentWriter<F> {
    /// Creates `shard_dir/cold/blob-NNNNNN.iblob` in `mode` with its
    /// header block and dir-fsync barriers (the name must be durable
    /// before the content can be referenced — the segment-create rule;
    /// `dir_fsync_fail` covers the barrier).
    ///
    /// # Errors
    /// I/O failures from the fs seam — including the typed `Unsupported`
    /// refusal when `Direct` does not take effect (ADR-0054 D3) and
    /// ENOSPC surfaced typed (the S21 blob taxonomy: refusal, never
    /// corruption).
    ///
    /// # Panics
    /// Panics when `data_len` is zero — an inline-able value out of line
    /// is a caller routing bug, not an operating condition.
    pub fn create(
        fs: &F,
        shard_dir: &Path,
        id: ExtentId,
        cell: u32,
        ns: NsId,
        data_len: u64,
        mode: TierIoMode,
    ) -> io::Result<ExtentWriter<F>> {
        assert!(data_len > 0, "an extent holds at least one value byte");
        let cold_dir = shard_dir.join("cold");
        fs.create_dir_all(&cold_dir)?;
        fs.sync_dir(shard_dir)?;
        let path = cold_dir.join(extent_file_name(id));
        let mut file = fs.create_tier(&path, mode)?;
        let mut staging = FrameStaging::new(1);
        let header = staging.frame_mut();
        header.fill(0);
        header[0..4].copy_from_slice(BLOB_MAGIC);
        header[4..8].copy_from_slice(&1u32.to_le_bytes()); // format version
        header[8..12].copy_from_slice(&cell.to_le_bytes());
        header[12..16].copy_from_slice(&ns.0.to_le_bytes());
        header[16..24].copy_from_slice(&id.0.to_le_bytes());
        header[24..32].copy_from_slice(&data_len.to_le_bytes());
        let crc = crc32c(&header[..BLOB_HEADER_CRC_COVER]);
        header[32..36].copy_from_slice(&crc.to_le_bytes());
        file.write_at(0, header)?;
        if inf_foundation::fault::fire(crate::fault::DIR_FSYNC_FAIL) {
            return Err(crate::fault::injected(crate::fault::DIR_FSYNC_FAIL));
        }
        fs.sync_dir(&cold_dir)?;
        Ok(ExtentWriter {
            file,
            path,
            ns,
            extent_id: id,
            data_len,
            written: 0,
            tail: vec![0u8; TIER_FRAME_DATA].into_boxed_slice(),
            tail_fill: 0,
            staging,
            batch: FrameStaging::new(TIER_BATCH_FRAMES),
            batch_frames: 0,
            batch_first_frame: 0,
            device_bytes: BLOB_HEADER_BYTES as u64,
        })
    }

    /// Appends one chunk of the value. Chunks arrive in order and the
    /// total must land exactly on the declared `data_len` by
    /// [`finish`](Self::finish) — both asserted (caller arithmetic, not
    /// operating conditions).
    ///
    /// # Errors
    /// I/O failures from the fs seam (`blob_short_write` injects here).
    ///
    /// # Panics
    /// Panics when the chunk would exceed the declared length.
    pub fn append_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        assert!(
            self.written + bytes.len() as u64 <= self.data_len,
            "chunks must not exceed the declared extent length"
        );
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let take = bytes.len().min(TIER_FRAME_DATA - self.tail_fill);
            self.tail[self.tail_fill..self.tail_fill + take].copy_from_slice(&bytes[..take]);
            self.tail_fill += take;
            self.written += take as u64;
            bytes = &bytes[take..];
            if self.tail_fill == TIER_FRAME_DATA {
                self.stage_full_frame()?;
                self.tail.fill(0);
                self.tail_fill = 0;
            }
        }
        Ok(())
    }

    /// Stages the (full) tail frame into the append batch; the batch
    /// reaches the device as one multi-frame write when the window fills
    /// (L3 — never one syscall per frame).
    fn stage_full_frame(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.tail_fill, TIER_FRAME_DATA, "staging a full frame");
        let frame_index = (self.written - 1) / TIER_FRAME_DATA as u64;
        if self.batch_frames == 0 {
            self.batch_first_frame = frame_index;
        }
        debug_assert_eq!(
            frame_index,
            self.batch_first_frame + self.batch_frames as u64,
            "batched frames are consecutive"
        );
        let crc = crc32c(&self.tail);
        let slot = self.batch.slot_mut(self.batch_frames);
        slot[..TIER_FRAME_DATA].copy_from_slice(&self.tail);
        slot[TIER_FRAME_DATA..].copy_from_slice(&crc.to_le_bytes());
        self.batch_frames += 1;
        if self.batch_frames == TIER_BATCH_FRAMES {
            self.flush_batch()?;
        }
        Ok(())
    }

    /// One device write for the staged batch (aligned offset, aligned
    /// length, aligned memory — legal in both I/O modes).
    fn flush_batch(&mut self) -> io::Result<()> {
        if self.batch_frames == 0 {
            return Ok(());
        }
        let offset = extent_frame_offset(self.batch_first_frame);
        let count = self.batch_frames;
        self.batch_frames = 0;
        let bytes = self.batch.filled(count);
        let len = bytes.len() as u64;
        device_write(&mut self.file, offset, bytes)?;
        self.device_bytes += len;
        Ok(())
    }

    /// Final tail frame + fdatasync; the returned token is the **only**
    /// proof-of-durability object reference position accepts (ADR-0061
    /// D3 — the ordering rule made structural).
    ///
    /// # Errors
    /// [`ExtentWriteFailure`] — either class **abandons** the extent:
    /// the caller surfaces a typed error, quarantines the id (never
    /// reused, never referenced), and the orphan sweep reclaims the
    /// file. `blob_fsync_err` injects the barrier failure.
    ///
    /// # Panics
    /// Panics when fewer bytes than declared were appended.
    pub fn finish(mut self) -> Result<SealedExtent, ExtentWriteFailure> {
        assert_eq!(self.written, self.data_len, "an extent finishes at its declared length");
        self.flush_batch().map_err(ExtentWriteFailure::Write)?;
        if self.tail_fill > 0 {
            let frame_index = (self.written - 1) / TIER_FRAME_DATA as u64;
            let offset = extent_frame_offset(frame_index);
            let frame = self.staging.frame_mut();
            frame.fill(0);
            frame[..self.tail_fill].copy_from_slice(&self.tail[..self.tail_fill]);
            let crc = crc32c(&frame[..TIER_FRAME_DATA]);
            frame[TIER_FRAME_DATA..].copy_from_slice(&crc.to_le_bytes());
            device_write(&mut self.file, offset, frame).map_err(ExtentWriteFailure::Write)?;
            self.device_bytes += TIER_FRAME_BYTES as u64;
        }
        // ADR-0061 D3/D9 `blob_fsync_err`: the barrier fails — typed
        // abort; the extent is abandoned, nothing durable references it.
        if inf_foundation::fault::fire(crate::fault::BLOB_FSYNC_ERR) {
            return Err(ExtentWriteFailure::Fsync(crate::fault::injected(
                crate::fault::BLOB_FSYNC_ERR,
            )));
        }
        self.file.sync_data().map_err(ExtentWriteFailure::Fsync)?;
        Ok(SealedExtent {
            ns: self.ns,
            extent_id: self.extent_id,
            data_len: self.data_len,
            device_bytes: self.device_bytes,
        })
    }

    /// [`finish`](Self::finish) with the fdatasync deferred to the
    /// commit ledger (M4-S26 realizing ADR-0061 D3): writes the tail
    /// frame and returns the durability token **plus the open handle**.
    /// The caller MUST register the handle as a coverage-neutral ledger
    /// barrier (`GroupCommit::register_extent_barrier`) before staging
    /// the referencing record, in one synchronous block — the
    /// done-prefix rule then fences the referencing ack behind the
    /// extent's fdatasync, and a barrier failure freezes the durable
    /// plane (`on_fsync_error` — the ADR's failure half).
    ///
    /// # Errors
    /// Write failures (the fdatasync itself rides the driver).
    pub fn finish_deferred(mut self) -> Result<(SealedExtent, F::File), ExtentWriteFailure> {
        assert_eq!(self.written, self.data_len, "an extent finishes at its declared length");
        self.flush_batch().map_err(ExtentWriteFailure::Write)?;
        if self.tail_fill > 0 {
            let frame_index = (self.written - 1) / TIER_FRAME_DATA as u64;
            let offset = extent_frame_offset(frame_index);
            let frame = self.staging.frame_mut();
            frame.fill(0);
            frame[..self.tail_fill].copy_from_slice(&self.tail[..self.tail_fill]);
            let crc = crc32c(&frame[..TIER_FRAME_DATA]);
            frame[TIER_FRAME_DATA..].copy_from_slice(&crc.to_le_bytes());
            device_write(&mut self.file, offset, frame).map_err(ExtentWriteFailure::Write)?;
            self.device_bytes += TIER_FRAME_BYTES as u64;
        }
        let sealed = SealedExtent {
            ns: self.ns,
            extent_id: self.extent_id,
            data_len: self.data_len,
            device_bytes: self.device_bytes,
        };
        Ok((sealed, self.file))
    }

    /// Bytes handed to the device so far (header + frames) — the
    /// `blob_bytes` accounting input (ADR-0061 D8).
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }

    /// Resident staging capacity (batch window + block window + tail) —
    /// the L5 term the S17 staging-bound assert reads.
    #[must_use]
    pub fn staging_bytes(&self) -> usize {
        BLOB_CHUNK_BYTES + TIER_FRAME_BYTES + self.tail.len()
    }

    /// The extent file's path (tests and the plane's pin bookkeeping).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The one device-write funnel for extent bytes — every value byte
/// reaches the fd here, so `blob_short_write` covers the whole surface.
fn device_write<File: SegmentFile>(file: &mut File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    // ADR-0063 D4 `blob_write_nospace`: the device refuses the
    // allocation — no byte lands, the write fails `StorageFull`-typed;
    // the caller abandons the extent (per-op `DISKFULL`, never a latch).
    if inf_foundation::fault::fire(crate::fault::BLOB_WRITE_NOSPACE) {
        return Err(crate::fault::injected_nospace(crate::fault::BLOB_WRITE_NOSPACE));
    }
    // ADR-0061 D9 `blob_short_write`: the device accepts a prefix and
    // the write FAILS — the caller abandons the extent whole.
    if inf_foundation::fault::fire(crate::fault::BLOB_SHORT_WRITE) {
        let cut = bytes.len() / 2;
        let torn: Vec<u8> = bytes[..cut].to_vec();
        let _ = file.write_at(offset, &torn);
        return Err(crate::fault::injected(crate::fault::BLOB_SHORT_WRITE));
    }
    file.write_at(offset, bytes)
}

/// Device offset of extent frame `frame` (header block first — the tier
/// arithmetic with the blob header).
#[must_use]
pub fn extent_frame_offset(frame: u64) -> u64 {
    BLOB_HEADER_BYTES as u64 + frame * TIER_FRAME_BYTES as u64
}

/// On-disk size of a complete extent holding `data_len` value bytes:
/// the header block plus whole CRC frames. The disk-budget accounting's
/// per-extent term (M4-S19, ADR-0062 D5) — one formula, shared with the
/// writer's cumulative figure so the two can never drift.
#[must_use]
pub fn extent_device_bytes(data_len: u64) -> u64 {
    let payload = TIER_FRAME_BYTES as u64 - 4; // 4092 payload + 4 CRC
    BLOB_HEADER_BYTES as u64 + data_len.div_ceil(payload) * TIER_FRAME_BYTES as u64
}

/// Parses a v1 extent header block from untrusted bytes (ADR-0061 D9,
/// L9 — typed, bounded; `extent_decode` fuzzes exactly this).
///
/// # Errors
/// [`TierDecodeError`] naming the first check that failed (`Geometry`
/// for a zero `data_len` — no extent holds zero value bytes).
pub fn parse_extent_header(block: &[u8]) -> Result<ExtentHeaderV1, TierDecodeError> {
    if block.len() < BLOB_HEADER_CRC_COVER + 4 {
        return Err(TierDecodeError::TooShort);
    }
    if &block[0..4] != BLOB_MAGIC {
        return Err(TierDecodeError::BadMagic);
    }
    let version = u32::from_le_bytes(block[4..8].try_into().expect("4 bytes"));
    if version != 1 {
        return Err(TierDecodeError::BadVersion(version));
    }
    let stored = u32::from_le_bytes(block[32..36].try_into().expect("4 bytes"));
    if crc32c(&block[..BLOB_HEADER_CRC_COVER]) != stored {
        return Err(TierDecodeError::BadCrc);
    }
    let cell = u32::from_le_bytes(block[8..12].try_into().expect("4 bytes"));
    let ns = NsId(u32::from_le_bytes(block[12..16].try_into().expect("4 bytes")));
    let extent_id = ExtentId(u64::from_le_bytes(block[16..24].try_into().expect("8 bytes")));
    let data_len = u64::from_le_bytes(block[24..32].try_into().expect("8 bytes"));
    if data_len == 0 {
        return Err(TierDecodeError::Geometry);
    }
    Ok(ExtentHeaderV1 { cell, ns, extent_id, data_len })
}

/// Reads and verifies an extent file's header — exactly one block read
/// (the orphan sweep and the reopen path both use this; content frames
/// verify lazily, on read, exactly like tier files).
///
/// # Errors
/// I/O failures from the fs seam; `InvalidData` wrapping the typed
/// decode error when the header does not verify.
pub fn probe_extent_file<F: SegmentFs>(fs: &F, path: &Path) -> io::Result<ExtentHeaderV1> {
    let file = fs.open_read(path)?;
    let mut block = vec![0u8; BLOB_HEADER_BYTES];
    let got = file.read_at(0, &mut block)?;
    parse_extent_header(&block[..got])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{path:?}: {e}")))
}

/// What [`inspect_extent_bytes`] found — verification's view of one
/// extent-file image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExtentSummary {
    /// The verified header.
    pub header: ExtentHeaderV1,
    /// Whole frames present in the image.
    pub frames: u64,
    /// Frames the declared `data_len` requires.
    pub expected_frames: u64,
    /// Every expected frame is present and CRC-valid — the shape every
    /// *referenced* extent has by the ordering rule; anything else is an
    /// orphan the sweep reclaims without reading it.
    pub complete: bool,
    /// First expected frame whose CRC fails, if any.
    pub first_bad_frame: Option<u64>,
}

/// The untrusted-input decoder (ADR-0061 D9, L9 — `extent_decode`
/// drives exactly this): header parse, geometry, then frame-CRC
/// verification over the expected range. Iterative, bounded by the
/// input length, never panicking on any byte pattern.
///
/// # Errors
/// [`TierDecodeError`] from the header parse only — an incomplete or
/// corrupt body is a *summary* fact (`complete == false`), because an
/// unreferenced extent in any state is the same thing: garbage.
pub fn inspect_extent_bytes(bytes: &[u8]) -> Result<ExtentSummary, TierDecodeError> {
    if bytes.len() < BLOB_HEADER_BYTES {
        return Err(TierDecodeError::TooShort);
    }
    let header = parse_extent_header(&bytes[..BLOB_HEADER_BYTES])?;
    let body = &bytes[BLOB_HEADER_BYTES..];
    let frames = (body.len() / TIER_FRAME_BYTES) as u64;
    let expected_frames = header.data_len.div_ceil(TIER_FRAME_DATA as u64);
    let mut first_bad_frame = None;
    let checkable = frames.min(expected_frames);
    for frame in 0..checkable {
        let from = (frame as usize) * TIER_FRAME_BYTES;
        let block = &body[from..from + TIER_FRAME_BYTES];
        let stored = u32::from_le_bytes(block[TIER_FRAME_DATA..].try_into().expect("4 bytes"));
        if crc32c(&block[..TIER_FRAME_DATA]) != stored {
            first_bad_frame = Some(frame);
            break;
        }
    }
    let complete = frames >= expected_frames && first_bad_frame.is_none();
    Ok(ExtentSummary { header, frames, expected_frames, complete, first_bad_frame })
}

/// Chunked, CRC-verified extent reads over one open file. The window is
/// bounded by [`BLOB_CHUNK_BYTES`] plus one frame — the reader never
/// holds more, which is the read half of the S17 staging bound (L5).
pub struct ExtentReader<File: SegmentFile> {
    file: File,
    data_len: u64,
    window: Vec<u8>,
}

impl<File: SegmentFile> ExtentReader<File> {
    /// Opens a reader over an already-verified extent (`data_len` from
    /// the header probe or the record's reference — the pair assertion
    /// happens at [`read`](Self::read) bounds).
    pub fn new(file: File, data_len: u64) -> ExtentReader<File> {
        ExtentReader { file, data_len, window: Vec::new() }
    }

    /// The underlying raw fd, when the tier has real fds — the plane's
    /// async blob-read path targets it with `IoOp::TierRead` while this
    /// reader (held across the await) keeps the handle open (M4-S26).
    #[must_use]
    pub fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.file.raw_fd()
    }

    /// Reads `[offset, offset + len)` of the value into `out`
    /// (appended), verifying every covering frame's CRC. One call reads
    /// at most [`BLOB_CHUNK_BYTES`] — callers stream larger ranges in
    /// chunks (the whole point of extents).
    ///
    /// # Errors
    /// I/O failures from the fs seam; `Ok(Err(TierCorruption))` when a
    /// covering frame fails its CRC — served-data corruption, typed
    /// (§3.1: the durable copy is authoritative only when it verifies).
    ///
    /// # Panics
    /// Panics when the requested range exceeds the extent's `data_len`
    /// or the chunk budget — caller arithmetic, not operating
    /// conditions.
    pub fn read(
        &mut self,
        offset: u64,
        len: usize,
        out: &mut Vec<u8>,
    ) -> io::Result<Result<(), TierCorruption>> {
        assert!(len <= BLOB_CHUNK_BYTES, "extent reads are chunk-bounded (stream larger ranges)");
        assert!(
            offset + len as u64 <= self.data_len,
            "extent read inside the value range (offset {offset} + len {len} > {})",
            self.data_len
        );
        if len == 0 {
            return Ok(Ok(()));
        }
        let (first_frame, frame_count, skip) = tier_frame_span(offset, len);
        let window_len = frame_count as usize * TIER_FRAME_BYTES;
        // Exact growth, never amortized doubling: the window is the
        // reader's whole resident staging and the AC 2 bound is checked
        // against its capacity — a doubled allocation would double the
        // reported term for zero benefit.
        if self.window.capacity() < window_len {
            self.window = Vec::new();
            self.window.reserve_exact(window_len);
        }
        self.window.resize(window_len, 0);
        let device_at = extent_frame_offset(first_frame);
        let got = self.file.read_at(device_at, &mut self.window)?;
        if got < window_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "extent shorter than its declared frames",
            ));
        }
        // Verify-then-append per frame, straight into the caller's
        // buffer: streamed reads compose (append, never replace) and the
        // reader's resident staging is exactly one window — the ≤ 2×
        // chunk-budget bound holds with the caller's own chunk counted
        // (AC 2; `tier_extract` replaces its output, so it is not usable
        // here).
        let mut taken = 0usize;
        let mut in_frame = skip;
        for frame in 0..frame_count as usize {
            let block = &self.window[frame * TIER_FRAME_BYTES..(frame + 1) * TIER_FRAME_BYTES];
            let stored = u32::from_le_bytes(block[TIER_FRAME_DATA..].try_into().expect("4 bytes"));
            if crc32c(&block[..TIER_FRAME_DATA]) != stored {
                return Ok(Err(TierCorruption { window_frame: frame as u32 }));
            }
            let take = (len - taken).min(TIER_FRAME_DATA - in_frame);
            out.extend_from_slice(&block[in_frame..in_frame + take]);
            taken += take;
            in_frame = 0;
        }
        debug_assert_eq!(taken, len, "the window covers the requested span");
        Ok(Ok(()))
    }

    /// Resident window capacity — the L5 read-side staging term
    /// (bounded by the chunk budget plus one frame).
    #[must_use]
    pub fn staging_bytes(&self) -> usize {
        self.window.capacity()
    }
}

/// Lists extent ids present in `shard_dir/cold/` — **names only**, no
/// content reads (the boot sweep's collection pass; ADR-0061 D6). A
/// missing `cold/` directory is an empty list, not an error.
///
/// # Errors
/// I/O failures from the directory listing itself.
pub fn list_extent_ids<F: SegmentFs>(fs: &F, shard_dir: &Path) -> io::Result<Vec<ExtentId>> {
    let cold_dir = shard_dir.join("cold");
    let names = match fs.list_dir(&cold_dir) {
        Ok(names) => names,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids: Vec<ExtentId> = names.iter().filter_map(|n| parse_extent_file_name(n)).collect();
    ids.sort_unstable();
    Ok(ids)
}

/// Opens an existing extent for reading in `mode` (cold blob reads).
///
/// # Errors
/// I/O failures from the fs seam; `InvalidData` when the header does
/// not verify or names a different extent.
pub fn open_extent<F: SegmentFs>(
    fs: &F,
    shard_dir: &Path,
    id: ExtentId,
    mode: TierIoMode,
) -> io::Result<ExtentReader<F::File>> {
    let path = shard_dir.join("cold").join(extent_file_name(id));
    let header = probe_extent_file(fs, &path)?;
    if header.extent_id != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("extent {path:?} header names id {} (expected {id})", header.extent_id),
        ));
    }
    let file = fs.open_tier(&path, mode)?;
    Ok(ExtentReader::new(file, header.data_len))
}

/// Unlinks one extent file (reclaim + orphan sweep). Failure is
/// **non-fatal and counted** (`blob_unlink_fail` — the `tier_unlink_fail`
/// posture, ADR-0061 D5): the durable truth never names a reclaimable
/// extent, so a failed unlink defers disk space, never durability; the
/// caller re-offers the candidate and the boot sweep re-drives it.
///
/// An already-absent file is **success**: a death replayed from the WAL
/// tail legitimately re-offers an extent unlinked in a prior life
/// (ADR-0061 D6 — the checkpoint restored the reference, the replayed
/// death killed it again), and "already gone" is the goal state.
///
/// # Errors
/// The typed I/O failure — callers count it and retry next round.
pub fn unlink_extent_file<F: SegmentFs>(fs: &F, shard_dir: &Path, id: ExtentId) -> io::Result<()> {
    if inf_foundation::fault::fire(crate::fault::BLOB_UNLINK_FAIL) {
        return Err(crate::fault::injected(crate::fault::BLOB_UNLINK_FAIL));
    }
    let path = shard_dir.join("cold").join(extent_file_name(id));
    match fs.remove_file(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::mem::MemFs;
    use std::path::Path;

    const SHARD: &str = "/shard-0";

    fn value(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn write_extent(fs: &MemFs, id: u64, bytes: &[u8]) -> SealedExtent {
        let mut w = ExtentWriter::create(
            fs,
            Path::new(SHARD),
            ExtentId(id),
            0,
            NsId(7),
            bytes.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create");
        // Uneven chunking on purpose — the frame walk must not care.
        for chunk in bytes.chunks(1000) {
            w.append_chunk(chunk).expect("append");
        }
        w.finish().expect("finish")
    }

    #[test]
    fn extent_round_trips_across_frame_boundaries() {
        // Goal: bytes written through chunked staging read back exactly,
        // at offsets that start mid-frame and spans that cross frames.
        let fs = MemFs::default();
        let bytes = value(3 * TIER_FRAME_DATA + 123, 5);
        let sealed = write_extent(&fs, 1, &bytes);
        assert_eq!(sealed.data_len(), bytes.len() as u64);
        let mut reader =
            open_extent(&fs, Path::new(SHARD), ExtentId(1), TierIoMode::Buffered).expect("open");
        for (offset, len) in [
            (0usize, bytes.len()),
            (0, 1),
            (4091, 3),
            (TIER_FRAME_DATA, TIER_FRAME_DATA),
            (7000, 9000),
        ] {
            if len > BLOB_CHUNK_BYTES || offset + len > bytes.len() {
                continue;
            }
            let mut out = Vec::new();
            reader.read(offset as u64, len, &mut out).expect("io").expect("crc");
            assert_eq!(out, &bytes[offset..offset + len], "range {offset}+{len}");
        }
        // Streamed reads COMPOSE: many small calls into one buffer
        // reproduce the value exactly (the read contract is append —
        // regression for the extract-replaces-output bug the AC1
        // crash row surfaced).
        let mut streamed = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let take = (bytes.len() - offset).min(1000);
            reader.read(offset as u64, take, &mut streamed).expect("io").expect("crc");
            offset += take;
        }
        assert_eq!(streamed, bytes, "chunked reads append, never replace");
    }

    #[test]
    fn the_header_is_the_ground_truth_the_sweep_reads() {
        // Goal: probe + inspect agree with what the writer declared, and
        // a complete image says so.
        let fs = MemFs::default();
        let bytes = value(2 * TIER_FRAME_DATA, 9);
        write_extent(&fs, 42, &bytes);
        let path = Path::new(SHARD).join("cold").join(extent_file_name(ExtentId(42)));
        let header = probe_extent_file(&fs, &path).expect("probe");
        assert_eq!(header.extent_id, ExtentId(42));
        assert_eq!(header.ns, NsId(7));
        assert_eq!(header.data_len, bytes.len() as u64);
        let image = fs.contents(&path).expect("image");
        let summary = inspect_extent_bytes(&image).expect("inspect");
        assert!(summary.complete, "a finished extent is complete");
        assert_eq!(summary.expected_frames, 2);
        assert_eq!(summary.first_bad_frame, None);
    }

    #[test]
    fn a_torn_image_is_incomplete_never_a_panic() {
        // Goal: the decoder classifies truncated and corrupted images as
        // incomplete (orphan shapes) with typed errors only for the
        // header — never a panic on any byte pattern.
        let fs = MemFs::default();
        let bytes = value(3 * TIER_FRAME_DATA, 3);
        write_extent(&fs, 7, &bytes);
        let path = Path::new(SHARD).join("cold").join(extent_file_name(ExtentId(7)));
        let image = fs.contents(&path).expect("image");
        // Truncated mid-body: incomplete.
        let cut = BLOB_HEADER_BYTES + TIER_FRAME_BYTES + 100;
        let summary = inspect_extent_bytes(&image[..cut]).expect("header still parses");
        assert!(!summary.complete);
        // One flipped payload byte: the covering frame reports bad.
        let mut corrupt = image.clone();
        corrupt[BLOB_HEADER_BYTES + TIER_FRAME_BYTES + 10] ^= 0x40;
        let summary = inspect_extent_bytes(&corrupt).expect("header still parses");
        assert!(!summary.complete);
        assert_eq!(summary.first_bad_frame, Some(1));
        // Garbage header: typed.
        assert_eq!(inspect_extent_bytes(&[0u8; 4096]), Err(TierDecodeError::BadMagic));
    }

    #[test]
    fn names_parse_back_and_foreign_names_do_not() {
        assert_eq!(extent_file_name(ExtentId(3)), "blob-000003.iblob");
        assert_eq!(parse_extent_file_name("blob-000003.iblob"), Some(ExtentId(3)));
        assert_eq!(parse_extent_file_name("blob-1234567890.iblob"), Some(ExtentId(1_234_567_890)));
        assert_eq!(parse_extent_file_name("tier-000003.itier"), None);
        assert_eq!(parse_extent_file_name("blob-3.iblob"), None);
        assert_eq!(parse_extent_file_name("blob-00000a.iblob"), None);
    }

    #[test]
    fn listing_returns_extent_ids_only_and_tolerates_absence() {
        let fs = MemFs::default();
        assert_eq!(list_extent_ids(&fs, Path::new(SHARD)).expect("empty"), Vec::new());
        write_extent(&fs, 2, &value(10, 1));
        write_extent(&fs, 9, &value(10, 2));
        let ids = list_extent_ids(&fs, Path::new(SHARD)).expect("ids");
        assert_eq!(ids, vec![ExtentId(2), ExtentId(9)]);
    }

    #[test]
    fn unlink_reclaims_and_the_fault_defers_nonfatally() {
        let fs = MemFs::default();
        write_extent(&fs, 5, &value(10, 1));
        inf_foundation::fault::arm(
            crate::fault::BLOB_UNLINK_FAIL,
            inf_foundation::fault::FaultSpec::Nth(1),
        );
        let deferred = unlink_extent_file(&fs, Path::new(SHARD), ExtentId(5));
        assert!(deferred.is_err(), "the armed unlink fails typed");
        inf_foundation::fault::disarm_all();
        assert_eq!(list_extent_ids(&fs, Path::new(SHARD)).expect("ids"), vec![ExtentId(5)]);
        unlink_extent_file(&fs, Path::new(SHARD), ExtentId(5)).expect("retry succeeds");
        assert_eq!(list_extent_ids(&fs, Path::new(SHARD)).expect("ids"), Vec::new());
    }

    #[test]
    fn write_faults_abandon_the_extent_typed() {
        // Goal: blob_short_write and blob_fsync_err surface typed and
        // never produce a SealedExtent — an unfsynced extent stays
        // unrepresentable in reference position.
        let fs = MemFs::default();
        inf_foundation::fault::arm(
            crate::fault::BLOB_SHORT_WRITE,
            inf_foundation::fault::FaultSpec::Nth(1),
        );
        // A full batch window of frames forces a mid-append device
        // write — the funnel the fault covers.
        let big = value(TIER_BATCH_FRAMES * TIER_FRAME_DATA + 1, 4);
        let mut w = ExtentWriter::create(
            &fs,
            Path::new(SHARD),
            ExtentId(11),
            0,
            NsId(7),
            big.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create");
        let failed = w.append_chunk(&big);
        assert!(failed.is_err(), "short write fails the append whole");
        inf_foundation::fault::disarm_all();

        let bytes = value(2 * TIER_FRAME_DATA, 4);
        inf_foundation::fault::arm(
            crate::fault::BLOB_FSYNC_ERR,
            inf_foundation::fault::FaultSpec::Nth(1),
        );
        let mut w = ExtentWriter::create(
            &fs,
            Path::new(SHARD),
            ExtentId(12),
            0,
            NsId(7),
            bytes.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create");
        w.append_chunk(&bytes).expect("append");
        let aborted = w.finish();
        assert!(
            matches!(aborted, Err(ExtentWriteFailure::Fsync(_))),
            "fsync failure is a typed abort (ADR-0061 D3)"
        );
        inf_foundation::fault::disarm_all();
    }

    #[test]
    fn staging_stays_bounded_regardless_of_value_size() {
        // Goal: the writer's resident staging is a constant (window +
        // tail), independent of the declared length — the structural
        // half of the S17 ≤ 2× chunk-budget AC.
        let fs = MemFs::default();
        let w: ExtentWriter<MemFs> = ExtentWriter::create(
            &fs,
            Path::new(SHARD),
            ExtentId(20),
            0,
            NsId(7),
            1 << 30, // a declared GiB costs no more staging than a KiB
            TierIoMode::Buffered,
        )
        .expect("create");
        assert!(
            w.staging_bytes() <= 2 * BLOB_CHUNK_BYTES,
            "writer staging {} exceeds 2x chunk budget {}",
            w.staging_bytes(),
            2 * BLOB_CHUNK_BYTES
        );
    }
}
