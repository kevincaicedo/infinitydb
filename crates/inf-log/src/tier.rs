//! Tier-file **format v1** (M4-S11, ADR-0056; §3.2 freeze row at M4
//! exit) — the cold tier's on-disk unit.
//!
//! One file covers one contiguous logical range of one (cell, namespace)
//! address space: data byte `delta` of the range lives in frame
//! `delta / TIER_FRAME_DATA` at in-frame offset `delta % TIER_FRAME_DATA`
//! — pure arithmetic, no per-record directory (the §3.2 mapping rule).
//! On disk the file is a 4 KiB header block (identity + CRC), then 4 KiB
//! frames (`TIER_FRAME_DATA` payload bytes + a CRC32C trailer each), so
//! every read is naturally 4 KiB-aligned; a **sealed** file ends with a
//! 4 KiB footer block (ADR-0056 D1: footer-last is the seal's on-disk
//! commit record — `{data_len, seal reason}`, separately CRC'd). The
//! partial tail frame is written zero-padded and rewritten in place as
//! later flushes extend it; frames below the last `sync` are covered by
//! fdatasync. A record never spans files; sealing cuts only at record
//! boundaries (the flush pipeline's contract, [`crate::flush`]).
//!
//! Recovery reopens an unsealed file through
//! [`TierWriter::open_existing`] with the **manifested** durable length
//! as the sole authority (ADR-0056 D5): bytes beyond it are dead-life
//! garbage, truncated before any new flush — never trusted, never
//! "recovered". [`inspect_tier_bytes`] is the typed, iterative,
//! bounded decoder for untrusted disk bytes (`fuzz_tierfile_decode`
//! drives it — L9). fsync failure anywhere on this path is
//! fatal-by-default (§8.4; ADR-0056 D4) — surfaced typed via
//! [`TierWriteFailure::Fsync`], never retried.

use std::io;
use std::path::{Path, PathBuf};

use inf_foundation::LogicalAddr;
use inf_simd::crc32c;

use crate::fs::{SegmentFile, SegmentFs, TierIoMode};
use crate::record::NsId;

/// On-disk frame size — one aligned read unit.
pub const TIER_FRAME_BYTES: usize = 4096;
/// Payload bytes per frame (the rest is the CRC32C trailer).
pub const TIER_FRAME_DATA: usize = TIER_FRAME_BYTES - 4;
/// Header block size (magic + identity, zero-padded to one frame).
pub const TIER_HEADER_BYTES: usize = 4096;
/// Footer block size (sealed files only — ADR-0056 D1).
pub const TIER_FOOTER_BYTES: usize = 4096;
const TIER_MAGIC: &[u8; 4] = b"ITF0";
const TIER_FOOTER_MAGIC: &[u8; 4] = b"ITFS";
/// Header bytes covered by the header CRC (fields 0..32, CRC at 32..36).
const TIER_HEADER_CRC_COVER: usize = 32;
/// Footer bytes covered by the footer CRC (fields 0..24, CRC at 24..28).
const TIER_FOOTER_CRC_COVER: usize = 24;

/// Why a tier file sealed (ADR-0056 D1) — stored in the footer, echoed
/// in the flush catalog for the MANIFEST (S12).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SealReason {
    /// The next flush range would overflow the file-capacity target.
    Capacity = 1,
    /// An ADR-0052 D2 ring-top sealed-dead interval follows this file.
    RingTopGap = 2,
    /// Orderly close (shutdown, tests).
    Shutdown = 3,
    /// Sealed by recovery at the manifested watermark (ADR-0056 D5 —
    /// recovery never resumes appends into recovered frames).
    Recovered = 4,
    /// Barrier seal under tail-allocation backpressure (ADR-0056 D8):
    /// a stalled writer waits on `flushed` progress, the pipeline is
    /// dry, and only the partial-frame holdback remains — sealing makes
    /// it claimable and keeps the wake chain live.
    Stall = 5,
}

impl SealReason {
    fn from_u8(raw: u8) -> Option<SealReason> {
        match raw {
            1 => Some(SealReason::Capacity),
            2 => Some(SealReason::RingTopGap),
            3 => Some(SealReason::Shutdown),
            4 => Some(SealReason::Recovered),
            5 => Some(SealReason::Stall),
            _ => None,
        }
    }
}

/// A write-path failure, split so callers can honor §8.4: `Fsync` is
/// fatal-by-default (the flushed watermark freezes — ADR-0056 D4);
/// `Write` is a typed I/O failure of the append itself.
#[derive(Debug)]
pub enum TierWriteFailure {
    /// A device write failed — the append never happened.
    Write(io::Error),
    /// An fdatasync-class barrier failed — non-recoverable by contract.
    Fsync(io::Error),
}

impl core::fmt::Display for TierWriteFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TierWriteFailure::Write(e) => write!(f, "tier write failed: {e}"),
            TierWriteFailure::Fsync(e) => {
                write!(f, "FATAL: tier fsync failed — cell must stop: {e}")
            }
        }
    }
}

/// `tier-NNNNNN.itier` (§4 layout).
#[must_use]
pub fn tier_file_name(id: u32) -> String {
    format!("tier-{id:06}.itier")
}

/// Parses `tier-NNNNNN.itier` → id (boot scan of the cold dir; foreign
/// names return `None` for the caller's GC policy).
#[must_use]
pub fn parse_tier_file_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("tier-")?.strip_suffix(".itier")?;
    if digits.len() < 6 || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Boot-time seal probe (M4-S12, ADR-0057 D6): header identity plus the
/// footer verdict from exactly two block reads. Sealed files verify
/// their frame CRCs **lazily, on read** — an eager pass would read the
/// whole cold tier at boot, the fat-checkpoint pathology the hybrid
/// exists to kill. `Some(footer)` = sealed (footer CRC-valid, geometry
/// agrees with the file length); `None` = unsealed or a torn seal —
/// recovery reseals at the manifested watermark either way
/// ([`TierWriter::recover_seal_existing`], safe because the manifested
/// prefix is the only durable claim).
///
/// # Errors
/// I/O failures, or a header that does not parse (`InvalidData` — the
/// manifest named this file, so damage is fail-stop, §8.4).
pub fn probe_tier_file<F: SegmentFs>(
    fs: &F,
    path: &Path,
) -> io::Result<(TierHeaderV1, Option<TierFooterV1>)> {
    let file = fs.open_read(path)?;
    let mut block = vec![0u8; TIER_HEADER_BYTES];
    read_block(&file, 0, &mut block)?;
    let header =
        parse_tier_header(&block).map_err(|e| invalid(path, format!("tier header: {e}")))?;
    let size = file.file_size()?;
    let mut footer = None;
    if size >= (TIER_HEADER_BYTES + TIER_FOOTER_BYTES) as u64
        && (size - TIER_HEADER_BYTES as u64).is_multiple_of(TIER_FRAME_BYTES as u64)
    {
        let mut fblock = vec![0u8; TIER_FOOTER_BYTES];
        read_block(&file, size - TIER_FOOTER_BYTES as u64, &mut fblock)?;
        if let Ok(parsed) = parse_tier_footer(&fblock) {
            let frames = parsed.data_len.div_ceil(TIER_FRAME_DATA as u64);
            let expect = TIER_HEADER_BYTES as u64
                + frames * TIER_FRAME_BYTES as u64
                + TIER_FOOTER_BYTES as u64;
            if expect == size {
                footer = Some(parsed);
            }
        }
    }
    Ok((header, footer))
}

