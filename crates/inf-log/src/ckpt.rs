//! Fuzzy checkpoint format **v1** + streaming assembler (M2-S10, ADR-0016;
//! freezes at M2 exit — milestone §3.2). A checkpoint is a **materialized
//! log prefix**: section bodies carry ordinary record-v1 encodings (the
//! walker's post-images), so recovery replays a `.ick` through the same
//! `apply_record` upsert as tail frames — one replay vocabulary (L2).
//!
//! ```text
//! header  := magic8 "INFICK1\0" · version u16 · cell u16 · ckpt_id u64 ·
//!            begin_lsn u64 (Lsn::to_u64) · ns_count u32 · ns_ids [u32] ·
//!            crc u32                       (CRC32C over everything before)
//! section := tag 0x01 · body_len u32 · record_count u32 ·
//!            body (record-v1 encodings) · crc u32   (over tag..body)
//! footer  := tag 0x02 · section_count u32 · records_total u64 ·
//!            ns_count u32 · (ns_id u32 · entries u64)* · digest u64 ·
//!            crc u32                                    (over tag..digest)
//! ```
//!
//! All integers little-endian. The footer's per-ns entry counts are S13's
//! table-presizing input. `digest` is a chained fold of
//! `inf_foundation::hash64` over the header CRC and each section CRC in
//! order (ADR-0016 D6 — the recorded deviation from the plan's "xxh3": no
//! external hashing dependency for one leaf digest; the version field is
//! the upgrade path).
//!
//! [`IckStream`] is the reactor-tier assembler: a double-buffered section
//! pair in the staging-ring shape (ADR-0012 D2) — one buffer accepts
//! records while at most one sealed section rides an in-flight driver
//! write under a [`SectionLease`] (the `StableBytes` stability proof).
//! [`SyncIckWriter`] drives the same stream synchronously through the
//! injected [`SegmentFs`] seam (tests, tooling, DST). [`read_ick`] is the
//! validating loader: CRC per section *before* yielding records, footer
//! digest + counts verified, trailing bytes rejected — an incomplete or
//! doctored checkpoint fails loudly, never partially applies (§8.4).

use std::io;
use std::path::{Path, PathBuf};

use inf_foundation::hash64;
use inf_simd::crc32c;

use crate::fs::{SegmentFile, SegmentFs};
use crate::lsn::Lsn;
use crate::record::{RecordDecodeError, RecordView, decode_record};

/// `.ick` v1 magic.
pub const ICK_MAGIC: [u8; 8] = *b"INFICK1\0";
/// Format version this module writes and reads.
pub const ICK_VERSION: u16 = 1;

const BLOCK_SECTION: u8 = 1;
const BLOCK_FOOTER: u8 = 2;
/// tag + body_len + record_count.
const SECTION_HEADER_LEN: usize = 1 + 4 + 4;
const CRC_LEN: usize = 4;
/// magic + version + cell + ckpt_id + begin_lsn + ns_count.
const HEADER_FIXED_LEN: usize = 8 + 2 + 2 + 8 + 8 + 4;
/// tag + section_count + records_total + ns_count.
const FOOTER_FIXED_LEN: usize = 1 + 4 + 8 + 4;

/// Digest chain seed — format v1 wire constant, fixed forever.
const DIGEST_SEED: u64 = 0x1CB0_0C4A_11D0_0D1E;

/// Default section seal target: large enough to amortize per-write cost,
/// small enough that one section is a bounded MAINTAIN slice (ADR-0016 D5).
pub const DEFAULT_SECTION_BYTES: u32 = 256 << 10;
/// Default bytes-appended-since-last-checkpoint trigger threshold.
pub const DEFAULT_CKPT_INTERVAL_BYTES: u64 = 256 << 20;
/// Default hard per-slice streamed-byte cap.
pub const DEFAULT_CKPT_SLICE_BYTES: u32 = 64 << 10;
/// Default streaming pace (bytes/second of wall — injected — time). The
/// per-slice cap bounds one MAINTAIN visit; this bounds the *rate*: an
/// unpaced walk dirties pages at memcpy speed, and the kernel's
/// dirty-page throttling then stalls the io-wq workers the log write
/// rides — a foreground p99.9 cliff under write-saturated load (the
/// M2-S12 pressure row measured it; ADR-0017). Checkpoints are background
/// by design: longer checkpoints are correct, bursts are not.
pub const DEFAULT_CKPT_STREAM_BYTES_PER_SEC: u32 = 64 << 20;

/// Checkpoint policy configuration (per cell).
#[derive(Copy, Clone, Debug)]
pub struct CkptConfig {
    /// Section seal target (bytes of record body per section).
    pub section_bytes: u32,
    /// Trigger: staged log bytes since the last completed checkpoint.
    pub interval_bytes: u64,
    /// Hard cap on bytes streamed per MAINTAIN slice (budget in bytes —
    /// the deficit scheduler's units convert against this, ADR-0016 D5).
    pub slice_bytes: u32,
    /// Streaming pace in bytes/second (0 = unpaced — tests/sync tier).
    pub stream_bytes_per_sec: u32,
}

impl Default for CkptConfig {
    fn default() -> CkptConfig {
        CkptConfig {
            section_bytes: DEFAULT_SECTION_BYTES,
            interval_bytes: DEFAULT_CKPT_INTERVAL_BYTES,
            slice_bytes: DEFAULT_CKPT_SLICE_BYTES,
            stream_bytes_per_sec: DEFAULT_CKPT_STREAM_BYTES_PER_SEC,
        }
    }
}

