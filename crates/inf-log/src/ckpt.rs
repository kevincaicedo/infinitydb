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
//! Format **v2** (M4-S12/S14) adds two section tags in the same envelope
//! (header shape, CRC discipline, footer audit all unchanged): 0x03
//! address references (ADR-0057 D3) and 0x04 per-tier-file live-set
//! counters (ADR-0058 D3) — bodies documented at their constants below.
//! v2 readers read v1 files; v1 readers refuse v2 typed.
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

/// `.ick` magic (all versions — `version` in the header discriminates).
pub const ICK_MAGIC: [u8; 8] = *b"INFICK1\0";
/// Format version for cells without tiered namespaces (M2 shape,
/// byte-identical — the degenerate case is absence, ADR-0057 D3).
pub const ICK_VERSION: u16 = 1;
/// Format version once address-reference sections may appear (M4-S12,
/// ADR-0057 D3). v2 readers read v1 files; v1 readers refuse v2 typed.
pub const ICK_VERSION_V2: u16 = 2;

const BLOCK_SECTION: u8 = 1;
const BLOCK_FOOTER: u8 = 2;
/// Address-reference section (v2 only — ADR-0057 D3).
const BLOCK_ADDR_SECTION: u8 = 3;
/// Live-set counter section (v2 only — M4-S14, ADR-0058 D3; activates
/// the tag ADR-0057's registry reserved). body := ns u32 · entries.
const BLOCK_LIVESET: u8 = 4;
/// Blob-reference section (v2 only — M4-S17, ADR-0061 D6): the
/// reference map's cold entries, so a released record's extent is
/// nameable at death time without a disk read. Activating this tag
/// re-coordinates the registry: the M4.5 index-sidecar reservation
/// moves to 0x06+ (ADR-0061). body := ns u32 · entries.
const BLOCK_BLOBREF: u8 = 5;
/// tag + body_len + record_count.
const SECTION_HEADER_LEN: usize = 1 + 4 + 4;
/// ns u32 + walk_watermark u64, at the head of an addr-ref body.
const ADDR_SECTION_META_LEN: usize = 4 + 8;
/// One address reference: sidecar hash u64 + logical addr u48 LE.
pub const ADDR_REF_ENTRY_LEN: usize = 8 + 6;
/// ns u32, at the head of a live-set body.
const LIVESET_META_LEN: usize = 4;
/// One live-set entry: file id u32 · data_len u64 · dead_bytes u64 ·
/// flags u8 (ADR-0058 D3).
pub const LIVESET_ENTRY_LEN: usize = 4 + 8 + 8 + 1;
/// ns u32, at the head of a blob-ref body.
const BLOBREF_META_LEN: usize = 4;
/// One blob-ref entry: logical addr u48 LE · extent id u64 · value len
/// u64 (ADR-0061 D6). Entries ascend strictly by address (the reference
/// map iterates ordered; decode enforces canonically).
pub const BLOBREF_ENTRY_LEN: usize = 6 + 8 + 8;
/// Known live-set flag bits — bit0 = byte-exact counters (ADR-0058 D1).
/// Any other bit is fail-stop at decode within this frozen version.
const LIVESET_FLAG_BYTE_EXACT: u8 = 0x01;
/// Logical addresses are 48-bit (§3.2 freeze).
const ADDR_LIMIT: u64 = 1 << 48;
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

/// What the pending (staging) section holds — sections are homogeneous
/// by class, sealed at class or namespace boundaries (ADR-0057 D3).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum SectionClass {
    /// Record-v1 post-images (tag 0x01) — the v1 vocabulary.
    Images,
    /// Address references (tag 0x03) for one namespace under one walk
    /// watermark.
    Refs { ns: u32, walk_watermark: u64 },
    /// Per-tier-file live-set counters (tag 0x04) for one namespace
    /// (M4-S14, ADR-0058 D3).
    LiveSet { ns: u32 },
    /// Cold blob-reference map entries (tag 0x05) for one namespace
    /// (M4-S17, ADR-0061 D6).
    BlobRefs { ns: u32 },
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
    staged_class: Option<SectionClass>,
    /// Last staged blob-ref address — the writer half of the tag-0x05
    /// strictly-ascending canon (decode enforces the reader half).
    staged_blob_prev_addr: u64,
    version: u16,
    sections: u32,
    records_total: u64,
    entries_per_ns: Vec<(u32, u64)>,
    digest: u64,
    header_written: bool,
    finished: bool,
}

impl IckStream {
    /// Allocates the domain (checkpoint-start, not loop-local: one
    /// checkpoint per cell at a time — ADR-0016 D7). Writes format v1 —
    /// cells without tiered namespaces stay byte-identical to M2.
    #[must_use]
    pub fn new(cfg: &CkptConfig) -> IckStream {
        Self::with_version(cfg, ICK_VERSION)
    }

    /// A v2 stream — address-reference sections may be staged (M4-S12,
    /// ADR-0057 D3). Only cells owning tiered namespaces construct this.
    #[must_use]
    pub fn new_v2(cfg: &CkptConfig) -> IckStream {
        Self::with_version(cfg, ICK_VERSION_V2)
    }