/// The frame window covering `len` data bytes at range-relative offset
/// `delta`: (first frame index, frame count, skip inside the first
/// frame's payload).
#[must_use]
pub fn tier_frame_span(delta: u64, len: usize) -> (u64, u32, usize) {
    assert!(len > 0, "empty read");
    let data = TIER_FRAME_DATA as u64;
    let first = delta / data;
    let last = (delta + len as u64 - 1) / data;
    let count = u32::try_from(last - first + 1).expect("cold read spans a sane frame count");
    (first, count, (delta % data) as usize)
}

/// Disk byte offset of frame `frame` (the read offset for the window).
#[must_use]
pub fn tier_frame_offset(frame: u64) -> u64 {
    TIER_HEADER_BYTES as u64 + frame * TIER_FRAME_BYTES as u64
}

/// A CRC-failed frame in a cold read — served-data corruption, typed
/// (an operating condition of the storage, never a panic: the §3.1
/// durable copy is authoritative only when it verifies).
#[derive(Debug, PartialEq, Eq)]
pub struct TierCorruption {
    /// Frame index within the read window.
    pub window_frame: u32,
}

/// Verifies every frame CRC in a read window and copies `len` record
/// bytes (starting `skip` into the first frame's payload) into `out`.
/// `window` is raw disk frames as read (a multiple of
/// [`TIER_FRAME_BYTES`]).
///
/// # Errors
/// [`TierCorruption`] naming the first frame whose CRC fails.
///
/// # Panics
/// Panics when the window is too small for `skip + len` — the caller
/// computed the span, so a mismatch is a programmer error.
pub fn tier_extract(
    window: &[u8],
    skip: usize,
    len: usize,
    out: &mut Vec<u8>,
) -> Result<(), TierCorruption> {
    assert_eq!(window.len() % TIER_FRAME_BYTES, 0, "window is whole frames");
    let frames = window.len() / TIER_FRAME_BYTES;
    assert!(skip + len <= frames * TIER_FRAME_DATA, "window too small for the record");
    out.clear();
    out.reserve(len);
    let mut remaining = len;
    let mut skip = skip;
    for frame in 0..frames {
        let at = frame * TIER_FRAME_BYTES;
        let payload = &window[at..at + TIER_FRAME_DATA];
        let stored = u32::from_le_bytes(
            window[at + TIER_FRAME_DATA..at + TIER_FRAME_BYTES].try_into().expect("4 bytes"),
        );
        if crc32c(payload) != stored {
            return Err(TierCorruption { window_frame: frame as u32 });
        }
        let take = remaining.min(TIER_FRAME_DATA - skip);
        out.extend_from_slice(&payload[skip..skip + take]);
        remaining -= take;
        skip = 0;
        if remaining == 0 {
            break;
        }
    }
    assert_eq!(remaining, 0, "window covered the record");
    Ok(())
}

/// Frames staged per device write (M4-S11, L3): full frames batch into
/// one aligned window and reach the device as single multi-frame writes
/// — 4 KiB-per-syscall flushing is the shape the S11 bandwidth bench
/// exists to forbid (a frame-granular pipeline measured ~0.25× the
/// device's sequential ceiling).
pub(crate) const TIER_BATCH_FRAMES: usize = 256; // 1 MiB per device write

/// A 4 KiB-aligned staging window over a safe over-allocation (ADR-0054
/// D2): `O_DIRECT` requires the *source buffer* aligned, and both modes
/// share the one staged write path so alignment holds by construction,
/// never by mode review. No unsafe — the aligned view is `align_offset`
/// arithmetic over a plain box. `frames` = window capacity in frames
/// (1 for header/footer blocks, [`TIER_BATCH_FRAMES`] for the append
/// batch).
pub(crate) struct FrameStaging {
    raw: Box<[u8]>,
    at: usize,
    frames: usize,
}

impl FrameStaging {
    pub(crate) fn new(frames: usize) -> FrameStaging {
        let raw = vec![0u8; TIER_FRAME_BYTES * (frames + 1)].into_boxed_slice();
        let at = raw.as_ptr().align_offset(TIER_FRAME_BYTES);
        assert!(at < TIER_FRAME_BYTES, "an aligned window fits the over-allocation");
        FrameStaging { raw, at, frames }
    }

    /// The aligned single-frame window (zeroing is the caller's per-write
    /// job).
    pub(crate) fn frame_mut(&mut self) -> &mut [u8] {
        let frame = &mut self.raw[self.at..self.at + TIER_FRAME_BYTES];
        debug_assert_eq!(frame.as_ptr() as usize % TIER_FRAME_BYTES, 0, "staging is aligned");
        frame
    }

    /// The aligned window for frame slot `index` (batch fill).
    pub(crate) fn slot_mut(&mut self, index: usize) -> &mut [u8] {
        debug_assert!(index < self.frames, "slot inside the window");
        let from = self.at + index * TIER_FRAME_BYTES;
        &mut self.raw[from..from + TIER_FRAME_BYTES]
    }

    /// The aligned prefix of `count` filled slots (the device write).
    pub(crate) fn filled(&self, count: usize) -> &[u8] {
        debug_assert!(count <= self.frames, "count inside the window");
        &self.raw[self.at..self.at + count * TIER_FRAME_BYTES]
    }
}

/// Tier-file writer over the injected fs seam (the `SyncIckWriter`
/// pattern — blocking writes, driven from flush slices and tests, never
/// from command futures; the reactor-tier drive reuses the staged
/// `{fd, offset, aligned bytes}` intents via `IoOp::LogWrite` — ADR-0056
/// D3). The I/O mode (ADR-0054) is fixed at creation; every device write
/// goes through the aligned staging window in both modes.
pub struct TierWriter<F: SegmentFs> {
    file: F::File,
    path: PathBuf,
    base: LogicalAddr,
    mode: TierIoMode,
    /// Data bytes appended (range-relative length).
    data_len: u64,
    /// Partial tail-frame payload (zero-padded on write).
    tail: Box<[u8]>,
    tail_fill: usize,
    /// Data bytes covered by the last `sync` (fdatasync barrier).
    durable_len: u64,
    /// Single-block window (header, footer, partial tail frame, reads).
    staging: FrameStaging,
    /// Multi-frame append batch (one device write per full window — L3).
    batch: FrameStaging,
    batch_frames: usize,
    batch_first_frame: u64,
    /// Bytes this writer handed the device (M4-S13): the header block,
    /// every frame write — **partial-tail-frame rewrites included, they
    /// are real amplification** — and the footer block. This is what the
    /// block layer counts, which is what makes the S13 iostat
    /// reconciliation and the S16 write-amp numerator honest.
    device_bytes: u64,
}