/// `ckpt-{id:06}.ick` (ids > 999999 grow digits, same as segments).
#[must_use]
pub fn ick_file_name(id: u64) -> String {
    format!("ckpt-{id:06}.ick")
}

/// The pre-publication staging name: a crash mid-stream leaves only this
/// orphan; a file named `*.ick` is always footer-complete (ADR-0016 D4).
#[must_use]
pub fn ick_staging_file_name(id: u64) -> String {
    format!("ckpt-{id:06}.ick.new")
}

/// Parses `ckpt-NNNNNN.ick` → id (boot scan of the ckpt dir; foreign names
/// return `None` for the caller's naming policy).
#[must_use]
pub fn parse_ick_file_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("ckpt-")?.strip_suffix(".ick")?;
    if digits.len() < 6 || digits.len() > 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[inline]
fn fold_digest(digest: u64, crc: u32) -> u64 {
    hash64(&crc.to_le_bytes(), digest)
}

/// What a finished checkpoint contains (writer summary == loader audit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IckSummary {
    pub sections: u32,
    pub records: u64,
    /// Live entries per namespace at walk completion (S13 presizing).
    pub entries_per_ns: Vec<(u32, u64)>,
    pub digest: u64,
    /// Total file bytes.
    pub bytes: u64,
}

/// Decoded `.ick` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IckInfo {
    pub version: u16,
    pub cell: u16,
    pub ckpt_id: u64,
    pub begin_lsn: Lsn,
    pub ns_ids: Vec<u32>,
}

/// Exclusive handle on the sealed, in-flight section: produced by
/// [`IckStream::seal_section`]/[`begin`](IckStream::begin)/
/// [`finish`](IckStream::finish), surrendered to [`IckStream::release`]
/// when the covering write completes. The leased buffer is never touched
/// until release — the `StableBytes` stability proof for the reactor tier.
#[derive(Debug)]
#[must_use = "an in-flight section lease must be released on write completion"]
pub struct SectionLease {
    generation: u64,
    offset: u64,
    len: u32,
}

impl SectionLease {
    /// File offset this section's write targets.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Block length in bytes.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Leases are never empty (header/sections/footer all carry bytes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

struct InFlight {
    buf: usize,
    generation: u64,
}

/// The checkpoint-buffer domain of one cell: a double-buffered section
/// pair. Buffers are `Vec`s sized to the section target at construction;
/// the *staging* buffer may grow past the target mid-emission (a record
/// larger than the remaining slack — the walker seals right after), while
/// the *leased* buffer is immutable until release. `resident_bytes` is the
/// exact `ckpt_buffer_bytes` gauge (L5).
pub struct IckStream {
    bufs: [Vec<u8>; 2],
    staging: usize,
    in_flight: Option<InFlight>,
    generation: u64,
    section_target: u32,
    file_offset: u64,
    staged_records: u32,
    sections: u32,
    records_total: u64,
    entries_per_ns: Vec<(u32, u64)>,
    digest: u64,
    header_written: bool,
    finished: bool,
}

impl IckStream {
    /// Allocates the domain (checkpoint-start, not loop-local: one
    /// checkpoint per cell at a time — ADR-0016 D7).
    #[must_use]
    pub fn new(cfg: &CkptConfig) -> IckStream {
        let capacity = cfg.section_bytes as usize + SECTION_HEADER_LEN + CRC_LEN;
        IckStream {
            bufs: [Vec::with_capacity(capacity), Vec::with_capacity(capacity)],
            staging: 0,
            in_flight: None,
            generation: 0,
            section_target: cfg.section_bytes,
            file_offset: 0,
            staged_records: 0,
            sections: 0,
            records_total: 0,
            entries_per_ns: Vec::new(),
            digest: DIGEST_SEED,
            header_written: false,
            finished: false,
        }
    }