    fn with_version(cfg: &CkptConfig, version: u16) -> IckStream {
        let capacity = cfg.section_bytes as usize + SECTION_HEADER_LEN + CRC_LEN;
        IckStream {
            bufs: [Vec::with_capacity(capacity), Vec::with_capacity(capacity)],
            staging: 0,
            in_flight: None,
            generation: 0,
            section_target: cfg.section_bytes,
            file_offset: 0,
            staged_records: 0,
            staged_class: None,
            staged_blob_prev_addr: 0,
            version,
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
        buf.extend_from_slice(&self.version.to_le_bytes());
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
    ///
    /// # Panics
    /// Panics when the pending section holds address references — the
    /// caller seals at class boundaries (`SyncIckWriter` does this
    /// internally; sections are homogeneous by construction).
    pub fn stage_record(&mut self, view: &RecordView<'_>) {
        assert!(self.header_written && !self.finished, "stage outside header..finish");
        let buf = &mut self.bufs[self.staging];
        if self.staged_records == 0 {
            debug_assert!(buf.is_empty());
            self.staged_class = Some(SectionClass::Images);
            buf.resize(SECTION_HEADER_LEN, 0); // header placeholder, filled at seal
        }
        assert_eq!(self.staged_class, Some(SectionClass::Images), "seal before switching class");
        view.encode_into(buf);
        self.staged_records += 1;
        if let RecordView::StringPostImage { ns, .. } | RecordView::DocFull { ns, .. } = view {
            match self.entries_per_ns.iter_mut().find(|(id, _)| *id == ns.0) {
                Some((_, n)) => *n += 1,
                None => self.entries_per_ns.push((ns.0, 1)),
            }
        }
    }

    /// Appends one address reference `{sidecar hash, logical addr}` to
    /// the staging section (the v2 hybrid walk's cold-majority emission —
    /// ADR-0057 D1/D3). Ref sections are per-namespace, per-walk-
    /// watermark; the caller seals at every boundary.
    ///
    /// # Panics
    /// Panics on a v1 stream, when the pending section holds images or a
    /// different `{ns, walk_watermark}`, or when an address breaches the
    /// watermark or the 48-bit space (walker bugs, never input).
    pub fn stage_addr_ref(&mut self, ns: u32, walk_watermark: u64, hash: u64, addr: u64) {
        assert!(self.header_written && !self.finished, "stage outside header..finish");
        assert_eq!(self.version, ICK_VERSION_V2, "addr refs are a v2 vocabulary");
        assert!(addr < walk_watermark, "a ref must sit below its walk watermark");
        assert!(walk_watermark < ADDR_LIMIT, "watermarks are 48-bit");
        let buf = &mut self.bufs[self.staging];
        if self.staged_records == 0 {
            debug_assert!(buf.is_empty());
            self.staged_class = Some(SectionClass::Refs { ns, walk_watermark });
            buf.resize(SECTION_HEADER_LEN, 0);
            buf.extend_from_slice(&ns.to_le_bytes());
            buf.extend_from_slice(&walk_watermark.to_le_bytes());
        }
        assert_eq!(
            self.staged_class,
            Some(SectionClass::Refs { ns, walk_watermark }),
            "seal before switching class, namespace, or watermark"
        );
        buf.extend_from_slice(&hash.to_le_bytes());
        buf.extend_from_slice(&addr.to_le_bytes()[..6]);
        self.staged_records += 1;
        match self.entries_per_ns.iter_mut().find(|(id, _)| *id == ns) {
            Some((_, n)) => *n += 1,
            None => self.entries_per_ns.push((ns, 1)),
        }
    }

    /// Appends one per-tier-file live-set entry (M4-S14, ADR-0058 D3 —
    /// the walk driver emits one live-set section per tiered namespace
    /// after that namespace's record/ref emission, so the counters cover
    /// every attribution up to walk end). Entries count into the footer's
    /// `records_total` (the audit stays "the footer counts what apply
    /// saw") but **not** into the per-ns entry counts — those presize the
    /// index at recovery, and a file entry is not an index entry.
    ///
    /// # Panics
    /// Panics on a v1 stream, when the pending section holds a different
    /// class or namespace, or when `dead_bytes` exceeds `data_len` —
    /// counter-invariant violations are walker bugs, never input.
    pub fn stage_live_set(
        &mut self,
        ns: u32,
        file_id: u32,
        data_len: u64,
        dead_bytes: u64,
        byte_exact: bool,
    ) {
        assert!(self.header_written && !self.finished, "stage outside header..finish");
        assert_eq!(self.version, ICK_VERSION_V2, "live-set sections are a v2 vocabulary");
        assert!(dead_bytes <= data_len, "dead bytes exceed the file's data bytes");
        let buf = &mut self.bufs[self.staging];
        if self.staged_records == 0 {
            debug_assert!(buf.is_empty());
            self.staged_class = Some(SectionClass::LiveSet { ns });
            buf.resize(SECTION_HEADER_LEN, 0);
            buf.extend_from_slice(&ns.to_le_bytes());
        }
        assert_eq!(
            self.staged_class,
            Some(SectionClass::LiveSet { ns }),
            "seal before switching class or namespace"
        );
        buf.extend_from_slice(&file_id.to_le_bytes());
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend_from_slice(&dead_bytes.to_le_bytes());
        buf.push(if byte_exact { LIVESET_FLAG_BYTE_EXACT } else { 0 });
        self.staged_records += 1;
    }

    /// Appends one cold blob-reference entry (M4-S17, ADR-0061 D6 — the
    /// walk driver emits the reference map's `addr < W` entries per
    /// tiered namespace at walk end; RAM-resident extent records ride
    /// tag-9 images instead, so including them here would double count
    /// at restore). Entries count into the footer's `records_total` but
    /// **not** into the per-ns entry counts — a cold blob record's index
    /// slot was already counted by its 0x03 ref entry.
    ///
    /// # Panics
    /// Panics on a v1 stream, on a class/namespace mix, on an address
    /// outside 48 bits, on a zero-length reference, or on out-of-order
    /// addresses — walker bugs, never input.
    pub fn stage_blob_ref(&mut self, ns: u32, addr: u64, extent_id: u64, len: u64) {
        assert!(self.header_written && !self.finished, "stage outside header..finish");
        assert_eq!(self.version, ICK_VERSION_V2, "blob-ref sections are a v2 vocabulary");
        assert!(addr < ADDR_LIMIT, "logical addresses are 48-bit");
        assert!(len > 0, "an extent reference names at least one byte");
        let buf = &mut self.bufs[self.staging];
        if self.staged_records == 0 {
            debug_assert!(buf.is_empty());
            self.staged_class = Some(SectionClass::BlobRefs { ns });
            self.staged_blob_prev_addr = 0;
            buf.resize(SECTION_HEADER_LEN, 0);
            buf.extend_from_slice(&ns.to_le_bytes());
        } else {
            assert!(
                addr > self.staged_blob_prev_addr,
                "blob-ref entries ascend strictly by address"
            );
        }
        assert_eq!(
            self.staged_class,
            Some(SectionClass::BlobRefs { ns }),
            "seal before switching class or namespace"
        );
        self.staged_blob_prev_addr = addr;
        buf.extend_from_slice(&addr.to_le_bytes()[..6]);
        buf.extend_from_slice(&extent_id.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        self.staged_records += 1;
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
    /// fold, buffer swap. The lease targets `offset()` in the file. The
    /// block tag follows the staged class (images 0x01, refs 0x03 — the
    /// ns/watermark meta was written at first stage).
    ///
    /// # Panics
    /// If nothing is staged or a lease is outstanding (`can_seal`).
    pub fn seal_section(&mut self) -> SectionLease {
        assert!(self.can_seal(), "seal_section without can_seal");
        let class = self.staged_class.take().expect("staged records imply a class");
        let buf = &mut self.bufs[self.staging];
        let body_len = u32::try_from(buf.len() - SECTION_HEADER_LEN).expect("body fits u32");
        buf[0] = match class {
            SectionClass::Images => BLOCK_SECTION,
            SectionClass::Refs { .. } => BLOCK_ADDR_SECTION,
            SectionClass::LiveSet { .. } => BLOCK_LIVESET,
            SectionClass::BlobRefs { .. } => BLOCK_BLOBREF,
        };
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
        Self::create_with_stream(
            fs,
            ckpt_dir,
            IckStream::new(cfg),
            cell,
            ckpt_id,
            begin_lsn,
            ns_ids,
        )
    }

    /// Creates a **v2** checkpoint writer — address references may be
    /// appended (M4-S12, ADR-0057 D3).
    ///
    /// # Errors
    /// File creation or write failure.
    pub fn create_v2(
        fs: F,
        ckpt_dir: &Path,
        cfg: &CkptConfig,
        cell: u16,
        ckpt_id: u64,
        begin_lsn: Lsn,
        ns_ids: &[u32],
    ) -> io::Result<SyncIckWriter<F>> {
        Self::create_with_stream(
            fs,
            ckpt_dir,
            IckStream::new_v2(cfg),
            cell,
            ckpt_id,
            begin_lsn,
            ns_ids,
        )
    }

    fn create_with_stream(
        fs: F,
        ckpt_dir: &Path,
        mut stream: IckStream,
        cell: u16,
        ckpt_id: u64,
        begin_lsn: Lsn,
        ns_ids: &[u32],
    ) -> io::Result<SyncIckWriter<F>> {
        let mut file = fs.create_segment(&ckpt_dir.join(ick_staging_file_name(ckpt_id)), 0)?;
        let lease = stream.begin(cell, ckpt_id, begin_lsn, ns_ids);
        file.write_at(lease.offset(), stream.leased_bytes(&lease))?;
        stream.release(lease);
        Ok(SyncIckWriter { fs, dir: ckpt_dir.to_path_buf(), ckpt_id, stream, file })
    }

    /// Appends one record, sealing + writing the section when it reaches
    /// the target. Seals a pending ref section first — sections are
    /// homogeneous by class (ADR-0057 D3).
    ///
    /// # Errors
    /// Write failure.
    pub fn append(&mut self, view: &RecordView<'_>) -> io::Result<()> {
        if self.stream.staged_class.is_some_and(|class| class != SectionClass::Images) {
            self.write_sealed()?;
        }
        self.stream.stage_record(view);
        if self.stream.section_full() {
            self.write_sealed()?;
        }
        Ok(())
    }

    /// Appends one address reference, sealing the pending section first
    /// when it holds another class or a different `{ns, walk_watermark}`.
    ///
    /// # Errors
    /// Write failure.
    pub fn append_ref(
        &mut self,
        ns: u32,
        walk_watermark: u64,
        hash: u64,
        addr: u64,
    ) -> io::Result<()> {
        let key = SectionClass::Refs { ns, walk_watermark };
        if self.stream.staged_class.is_some_and(|class| class != key) {
            self.write_sealed()?;
        }
        self.stream.stage_addr_ref(ns, walk_watermark, hash, addr);
        if self.stream.section_full() {
            self.write_sealed()?;
        }
        Ok(())
    }

    /// Appends one per-tier-file live-set entry (M4-S14, ADR-0058 D3),
    /// sealing the pending section first when it holds another class or
    /// namespace.
    ///
    /// # Errors
    /// Write failure.
    pub fn append_live_set(
        &mut self,
        ns: u32,
        file_id: u32,
        data_len: u64,
        dead_bytes: u64,
        byte_exact: bool,
    ) -> io::Result<()> {
        let key = SectionClass::LiveSet { ns };
        if self.stream.staged_class.is_some_and(|class| class != key) {
            self.write_sealed()?;
        }
        self.stream.stage_live_set(ns, file_id, data_len, dead_bytes, byte_exact);
        if self.stream.section_full() {
            self.write_sealed()?;
        }
        Ok(())
    }

    /// Appends one cold blob-reference entry (M4-S17, ADR-0061 D6),
    /// sealing across class/namespace boundaries like
    /// [`append_ref`](Self::append_ref).
    ///
    /// # Errors
    /// Write failure from the fs seam.
    pub fn append_blob_ref(
        &mut self,
        ns: u32,
        addr: u64,
        extent_id: u64,
        len: u64,
    ) -> io::Result<()> {
        let key = SectionClass::BlobRefs { ns };
        if self.stream.staged_class.is_some_and(|class| class != key) {
            self.write_sealed()?;
        }
        self.stream.stage_blob_ref(ns, addr, extent_id, len);
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
    /// An addr-ref section's body is not `meta + count × entry` shaped,
    /// or its watermark breaches the 48-bit space (v2, ADR-0057 D3).
    RefSectionMalformed {
        index: u32,
        at: u64,
    },
    /// A reference names an address at or above its walk watermark — the
    /// §3.1 corollary violated on disk.
    RefBeyondWatermark {
        index: u32,
        at: u64,
    },
    /// The load path cannot apply address references (a records-only
    /// loader opened a hybrid v2 checkpoint).
    RefSectionUnsupported {
        at: u64,
    },
    /// A live-set section's body is not `ns + count × entry` shaped, an
    /// entry carries an unknown flag bit, or its dead bytes exceed its
    /// data bytes (v2, ADR-0058 D3).
    LiveSetSectionMalformed {
        index: u32,
        at: u64,
    },
    /// The load path cannot apply live-set counters (a loader without
    /// the live-set arm opened a v2 checkpoint carrying tag 0x04).
    LiveSetSectionUnsupported {
        at: u64,
    },
    /// A blob-ref section's body is not `ns + count × entry` shaped, an
    /// entry names zero bytes, or addresses are out of order (v2,
    /// ADR-0061 D6).
    BlobRefSectionMalformed {
        index: u32,
        at: u64,
    },
    /// The load path cannot apply blob references (a loader without the
    /// blob-ref arm opened a v2 checkpoint carrying tag 0x05).
    BlobRefSectionUnsupported {
        at: u64,
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
            IckReadError::RefSectionMalformed { index, at } => {
                write!(f, "ick addr-ref section {index} malformed at offset {at}")
            }
            IckReadError::RefBeyondWatermark { index, at } => {
                write!(
                    f,
                    "ick addr-ref section {index} names an address beyond its watermark ({at})"
                )
            }
            IckReadError::RefSectionUnsupported { at } => {
                write!(
                    f,
                    "ick addr-ref section at offset {at} but the load path applies records only"
                )
            }
            IckReadError::LiveSetSectionMalformed { index, at } => {
                write!(f, "ick live-set section {index} malformed at offset {at}")
            }
            IckReadError::LiveSetSectionUnsupported { at } => {
                write!(
                    f,
                    "ick live-set section at offset {at} but the load path has no live-set arm"
                )
            }
            IckReadError::BlobRefSectionMalformed { index, at } => {
                write!(f, "ick blob-ref section {index} malformed at offset {at}")
            }
            IckReadError::BlobRefSectionUnsupported { at } => {
                write!(
                    f,
                    "ick blob-ref section at offset {at} but the load path has no blob-ref arm"
                )
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
    if version != ICK_VERSION && version != ICK_VERSION_V2 {
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
            // All section classes hop identically: the class meta lives
            // inside body_len (ADR-0057 D3 / ADR-0058 D3, deliberately).
            // The 0x03/0x04/0x05 tags are a v2 vocabulary — in a v1 file
            // they are corruption. Every tag `seal_section` can emit must
            // appear here or the footer-probe fallback misdiagnoses a
            // valid file as corrupt (the ADR-0073 D4 three-site rule —
            // 0x05 was missing until M4.5-S00's audit).
            BLOCK_SECTION | BLOCK_ADDR_SECTION | BLOCK_LIVESET | BLOCK_BLOBREF => {
                if head[0] != BLOCK_SECTION && version != ICK_VERSION_V2 {
                    return Err(IckReadError::UnknownBlock { tag: head[0], at: offset });
                }
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

/// The per-section addr-ref handler `step_inner` dispatches to — dyn on
/// purpose: dispatch cost lands per section, never per 14-byte entry.
type RefHandler<'a, E> = &'a mut dyn FnMut(IckRefSection<'_>) -> Result<(), E>;

/// The per-section live-set handler (M4-S14) — same per-section dyn
/// dispatch shape as [`RefHandler`].
type LiveSetHandler<'a, E> = &'a mut dyn FnMut(IckLiveSetSection<'_>) -> Result<(), E>;

/// The per-section blob-reference handler (M4-S17) — same shape.
type BlobRefHandler<'a, E> = &'a mut dyn FnMut(IckBlobRefSection<'_>) -> Result<(), E>;

/// One [`IckReader::next_step`] outcome.
#[derive(Debug)]
pub enum IckStep {
    /// One section validated and applied; `bytes` = on-disk block bytes
    /// consumed (the M2-S15 progress currency).
    Section { bytes: u64 },
    /// Footer validated — the load is complete and fully audited.
    Done(IckSummary),
}

/// One validated address-reference section (v2, ADR-0057 D3): every
/// entry already passed the shape and watermark audit — the applier's
/// [`iter`](Self::iter) is a tight trusted loop (per-section dispatch
/// keeps dyn overhead off the per-entry path; refs are the cold-majority
/// bulk of a beyond-RAM recovery).
pub struct IckRefSection<'a> {
    /// Owning namespace.
    pub ns: u32,
    /// The walk watermark every entry sits under — recovery additionally
    /// asserts it at or below the manifested flushed watermark (D6).
    pub walk_watermark: u64,
    entries: &'a [u8],
}

impl IckRefSection<'_> {
    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / ADDR_REF_ENTRY_LEN
    }

    /// True when the section carries no entries (never on disk — the
    /// writer only seals non-empty sections).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(sidecar hash, logical addr)` pairs in file order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.entries.chunks_exact(ADDR_REF_ENTRY_LEN).map(|entry| {
            let hash = le_u64(&entry[0..8]);
            let mut addr = [0u8; 8];
            addr[..6].copy_from_slice(&entry[8..14]);
            (hash, u64::from_le_bytes(addr))
        })
    }
}

/// One decoded live-set entry (M4-S14, ADR-0058 D3): a tier file's byte
/// counters as of walk end. `dead_bytes ≤ data_len` and the flag byte
/// are audited at decode — the applier trusts the shape.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveSetFileEntry {
    /// Tier file id (`tier-NNNNNN.itier`) — the restore match key
    /// against the manifested catalog.
    pub file_id: u32,
    /// Data bytes the emitting life had filed into the file.
    pub data_len: u64,
    /// Dead bytes attributed to the file's range at emission time.
    pub dead_bytes: u64,
    /// Whether `data_len − dead_bytes` was exact live bytes (ADR-0058
    /// D1; restore additionally applies the D5 clamp rules).
    pub byte_exact: bool,
}

/// One decoded blob-reference entry (M4-S17, ADR-0061 D6): a cold
/// extent-carrying record's reference-map entry as of walk end. Shape,
/// address bound, zero-length, and ascending-order audits ran at decode
/// — the applier trusts the shape.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlobRefEntry {
    /// The record's logical address (below the emitting walk's
    /// watermark — a cold, address-preserved reference).
    pub addr: u64,
    /// The referenced blob extent (`blob-NNNNNN.iblob`).
    pub extent_id: u64,
    /// The referenced value's exact byte length.
    pub len: u64,
}

/// One validated blob-reference section (v2, ADR-0061 D6): per-section
/// dispatch, tight per-entry loop — the [`IckRefSection`] posture.
pub struct IckBlobRefSection<'a> {
    /// Owning namespace.
    pub ns: u32,
    entries: &'a [u8],
}

impl IckBlobRefSection<'_> {
    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / BLOBREF_ENTRY_LEN
    }

    /// True when the section carries no entries (never on disk — the
    /// writer only seals non-empty sections).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decoded entries in file (= ascending address) order.
    pub fn iter(&self) -> impl Iterator<Item = BlobRefEntry> + '_ {
        self.entries.chunks_exact(BLOBREF_ENTRY_LEN).map(|entry| {
            let mut addr = [0u8; 8];
            addr[..6].copy_from_slice(&entry[0..6]);
            BlobRefEntry {
                addr: u64::from_le_bytes(addr),
                extent_id: le_u64(&entry[6..14]),
                len: le_u64(&entry[14..22]),
            }
        })
    }
}

/// One validated live-set section (v2, ADR-0058 D3): every entry passed
/// the shape, flag, and `dead ≤ len` audit — per-section dispatch, tight
/// per-entry loop, the [`IckRefSection`] posture.
pub struct IckLiveSetSection<'a> {
    /// Owning namespace.
    pub ns: u32,
    entries: &'a [u8],
}