/// What a [`TierWriter::seal`] produced (M4-S13: the third field is the
/// pipeline's device-byte roll-up; the first two are the catalog entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealOutcome {
    /// Exact data bytes = the file's logical range length.
    pub data_len: u64,
    /// The sealed file's path.
    pub path: PathBuf,
    /// Bytes this file handed the device over its whole life.
    pub device_bytes: u64,
}

/// Identity fields a reopen must match (ADR-0056 D5) — a wrong file at
/// the manifested path is corruption, typed, never overwritten silently.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TierIdentity {
    /// Owning cell.
    pub cell: u32,
    /// Owning namespace.
    pub ns: NsId,
    /// First logical address of the file's range.
    pub base: LogicalAddr,
}

impl<F: SegmentFs> TierWriter<F> {
    /// Creates `shard_dir/cold/tier-NNNNNN.itier` in `mode` with its
    /// header block and dir-fsync barriers (a file must exist durably
    /// before anything refers to it — the segment-create rule).
    ///
    /// # Errors
    /// I/O failures from the fs seam — including the typed `Unsupported`
    /// refusal when `Direct` does not take effect (ADR-0054 D3).
    pub fn create(
        fs: &F,
        shard_dir: &Path,
        id: u32,
        cell: u32,
        ns: NsId,
        base: LogicalAddr,
        mode: TierIoMode,
    ) -> io::Result<TierWriter<F>> {
        TierWriter::create_with_capacity(fs, shard_dir, id, cell, ns, base, mode, 0)
    }

    /// [`create`](Self::create) with the flush pipeline's file-capacity
    /// target recorded in the header (informational — ADR-0056 D1; 0 =
    /// unstated).
    ///
    /// # Errors
    /// I/O failures from the fs seam (see [`create`](Self::create)).
    #[allow(clippy::too_many_arguments)] // creation names the full identity once
    pub fn create_with_capacity(
        fs: &F,
        shard_dir: &Path,
        id: u32,
        cell: u32,
        ns: NsId,
        base: LogicalAddr,
        mode: TierIoMode,
        capacity_hint: u64,
    ) -> io::Result<TierWriter<F>> {
        let cold_dir = shard_dir.join("cold");
        fs.create_dir_all(&cold_dir)?;
        fs.sync_dir(shard_dir)?;
        let path = cold_dir.join(tier_file_name(id));
        let mut file = fs.create_tier(&path, mode)?;
        let mut staging = FrameStaging::new(1);
        let header = staging.frame_mut();
        header.fill(0);
        header[0..4].copy_from_slice(TIER_MAGIC);
        header[4..8].copy_from_slice(&1u32.to_le_bytes()); // format version
        header[8..12].copy_from_slice(&cell.to_le_bytes());
        header[12..16].copy_from_slice(&ns.0.to_le_bytes());
        header[16..24].copy_from_slice(&base.to_raw().to_le_bytes());
        header[24..32].copy_from_slice(&capacity_hint.to_le_bytes());
        let crc = crc32c(&header[..TIER_HEADER_CRC_COVER]);
        header[32..36].copy_from_slice(&crc.to_le_bytes());
        file.write_at(0, header)?;
        fs.sync_dir(&cold_dir)?;
        Ok(TierWriter {
            file,
            path,
            base,
            mode,
            data_len: 0,
            tail: vec![0u8; TIER_FRAME_DATA].into_boxed_slice(),
            tail_fill: 0,
            durable_len: 0,
            staging,
            batch: FrameStaging::new(TIER_BATCH_FRAMES),
            batch_frames: 0,
            batch_first_frame: 0,
            // The header block is already on the device (written above).
            device_bytes: TIER_HEADER_BYTES as u64,
        })
    }

    /// Recovers an **unsealed** tier file to the manifested durable
    /// length and seals it there (ADR-0056 D5 — the recovery pre-flush
    /// rule): verifies the header against `expect`, **truncates** to
    /// exactly `header + ceil(durable_len / TIER_FRAME_DATA)` frames
    /// (torn tails, un-manifested bytes, and stale footers beyond are
    /// dead-life garbage — dropped, never trusted), CRC-verifies every
    /// retained frame, writes a [`SealReason::Recovered`] footer at
    /// `durable_len`, and fdatasyncs. The next flush starts a **new**
    /// file at the adjacent base — recovery never resumes appends into
    /// recovered frames, so no claimed byte is ever rewritten in place
    /// (the torn-rewrite hazard v1 exists to remove).
    ///
    /// # Errors
    /// I/O failures from the fs seam; `InvalidData` when the header does
    /// not verify or match `expect`, or when a frame at or below the
    /// manifested watermark fails its CRC (served-data corruption —
    /// §3.1: the durable copy is authoritative only when it verifies).
    /// fsync-class failures are fatal per §8.4 (ADR-0056 D4).
    pub fn recover_seal_existing(
        fs: &F,
        shard_dir: &Path,
        id: u32,
        expect: TierIdentity,
        durable_len: u64,
        mode: TierIoMode,
    ) -> io::Result<PathBuf> {
        let path = shard_dir.join("cold").join(tier_file_name(id));
        let mut file = fs.open_tier(&path, mode)?;
        let mut staging = FrameStaging::new(1);
        read_block(&file, 0, staging.frame_mut())?;
        let header = parse_tier_header(staging.frame_mut())
            .map_err(|e| invalid(&path, format!("tier header: {e}")))?;
        if header.identity != expect {
            return Err(invalid(
                &path,
                format!(
                    "tier identity mismatch: {:?} on disk, {expect:?} manifested",
                    header.identity
                ),
            ));
        }
        let frames = durable_len.div_ceil(TIER_FRAME_DATA as u64);
        let cut = TIER_HEADER_BYTES as u64 + frames * TIER_FRAME_BYTES as u64;
        file.truncate(cut)?;
        // Every retained frame must verify — the §3.1 durable copy is
        // authoritative only when it does (bounded loop: `frames`).
        for frame_index in 0..frames {
            read_block(&file, tier_frame_offset(frame_index), staging.frame_mut())?;
            let frame = staging.frame_mut();
            let stored = u32::from_le_bytes(
                frame[TIER_FRAME_DATA..TIER_FRAME_BYTES].try_into().expect("4 bytes"),
            );
            if crc32c(&frame[..TIER_FRAME_DATA]) != stored {
                return Err(invalid(
                    &path,
                    format!("frame {frame_index} fails CRC below the manifested watermark"),
                ));
            }
        }
        let footer = staging.frame_mut();
        encode_tier_footer(footer, durable_len, SealReason::Recovered);
        file.write_at(cut, footer)?;
        // One barrier covers the truncation, the footer, and the final
        // state — durable before any new flush references the range.
        file.sync_data()?;
        Ok(path)
    }