    /// Stages the header block and leases it for the first write.
    ///
    /// # Panics
    /// If called twice, after records were staged, or with a lease
    /// outstanding — checkpoint-driver invariants.
    pub fn begin(
        &mut self,
        cell: u16,
        ckpt_id: u64,
        begin_lsn: Lsn,
        ns_ids: &[u32],
    ) -> SectionLease {
        assert!(!self.header_written, "ick header staged twice");
        assert!(self.in_flight.is_none() && self.staged_records == 0, "begin on a dirty stream");
        let buf = &mut self.bufs[self.staging];
        buf.clear();
        buf.extend_from_slice(&ICK_MAGIC);
        buf.extend_from_slice(&ICK_VERSION.to_le_bytes());
        buf.extend_from_slice(&cell.to_le_bytes());
        buf.extend_from_slice(&ckpt_id.to_le_bytes());
        buf.extend_from_slice(&begin_lsn.to_u64().to_le_bytes());
        buf.extend_from_slice(
            &(u32::try_from(ns_ids.len()).expect("ns set fits u32")).to_le_bytes(),
        );
        for id in ns_ids {
            buf.extend_from_slice(&id.to_le_bytes());
        }
        let crc = crc32c(buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.digest = fold_digest(DIGEST_SEED, crc);
        self.header_written = true;
        self.lease_staging()
    }

    /// Appends one record to the staging section (walker emission). The
    /// staging buffer grows past the target if a record outruns the slack —
    /// the caller seals immediately after (bounded by one emission call).
    pub fn stage_record(&mut self, view: &RecordView<'_>) {
        assert!(self.header_written && !self.finished, "stage outside header..finish");
        let buf = &mut self.bufs[self.staging];
        if self.staged_records == 0 {
            debug_assert!(buf.is_empty());
            buf.resize(SECTION_HEADER_LEN, 0); // header placeholder, filled at seal
        }
        view.encode_into(buf);
        self.staged_records += 1;
        if let RecordView::StringPostImage { ns, .. } = view {
            match self.entries_per_ns.iter_mut().find(|(id, _)| *id == ns.0) {
                Some((_, n)) => *n += 1,
                None => self.entries_per_ns.push((ns.0, 1)),
            }
        }
    }

    /// True once the staging section reached its seal target.
    #[must_use]
    pub fn section_full(&self) -> bool {
        self.staged_body_bytes() >= self.section_target
    }

    /// Record bytes staged into the pending section.
    #[must_use]
    pub fn staged_body_bytes(&self) -> u32 {
        let len = self.bufs[self.staging].len();
        u32::try_from(len.saturating_sub(SECTION_HEADER_LEN)).expect("section fits u32")
    }

    /// True when a sealed block is still riding a write — the caller's
    /// backpressure signal (at most one section in flight, ever).
    #[must_use]
    pub fn backlogged(&self) -> bool {
        self.in_flight.is_some()
    }

    /// True when `seal_section` may run now.
    #[must_use]
    pub fn can_seal(&self) -> bool {
        self.staged_records > 0 && self.in_flight.is_none()
    }

    /// Seals the staging section: header fields + trailing CRC32C, digest
    /// fold, buffer swap. The lease targets `offset()` in the file.
    ///
    /// # Panics
    /// If nothing is staged or a lease is outstanding (`can_seal`).
    pub fn seal_section(&mut self) -> SectionLease {
        assert!(self.can_seal(), "seal_section without can_seal");
        let buf = &mut self.bufs[self.staging];
        let body_len = u32::try_from(buf.len() - SECTION_HEADER_LEN).expect("body fits u32");
        buf[0] = BLOCK_SECTION;
        buf[1..5].copy_from_slice(&body_len.to_le_bytes());
        buf[5..9].copy_from_slice(&self.staged_records.to_le_bytes());
        let crc = crc32c(buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.digest = fold_digest(self.digest, crc);
        self.sections += 1;
        self.records_total += u64::from(self.staged_records);
        self.staged_records = 0;
        self.lease_staging()
    }

    /// Seals the footer block after the walk completed and every section
    /// lease was released. The stream is finished afterwards.
    ///
    /// # Panics
    /// If records are still staged, a lease is outstanding, or the header
    /// was never staged.
    pub fn finish(&mut self) -> SectionLease {
        assert!(self.header_written && !self.finished, "finish outside header..finish");
        assert!(self.staged_records == 0, "finish with a partial section staged");
        assert!(self.in_flight.is_none(), "finish with a section in flight");
        let entries = std::mem::take(&mut self.entries_per_ns);
        let buf = &mut self.bufs[self.staging];
        buf.clear();
        buf.push(BLOCK_FOOTER);
        buf.extend_from_slice(&self.sections.to_le_bytes());
        buf.extend_from_slice(&self.records_total.to_le_bytes());
        buf.extend_from_slice(
            &(u32::try_from(entries.len()).expect("ns set fits u32")).to_le_bytes(),
        );
        for (id, n) in &entries {
            buf.extend_from_slice(&id.to_le_bytes());
            buf.extend_from_slice(&n.to_le_bytes());
        }
        buf.extend_from_slice(&self.digest.to_le_bytes());
        let crc = crc32c(buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.entries_per_ns = entries;
        self.finished = true;
        self.lease_staging()
    }

    fn lease_staging(&mut self) -> SectionLease {
        let sealed = self.staging;
        let generation = self.generation;
        let len = u32::try_from(self.bufs[sealed].len()).expect("block fits u32");
        let offset = self.file_offset;
        self.file_offset += u64::from(len);
        self.in_flight = Some(InFlight { buf: sealed, generation });
        self.staging = 1 - sealed;
        self.generation += 1;
        debug_assert!(self.bufs[self.staging].is_empty(), "swap target not released");
        SectionLease { generation, offset, len }
    }

    /// The sealed block's bytes — what rides the write. Borrowed only for
    /// the submission; the lease, not this slice, crosses iterations.
    #[must_use]
    pub fn leased_bytes(&self, lease: &SectionLease) -> &[u8] {
        let in_flight = self.in_flight.as_ref().expect("no section in flight");
        assert_eq!(in_flight.generation, lease.generation, "lease does not match in-flight block");
        &self.bufs[in_flight.buf]
    }

    /// Returns the lease after the covering write completes.
    pub fn release(&mut self, lease: SectionLease) {
        let in_flight = self.in_flight.take().expect("release with no section in flight");
        assert_eq!(in_flight.generation, lease.generation, "lease does not match in-flight block");
        self.bufs[in_flight.buf].clear();
    }

    /// True once `finish`'s lease was released — the fdatasync may go out.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.finished && self.in_flight.is_none()
    }

    /// Exact `ckpt_buffer_bytes` gauge: both buffers' capacity (L5).
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.bufs[0].capacity() + self.bufs[1].capacity()
    }

    /// Next write offset == file bytes once everything staged so far lands.
    #[must_use]
    pub fn file_bytes(&self) -> u64 {
        self.file_offset
    }

    #[must_use]
    pub fn summary(&self) -> IckSummary {
        IckSummary {
            sections: self.sections,
            records: self.records_total,
            entries_per_ns: self.entries_per_ns.clone(),
            digest: self.digest,
            bytes: self.file_offset,
        }
    }
}

/// Synchronous whole-checkpoint writer over the [`SegmentFs`] seam — the
/// test/tooling/DST tier. The reactor tier drives the same [`IckStream`]
/// through driver ops instead (`inf-server`, ADR-0016 D4); both produce
/// byte-identical files for the same record sequence (asserted in tests).
pub struct SyncIckWriter<F: SegmentFs> {
    fs: F,
    dir: PathBuf,
    ckpt_id: u64,
    stream: IckStream,
    file: F::File,
}

impl<F: SegmentFs> SyncIckWriter<F> {
    /// Creates `ckpt-{id}.ick.new` in `ckpt_dir` and writes the header.
    ///
    /// # Errors
    /// File creation or write failure.
    pub fn create(
        fs: F,
        ckpt_dir: &Path,
        cfg: &CkptConfig,
        cell: u16,
        ckpt_id: u64,
        begin_lsn: Lsn,
        ns_ids: &[u32],
    ) -> io::Result<SyncIckWriter<F>> {
        let mut stream = IckStream::new(cfg);
        let mut file = fs.create_segment(&ckpt_dir.join(ick_staging_file_name(ckpt_id)), 0)?;
        let lease = stream.begin(cell, ckpt_id, begin_lsn, ns_ids);
        file.write_at(lease.offset(), stream.leased_bytes(&lease))?;
        stream.release(lease);
        Ok(SyncIckWriter { fs, dir: ckpt_dir.to_path_buf(), ckpt_id, stream, file })
    }