impl IckLiveSetSection<'_> {
    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / LIVESET_ENTRY_LEN
    }

    /// True when the section carries no entries (never on disk — the
    /// writer only seals non-empty sections).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decoded entries in file order.
    pub fn iter(&self) -> impl Iterator<Item = LiveSetFileEntry> + '_ {
        self.entries.chunks_exact(LIVESET_ENTRY_LEN).map(|entry| LiveSetFileEntry {
            file_id: le_u32(&entry[0..4]),
            data_len: le_u64(&entry[4..12]),
            dead_bytes: le_u64(&entry[12..20]),
            byte_exact: entry[20] & LIVESET_FLAG_BYTE_EXACT != 0,
        })
    }
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
        if version != ICK_VERSION && version != ICK_VERSION_V2 {
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
    /// Records-only: an addr-ref section (hybrid v2 checkpoint) fails
    /// typed — use [`next_step_hybrid`](Self::next_step_hybrid).
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
        self.step_inner(&mut apply, None, None, None)
    }

    /// [`next_step`](Self::next_step) with the v2 arms: ref and live-set
    /// sections arrive whole, post-audit (shape, CRC, per-entry
    /// invariants), one callback per section (M4-S12/S14, ADR-0057 D3/D6,
    /// ADR-0058 D3).
    ///
    /// # Errors
    /// As [`next_step`](Self::next_step).
    pub fn next_step_hybrid<E>(
        &mut self,
        mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
        mut on_refs: impl FnMut(IckRefSection<'_>) -> Result<(), E>,
        mut on_live_set: impl FnMut(IckLiveSetSection<'_>) -> Result<(), E>,
        mut on_blob_refs: impl FnMut(IckBlobRefSection<'_>) -> Result<(), E>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.step_inner(
            &mut apply,
            Some(&mut on_refs),
            Some(&mut on_live_set),
            Some(&mut on_blob_refs),
        )
    }

    fn step_inner<E>(
        &mut self,
        apply: &mut dyn FnMut(RecordView<'_>) -> Result<(), E>,
        refs: Option<RefHandler<'_, E>>,
        live_set: Option<LiveSetHandler<'_, E>>,
        blob_refs: Option<BlobRefHandler<'_, E>>,
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
                    if let RecordView::StringPostImage { ns, .. } | RecordView::DocFull { ns, .. } =
                        view
                    {
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
            BLOCK_ADDR_SECTION if self.info.version == ICK_VERSION_V2 => {
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
                self.file.advise_read_ahead(self.offset + block_len as u64, 4 * block_len as u64);
                let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
                if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(
                        IckReadError::SectionCrc { index: self.sections, at: self.offset }.into()
                    );
                }
                self.digest = fold_digest(self.digest, stored_crc);
                // Shape audit: body = {ns, walk_watermark} + exactly
                // record_count entries; never empty (the writer only
                // seals non-empty sections).
                let body = &self.block[SECTION_HEADER_LEN..block_len - CRC_LEN];
                let entry_bytes = body.len().saturating_sub(ADDR_SECTION_META_LEN);
                if body.len() < ADDR_SECTION_META_LEN
                    || record_count == 0
                    || !entry_bytes.is_multiple_of(ADDR_REF_ENTRY_LEN)
                    || entry_bytes / ADDR_REF_ENTRY_LEN != record_count as usize
                {
                    return Err(IckReadError::RefSectionMalformed {
                        index: self.sections,
                        at: self.offset,
                    }
                    .into());
                }
                let ns = le_u32(&body[0..4]);
                let walk_watermark = le_u64(&body[4..12]);
                if walk_watermark >= ADDR_LIMIT {
                    return Err(IckReadError::RefSectionMalformed {
                        index: self.sections,
                        at: self.offset,
                    }
                    .into());
                }
                let entries = &body[ADDR_SECTION_META_LEN..];
                // Watermark audit (the §3.1 corollary's decode half)
                // before the applier sees a single entry.
                for entry in entries.chunks_exact(ADDR_REF_ENTRY_LEN) {
                    let mut addr = [0u8; 8];
                    addr[..6].copy_from_slice(&entry[8..14]);
                    if u64::from_le_bytes(addr) >= walk_watermark {
                        return Err(IckReadError::RefBeyondWatermark {
                            index: self.sections,
                            at: self.offset,
                        }
                        .into());
                    }
                }
                let Some(on_refs) = refs else {
                    return Err(IckReadError::RefSectionUnsupported { at: self.offset }.into());
                };
                match self.entries_seen.iter_mut().find(|(id, _)| *id == ns) {
                    Some((_, n)) => *n += u64::from(record_count),
                    None => self.entries_seen.push((ns, u64::from(record_count))),
                }
                on_refs(IckRefSection { ns, walk_watermark, entries })
                    .map_err(|error| IckApplyError::Apply { section: self.sections, error })?;
                self.sections += 1;
                self.records_total += u64::from(record_count);
                self.offset += block_len as u64;
                Ok(IckStep::Section { bytes: block_len as u64 })
            }
            BLOCK_LIVESET if self.info.version == ICK_VERSION_V2 => {
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
                self.file.advise_read_ahead(self.offset + block_len as u64, 4 * block_len as u64);
                let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
                if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(
                        IckReadError::SectionCrc { index: self.sections, at: self.offset }.into()
                    );
                }
                self.digest = fold_digest(self.digest, stored_crc);
                // Shape audit: body = ns + exactly record_count entries;
                // never empty (the writer only seals non-empty sections).
                let body = &self.block[SECTION_HEADER_LEN..block_len - CRC_LEN];
                let entry_bytes = body.len().saturating_sub(LIVESET_META_LEN);
                if body.len() < LIVESET_META_LEN
                    || record_count == 0
                    || !entry_bytes.is_multiple_of(LIVESET_ENTRY_LEN)
                    || entry_bytes / LIVESET_ENTRY_LEN != record_count as usize
                {
                    return Err(IckReadError::LiveSetSectionMalformed {
                        index: self.sections,
                        at: self.offset,
                    }
                    .into());
                }
                let ns = le_u32(&body[0..4]);
                let entries = &body[LIVESET_META_LEN..];
                // Entry audit (ADR-0058 D3): unknown flag bits are
                // fail-stop within the frozen version, and a dead count
                // above the file's data bytes is the over-count the D4
                // sound-direction rule exists to make unrepresentable.
                for entry in entries.chunks_exact(LIVESET_ENTRY_LEN) {
                    if entry[20] & !LIVESET_FLAG_BYTE_EXACT != 0
                        || le_u64(&entry[12..20]) > le_u64(&entry[4..12])
                    {
                        return Err(IckReadError::LiveSetSectionMalformed {
                            index: self.sections,
                            at: self.offset,
                        }
                        .into());
                    }
                }
                let Some(on_live_set) = live_set else {
                    return Err(IckReadError::LiveSetSectionUnsupported { at: self.offset }.into());
                };
                // Deliberately NOT entries_seen: the per-ns counts
                // presize the index at recovery, and a file entry is not
                // an index entry (mirrors the writer).
                on_live_set(IckLiveSetSection { ns, entries })
                    .map_err(|error| IckApplyError::Apply { section: self.sections, error })?;
                self.sections += 1;
                self.records_total += u64::from(record_count);
                self.offset += block_len as u64;
                Ok(IckStep::Section { bytes: block_len as u64 })
            }
            BLOCK_BLOBREF if self.info.version == ICK_VERSION_V2 => {
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
                self.file.advise_read_ahead(self.offset + block_len as u64, 4 * block_len as u64);
                let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
                if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
                    return Err(
                        IckReadError::SectionCrc { index: self.sections, at: self.offset }.into()
                    );
                }
                self.digest = fold_digest(self.digest, stored_crc);
                // Shape audit: body = ns + exactly record_count entries;
                // never empty (the writer only seals non-empty sections).
                let body = &self.block[SECTION_HEADER_LEN..block_len - CRC_LEN];
                let entry_bytes = body.len().saturating_sub(BLOBREF_META_LEN);
                if body.len() < BLOBREF_META_LEN
                    || record_count == 0
                    || !entry_bytes.is_multiple_of(BLOBREF_ENTRY_LEN)
                    || entry_bytes / BLOBREF_ENTRY_LEN != record_count as usize
                {
                    return Err(IckReadError::BlobRefSectionMalformed {
                        index: self.sections,
                        at: self.offset,
                    }
                    .into());
                }
                let ns = le_u32(&body[0..4]);
                let entries = &body[BLOBREF_META_LEN..];
                // Entry audit (ADR-0061 D6): a zero-length reference and
                // out-of-order addresses are non-canonical — fail-stop
                // within the frozen version. (Addresses are 48-bit by
                // encoding: six bytes cannot exceed the limit.)
                let mut prev_addr: Option<u64> = None;
                for entry in entries.chunks_exact(BLOBREF_ENTRY_LEN) {
                    let mut addr = [0u8; 8];
                    addr[..6].copy_from_slice(&entry[0..6]);
                    let addr = u64::from_le_bytes(addr);
                    if le_u64(&entry[14..22]) == 0 || prev_addr.is_some_and(|p| addr <= p) {
                        return Err(IckReadError::BlobRefSectionMalformed {
                            index: self.sections,
                            at: self.offset,
                        }
                        .into());
                    }
                    prev_addr = Some(addr);
                }
                let Some(on_blob_refs) = blob_refs else {
                    return Err(IckReadError::BlobRefSectionUnsupported { at: self.offset }.into());
                };
                // Deliberately NOT entries_seen: a cold blob record's
                // index slot was already counted by its 0x03 ref entry —
                // this section is bookkeeping, not index content
                // (mirrors the writer).
                on_blob_refs(IckBlobRefSection { ns, entries })
                    .map_err(|error| IckApplyError::Apply { section: self.sections, error })?;
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

/// [`read_ick`] with the v2 arms (M4-S12/S14, ADR-0057 D3/D6, ADR-0058
/// D3): the hybrid load recovery drives — records through `apply`,
/// validated ref sections through `on_refs`, validated live-set sections
/// through `on_live_set`. Same audit, same fuzz surface.
///
/// # Errors
/// As [`read_ick`].
pub fn read_ick_hybrid<F: SegmentFs, E>(
    fs: &F,
    path: &Path,
    cfg: IckReaderConfig,
    mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
    mut on_refs: impl FnMut(IckRefSection<'_>) -> Result<(), E>,
    mut on_live_set: impl FnMut(IckLiveSetSection<'_>) -> Result<(), E>,
    mut on_blob_refs: impl FnMut(IckBlobRefSection<'_>) -> Result<(), E>,
) -> Result<(IckInfo, IckSummary), IckApplyError<E>> {
    let mut reader = IckReader::open(fs, path, cfg)?;
    loop {
        match reader.next_step_hybrid(
            &mut apply,
            &mut on_refs,
            &mut on_live_set,
            &mut on_blob_refs,
        )? {
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

    // The hop fallback must recognize every tag `seal_section` can emit:
    // a v2 file carrying all three v2 section classes, with the direct
    // footer probe defeated by trailing bytes, still yields the footer's
    // counts by hopping. Regression for the M4.5-S00 audit finding — the
    // hop arm omitted BLOCK_BLOBREF (0x05), so this shape misdiagnosed as
    // `UnknownBlock { tag: 5 }` (ADR-0073 D4).
    #[test]
    fn counts_hop_fallback_covers_every_v2_section_class() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v2(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            21,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v2");
        w.append(&RecordView::StringPostImage {
            ns: crate::record::NsId(16),
            key: b"k",
            value: b"v",
        })
        .expect("image");
        w.append_ref(16, 4096, 0xfeed_beef, 128).expect("addr ref");
        w.append_live_set(16, 1, 4096, 0, true).expect("live set");
        w.append_blob_ref(16, 100, 7, 4096).expect("blob ref");
        let summary = w.finish().expect("finish");

        let path = dir.join(ick_file_name(21));
        let mut padded = fs.contents(&path).expect("ick bytes");
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

    /// M4-S12 (ADR-0057 D3): a hybrid v2 checkpoint interleaves image and
    /// addr-ref sections; the hybrid loader replays both in file order,
    /// the footer audit counts refs into per-ns entries, and the counts
    /// probe returns the same totals.
    #[test]
    fn v2_hybrid_round_trips_and_audits() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let w_mark = 10_000u64;
        let mut w = SyncIckWriter::create_v2(
            fs.clone(),
            dir,
            &small_cfg(),
            3,
            9,
            Lsn::new(crate::lsn::SegmentId(2), 4096),
            &[16, 17],
        )
        .expect("create");
        // Interleave classes the way a home-group walk does: the writer
        // seals at every class/namespace boundary internally.
        let mut want_refs: Vec<(u32, u64, u64)> = Vec::new();
        for i in 0..40u32 {
            let key = format!("hot:{i:04}").into_bytes();
            w.append(&RecordView::StringPostImage { ns: NsId(16), key: &key, value: b"vv" })
                .expect("append");
            let (hash, addr) = (0x1000 + u64::from(i), u64::from(i) * 100);
            w.append_ref(16, w_mark, hash, addr).expect("ref");
            want_refs.push((16, hash, addr));
        }
        // A second namespace's refs under a different watermark.
        w.append_ref(17, 500, 0xAA, 12).expect("ref");
        want_refs.push((17, 0xAA, 12));
        let summary = w.finish().expect("finish");
        assert_eq!(summary.records, 81);
        let mut counts = summary.entries_per_ns.clone();
        counts.sort_unstable();
        assert_eq!(counts, vec![(16, 80), (17, 1)], "refs count as live entries");

        let path = dir.join(ick_file_name(9));
        let mut got_images = 0u64;
        let mut got_refs: Vec<(u32, u64, u64)> = Vec::new();
        let (info, audit) = read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |view| {
                if matches!(view, RecordView::StringPostImage { .. }) {
                    got_images += 1;
                }
                Ok::<(), ()>(())
            },
            |section| {
                assert!(section.walk_watermark == w_mark || section.walk_watermark == 500);
                assert!(!section.is_empty());
                for (hash, addr) in section.iter() {
                    assert!(addr < section.walk_watermark);
                    got_refs.push((section.ns, hash, addr));
                }
                Ok::<(), ()>(())
            },
            |_| panic!("no live-set sections in this image"),
            |_| panic!("no blob-ref sections in this image"),
        )
        .expect("hybrid load");
        assert_eq!(info.version, ICK_VERSION_V2);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        assert_eq!(got_images, 40);
        assert_eq!(got_refs, want_refs, "refs replay in file order, exact");

        let probe = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("counts");
        let mut probe = probe;
        probe.sort_unstable();
        assert_eq!(probe, counts, "the presize hint includes refs");

        // A records-only loader refuses the hybrid file typed — never a
        // silent skip of the cold majority.
        let err = read_ick(&fs, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("records-only load must refuse refs");
        assert!(matches!(err, IckApplyError::Read(IckReadError::RefSectionUnsupported { .. })));

        // Single-byte corruption anywhere is caught (CRC, shape, or
        // watermark audit — never a clean load).
        let image = fs.contents(&path).expect("image");
        for at in (0..image.len()).step_by(7) {
            let mut damaged = image.clone();
            damaged[at] ^= 0x20;
            let fs2 = MemFs::new();
            fs2.create_dir_all(dir).unwrap();
            let mut f = fs2.create_segment(&path, 0).unwrap();
            f.write_at(0, &damaged).unwrap();
            drop(f);
            assert!(
                read_ick_hybrid(
                    &fs2,
                    &path,
                    IckReaderConfig::default(),
                    |_| Ok::<(), ()>(()),
                    |_| Ok::<(), ()>(()),
                    |_| Ok::<(), ()>(()),
                    |_| Ok::<(), ()>(())
                )
                .is_err(),
                "flip at {at} must not load cleanly"
            );
        }
    }

    /// M4-S14 (ADR-0058 D3): live-set sections round-trip inside the v2
    /// envelope — counted in `records_total`, absent from the per-ns
    /// presize counts, refused by loaders without the arm, and fail-stop
    /// on unknown flag bits or `dead > len` at decode.
    #[test]
    fn v2_live_set_round_trips_and_audits() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v2(
            fs.clone(),
            dir,
            &small_cfg(),
            3,
            11,
            Lsn::new(crate::lsn::SegmentId(2), 4096),
            &[16],
        )
        .expect("create");
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v" })
            .expect("append");
        w.append_ref(16, 10_000, 0x1000, 96).expect("ref");
        let want = [
            LiveSetFileEntry { file_id: 0, data_len: 4096, dead_bytes: 4096, byte_exact: true },
            LiveSetFileEntry { file_id: 1, data_len: 65_536, dead_bytes: 700, byte_exact: false },
            LiveSetFileEntry { file_id: 2, data_len: 300, dead_bytes: 0, byte_exact: true },
        ];
        for e in &want {
            w.append_live_set(16, e.file_id, e.data_len, e.dead_bytes, e.byte_exact)
                .expect("live set");
        }
        let summary = w.finish().expect("finish");
        assert_eq!(summary.records, 5, "live-set entries count into records_total");
        assert_eq!(
            summary.entries_per_ns,
            vec![(16, 2)],
            "file entries never pollute the index presize hint"
        );

        let path = dir.join(ick_file_name(11));
        let mut got: Vec<LiveSetFileEntry> = Vec::new();
        let (info, audit) = read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |_| Ok::<(), ()>(()),
            |_| Ok::<(), ()>(()),
            |section| {
                assert_eq!(section.ns, 16);
                assert!(!section.is_empty());
                got.extend(section.iter());
                Ok::<(), ()>(())
            },
            |_| panic!("no blob-ref sections in this image"),
        )
        .expect("hybrid load");
        assert_eq!(info.version, ICK_VERSION_V2);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        assert_eq!(got, want, "entries replay in file order, exact");

        let probe = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("counts");
        assert_eq!(probe, summary.entries_per_ns, "the counts probe hops 0x04 sections");

        // Loaders without the live-set arm refuse typed — never a
        // silent skip of the counters (the 0x03 posture, kept).
        let err = read_ick(&fs, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("records-only load must refuse live-set sections");
        assert!(matches!(
            err,
            IckApplyError::Read(
                IckReadError::RefSectionUnsupported { .. }
                    | IckReadError::LiveSetSectionUnsupported { .. }
            )
        ));

        // Targeted decode audits: find the 0x04 section in the image and
        // damage exactly the audited invariants (an unknown flag bit; a
        // dead count above the data bytes). CRCs are recomputed so the
        // *semantic* audit, not the checksum, must catch each one.
        let image = fs.contents(&path).expect("image");
        let sec_at = find_block(&image, BLOCK_LIVESET);
        let body_len = le_u32(&image[sec_at + 1..sec_at + 5]) as usize;
        let entry0 = sec_at + SECTION_HEADER_LEN + LIVESET_META_LEN;
        for damage in [
            (entry0 + 20, 0x82u8), // unknown flag bits alongside bit0
            (entry0 + 13, 0x11u8), // dead 4096 → 4353 > data_len 4096
        ] {
            let mut damaged = image.clone();
            damaged[damage.0] = damage.1;
            let crc_at = sec_at + SECTION_HEADER_LEN + body_len;
            let crc = crc32c(&damaged[sec_at..crc_at]);
            damaged[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
            let fs2 = MemFs::new();
            fs2.create_dir_all(dir).unwrap();
            let mut f = fs2.create_segment(&path, 0).unwrap();
            f.write_at(0, &damaged).unwrap();
            drop(f);
            let err = read_ick_hybrid(
                &fs2,
                &path,
                IckReaderConfig::default(),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
            )
            .expect_err("semantic damage must not load");
            assert!(
                matches!(err, IckApplyError::Read(IckReadError::LiveSetSectionMalformed { .. })),
                "expected the live-set shape audit, got {err:?}"
            );
        }

        // Writer-side invariant: dead > len is a walker bug, refused loud.
        let result = std::panic::catch_unwind(|| {
            let mut stream = IckStream::new_v2(&small_cfg());
            let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
            stream.release(lease);
            stream.stage_live_set(16, 0, 100, 101, false);
        });
        assert!(result.is_err(), "dead > len must panic at stage time");
        let result = std::panic::catch_unwind(|| {
            let mut stream = IckStream::new(&small_cfg());
            let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
            stream.release(lease);
            stream.stage_live_set(16, 0, 100, 0, false);
        });
        assert!(result.is_err(), "v1 streams must refuse live-set sections");
    }

    /// Blob-reference sections (tag 0x05, M4-S17, ADR-0061 D6): exact
    /// round trip, footer-count semantics (records_total yes, per-ns
    /// presize no — the slot was counted by its 0x03 ref), the
    /// records-only refusal, and the semantic decode audits (zero-length
    /// reference; out-of-order addresses) with CRCs recomputed so the
    /// shape audit, not the checksum, must catch each one.
    #[test]
    fn blob_ref_sections_round_trip_and_audit() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v2(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            13,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v2");
        w.append(&RecordView::StringPostImage {
            ns: crate::record::NsId(16),
            key: b"k",
            value: b"v",
        })
        .expect("image");
        let want = [(100u64, 7u64, 4096u64), (250, 9, 1 << 24), (300, 12, 17)];
        for (addr, extent_id, len) in want {
            w.append_blob_ref(16, addr, extent_id, len).expect("blob ref");
        }
        let summary = w.finish().expect("finish");
        assert_eq!(summary.records, 4, "blob refs count into records_total");
        assert_eq!(
            summary.entries_per_ns,
            vec![(16, 1)],
            "blob-ref entries never pollute the index presize hint"
        );

        let path = dir.join(ick_file_name(13));
        let mut got: Vec<(u64, u64, u64)> = Vec::new();
        let (info, audit) = read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |_| Ok::<(), ()>(()),
            |_| Ok::<(), ()>(()),
            |_| Ok::<(), ()>(()),
            |section| {
                assert_eq!(section.ns, 16);
                assert!(!section.is_empty());
                got.extend(section.iter().map(|e| (e.addr, e.extent_id, e.len)));
                Ok::<(), ()>(())
            },
        )
        .expect("hybrid load");
        assert_eq!(info.version, ICK_VERSION_V2);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        assert_eq!(got, want.to_vec(), "entries replay in ascending address order, exact");

        // Loaders without the blob-ref arm refuse typed (the 0x03/0x04
        // posture, kept).
        let err = read_ick(&fs, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("records-only load must refuse blob-ref sections");
        assert!(matches!(err, IckApplyError::Read(IckReadError::BlobRefSectionUnsupported { .. })));

        // Semantic audits: a zero-length reference and an address-order
        // inversion, each behind a valid CRC.
        let image = fs.contents(&path).expect("image");
        let sec_at = find_block(&image, BLOCK_BLOBREF);
        let body_len = le_u32(&image[sec_at + 1..sec_at + 5]) as usize;
        let entry0 = sec_at + SECTION_HEADER_LEN + BLOBREF_META_LEN;
        let damages: [&[(usize, u8)]; 2] = [
            // Entry 2's len (17) → 0.
            &[(entry0 + 2 * BLOBREF_ENTRY_LEN + 14, 0)],
            // Entry 1's addr (250) → 50: descends after entry 0's 100.
            &[(entry0 + BLOBREF_ENTRY_LEN, 50)],
        ];
        for damage in damages {
            let mut damaged = image.clone();
            for (at, byte) in damage {
                damaged[*at] = *byte;
            }
            let crc_at = sec_at + SECTION_HEADER_LEN + body_len;
            let crc = crc32c(&damaged[sec_at..crc_at]);
            damaged[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
            let fs2 = MemFs::new();
            fs2.create_dir_all(dir).unwrap();
            let mut f = fs2.create_segment(&path, 0).unwrap();
            f.write_at(0, &damaged).unwrap();
            drop(f);
            let err = read_ick_hybrid(
                &fs2,
                &path,
                IckReaderConfig::default(),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
            )
            .expect_err("semantic damage must not load");
            assert!(
                matches!(err, IckApplyError::Read(IckReadError::BlobRefSectionMalformed { .. })),
                "expected the blob-ref shape audit, got {err:?}"
            );
        }

        // Writer-side gates: v1 refusal and the ascending-order panic
        // are walker bugs, refused loud.
        let result = std::panic::catch_unwind(|| {
            let mut stream = IckStream::new(&small_cfg());
            let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
            stream.release(lease);
            stream.stage_blob_ref(16, 100, 1, 10);
        });
        assert!(result.is_err(), "v1 streams must refuse blob-ref sections");
        let result = std::panic::catch_unwind(|| {
            let mut stream = IckStream::new_v2(&small_cfg());
            let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
            stream.release(lease);
            stream.stage_blob_ref(16, 100, 1, 10);
            stream.stage_blob_ref(16, 100, 2, 10);
        });
        assert!(result.is_err(), "non-ascending addresses must panic at stage time");
    }

    /// Locates the first block with `tag` by hopping section headers —
    /// test-only mirror of the reader's walk.
    fn find_block(image: &[u8], tag: u8) -> usize {
        let ns_count = le_u32(&image[28..32]) as usize;
        let mut at = HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN;
        loop {
            assert!(at < image.len(), "tag {tag} not found");
            if image[at] == tag {
                return at;
            }
            assert_ne!(image[at], BLOCK_FOOTER, "tag {tag} not found before the footer");
            let body_len = le_u32(&image[at + 1..at + 5]) as usize;
            at += SECTION_HEADER_LEN + body_len + CRC_LEN;
        }
    }

    /// The version gate: a v1 stream refuses ref staging (panic — walker
    /// bug class), and an unknown version refuses typed at open.
    #[test]
    fn version_gates_hold() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        write_sample(&fs, dir);
        let path = dir.join(ick_file_name(7));
        let mut image = fs.contents(&path).expect("image");
        image[8] = 3; // version 3: unknown to this reader
        let fs2 = MemFs::new();
        fs2.create_dir_all(dir).unwrap();
        let mut f = fs2.create_segment(&path, 0).unwrap();
        f.write_at(0, &image).unwrap();
        drop(f);
        let err = read_ick(&fs2, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("unknown version");
        assert!(matches!(err, IckApplyError::Read(IckReadError::UnsupportedVersion(3))));

        let result = std::panic::catch_unwind(|| {
            let mut stream = IckStream::new(&small_cfg());
            let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
            stream.release(lease);
            stream.stage_addr_ref(16, 100, 0x1, 0);
        });
        assert!(result.is_err(), "v1 streams must refuse addr refs");
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