    /// First logical address of this file's range.
    #[must_use]
    pub fn base(&self) -> LogicalAddr {
        self.base
    }

    /// Data bytes appended so far (`base + data_len` = next address).
    #[must_use]
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Data bytes covered by fdatasync — the `flushed`-watermark input:
    /// the caller advances `flushed` to at most `base + durable_len`.
    #[must_use]
    pub fn durable_len(&self) -> u64 {
        self.durable_len
    }

    /// Data bytes the flush may **claim** durable while the file is
    /// unsealed (ADR-0056 D5): the last *full, final* frame boundary at
    /// or below [`durable_len`](Self::durable_len). The partial tail
    /// frame is rewritten in place as appends extend it — a torn rewrite
    /// after a crash may destroy its bytes, so they are claimable only
    /// once the file seals (the footer ends all rewrites). `flushed`
    /// never advances into a rewritable frame through this bound.
    #[must_use]
    pub fn confirmable_len(&self) -> u64 {
        (self.durable_len / TIER_FRAME_DATA as u64) * TIER_FRAME_DATA as u64
    }

    /// Backend fd for cold-read ops (`None` on fd-less test tiers).
    #[must_use]
    pub fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        self.file.raw_fd()
    }

    /// The file's path (test observability).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The I/O mode this file was created in (report/attribution input —
    /// ADR-0054 D4 rows disclose the mode per leg).
    #[must_use]
    pub fn mode(&self) -> TierIoMode {
        self.mode
    }

    /// Bytes handed to the device so far (M4-S13 write accounting):
    /// header block + every frame write (rewrites of the partial tail
    /// frame counted each time — the amplification is real) + the footer
    /// block once sealed. Always ≥ [`data_len`](Self::data_len).
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }

    /// Appends record bytes at `addr`. The steel thread flushes one
    /// contiguous range into one file, so appends must be contiguous —
    /// S11's early-seal/gap machinery replaces this assert.
    ///
    /// **Atomic per call** (M4-S21, ADR-0063 D4): a device-write failure
    /// mid-range rewinds the writer to the range's start — a
    /// record-aligned resume point — so the drive loop's retry re-pulls
    /// exactly this range. Without the rewind, a failed batch leaves the
    /// cursor mid-record past frames that never reached the device: the
    /// resume misaligns and the file grows a hole. The snapshot is
    /// counters plus one ≤ 4 KiB tail copy — noise against the ≥ 1 MiB
    /// device write it guards.
    ///
    /// # Errors
    /// I/O failures from the fs seam.
    ///
    /// # Panics
    /// Panics when `addr` is not the write cursor (`base + data_len`).
    pub fn append(&mut self, addr: LogicalAddr, bytes: &[u8]) -> io::Result<()> {
        assert_eq!(
            addr.to_raw(),
            self.base.to_raw() + self.data_len,
            "tier appends are contiguous (gaps live between files — crate::flush)"
        );
        let data_len0 = self.data_len;
        let tail_fill0 = self.tail_fill;
        let batch_frames0 = self.batch_frames;
        let device_bytes0 = self.device_bytes;
        let mut tail0 = [0u8; TIER_FRAME_DATA];
        tail0[..tail_fill0].copy_from_slice(&self.tail[..tail_fill0]);
        let mut bytes = bytes;
        let result = loop {
            if bytes.is_empty() {
                break Ok(());
            }
            let take = bytes.len().min(TIER_FRAME_DATA - self.tail_fill);
            self.tail[self.tail_fill..self.tail_fill + take].copy_from_slice(&bytes[..take]);
            self.tail_fill += take;
            self.data_len += take as u64;
            bytes = &bytes[take..];
            if self.tail_fill == TIER_FRAME_DATA {
                if let Err(e) = self.stage_full_frame() {
                    break Err(e);
                }
                self.tail.fill(0);
                self.tail_fill = 0;
            }
        };
        if result.is_err() {
            // Rewind to the range start. Frames this range staged are
            // dropped from the batch; frames staged *before* the range
            // survive only if no batch write happened inside it (a
            // successful mid-range `flush_batch` wrote them to the
            // device and recycled their slots — resurrecting the old
            // window over the new slot contents would rewrite garbage).
            self.data_len = data_len0;
            self.tail_fill = tail_fill0;
            self.tail[..tail_fill0].copy_from_slice(&tail0[..tail_fill0]);
            self.tail[tail_fill0..].fill(0);
            self.batch_frames = if self.device_bytes > device_bytes0 { 0 } else { batch_frames0 };
        }
        result
    }

    /// Stages the (full) tail frame into the append batch; the batch
    /// reaches the device as one multi-frame write when the window fills
    /// or at the next barrier/seal (L3 — never one syscall per frame).
    fn stage_full_frame(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.tail_fill, TIER_FRAME_DATA, "staging a full frame");
        let frame_index = (self.data_len - 1) / TIER_FRAME_DATA as u64;
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
    /// length, aligned memory — legal in both I/O modes). The window
    /// resets only on success (M4-S21): a failed write retains every
    /// staged frame, so a later barrier — the ENOSPC retry probe —
    /// rewrites the same frames at the same offsets (positional writes;
    /// a torn prefix from the failure heals under the rewrite).
    fn flush_batch(&mut self) -> io::Result<()> {
        if self.batch_frames == 0 {
            return Ok(());
        }
        let offset = tier_frame_offset(self.batch_first_frame);
        let count = self.batch_frames;
        // The borrow is split by hand: `filled` reads the batch window,
        // the write targets the file.
        let bytes = self.batch.filled(count);
        let len = bytes.len() as u64;
        device_write(&mut self.file, offset, bytes)?;
        self.batch_frames = 0;
        self.device_bytes += len;
        Ok(())
    }

    /// Writes the partial tail frame (if any) and fdatasyncs; afterwards
    /// every appended byte is durable and [`durable_len`](Self::durable_len)
    /// says so (the flush *claims* only
    /// [`confirmable_len`](Self::confirmable_len) until seal).
    ///
    /// # Errors
    /// [`TierWriteFailure::Fsync`] is fatal-by-default (§8.4, ADR-0056
    /// D4) — the caller freezes the flushed watermark and stops;
    /// [`TierWriteFailure::Write`] is the append device write failing.
    pub fn sync(&mut self) -> Result<(), TierWriteFailure> {
        self.flush_batch().map_err(TierWriteFailure::Write)?;
        if self.tail_fill > 0 {
            self.write_tail_frame().map_err(TierWriteFailure::Write)?;
        }
        // ADR-0056 D6 `tier_fsync_err`: the barrier fails — typed,
        // non-recoverable by contract (§8.4: no caller may catch and
        // continue past it).
        if inf_foundation::fault::fire(crate::fault::TIER_FSYNC_ERR) {
            return Err(TierWriteFailure::Fsync(crate::fault::injected(
                crate::fault::TIER_FSYNC_ERR,
            )));
        }
        self.file.sync_data().map_err(TierWriteFailure::Fsync)?;
        self.durable_len = self.data_len;
        Ok(())
    }

    /// Seals the file (ADR-0056 D1): final tail frame, footer block,
    /// one fdatasync. A sealed file is terminal — no rewrite ever
    /// follows, so its full `data_len` becomes claimable. Returns the
    /// exact data length (= the file's logical range length), the path,
    /// the file's lifetime device bytes (M4-S13) — and the open file
    /// handle, so the plane's cold-read table inherits the fd in its
    /// creation-time I/O mode instead of reopening (ADR-0054: one fd,
    /// one mode; M4-S26). Callers that drop the handle just close it.
    ///
    /// # Errors
    /// As [`sync`](Self::sync); a failure mid-seal leaves the file
    /// unsealed on disk (no footer or a torn one) — recovery treats it
    /// per the manifested watermark (D5), the seal is redone by rule.
    pub fn seal(mut self, reason: SealReason) -> Result<(SealOutcome, F::File), TierWriteFailure> {
        self.flush_batch().map_err(TierWriteFailure::Write)?;
        if self.tail_fill > 0 {
            self.write_tail_frame().map_err(TierWriteFailure::Write)?;
        }
        let frames = self.data_len.div_ceil(TIER_FRAME_DATA as u64);
        let footer_at = TIER_HEADER_BYTES as u64 + frames * TIER_FRAME_BYTES as u64;
        let footer = self.staging.frame_mut();
        encode_tier_footer(footer, self.data_len, reason);
        // ADR-0056 D6 `tier_footer_torn`: crash physics between the data
        // reaching the device and the footer's durability — a prefix of
        // the footer lands, the process dies (the typed error stands in
        // for death). Recovery sees an unsealed file and re-seals at the
        // manifested watermark (the crash-matrix row's contract).
        if inf_foundation::fault::fire(crate::fault::TIER_FOOTER_TORN) {
            let cut = TIER_FOOTER_CRC_COVER / 2;
            let torn: Vec<u8> = footer[..cut].to_vec();
            self.file.write_at(footer_at, &torn).map_err(TierWriteFailure::Write)?;
            return Err(TierWriteFailure::Write(crate::fault::injected(
                crate::fault::TIER_FOOTER_TORN,
            )));
        }
        self.file.write_at(footer_at, footer).map_err(TierWriteFailure::Write)?;
        self.device_bytes += TIER_FOOTER_BYTES as u64;
        if inf_foundation::fault::fire(crate::fault::TIER_FSYNC_ERR) {
            return Err(TierWriteFailure::Fsync(crate::fault::injected(
                crate::fault::TIER_FSYNC_ERR,
            )));
        }
        self.file.sync_data().map_err(TierWriteFailure::Fsync)?;
        let outcome = SealOutcome {
            data_len: self.data_len,
            path: self.path,
            device_bytes: self.device_bytes,
        };
        Ok((outcome, self.file))
    }

    /// Writes the current tail frame at its disk slot (full frames land
    /// here once; the partial tail is rewritten in place as it fills)
    /// through the aligned staging window — a whole 4 KiB block at a
    /// 4 KiB offset from 4 KiB-aligned memory, so the write is legal in
    /// both I/O modes by construction (ADR-0054 D2).
    fn write_tail_frame(&mut self) -> io::Result<()> {
        let frame_index = (self.data_len - 1) / TIER_FRAME_DATA as u64;
        let offset = tier_frame_offset(frame_index);
        let frame = self.staging.frame_mut();
        frame[..TIER_FRAME_DATA].copy_from_slice(&self.tail);
        frame[TIER_FRAME_DATA..].copy_from_slice(&crc32c(&self.tail).to_le_bytes());
        device_write(&mut self.file, offset, frame)?;
        self.device_bytes += TIER_FRAME_BYTES as u64;
        Ok(())
    }
}