    /// Appends one record, sealing + writing the section when it reaches
    /// the target.
    ///
    /// # Errors
    /// Write failure.
    pub fn append(&mut self, view: &RecordView<'_>) -> io::Result<()> {
        self.stream.stage_record(view);
        if self.stream.section_full() {
            self.write_sealed()?;
        }
        Ok(())
    }

    fn write_sealed(&mut self) -> io::Result<()> {
        let lease = self.stream.seal_section();
        self.file.write_at(lease.offset(), self.stream.leased_bytes(&lease))?;
        self.stream.release(lease);
        Ok(())
    }

    /// Seals the tail section + footer, fdatasyncs, and publishes
    /// (`rename` + dir-fsync — the `meta.rs` protocol class). Returns the
    /// summary the loader must reproduce.
    ///
    /// # Errors
    /// Write, sync, or rename failure (fsync failure is fatal at the
    /// caller — §8.4).
    pub fn finish(mut self) -> io::Result<IckSummary> {
        if self.stream.can_seal() {
            self.write_sealed()?;
        }
        let lease = self.stream.finish();
        self.file.write_at(lease.offset(), self.stream.leased_bytes(&lease))?;
        self.stream.release(lease);
        self.file.sync_data()?;
        drop(self.file);
        self.fs.rename(
            &self.dir.join(ick_staging_file_name(self.ckpt_id)),
            &self.dir.join(ick_file_name(self.ckpt_id)),
        )?;
        self.fs.sync_dir(&self.dir)?;
        Ok(self.stream.summary())
    }
}

/// Why a `.ick` failed to load. Every variant is fail-stop for recovery:
/// the MANIFEST named this file, so damage is corruption-or-bug (§8.4).
#[derive(Debug)]
pub enum IckReadError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u16),
    HeaderCrc,
    /// File ends inside a declared extent (`at` = the truncated offset).
    Truncated {
        at: u64,
    },
    UnknownBlock {
        tag: u8,
        at: u64,
    },
    SectionTooLarge {
        len: u32,
        max: u32,
    },
    SectionCrc {
        index: u32,
        at: u64,
    },
    FooterCrc {
        at: u64,
    },
    Record {
        section: u32,
        error: RecordDecodeError,
    },
    /// A footer field disagrees with what the sections actually contained.
    FooterMismatch {
        field: &'static str,
    },
    /// Bytes follow the footer.
    TrailingData {
        at: u64,
    },
    MissingFooter,
}

impl std::fmt::Display for IckReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IckReadError::Io(e) => write!(f, "ick io: {e}"),
            IckReadError::BadMagic => write!(f, "not an .ick file (bad magic)"),
            IckReadError::UnsupportedVersion(v) => write!(f, "unsupported .ick version {v}"),
            IckReadError::HeaderCrc => write!(f, "ick header CRC mismatch"),
            IckReadError::Truncated { at } => write!(f, "ick truncated at offset {at}"),
            IckReadError::UnknownBlock { tag, at } => {
                write!(f, "unknown ick block tag {tag} at offset {at}")
            }
            IckReadError::SectionTooLarge { len, max } => {
                write!(f, "ick section of {len} bytes exceeds the {max}-byte bound")
            }
            IckReadError::SectionCrc { index, at } => {
                write!(f, "ick section {index} CRC mismatch at offset {at}")
            }
            IckReadError::FooterCrc { at } => write!(f, "ick footer CRC mismatch at offset {at}"),
            IckReadError::Record { section, error } => {
                write!(f, "ick record error in section {section}: {error}")
            }
            IckReadError::FooterMismatch { field } => {
                write!(f, "ick footer disagrees with sections: {field}")
            }
            IckReadError::TrailingData { at } => {
                write!(f, "trailing bytes after ick footer ({at})")
            }
            IckReadError::MissingFooter => write!(f, "ick has no footer (incomplete checkpoint)"),
        }
    }
}

impl std::error::Error for IckReadError {}

impl From<io::Error> for IckReadError {
    fn from(e: io::Error) -> IckReadError {
        IckReadError::Io(e)
    }
}

/// Read failure or the apply callback's error (the `ApplyError` shape).
#[derive(Debug)]
pub enum IckApplyError<E> {
    Read(IckReadError),
    Apply { section: u32, error: E },
}

impl<E> From<IckReadError> for IckApplyError<E> {
    fn from(e: IckReadError) -> IckApplyError<E> {
        IckApplyError::Read(e)
    }
}

impl<E> From<io::Error> for IckApplyError<E> {
    fn from(e: io::Error) -> IckApplyError<E> {
        IckApplyError::Read(IckReadError::Io(e))
    }
}

/// Loader bounds (defensive: lengths are attacker/corruption-controlled).
#[derive(Copy, Clone, Debug)]
pub struct IckReaderConfig {
    /// Largest section body accepted (writer sections are bounded by the
    /// staging capacity class; the bound only guards allocation).
    pub max_section_bytes: u32,
}

impl Default for IckReaderConfig {
    fn default() -> IckReaderConfig {
        IckReaderConfig { max_section_bytes: crate::frame::DEFAULT_MAX_FRAME_LEN }
    }
}

fn read_exact_at<File: SegmentFile>(
    file: &File,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), IckReadError> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.read_at(offset + done as u64, &mut buf[done..])?;
        if n == 0 {
            return Err(IckReadError::Truncated { at: offset + done as u64 });
        }
        done += n;
    }
    Ok(())
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("4 bytes"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
}

/// Footer peek (M2-S13): hop the section headers to the footer and return
/// its per-ns entry counts — the presize hint recovery applies *before*
/// streaming [`read_ick`], so the bulk apply avoids the doubling-rehash
/// storm (measured 0.84 → 1.0 GiB/s on the S13 dev rehearsal). Sections
/// are length-hopped, not CRC-validated here: the streaming pass that
/// follows still performs the complete audit; the counts themselves are
/// protected by the footer's own CRC, and a wrong hint could only cost
/// memory geometry, never correctness.
///
/// # Errors
/// Structural damage (bad magic/version, truncation, absurd lengths,
/// unknown block tags, footer CRC mismatch) — the same fail-stop class as
/// [`read_ick`].
pub fn read_ick_counts<F: SegmentFs>(
    fs: &F,
    path: &Path,
    cfg: IckReaderConfig,
) -> Result<Vec<(u32, u64)>, IckReadError> {
    let file = fs.open_read(path).map_err(IckReadError::Io)?;
    let mut fixed = [0u8; HEADER_FIXED_LEN];
    read_exact_at(&file, 0, &mut fixed)?;
    if fixed[0..8] != ICK_MAGIC {
        return Err(IckReadError::BadMagic);
    }
    let version = u16::from_le_bytes([fixed[8], fixed[9]]);
    if version != ICK_VERSION {
        return Err(IckReadError::UnsupportedVersion(version));
    }
    let ns_count = le_u32(&fixed[28..32]) as usize;
    if ns_count > (1 << 20) {
        return Err(IckReadError::Truncated { at: 28 });
    }
    let file_size = file.file_size()?;
    let mut offset = (HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN) as u64;
    // Direct footer probe (M2.5-S08): a well-formed `.ick` ends exactly at
    // its footer, whose length is computable from the header's `ns_count` —
    // two reads instead of hopping every section header (a chain of
    // *dependent* small reads; cold, each hop is a synchronous page fault —
    // measured as the dominant cold ick cost). The footer CRC validates the
    // probe; any mismatch falls back to the hop below, and a wrong hint
    // could only ever cost memory geometry (the streaming pass re-audits).
    let probe_len = FOOTER_FIXED_LEN + ns_count * 12 + 8 + CRC_LEN;
    if file_size >= offset + probe_len as u64 {
        let probe_at = file_size - probe_len as u64;
        let mut block = vec![0u8; probe_len];
        if read_exact_at(&file, probe_at, &mut block).is_ok()
            && block[0] == BLOCK_FOOTER
            && le_u32(&block[13..17]) as usize == ns_count
            && crc32c(&block[..probe_len - CRC_LEN]) == le_u32(&block[probe_len - CRC_LEN..])
        {
            return Ok(block[FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + ns_count * 12]
                .chunks_exact(12)
                .map(|chunk| (le_u32(&chunk[0..4]), le_u64(&chunk[4..12])))
                .collect());
        }
    }
    loop {
        if offset >= file_size {
            return Err(IckReadError::MissingFooter);
        }
        let mut head = [0u8; SECTION_HEADER_LEN];
        read_exact_at(&file, offset, &mut head)?;
        match head[0] {
            BLOCK_SECTION => {
                let body_len = le_u32(&head[1..5]);
                if body_len > cfg.max_section_bytes {
                    return Err(IckReadError::SectionTooLarge {
                        len: body_len,
                        max: cfg.max_section_bytes,
                    });
                }
                offset += (SECTION_HEADER_LEN + body_len as usize + CRC_LEN) as u64;
            }
            BLOCK_FOOTER => {
                let mut fixed = [0u8; FOOTER_FIXED_LEN];
                read_exact_at(&file, offset, &mut fixed)?;
                let footer_ns = le_u32(&fixed[13..17]) as usize;
                if footer_ns > (1 << 20) {
                    return Err(IckReadError::Truncated { at: offset + 13 });
                }
                let block_len = FOOTER_FIXED_LEN + footer_ns * 12 + 8 + CRC_LEN;
                let mut block = vec![0u8; block_len];
                read_exact_at(&file, offset, &mut block)?;
                let stored_crc = le_u32(&block[block_len - CRC_LEN..]);
                if crc32c(&block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(IckReadError::FooterCrc { at: offset });
                }
                return Ok(block[FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + footer_ns * 12]
                    .chunks_exact(12)
                    .map(|chunk| (le_u32(&chunk[0..4]), le_u64(&chunk[4..12])))
                    .collect());
            }
            tag => return Err(IckReadError::UnknownBlock { tag, at: offset }),
        }
    }
}