/// The one device-write funnel — every data byte reaches the fd here
/// (batch flushes and partial tail frames alike), so the D6 write fault
/// points cover the whole write surface.
fn device_write<File: SegmentFile>(file: &mut File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    // ADR-0063 D4 `tier_write_nospace`: the device refuses the
    // allocation — no byte lands, the write fails `StorageFull`-typed.
    // `FromNth` arming models the disk *staying* full until disarmed.
    if inf_foundation::fault::fire(crate::fault::TIER_WRITE_NOSPACE) {
        return Err(crate::fault::injected_nospace(crate::fault::TIER_WRITE_NOSPACE));
    }
    // ADR-0056 D6 `tier_short_write`: the device accepts a prefix and
    // the write FAILS — the caller treats the range as never written.
    if inf_foundation::fault::fire(crate::fault::TIER_SHORT_WRITE) {
        let cut = bytes.len() / 2;
        let torn: Vec<u8> = bytes[..cut].to_vec();
        let _ = file.write_at(offset, &torn);
        return Err(crate::fault::injected(crate::fault::TIER_SHORT_WRITE));
    }
    // ADR-0056 D6 `tier_torn_frame`: a prefix lands and the write
    // *succeeds* — lying-disk/power-cut physics, meaningful as the
    // final write before a crash (recovery truncates or CRC-refuses
    // per D5; the crash-matrix row proves which).
    if inf_foundation::fault::fire(crate::fault::TIER_TORN_FRAME) {
        let cut = bytes.len() * 2 / 3;
        let torn: Vec<u8> = bytes[..cut].to_vec();
        return file.write_at(offset, &torn);
    }
    file.write_at(offset, bytes)
}

// ---- v1 block codecs + the untrusted-input decoder (ADR-0056 D1/D7) ----

/// Parsed v1 header — the identity a reopen must match and the mapping
/// base the MANIFEST cross-checks (S12).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TierHeaderV1 {
    /// {cell, ns, base} — who owns the file and where its range starts.
    pub identity: TierIdentity,
    /// The flush pipeline's capacity target at creation (0 = unstated).
    pub capacity_hint: u64,
}

/// Parsed v1 footer — present ⇔ the file is sealed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TierFooterV1 {
    /// Exact data bytes ⇒ the file's logical range is `{base, data_len}`.
    pub data_len: u64,
    /// Why the file sealed.
    pub reason: SealReason,
}