/// One [`IckReader::next_step`] outcome.
#[derive(Debug)]
pub enum IckStep {
    /// One section validated and applied; `bytes` = on-disk block bytes
    /// consumed (the M2-S15 progress currency).
    Section { bytes: u64 },
    /// Footer validated — the load is complete and fully audited.
    Done(IckSummary),
}

/// Pull-based validating `.ick` loader (M2-S15): the same header → section
/// CRC-then-apply → footer audit as [`read_ick`], one section per
/// [`next_step`](Self::next_step) call, so boot recovery can load a
/// checkpoint in bounded MAINTAIN slices while the cell answers
/// `-LOADING`. [`read_ick`] is this reader run to completion — one code
/// path, one audit, one fuzz surface.
pub struct IckReader<File: SegmentFile> {
    file: File,
    cfg: IckReaderConfig,
    info: IckInfo,
    file_size: u64,
    offset: u64,
    sections: u32,
    records_total: u64,
    entries_seen: Vec<(u32, u64)>,
    digest: u64,
    block: Vec<u8>,
    done: bool,
}

impl<File: SegmentFile> IckReader<File> {
    /// Opens `path` and validates the header (magic, version, header CRC).
    ///
    /// # Errors
    /// Structural damage in the header — the [`read_ick`] fail-stop class.
    pub fn open<F: SegmentFs<File = File>>(
        fs: &F,
        path: &Path,
        cfg: IckReaderConfig,
    ) -> Result<IckReader<File>, IckReadError> {
        let file = fs.open_read(path).map_err(IckReadError::Io)?;
        let mut fixed = [0u8; HEADER_FIXED_LEN];
        read_exact_at(&file, 0, &mut fixed)?;
        if fixed[0..8] != ICK_MAGIC {
            return Err(IckReadError::BadMagic);
        }
        let version = u16::from_le_bytes([fixed[8], fixed[9]]);
        if version != ICK_VERSION {
            return Err(IckReadError::UnsupportedVersion(version));
        }
        let cell = u16::from_le_bytes([fixed[10], fixed[11]]);
        let ckpt_id = le_u64(&fixed[12..20]);
        let begin_lsn = Lsn::from_u64(le_u64(&fixed[20..28]));
        let ns_count = le_u32(&fixed[28..32]) as usize;
        if ns_count > (1 << 20) {
            return Err(IckReadError::Truncated { at: 28 }); // absurd count: damaged length
        }
        let mut rest = vec![0u8; ns_count * 4 + CRC_LEN];
        read_exact_at(&file, HEADER_FIXED_LEN as u64, &mut rest)?;
        let mut header_crc_input = Vec::with_capacity(HEADER_FIXED_LEN + ns_count * 4);
        header_crc_input.extend_from_slice(&fixed);
        header_crc_input.extend_from_slice(&rest[..ns_count * 4]);
        let stored_header_crc = le_u32(&rest[ns_count * 4..]);
        if crc32c(&header_crc_input) != stored_header_crc {
            return Err(IckReadError::HeaderCrc);
        }
        let ns_ids: Vec<u32> = rest[..ns_count * 4].chunks_exact(4).map(le_u32).collect();
        let file_size = file.file_size()?;
        Ok(IckReader {
            file,
            cfg,
            info: IckInfo { version, cell, ckpt_id, begin_lsn, ns_ids },
            file_size,
            offset: (HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN) as u64,
            sections: 0,
            records_total: 0,
            entries_seen: Vec::new(),
            digest: fold_digest(DIGEST_SEED, stored_header_crc),
            block: Vec::new(),
            done: false,
        })
    }

    /// The validated header.
    #[must_use]
    pub fn info(&self) -> &IckInfo {
        &self.info
    }