/// Typed decode failure on untrusted tier-file bytes (D7): every variant
/// is an operating condition of the storage — never a panic.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TierDecodeError {
    /// Input shorter than the block being parsed.
    TooShort,
    /// Header/footer magic mismatch.
    BadMagic,
    /// Unknown format version.
    BadVersion(u32),
    /// Block CRC failed.
    BadCrc,
    /// Base address exceeds 48 bits.
    BadAddr,
    /// Unknown seal reason.
    BadReason(u8),
    /// Footer geometry disagrees with the file length.
    Geometry,
}

impl core::fmt::Display for TierDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TierDecodeError::TooShort => write!(f, "input too short"),
            TierDecodeError::BadMagic => write!(f, "bad magic"),
            TierDecodeError::BadVersion(v) => write!(f, "unknown version {v}"),
            TierDecodeError::BadCrc => write!(f, "block CRC failed"),
            TierDecodeError::BadAddr => write!(f, "base address exceeds 48 bits"),
            TierDecodeError::BadReason(r) => write!(f, "unknown seal reason {r}"),
            TierDecodeError::Geometry => write!(f, "footer geometry disagrees with file length"),
        }
    }
}

fn encode_tier_footer(block: &mut [u8], data_len: u64, reason: SealReason) {
    debug_assert_eq!(block.len(), TIER_FOOTER_BYTES, "footer is one block");
    block.fill(0);
    block[0..4].copy_from_slice(TIER_FOOTER_MAGIC);
    block[4..8].copy_from_slice(&1u32.to_le_bytes());
    block[8..16].copy_from_slice(&data_len.to_le_bytes());
    block[16] = reason as u8;
    let crc = crc32c(&block[..TIER_FOOTER_CRC_COVER]);
    block[24..28].copy_from_slice(&crc.to_le_bytes());
}

/// Parses a v1 header block from untrusted bytes (D7 — typed, bounded).
///
/// # Errors
/// [`TierDecodeError`] naming the first check that failed.
pub fn parse_tier_header(block: &[u8]) -> Result<TierHeaderV1, TierDecodeError> {
    if block.len() < TIER_HEADER_CRC_COVER + 4 {
        return Err(TierDecodeError::TooShort);
    }
    if &block[0..4] != TIER_MAGIC {
        return Err(TierDecodeError::BadMagic);
    }
    let version = u32::from_le_bytes(block[4..8].try_into().expect("4 bytes"));
    if version != 1 {
        return Err(TierDecodeError::BadVersion(version));
    }
    let stored = u32::from_le_bytes(block[32..36].try_into().expect("4 bytes"));
    if crc32c(&block[..TIER_HEADER_CRC_COVER]) != stored {
        return Err(TierDecodeError::BadCrc);
    }
    let cell = u32::from_le_bytes(block[8..12].try_into().expect("4 bytes"));
    let ns = NsId(u32::from_le_bytes(block[12..16].try_into().expect("4 bytes")));
    let raw_base = u64::from_le_bytes(block[16..24].try_into().expect("8 bytes"));
    let base = LogicalAddr::from_raw(raw_base).ok_or(TierDecodeError::BadAddr)?;
    let capacity_hint = u64::from_le_bytes(block[24..32].try_into().expect("8 bytes"));
    Ok(TierHeaderV1 { identity: TierIdentity { cell, ns, base }, capacity_hint })
}

/// Parses a v1 footer block from untrusted bytes (D7 — typed, bounded).
///
/// # Errors
/// [`TierDecodeError`] naming the first check that failed.
pub fn parse_tier_footer(block: &[u8]) -> Result<TierFooterV1, TierDecodeError> {
    if block.len() < TIER_FOOTER_CRC_COVER + 4 {
        return Err(TierDecodeError::TooShort);
    }
    if &block[0..4] != TIER_FOOTER_MAGIC {
        return Err(TierDecodeError::BadMagic);
    }
    let version = u32::from_le_bytes(block[4..8].try_into().expect("4 bytes"));
    if version != 1 {
        return Err(TierDecodeError::BadVersion(version));
    }
    let stored = u32::from_le_bytes(block[24..28].try_into().expect("4 bytes"));
    if crc32c(&block[..TIER_FOOTER_CRC_COVER]) != stored {
        return Err(TierDecodeError::BadCrc);
    }
    let data_len = u64::from_le_bytes(block[8..16].try_into().expect("8 bytes"));
    let reason = SealReason::from_u8(block[16]).ok_or(TierDecodeError::BadReason(block[16]))?;
    Ok(TierFooterV1 { data_len, reason })
}

/// What [`inspect_tier_bytes`] found — recovery/verification's view of
/// one file image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TierSummary {
    /// The verified header.
    pub header: TierHeaderV1,
    /// The verified footer, when the image is a sealed file.
    pub sealed: Option<TierFooterV1>,
    /// Whole frames present in the image (sealed: exactly the data
    /// frames; unsealed: whatever the image holds).
    pub frames: u64,
    /// First frame whose CRC fails, if any (frames after it unchecked).
    pub first_bad_frame: Option<u64>,
}

/// The untrusted-input decoder (D7, L9 — `fuzz_tierfile_decode` drives
/// exactly this): header parse, footer probe on the last block, geometry
/// cross-check, then frame-CRC verification. Iterative, bounded by the
/// input length, never panicking on any byte pattern.
///
/// # Errors
/// [`TierDecodeError`] from the header, the footer (only when its
/// geometry claims sealed-ness), or `Geometry` when a sealed footer's
/// frame count disagrees with the image.
pub fn inspect_tier_bytes(bytes: &[u8]) -> Result<TierSummary, TierDecodeError> {
    if bytes.len() < TIER_HEADER_BYTES {
        return Err(TierDecodeError::TooShort);
    }
    let header = parse_tier_header(&bytes[..TIER_HEADER_BYTES])?;
    let body = &bytes[TIER_HEADER_BYTES..];
    let whole_blocks = (body.len() / TIER_FRAME_BYTES) as u64;
    // Footer probe: a sealed image ends block-aligned with an `ITFS`
    // block whose CRC verifies and whose data_len matches the frame
    // count exactly. Anything else is an unsealed (open/crashed) image —
    // its valid extent is the MANIFEST's to name, not this decoder's.
    let mut sealed = None;
    let mut frames = whole_blocks;
    if body.len().is_multiple_of(TIER_FRAME_BYTES) && whole_blocks >= 1 {
        let last = &body[(whole_blocks as usize - 1) * TIER_FRAME_BYTES..];
        if last[0..4] == *TIER_FOOTER_MAGIC {
            let footer = parse_tier_footer(last)?;
            let data_frames = footer.data_len.div_ceil(TIER_FRAME_DATA as u64);
            if data_frames != whole_blocks - 1 {
                return Err(TierDecodeError::Geometry);
            }
            sealed = Some(footer);
            frames = data_frames;
        }
    }
    let mut first_bad_frame = None;
    for frame_index in 0..frames {
        let at = frame_index as usize * TIER_FRAME_BYTES;
        let frame = &body[at..at + TIER_FRAME_BYTES];
        let stored = u32::from_le_bytes(
            frame[TIER_FRAME_DATA..TIER_FRAME_BYTES].try_into().expect("4 bytes"),
        );
        if crc32c(&frame[..TIER_FRAME_DATA]) != stored {
            first_bad_frame = Some(frame_index);
            break;
        }
    }
    Ok(TierSummary { header, sealed, frames, first_bad_frame })
}