    /// Total file bytes (the progress denominator).
    #[must_use]
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Validates and applies the next block. Sections yield
    /// [`IckStep::Section`]; the footer completes the audit and yields
    /// [`IckStep::Done`] (calling again afterwards is a caller bug).
    ///
    /// # Errors
    /// [`IckApplyError::Read`] for any structural damage (fail-stop for
    /// recovery); [`IckApplyError::Apply`] propagates the callback's error
    /// at the failing section.
    ///
    /// # Panics
    /// If called after [`IckStep::Done`] was returned.
    pub fn next_step<E>(
        &mut self,
        mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
    ) -> Result<IckStep, IckApplyError<E>> {
        assert!(!self.done, "IckReader stepped past its footer");
        if self.offset == self.file_size {
            return Err(IckReadError::MissingFooter.into());
        }
        let mut tag = [0u8; 1];
        read_exact_at(&self.file, self.offset, &mut tag)?;
        match tag[0] {
            BLOCK_SECTION => {
                let mut head = [0u8; SECTION_HEADER_LEN];
                read_exact_at(&self.file, self.offset, &mut head)?;
                let body_len = le_u32(&head[1..5]);
                if body_len > self.cfg.max_section_bytes {
                    return Err(IckReadError::SectionTooLarge {
                        len: body_len,
                        max: self.cfg.max_section_bytes,
                    }
                    .into());
                }
                let record_count = le_u32(&head[5..9]);
                let block_len = SECTION_HEADER_LEN + body_len as usize + CRC_LEN;
                self.block.resize(block_len, 0);
                read_exact_at(&self.file, self.offset, &mut self.block)?;
                // Read-ahead the next blocks (M2.5-S08): their device reads
                // overlap this section's CRC + decode + apply. Four blocks
                // deep — sections share the staging capacity class and are
                // small enough that one-ahead loses the race against the
                // prefetcher's wakeup latency. Hint-only; EOF-safe.
                self.file.advise_read_ahead(self.offset + block_len as u64, 4 * block_len as u64);
                let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
                if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(
                        IckReadError::SectionCrc { index: self.sections, at: self.offset }.into()
                    );
                }
                self.digest = fold_digest(self.digest, stored_crc);
                let mut body = &self.block[SECTION_HEADER_LEN..block_len - CRC_LEN];
                let mut decoded = 0u32;
                while !body.is_empty() {
                    let (view, consumed) = decode_record(body)
                        .map_err(|error| IckReadError::Record { section: self.sections, error })?;
                    if let RecordView::StringPostImage { ns, .. } = view {
                        match self.entries_seen.iter_mut().find(|(id, _)| *id == ns.0) {
                            Some((_, n)) => *n += 1,
                            None => self.entries_seen.push((ns.0, 1)),
                        }
                    }
                    apply(view)
                        .map_err(|error| IckApplyError::Apply { section: self.sections, error })?;
                    decoded += 1;
                    body = &body[consumed..];
                }
                if decoded != record_count {
                    return Err(
                        IckReadError::FooterMismatch { field: "section record_count" }.into()
                    );
                }
                self.sections += 1;
                self.records_total += u64::from(record_count);
                self.offset += block_len as u64;
                Ok(IckStep::Section { bytes: block_len as u64 })
            }
            BLOCK_FOOTER => {
                let mut fixed = [0u8; FOOTER_FIXED_LEN];
                read_exact_at(&self.file, self.offset, &mut fixed)?;
                let footer_sections = le_u32(&fixed[1..5]);
                let footer_records = le_u64(&fixed[5..13]);
                let footer_ns = le_u32(&fixed[13..17]) as usize;
                if footer_ns > (1 << 20) {
                    return Err(IckReadError::Truncated { at: self.offset + 13 }.into());
                }
                let tail_len = footer_ns * 12 + 8 + CRC_LEN;
                let block_len = FOOTER_FIXED_LEN + tail_len;
                self.block.resize(block_len, 0);
                read_exact_at(&self.file, self.offset, &mut self.block)?;
                let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
                if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(IckReadError::FooterCrc { at: self.offset }.into());
                }
                let stored_digest =
                    le_u64(&self.block[block_len - CRC_LEN - 8..block_len - CRC_LEN]);
                if footer_sections != self.sections {
                    return Err(IckReadError::FooterMismatch { field: "section_count" }.into());
                }
                if footer_records != self.records_total {
                    return Err(IckReadError::FooterMismatch { field: "records_total" }.into());
                }
                if stored_digest != self.digest {
                    return Err(IckReadError::FooterMismatch { field: "digest" }.into());
                }
                let mut footer_entries: Vec<(u32, u64)> = Vec::with_capacity(footer_ns);
                for chunk in
                    self.block[FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + footer_ns * 12].chunks_exact(12)
                {
                    footer_entries.push((le_u32(&chunk[0..4]), le_u64(&chunk[4..12])));
                }
                let mut seen_sorted = self.entries_seen.clone();
                seen_sorted.sort_unstable();
                let mut footer_sorted = footer_entries.clone();
                footer_sorted.sort_unstable();
                if seen_sorted != footer_sorted {
                    return Err(IckReadError::FooterMismatch { field: "entries_per_ns" }.into());
                }
                let end = self.offset + block_len as u64;
                if end != self.file_size {
                    return Err(IckReadError::TrailingData { at: end }.into());
                }
                self.done = true;
                Ok(IckStep::Done(IckSummary {
                    sections: self.sections,
                    records: self.records_total,
                    entries_per_ns: footer_entries,
                    digest: self.digest,
                    bytes: end,
                }))
            }
            tag => Err(IckReadError::UnknownBlock { tag, at: self.offset }.into()),
        }
    }
}

/// Validating streaming load: header → per-section CRC-then-apply → footer
/// audit (counts + digest + no trailing bytes). `apply` sees every record
/// in file order — S13 feeds `Keyspace::apply_record` here (presized via
/// [`read_ick_counts`]), then replays the tail from `info.begin_lsn` via
/// the S04 reader. Implemented as [`IckReader`] run to completion (S15
/// chunks the same reader across MAINTAIN slices).
///
/// # Errors
/// [`IckApplyError::Read`] for any structural damage (fail-stop for
/// recovery); [`IckApplyError::Apply`] propagates the callback's error at
/// the failing section.
pub fn read_ick<F: SegmentFs, E>(
    fs: &F,
    path: &Path,
    cfg: IckReaderConfig,
    mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
) -> Result<(IckInfo, IckSummary), IckApplyError<E>> {
    let mut reader = IckReader::open(fs, path, cfg)?;
    loop {
        match reader.next_step(&mut apply)? {
            IckStep::Section { .. } => {}
            IckStep::Done(summary) => return Ok((reader.info, summary)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::mem::MemFs;
    use crate::record::NsId;

    fn small_cfg() -> CkptConfig {
        CkptConfig { section_bytes: 64, ..Default::default() }
    }

    fn sample_records() -> Vec<(Vec<u8>, Vec<u8>, Option<u64>)> {
        (0..50u32)
            .map(|i| {
                let key = format!("key:{i:04}").into_bytes();
                let value = vec![b'v'; (i as usize * 7) % 40];
                let exp = (i % 3 == 0).then(|| 1_780_000_000_000 + u64::from(i));
                (key, value, exp)
            })
            .collect()
    }

    fn write_sample(fs: &MemFs, dir: &Path) -> IckSummary {
        let mut w = SyncIckWriter::create(
            fs.clone(),
            dir,
            &small_cfg(),
            3,
            7,
            Lsn::new(crate::lsn::SegmentId(2), 4096),
            &[16, 17],
        )
        .expect("create");
        for (key, value, exp) in sample_records() {
            let ns = NsId(16);
            w.append(&RecordView::StringPostImage { ns, key: &key, value: &value })
                .expect("append");
            if let Some(at) = exp {
                w.append(&RecordView::ExpireAt { ns, at_unix_ms: at, key: &key }).expect("append");
            }
        }
        w.finish().expect("finish")
    }

    #[test]
    fn round_trips_and_audits() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let summary = write_sample(&fs, dir);
        assert!(summary.sections > 1, "sample must span sections");
        assert_eq!(summary.entries_per_ns, vec![(16, 50)]);

        let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let (info, audit) =
            read_ick(&fs, &dir.join(ick_file_name(7)), IckReaderConfig::default(), |view| {
                if let RecordView::StringPostImage { key, value, .. } = view {
                    got.push((key.to_vec(), value.to_vec()));
                }
                Ok::<(), ()>(())
            })
            .expect("load");
        assert_eq!(info.cell, 3);
        assert_eq!(info.ckpt_id, 7);
        assert_eq!(info.begin_lsn, Lsn::new(crate::lsn::SegmentId(2), 4096));
        assert_eq!(info.ns_ids, vec![16, 17]);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        let want: Vec<(Vec<u8>, Vec<u8>)> =
            sample_records().into_iter().map(|(k, v, _)| (k, v)).collect();
        assert_eq!(got, want, "records replay in file order, byte-identical");
    }

    #[test]
    fn counts_peek_matches_the_streamed_footer() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let summary = write_sample(&fs, dir);
        let path = dir.join(ick_file_name(7));

        let counts = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("footer peek");
        assert_eq!(counts, summary.entries_per_ns, "the presize hint is the footer's truth");

        // The peek's own integrity: any single-byte corruption of the
        // footer block is caught by its CRC.
        let bytes = fs.contents(&path).expect("ick bytes");
        let footer_at = bytes.len() - (FOOTER_FIXED_LEN + 12 + 8 + CRC_LEN);
        for at in footer_at..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x01;
            let dmg = MemFs::new();
            dmg.create_dir_all(dir).unwrap();
            use crate::fs::{SegmentFile, SegmentFs as _};
            let mut f = dmg.create_meta(&path).expect("create");
            f.write_at(0, &damaged).expect("write");
            assert!(
                read_ick_counts(&dmg, &path, IckReaderConfig::default()).is_err(),
                "corrupt footer byte {at} must not yield counts"
            );
        }

        // Fallback (M2.5-S08): trailing bytes defeat the direct end-of-file
        // footer probe; the section hop still finds the footer and returns
        // the same hint.
        let mut padded = bytes.clone();
        padded.extend_from_slice(b"junk");
        let pad = MemFs::new();
        pad.create_dir_all(dir).unwrap();
        use crate::fs::{SegmentFile, SegmentFs as _};
        let mut f = pad.create_meta(&path).expect("create");
        f.write_at(0, &padded).expect("write");
        let counts =
            read_ick_counts(&pad, &path, IckReaderConfig::default()).expect("hop fallback");
        assert_eq!(counts, summary.entries_per_ns, "fallback hint matches the footer");
    }

    #[test]
    fn staging_orphan_never_parses_as_complete() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            1,
            Lsn::new(crate::lsn::SegmentId(0), 0),
            &[16],
        )
        .unwrap();
        for (key, value, _) in sample_records() {
            w.append(&RecordView::StringPostImage { ns: NsId(16), key: &key, value: &value })
                .unwrap();
        }
        // No finish: the crash shape. Only the .new orphan exists…
        assert!(fs.contents(&dir.join(ick_file_name(1))).is_none());
        // …and loading it fails loudly (no footer / truncated), never
        // partially applies as a complete checkpoint.
        let err =
            read_ick(&fs, &dir.join(ick_staging_file_name(1)), IckReaderConfig::default(), |_| {
                Ok::<(), ()>(())
            })
            .expect_err("incomplete checkpoint must not load");
        assert!(matches!(
            err,
            IckApplyError::Read(IckReadError::MissingFooter | IckReadError::Truncated { .. })
        ));
    }

    #[test]
    fn single_byte_corruption_is_always_caught() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        write_sample(&fs, dir);
        let path = dir.join(ick_file_name(7));
        let image = fs.contents(&path).expect("image");
        for at in 0..image.len() {
            let mut damaged = image.clone();
            damaged[at] ^= 0x40;
            let fs2 = MemFs::new();
            fs2.create_dir_all(dir).unwrap();
            let mut f = fs2.create_segment(&path, 0).unwrap();
            f.write_at(0, &damaged).unwrap();
            drop(f);
            assert!(
                read_ick(&fs2, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(())).is_err(),
                "flip at {at} must not load cleanly"
            );
        }
    }

    #[test]
    fn file_names_round_trip() {
        assert_eq!(ick_file_name(5), "ckpt-000005.ick");
        assert_eq!(parse_ick_file_name("ckpt-000005.ick"), Some(5));
        assert_eq!(parse_ick_file_name("ckpt-000005.ick.new"), None);
        assert_eq!(parse_ick_file_name("seg-000005.ilog"), None);
        assert_eq!(parse_ick_file_name("ckpt-x.ick"), None);
    }
}