/// Reads exactly one block at `offset` (aligned buffer from the staging
/// window, so the read is legal in both I/O modes).
fn read_block<File: SegmentFile>(file: &File, offset: u64, block: &mut [u8]) -> io::Result<()> {
    let mut read = 0usize;
    while read < block.len() {
        let n = file.read_at(offset + read as u64, &mut block[read..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tier block truncated mid-read",
            ));
        }
        read += n;
    }
    Ok(())
}

fn invalid(path: &Path, message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {message}", path.display()))
}

impl<F: SegmentFs> core::fmt::Debug for TierWriter<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TierWriter")
            .field("path", &self.path)
            .field("base", &self.base)
            .field("data_len", &self.data_len)
            .field("durable_len", &self.durable_len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::mem::MemFs;

    fn read_window(fs: &MemFs, path: &Path, first: u64, count: u32) -> Vec<u8> {
        let file = fs.open_read(path).expect("file exists");
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let n = file.read_at(tier_frame_offset(first), &mut window).expect("read");
        assert_eq!(n, window.len(), "window inside the synced file");
        window
    }

    /// Round trip: appended records read back byte-exact through the
    /// frame-span arithmetic, including a record spanning frames.
    #[test]
    fn append_sync_extract_round_trip() {
        let fs = MemFs::new();
        let base = LogicalAddr::from_raw(0x5000).expect("fits");
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            0,
            0,
            NsId(17),
            base,
            TierIoMode::Buffered,
        )
        .expect("create");
        // Three records; the middle one spans the first frame boundary.
        let records: [(u64, Vec<u8>); 3] = [
            (0, vec![0xAA; 100]),
            (100, vec![0xBB; TIER_FRAME_DATA]),
            (100 + TIER_FRAME_DATA as u64, vec![0xCC; 300]),
        ];
        for (delta, bytes) in &records {
            writer.append(base.advanced(*delta).expect("fits"), bytes).expect("append");
        }
        assert_eq!(writer.durable_len(), 0, "nothing durable before sync");
        writer.sync().expect("sync");
        assert_eq!(writer.durable_len(), writer.data_len());
        let mut out = Vec::new();
        for (delta, bytes) in &records {
            let (first, count, skip) = tier_frame_span(*delta, bytes.len());
            let window = read_window(&fs, writer.path(), first, count);
            tier_extract(&window, skip, bytes.len(), &mut out).expect("clean frames");
            assert_eq!(&out, bytes, "byte-exact round trip");
        }
    }

    /// The partial tail frame extends across syncs — rewritten in place,
    /// CRC always covering the current payload.
    #[test]
    fn tail_frame_extends_across_syncs() {
        let fs = MemFs::new();
        let base = LogicalAddr::ZERO;
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            1,
            0,
            NsId(17),
            base,
            TierIoMode::Buffered,
        )
        .expect("create");
        writer.append(base, &[0x11; 64]).expect("append");
        writer.sync().expect("sync");
        writer.append(base.advanced(64).expect("fits"), &[0x22; 64]).expect("append");
        writer.sync().expect("sync");
        let (first, count, skip) = tier_frame_span(0, 128);
        let window = read_window(&fs, writer.path(), first, count);
        let mut out = Vec::new();
        tier_extract(&window, skip, 128, &mut out).expect("clean frame");
        assert_eq!(&out[..64], &[0x11; 64]);
        assert_eq!(&out[64..], &[0x22; 64]);
    }

    /// A flipped bit fails the frame CRC — typed, not panicking.
    #[test]
    fn corruption_is_detected_per_frame() {
        let fs = MemFs::new();
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            2,
            0,
            NsId(17),
            LogicalAddr::ZERO,
            TierIoMode::Buffered,
        )
        .expect("create");
        writer.append(LogicalAddr::ZERO, &[0x33; 500]).expect("append");
        writer.sync().expect("sync");
        let (first, count, skip) = tier_frame_span(0, 500);
        let mut window = read_window(&fs, writer.path(), first, count);
        window[skip + 7] ^= 0x01;
        let mut out = Vec::new();
        assert_eq!(
            tier_extract(&window, skip, 500, &mut out),
            Err(TierCorruption { window_frame: 0 })
        );
    }

    /// A Direct-mode tier file on the real fs round-trips byte-exact
    /// through the same staged write path, and the open verifiably took
    /// effect (ADR-0054 D3 — the fdinfo check ran inside `create_tier`).
    /// Skips, disclosed, where the fs refuses O_DIRECT (tmpfs CI runners).
    #[cfg(target_os = "linux")]
    #[test]
    fn direct_mode_round_trip_on_real_fs() {
        use crate::fs::StdSegmentFs;
        let dir = std::env::temp_dir().join(format!("inf-tier-direct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let fs = StdSegmentFs;
        let writer =
            TierWriter::create(&fs, &dir, 9, 0, NsId(17), LogicalAddr::ZERO, TierIoMode::Direct);
        let mut writer = match writer {
            Ok(writer) => writer,
            Err(error)
                if error.kind() == io::ErrorKind::Unsupported
                    || error.raw_os_error() == Some(libc::EINVAL) =>
            {
                // tmpfs and friends: the typed refusal is the D3 contract
                // working; the round trip is covered where O_DIRECT exists.
                eprintln!("skipping: {error} (fs without O_DIRECT)");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            Err(error) => panic!("create_tier(Direct): {error}"),
        };
        assert_eq!(writer.mode(), TierIoMode::Direct);
        let payload = vec![0x5A; TIER_FRAME_DATA + 300];
        writer.append(LogicalAddr::ZERO, &payload).expect("append");
        writer.sync().expect("sync");
        let (first, count, skip) = tier_frame_span(0, payload.len());
        // Read back buffered via a fresh descriptor: the on-disk bytes are
        // mode-independent (the format does not know the mode).
        let file = fs.open_read(writer.path()).expect("reopen");
        let mut window = vec![0u8; count as usize * TIER_FRAME_BYTES];
        let n = file.read_at(tier_frame_offset(first), &mut window).expect("read");
        assert_eq!(n, window.len());
        let mut out = Vec::new();
        tier_extract(&window, skip, payload.len(), &mut out).expect("clean frames");
        assert_eq!(out, payload, "byte-exact across the mode boundary");
        drop(writer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Seal writes a valid footer; `inspect_tier_bytes` reads the file
    /// back as sealed with exact geometry and clean frames (the D1/D7
    /// pair assertion: what seal writes, inspect verifies).
    #[test]
    fn seal_footer_inspect_round_trip() {
        let fs = MemFs::new();
        let base = LogicalAddr::from_raw(0x9000).expect("fits");
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            10,
            3,
            NsId(17),
            base,
            TierIoMode::Buffered,
        )
        .expect("create");
        let payload = vec![0x7C; TIER_FRAME_DATA + 777];
        writer.append(base, &payload).expect("append");
        assert_eq!(writer.confirmable_len(), 0, "nothing claimable before sync");
        writer.sync().expect("sync");
        assert_eq!(
            writer.confirmable_len(),
            TIER_FRAME_DATA as u64,
            "partial tail frame held back until seal (ADR-0056 D5)"
        );
        let (sealed, _handle) = writer.seal(SealReason::Capacity).expect("seal");
        assert_eq!(sealed.data_len, payload.len() as u64);
        // M4-S13: the device saw header + three frame writes for two
        // frames (the partial tail is rewritten — once at `sync`, once
        // at seal) + footer.
        assert_eq!(
            sealed.device_bytes,
            (TIER_HEADER_BYTES + 3 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES) as u64,
            "device bytes count the tail-frame rewrite — the amplification is real"
        );
        let image = fs.contents(&sealed.path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("valid sealed image");
        assert_eq!(summary.header.identity, TierIdentity { cell: 3, ns: NsId(17), base });
        let footer = summary.sealed.expect("sealed");
        assert_eq!(footer.data_len, payload.len() as u64);
        assert_eq!(footer.reason, SealReason::Capacity);
        assert_eq!(summary.first_bad_frame, None, "every frame verifies");
    }

    /// Recovery re-seals an unsealed file at the manifested watermark:
    /// bytes beyond it (torn or clean) are dropped, the file gains a
    /// `Recovered` footer, and every retained frame verifies.
    #[test]
    fn recover_seal_truncates_to_the_manifested_watermark() {
        let fs = MemFs::new();
        let base = LogicalAddr::ZERO;
        let identity = TierIdentity { cell: 0, ns: NsId(17), base };
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            11,
            0,
            NsId(17),
            base,
            TierIoMode::Buffered,
        )
        .expect("create");
        // Manifested prefix: two full frames' worth, synced.
        let manifested = 2 * TIER_FRAME_DATA as u64;
        writer.append(base, &vec![0x11; manifested as usize]).expect("append");
        writer.sync().expect("sync");
        // Un-manifested tail: more bytes, synced or not — dead-life
        // garbage either way once the crash happens.
        writer.append(base.advanced(manifested).expect("fits"), &[0x22; 500]).expect("append");
        writer.sync().expect("sync");
        drop(writer); // crash
        let path = TierWriter::<MemFs>::recover_seal_existing(
            &fs,
            Path::new("shard-0"),
            11,
            identity,
            manifested,
            TierIoMode::Buffered,
        )
        .expect("recover");
        let image = fs.contents(&path).expect("file exists");
        let summary = inspect_tier_bytes(&image).expect("valid sealed image");
        let footer = summary.sealed.expect("sealed by recovery");
        assert_eq!(footer.data_len, manifested);
        assert_eq!(footer.reason, SealReason::Recovered);
        assert_eq!(summary.frames, 2);
        assert_eq!(summary.first_bad_frame, None);
        assert_eq!(
            image.len(),
            TIER_HEADER_BYTES + 2 * TIER_FRAME_BYTES + TIER_FOOTER_BYTES,
            "un-manifested frames are gone"
        );
    }

    /// Recovery refuses a wrong identity and a corrupt retained frame —
    /// typed, never a silent overwrite (D5).
    #[test]
    fn recover_seal_refuses_mismatch_and_corruption() {
        let fs = MemFs::new();
        let base = LogicalAddr::ZERO;
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            12,
            0,
            NsId(17),
            base,
            TierIoMode::Buffered,
        )
        .expect("create");
        writer.append(base, &[0x33; 100]).expect("append");
        writer.sync().expect("sync");
        let path = writer.path().to_path_buf();
        drop(writer);
        // Wrong namespace in `expect`.
        let wrong = TierIdentity { cell: 0, ns: NsId(99), base };
        let err = TierWriter::<MemFs>::recover_seal_existing(
            &fs,
            Path::new("shard-0"),
            12,
            wrong,
            100,
            TierIoMode::Buffered,
        )
        .expect_err("identity mismatch refuses");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Flip a bit inside the manifested frame: corruption, typed.
        {
            let mut file = fs.open_write(&path).expect("open");
            let mut block = vec![0u8; TIER_FRAME_BYTES];
            let n = file.read_at(tier_frame_offset(0), &mut block).expect("read");
            assert_eq!(n, TIER_FRAME_BYTES);
            block[7] ^= 0x01;
            file.write_at(tier_frame_offset(0), &block).expect("write");
        }
        let good = TierIdentity { cell: 0, ns: NsId(17), base };
        let err = TierWriter::<MemFs>::recover_seal_existing(
            &fs,
            Path::new("shard-0"),
            12,
            good,
            100,
            TierIoMode::Buffered,
        )
        .expect_err("corrupt manifested frame refuses");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// The untrusted decoder is total: header/footer/geometry failures
    /// are typed on hostile inputs (the fuzz target's oracle set).
    #[test]
    fn inspect_rejects_hostile_images_typed() {
        assert_eq!(inspect_tier_bytes(&[]), Err(TierDecodeError::TooShort));
        assert_eq!(
            inspect_tier_bytes(&vec![0u8; TIER_HEADER_BYTES]),
            Err(TierDecodeError::BadMagic)
        );
        // A valid header with a fake footer whose geometry lies.
        let fs = MemFs::new();
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            13,
            0,
            NsId(17),
            LogicalAddr::ZERO,
            TierIoMode::Buffered,
        )
        .expect("create");
        writer.append(LogicalAddr::ZERO, &[0x44; 64]).expect("append");
        let (sealed, _handle) = writer.seal(SealReason::Shutdown).expect("seal");
        let mut image = fs.contents(&sealed.path).expect("file exists");
        // Corrupt the footer CRC: probe fails typed.
        let at = image.len() - TIER_FOOTER_BYTES + 24;
        image[at] ^= 0xFF;
        assert_eq!(inspect_tier_bytes(&image), Err(TierDecodeError::BadCrc));
    }

    /// Non-contiguous appends are the S11 gap machinery's job — refused
    /// loudly here.
    #[test]
    #[should_panic(expected = "contiguous")]
    fn non_contiguous_append_panics() {
        let fs = MemFs::new();
        let mut writer = TierWriter::create(
            &fs,
            Path::new("shard-0"),
            3,
            0,
            NsId(17),
            LogicalAddr::ZERO,
            TierIoMode::Buffered,
        )
        .expect("create");
        writer.append(LogicalAddr::from_raw(64).expect("fits"), &[0u8; 8]).expect("append");
    }
}
