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

mod read;
pub use read::BlobRefEntry;
pub use read::IckBlobRefSection;
pub use read::IckIdxSidecarSection;
pub use read::IckLiveSetSection;
pub use read::IckRefSection;
#[cfg(test)]
use read::le_u32;
pub use read::{
    IckApplyError, IckReadError, IckReader, IckStep, read_ick_counts, read_ick_counts_probed,
};

pub use read::IckIdxSidecarStep;
pub use read::IckReaderConfig;
pub use read::LiveSetFileEntry;
pub use read::read_ick;
pub use read::read_ick_hybrid;

// ---- shared data definitions (behaviour lives in the child modules) ----------

/// `.ick` magic (all versions — `version` in the header discriminates).
pub const ICK_MAGIC: [u8; 8] = *b"INFICK1\0";
/// Format version for cells without tiered namespaces (M2 shape,
/// byte-identical — the degenerate case is absence, ADR-0057 D3).
pub const ICK_VERSION: u16 = 1;
/// Format version once address-reference sections may appear (M4-S12,
/// ADR-0057 D3). v2 readers read v1 files; v1 readers refuse v2 typed.
pub const ICK_VERSION_V2: u16 = 2;
/// Format v3 (M4.5-S36, ADR-0088 D3): the v2 vocabulary with every block
/// — header, sections, footer — starting on an [`ICK_BLOCK_ALIGN`]
/// boundary and zero-padded to the next one, so a sealed block is one
/// legal `O_DIRECT` write. Field layouts are unchanged; only the hop
/// rule differs (`align_up(len)` instead of `len`), and the padding is
/// outside every CRC's extent and asserted zero by the reader.
pub const ICK_VERSION_V3: u16 = 3;
/// Block alignment of the v3 container — the log's frame alignment
/// (`O_DIRECT` needs offsets, lengths, and buffer bases on it).
pub const ICK_BLOCK_ALIGN: usize = crate::frame::FRAME_ALIGN as usize;

/// Rounds a block length up to the next [`ICK_BLOCK_ALIGN`] boundary.
#[must_use]
pub const fn ick_align_up(len: usize) -> usize {
    len.div_ceil(ICK_BLOCK_ALIGN) * ICK_BLOCK_ALIGN
}

/// The section-body bound shared by the writer and the default-configured
/// loader (ADR-0117 D1): one maximal staging record (the frame bound —
/// `StagingRing::new` asserts the capacity under it) plus
/// [`ICK_SECTION_SLACK`], so a record at the staging ceiling and its
/// expiry companion always fit an empty section. `seal_section` asserts
/// it; the walker seals before a record that would breach it.
pub const ICK_MAX_SECTION_BYTES: u32 = crate::frame::DEFAULT_MAX_FRAME_LEN + ICK_SECTION_SLACK;
/// Room above one maximal record for its expiry record (≤ 274 B at the
/// store's 255 B key bound) — one alignment block, no arithmetic on the
/// record encoding.
pub const ICK_SECTION_SLACK: u32 = ICK_BLOCK_ALIGN as u32;

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
/// Index-sidecar section (v2 only — M4.5-S06, ADR-0073 D1 activates the
/// reservation, ADR-0078 D2 owns the schema): one converged index's
/// `(typed key bytes, entry_ref)` pairs, strictly ascending, one index
/// per section, possibly many sections per index. The only *soft*
/// body class in the file (ADR-0073 D6): the stored CRC folds into the
/// digest before verification, and body damage degrades to a rebuild of
/// that projection — never a refused boot. body := meta 36 B · entries.
const BLOCK_IDXSIDECAR: u8 = 6;
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
/// Index-sidecar body meta (ADR-0078 D2): ns u32 · index id u32 ·
/// generation u64 · key-encoding version u16 · key scheme u8 · flags u8
/// · entries_before u64 · total_entries u64.
const IDXSIDECAR_META_LEN: usize = 4 + 4 + 8 + 2 + 1 + 1 + 8 + 8;
/// Structural cap on a sidecar key (`ORDERED_KEY_MAX`, restated here —
/// `inf-log` never sees the tree; the two constants are cross-checked
/// by the S06 round-trip test).
pub const IDXSIDECAR_KEY_MAX: usize = 1024;
/// Fixed8 sidecar entry: key 8 B · entry_ref u64.
const IDXSIDECAR_FIXED_ENTRY_LEN: usize = 8 + 8;
/// `flags` bit 0: this is the index's last section; `total_entries` is
/// meaningful. Any other bit is a body-class failure within v1.
const IDXSIDECAR_FLAG_FINAL: u8 = 0x01;
/// `key_scheme` values (ADR-0078 D2).
const IDXSIDECAR_SCHEME_FIXED8: u8 = 0;
const IDXSIDECAR_SCHEME_VAR: u8 = 1;
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
/// The derived checkpoint interval (M4.5-S36, ADR-0088 D4):
/// `interval = clamp(α × ckpt_bytes_last, floor, replay_bytes_per_s ×
/// replay_budget_s)`, and a second trigger on records staged since the
/// last begin at `replay_records_per_s × replay_budget_s`. α = 2 bounds
/// the checkpoint's share of device writes to half the log's — (log +
/// checkpoint) / log ≤ 1.5 by construction; the two caps keep recovery
/// inside the boot gate in the same expression (recovery is bound by
/// record count — 476 k records/s/cell measured — and the M2 replay row
/// by bytes; either alone lets the other shape escape).
pub const DEFAULT_CKPT_ALPHA: u64 = 2;
/// The M2 replay gate's rate (≥ 1 GB/s/cell replay).
pub const DEFAULT_REPLAY_BYTES_PER_S: u64 = 1 << 30;
/// The measured record replay rate, rounded down (ops doc: 476 k/s/cell).
pub const DEFAULT_REPLAY_RECORDS_PER_S: u64 = 400_000;
/// The replay share of the 15 s boot gate: 15 s minus the `Start` row's
/// 5 s bar minus a 5 s `.ick` load allowance — three named terms.
pub const DEFAULT_REPLAY_BUDGET_S: u64 = 15 - 5 - 5;

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
    /// The body size a section may not be staged past (ADR-0117 D1):
    /// the walker seals first when the next record would breach it.
    /// Defaults to [`ICK_MAX_SECTION_BYTES`] and may never exceed it
    /// (asserted at stream construction); the DST lowers it so every
    /// record boundary becomes a split point.
    pub section_bound: u32,
    /// Trigger: staged log bytes since the last completed checkpoint.
    pub interval_bytes: u64,
    /// Hard cap on bytes streamed per MAINTAIN slice (budget in bytes —
    /// the deficit scheduler's units convert against this, ADR-0016 D5).
    pub slice_bytes: u32,
    /// Streaming pace in bytes/second (0 = unpaced — tests/sync tier).
    /// With a device model present the budget governs instead (ADR-0088
    /// D5): the server zeroes this when `DeviceModel` is probed.
    pub stream_bytes_per_sec: u32,
    /// ADR-0088 D4: the interval derivation's α (0 = the floor alone,
    /// i.e. the pre-S36 fixed trigger), the replay rates and budget the
    /// caps derive from.
    pub alpha: u64,
    pub replay_bytes_per_s: u64,
    pub replay_records_per_s: u64,
    pub replay_budget_s: u64,
}

impl Default for CkptConfig {
    fn default() -> CkptConfig {
        CkptConfig {
            section_bytes: DEFAULT_SECTION_BYTES,
            section_bound: ICK_MAX_SECTION_BYTES,
            interval_bytes: DEFAULT_CKPT_INTERVAL_BYTES,
            slice_bytes: DEFAULT_CKPT_SLICE_BYTES,
            stream_bytes_per_sec: DEFAULT_CKPT_STREAM_BYTES_PER_SEC,
            alpha: DEFAULT_CKPT_ALPHA,
            replay_bytes_per_s: DEFAULT_REPLAY_BYTES_PER_S,
            replay_records_per_s: DEFAULT_REPLAY_RECORDS_PER_S,
            replay_budget_s: DEFAULT_REPLAY_BUDGET_S,
        }
    }
}

impl CkptConfig {
    /// The byte cap of the derived interval: `replay_bytes_per_s ×
    /// replay_budget_s` (0 = uncapped when either term is 0).
    #[must_use]
    pub const fn cap_bytes(&self) -> u64 {
        self.replay_bytes_per_s.saturating_mul(self.replay_budget_s)
    }

    /// The record cap: records staged since the last begin that force a
    /// checkpoint regardless of bytes (0 = disabled).
    #[must_use]
    pub const fn cap_records(&self) -> u64 {
        self.replay_records_per_s.saturating_mul(self.replay_budget_s)
    }

    /// The derived interval (ADR-0088 D4): `clamp(α × ckpt_bytes_last,
    /// interval_bytes, cap_bytes)`. `interval_bytes == 0` (manual only)
    /// stays 0; `alpha == 0` or no prior checkpoint yields the floor.
    /// Release-asserted inside `[floor, cap]` — the S27 lesson in code.
    #[must_use]
    pub fn derive_interval(&self, ckpt_bytes_last: u64) -> u64 {
        let floor = self.interval_bytes;
        if floor == 0 {
            return 0;
        }
        let cap = self.cap_bytes();
        let wanted = self.alpha.saturating_mul(ckpt_bytes_last);
        let mut interval = wanted.max(floor);
        if cap > 0 {
            interval = interval.min(cap.max(floor));
        }
        assert!(interval >= floor, "derived checkpoint interval below its floor");
        assert!(
            cap == 0 || interval <= cap.max(floor),
            "derived checkpoint interval above its cap"
        );
        interval
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

/// One checkpoint block buffer whose content base is [`ICK_BLOCK_ALIGN`]-
/// aligned (ADR-0088 D3 — the `FrameBuilder` shape: a `Vec` with one
/// alignment of leading slack, the content at `at..`, no unsafe). Every
/// growth goes through [`Block::with_vec`], which re-bases the content
/// if the `Vec` moved — so a staged record that outruns the section
/// target (the documented growth case) never leaves the base unaligned.
/// Derefs to the content slice, so slice reads/writes are unchanged.
struct Block {
    buf: Vec<u8>,
    at: usize,
}

impl Block {
    fn with_capacity(capacity: usize) -> Block {
        let mut buf: Vec<u8> = Vec::with_capacity(capacity + 2 * ICK_BLOCK_ALIGN);
        let at = buf.as_ptr().align_offset(ICK_BLOCK_ALIGN);
        debug_assert!(at < ICK_BLOCK_ALIGN, "an aligned base fits the leading slack");
        buf.resize(at, 0);
        Block { buf, at }
    }

    /// Run a `Vec`-mutating closure, then re-base the content if the
    /// allocation moved (the alignment offset of a new allocation is
    /// arbitrary).
    fn with_vec(&mut self, f: impl FnOnce(&mut Vec<u8>)) {
        f(&mut self.buf);
        self.realign();
    }

    fn realign(&mut self) {
        // One alignment of headroom first, so the shift below cannot
        // itself reallocate (which would move the base again).
        self.buf.reserve(ICK_BLOCK_ALIGN);
        let want = self.buf.as_ptr().align_offset(ICK_BLOCK_ALIGN);
        if want == self.at {
            return;
        }
        let content = self.buf.len() - self.at;
        if want > self.at {
            self.buf.resize(want + content, 0);
        }
        self.buf.copy_within(self.at..self.at + content, want);
        self.buf.truncate(want + content);
        self.at = want;
        debug_assert_eq!(self.buf[self.at..].as_ptr().align_offset(ICK_BLOCK_ALIGN), 0);
    }

    fn clear(&mut self) {
        self.buf.truncate(self.at);
    }

    /// Drop a buffer that grew past its nominal capacity back to it (the
    /// v0.4.0 soak found `ckpt_buffer_bytes` ratcheting to 4× — L5).
    fn shrink_to(&mut self, nominal: usize) {
        debug_assert_eq!(self.buf.len(), self.at, "shrink on a non-empty block");
        if self.buf.capacity() > nominal + 2 * ICK_BLOCK_ALIGN {
            self.buf = Vec::<u8>::with_capacity(nominal + 2 * ICK_BLOCK_ALIGN);
            self.at = self.buf.as_ptr().align_offset(ICK_BLOCK_ALIGN);
            self.buf.resize(self.at, 0);
        }
    }

    fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.with_vec(|v| v.extend_from_slice(bytes));
    }

    fn push(&mut self, byte: u8) {
        self.with_vec(|v| v.push(byte));
    }

    /// Resize the *content* to `len` bytes.
    fn resize(&mut self, len: usize, value: u8) {
        let at = self.at;
        self.with_vec(|v| v.resize(at + len, value));
    }

    /// Zero-pad the content to the next [`ICK_BLOCK_ALIGN`] boundary;
    /// returns the padding bytes added.
    fn pad_to_alignment(&mut self) -> usize {
        let len = self.len();
        let padded = ick_align_up(len);
        self.resize(padded, 0);
        padded - len
    }

    fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl std::ops::Deref for Block {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf[self.at..]
    }
}

impl std::ops::DerefMut for Block {
    fn deref_mut(&mut self) -> &mut [u8] {
        let at = self.at;
        &mut self.buf[at..]
    }
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
    /// One converged index's pair stream (tag 0x06) — sections seal at
    /// every index boundary (M4.5-S06, ADR-0078 D2).
    IdxSidecar { ns: u32, index_id: u32, generation: u64 },
}

/// The writer-facing identity of one index's sidecar stream (M4.5-S06,
/// ADR-0078 D2): every field lands in every section's body meta, so
/// sections are independently attributable — the damage policy depends
/// on it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IdxSidecarMeta {
    pub ns: u32,
    pub index_id: u32,
    pub generation: u64,
    pub key_encoding_version: u16,
    /// True: `Fixed8` (keys exactly 8 bytes); false: `VarKey`
    /// (length-prefixed, ≤ [`IDXSIDECAR_KEY_MAX`]).
    pub fixed8: bool,
}

/// The checkpoint-buffer domain of one cell: a double-buffered section
/// pair. Buffers are `Vec`s sized to the section target at construction;
/// the *staging* buffer may grow past the target mid-emission (a record
/// larger than the remaining slack — the walker seals right after), while
/// the *leased* buffer is immutable until release. `resident_bytes` is the
/// exact `ckpt_buffer_bytes` gauge (L5).
pub struct IckStream {
    bufs: [Block; 2],
    /// The nominal per-buffer capacity (`release` shrinks back to it).
    nominal_capacity: usize,
    /// v3: blocks are padded to [`ICK_BLOCK_ALIGN`] at seal.
    aligned: bool,
    /// Zero bytes sealed as v3 block padding (`ckpt_padding_bytes`).
    padding_bytes: u64,
    staging: usize,
    in_flight: Option<InFlight>,
    generation: u64,
    section_target: u32,
    /// The split point `fits` answers against (ADR-0117 D1).
    section_bound: u32,
    file_offset: u64,
    staged_records: u32,
    staged_class: Option<SectionClass>,
    /// Last staged blob-ref address — the writer half of the tag-0x05
    /// strictly-ascending canon (decode enforces the reader half).
    staged_blob_prev_addr: u64,
    /// The pending sidecar section's first ordinal — the writer half of
    /// the tag-0x06 contiguity canon (M4.5-S06, ADR-0078 D2).
    staged_idx_entries_before: u64,
    /// Last staged sidecar pair — the writer half of the ascending
    /// canon (buffer reused across sections; cleared at first stage).
    staged_idx_prev_key: Vec<u8>,
    staged_idx_prev_ref: u64,
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

    /// A v3 stream (M4.5-S36, ADR-0088 D3): the v2 vocabulary on
    /// [`ICK_BLOCK_ALIGN`]-aligned, zero-padded blocks — what the reactor
    /// tier writes `O_DIRECT`.
    #[must_use]
    pub fn new_v3(cfg: &CkptConfig) -> IckStream {
        Self::with_version(cfg, ICK_VERSION_V3)
    }

    /// The stream's container version.
    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Zero bytes sealed as v3 padding so far.
    #[must_use]
    pub fn padding_bytes(&self) -> u64 {
        self.padding_bytes
    }

    /// Header block length for `ns_count` namespaces, padded under v3 —
    /// what the driver offers the device budget before `begin`.
    #[must_use]
    pub fn header_block_len(&self, ns_count: usize) -> usize {
        let raw = HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN;
        if self.aligned { ick_align_up(raw) } else { raw }
    }

    /// The pending section's sealed length (header + body + CRC, padded
    /// under v3) — what the driver offers the device budget before
    /// `seal_section`.
    #[must_use]
    pub fn pending_block_len(&self) -> usize {
        let raw = self.bufs[self.staging].len() + CRC_LEN;
        if self.aligned { ick_align_up(raw) } else { raw }
    }

    /// The footer block length, padded under v3.
    #[must_use]
    pub fn footer_block_len(&self) -> usize {
        let raw = FOOTER_FIXED_LEN + self.entries_per_ns.len() * 12 + 8 + CRC_LEN;
        if self.aligned { ick_align_up(raw) } else { raw }
    }

    fn with_version(cfg: &CkptConfig, version: u16) -> IckStream {
        assert!(
            cfg.section_bound <= ICK_MAX_SECTION_BYTES,
            "section bound exceeds the loader bound"
        );
        let raw = cfg.section_bytes as usize + SECTION_HEADER_LEN + CRC_LEN;
        let aligned = version >= ICK_VERSION_V3;
        let capacity = if aligned { ick_align_up(raw) } else { raw };
        IckStream {
            bufs: [Block::with_capacity(capacity), Block::with_capacity(capacity)],
            nominal_capacity: capacity,
            aligned,
            padding_bytes: 0,
            staging: 0,
            in_flight: None,
            generation: 0,
            section_target: cfg.section_bytes,
            section_bound: cfg.section_bound,
            file_offset: 0,
            staged_records: 0,
            staged_class: None,
            staged_blob_prev_addr: 0,
            staged_idx_entries_before: 0,
            staged_idx_prev_key: Vec::new(),
            staged_idx_prev_ref: 0,
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
        assert!(self.in_flight.is_none(), "begin with a section in flight");
        assert!(self.staged_class.is_none(), "begin with a staged class");
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
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        let buf = &mut self.bufs[self.staging];
        if self.staged_class.is_none() {
            debug_assert!(buf.is_empty());
            self.staged_class = Some(SectionClass::Images);
            buf.resize(SECTION_HEADER_LEN, 0); // header placeholder, filled at seal
        }
        assert_eq!(self.staged_class, Some(SectionClass::Images), "seal before switching class");
        buf.with_vec(|v| view.encode_into(v));
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
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        assert!(self.version >= ICK_VERSION_V2, "addr refs are a v2 vocabulary");
        assert!(addr < walk_watermark, "a ref must sit below its walk watermark");
        assert!(walk_watermark < ADDR_LIMIT, "watermarks are 48-bit");
        let buf = &mut self.bufs[self.staging];
        if self.staged_class.is_none() {
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
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        assert!(self.version >= ICK_VERSION_V2, "live-set sections are a v2 vocabulary");
        assert!(dead_bytes <= data_len, "dead bytes exceed the file's data bytes");
        let buf = &mut self.bufs[self.staging];
        if self.staged_class.is_none() {
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
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        assert!(self.version >= ICK_VERSION_V2, "blob-ref sections are a v2 vocabulary");
        assert!(addr < ADDR_LIMIT, "logical addresses are 48-bit");
        assert!(len > 0, "an extent reference names at least one byte");
        let buf = &mut self.bufs[self.staging];
        if self.staged_class.is_none() {
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

    /// Appends one index-sidecar pair (M4.5-S06, ADR-0078 D2). One
    /// index per section: the caller seals at every index boundary.
    /// `ordinal` is the pair's position in the index's whole emission —
    /// the section meta records the first one (`entries_before`) and
    /// continuity is asserted per stage (the writer half of the reader's
    /// contiguity canon). Ascending order is asserted in release like
    /// the tag-0x05 canon — walker bugs, never input.
    ///
    /// # Panics
    /// Panics on a v1 stream, a pending section of another class or
    /// index, an ordinal gap, a non-ascending pair, or a key outside
    /// the scheme's bounds.
    pub fn stage_idx_entry(
        &mut self,
        meta: &IdxSidecarMeta,
        ordinal: u64,
        key: &[u8],
        entry_ref: u64,
    ) {
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        assert!(self.version >= ICK_VERSION_V2, "index sidecars are a v2 vocabulary");
        if meta.fixed8 {
            assert!(key.len() == 8, "Fixed8 sidecar keys are exactly 8 bytes");
        } else {
            assert!(key.len() <= IDXSIDECAR_KEY_MAX, "sidecar key exceeds the structural cap");
        }
        let class = SectionClass::IdxSidecar {
            ns: meta.ns,
            index_id: meta.index_id,
            generation: meta.generation,
        };
        if self.staged_class.is_none() {
            self.open_idx_section(meta, ordinal, 0);
        }
        assert_eq!(
            self.staged_class,
            Some(class),
            "seal before switching class, index, or generation"
        );
        assert_eq!(
            ordinal,
            self.staged_idx_entries_before + u64::from(self.staged_records),
            "sidecar ordinals are contiguous within a section"
        );
        assert_eq!(
            self.bufs[self.staging][SECTION_HEADER_LEN + 19],
            0,
            "no entries after a FINAL marker"
        );
        assert!(
            self.staged_records == 0
                || (key, entry_ref)
                    > (self.staged_idx_prev_key.as_slice(), self.staged_idx_prev_ref),
            "sidecar pairs ascend strictly"
        );
        self.staged_idx_prev_key.clear();
        self.staged_idx_prev_key.extend_from_slice(key);
        self.staged_idx_prev_ref = entry_ref;
        let buf = &mut self.bufs[self.staging];
        if !meta.fixed8 {
            buf.extend_from_slice(
                &u16::try_from(key.len()).expect("sidecar key fits u16").to_le_bytes(),
            );
        }
        buf.extend_from_slice(key);
        buf.extend_from_slice(&entry_ref.to_le_bytes());
        self.staged_records += 1;
        // Deliberately NOT entries_per_ns and (at seal) NOT
        // records_total: 0x06 is the only soft body class — no body
        // byte may be load-bearing for the file-level audit (ADR-0078
        // D2's footer-accounting deviation).
    }

    /// Marks the pending sidecar section as the index's last, recording
    /// the whole-stream cardinality (ADR-0078 D2). With no pending
    /// section (the boundary landed exactly on a seal, or the tree was
    /// empty) a zero-entry FINAL section is opened — `entry_count == 0`
    /// is legal for tag 0x06 exactly in that configuration.
    ///
    /// # Panics
    /// Panics on a v1 stream or a pending section of another class or
    /// index.
    pub fn stage_idx_final(&mut self, meta: &IdxSidecarMeta, total_entries: u64) {
        assert!(self.header_written, "stage before the header");
        assert!(!self.finished, "stage after finish");
        assert!(self.version >= ICK_VERSION_V2, "index sidecars are a v2 vocabulary");
        let class = SectionClass::IdxSidecar {
            ns: meta.ns,
            index_id: meta.index_id,
            generation: meta.generation,
        };
        if self.staged_class.is_none() {
            self.open_idx_section(meta, total_entries, total_entries);
            self.patch_idx_final(total_entries);
            return;
        }
        assert_eq!(self.staged_class, Some(class), "seal before finalizing another index");
        assert_eq!(
            total_entries,
            self.staged_idx_entries_before + u64::from(self.staged_records),
            "the FINAL total equals the emitted ordinal count"
        );
        self.patch_idx_final(total_entries);
    }

    /// Writes a fresh sidecar body meta into the staging buffer.
    fn open_idx_section(&mut self, meta: &IdxSidecarMeta, entries_before: u64, total: u64) {
        let buf = &mut self.bufs[self.staging];
        debug_assert!(buf.is_empty());
        self.staged_class = Some(SectionClass::IdxSidecar {
            ns: meta.ns,
            index_id: meta.index_id,
            generation: meta.generation,
        });
        self.staged_idx_entries_before = entries_before;
        self.staged_idx_prev_key.clear();
        self.staged_idx_prev_ref = 0;
        buf.resize(SECTION_HEADER_LEN, 0);
        buf.extend_from_slice(&meta.ns.to_le_bytes());
        buf.extend_from_slice(&meta.index_id.to_le_bytes());
        buf.extend_from_slice(&meta.generation.to_le_bytes());
        buf.extend_from_slice(&meta.key_encoding_version.to_le_bytes());
        buf.push(if meta.fixed8 { IDXSIDECAR_SCHEME_FIXED8 } else { IDXSIDECAR_SCHEME_VAR });
        buf.push(0); // flags — patched by `stage_idx_final`.
        buf.extend_from_slice(&entries_before.to_le_bytes());
        buf.extend_from_slice(&total.to_le_bytes());
        debug_assert_eq!(buf.len(), SECTION_HEADER_LEN + IDXSIDECAR_META_LEN);
    }

    /// Patches FINAL + `total_entries` into the pending section's meta.
    fn patch_idx_final(&mut self, total_entries: u64) {
        let buf = &mut self.bufs[self.staging];
        let flags_at = SECTION_HEADER_LEN + 19;
        assert_eq!(buf[flags_at], 0, "an index finalizes once");
        buf[flags_at] = IDXSIDECAR_FLAG_FINAL;
        let total_at = SECTION_HEADER_LEN + 28;
        buf[total_at..total_at + 8].copy_from_slice(&total_entries.to_le_bytes());
    }

    /// True once the staging section reached its seal target.
    #[must_use]
    pub fn section_full(&self) -> bool {
        self.staged_body_bytes() >= self.section_target
    }

    /// True when `bytes` more of body keep the pending section within
    /// the configured bound (ADR-0117 D1) — the walker's stage-or-seal
    /// test before every image. An empty section always takes one legal
    /// record: the format bound holds a maximal record plus its expiry
    /// companion, and the configured bound only moves the split point.
    #[must_use]
    pub fn fits(&self, bytes: usize) -> bool {
        self.staged_class.is_none()
            || self.staged_body_bytes() as usize + bytes <= self.section_bound as usize
    }

    /// True while any section is open in staging (the seal-first signal
    /// for phase-boundary drivers — M4.5-S06).
    #[must_use]
    pub fn has_pending_section(&self) -> bool {
        self.staged_class.is_some()
    }

    /// The pending sidecar stream's identity `(ns, index id,
    /// generation)`, or `None` when the pending section is another
    /// class (or nothing is staged) — the checkpoint driver's
    /// continue-vs-seal-first test at sidecar boundaries (M4.5-S06).
    #[must_use]
    pub fn pending_idx_stream(&self) -> Option<(u32, u32, u64)> {
        match self.staged_class {
            Some(SectionClass::IdxSidecar { ns, index_id, generation }) => {
                Some((ns, index_id, generation))
            }
            _ => None,
        }
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

    /// True when `seal_section` may run now. A pending section usually
    /// holds records, but a zero-entry FINAL sidecar section (ADR-0078
    /// D2's empty-tree shape) is a sealable section too — the open
    /// class, not the record count, is the truth.
    #[must_use]
    pub fn can_seal(&self) -> bool {
        self.staged_class.is_some() && self.in_flight.is_none()
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
        let class = self.staged_class.take().expect("can_seal implies a class");
        let buf = &mut self.bufs[self.staging];
        let body_len = u32::try_from(buf.len() - SECTION_HEADER_LEN).expect("body fits u32");
        // ADR-0117 D1: a walker that staged past the loader bound fails
        // here, at write time — never at the next boot.
        assert!(body_len <= ICK_MAX_SECTION_BYTES, "section body exceeds the loader bound");
        buf[0] = match class {
            SectionClass::Images => BLOCK_SECTION,
            SectionClass::Refs { .. } => BLOCK_ADDR_SECTION,
            SectionClass::LiveSet { .. } => BLOCK_LIVESET,
            SectionClass::BlobRefs { .. } => BLOCK_BLOBREF,
            SectionClass::IdxSidecar { .. } => BLOCK_IDXSIDECAR,
        };
        buf[1..5].copy_from_slice(&body_len.to_le_bytes());
        buf[5..9].copy_from_slice(&self.staged_records.to_le_bytes());
        let crc = crc32c(buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.digest = fold_digest(self.digest, crc);
        self.sections += 1;
        // Sidecar entries stay out of `records_total`: 0x06 is the only
        // soft body class, and its counts must not be load-bearing for
        // the footer audit (ADR-0078 D2).
        if !matches!(class, SectionClass::IdxSidecar { .. }) {
            self.records_total += u64::from(self.staged_records);
        }
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
        assert!(self.header_written, "finish before the header");
        assert!(!self.finished, "finish twice");
        assert!(self.staged_class.is_none(), "finish with a partial section staged");
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
        if self.aligned {
            // ADR-0088 D3: the block is one aligned `O_DIRECT` write —
            // every padding byte is written as zero, never left over.
            let pad = self.bufs[sealed].pad_to_alignment();
            self.padding_bytes += pad as u64;
            debug_assert_eq!(self.file_offset % ICK_BLOCK_ALIGN as u64, 0, "aligned offset");
            debug_assert_eq!(self.bufs[sealed].as_ptr().align_offset(ICK_BLOCK_ALIGN), 0);
        }
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
        self.bufs[in_flight.buf].shrink_to(self.nominal_capacity);
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

    /// Creates a **v3** checkpoint writer (M4.5-S36, ADR-0088 D3): the v2
    /// vocabulary on aligned, zero-padded blocks — what the reactor tier
    /// writes `O_DIRECT`; the sync tier writes it buffered and
    /// byte-identical (tests).
    ///
    /// # Errors
    /// File creation or write failure.
    pub fn create_v3(
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
            IckStream::new_v3(cfg),
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
        if self.stream.staged_class.is_some_and(|class| class != SectionClass::Images)
            || !self.stream.fits(view.encoded_len())
        {
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

    /// Appends one index-sidecar pair (M4.5-S06, ADR-0078 D2), sealing
    /// the pending section first when it holds another class, index, or
    /// generation.
    ///
    /// # Errors
    /// Write failure from the fs seam.
    pub fn append_idx_entry(
        &mut self,
        meta: &IdxSidecarMeta,
        ordinal: u64,
        key: &[u8],
        entry_ref: u64,
    ) -> io::Result<()> {
        let key_class = SectionClass::IdxSidecar {
            ns: meta.ns,
            index_id: meta.index_id,
            generation: meta.generation,
        };
        if self.stream.staged_class.is_some_and(|class| class != key_class) {
            self.write_sealed()?;
        }
        self.stream.stage_idx_entry(meta, ordinal, key, entry_ref);
        if self.stream.section_full() {
            self.write_sealed()?;
        }
        Ok(())
    }

    /// Finalizes an index's sidecar stream (ADR-0078 D2) and seals the
    /// section — FINAL ends the stream, so nothing else may join it.
    ///
    /// # Errors
    /// Write failure from the fs seam.
    pub fn append_idx_final(
        &mut self,
        meta: &IdxSidecarMeta,
        total_entries: u64,
    ) -> io::Result<()> {
        let key_class = SectionClass::IdxSidecar {
            ns: meta.ns,
            index_id: meta.index_id,
            generation: meta.generation,
        };
        if self.stream.staged_class.is_some_and(|class| class != key_class) {
            self.write_sealed()?;
        }
        self.stream.stage_idx_final(meta, total_entries);
        self.write_sealed()
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

    /// F-L03-02 (review 2026-08-30; ADR-0117 D1): a partial section
    /// never grows past the loader's bound. A 100 B record leaves a
    /// 4 KiB-target section open; the next record is one byte under the
    /// bound on its own, so staging it behind the first would seal a
    /// body the default-configured loader refuses (`SectionTooLarge`) —
    /// the writer must seal first. The record is the format's largest
    /// legal image (one frame + the slack), so the test also proves a
    /// maximal record still fits an empty section.
    #[test]
    fn a_partial_section_never_grows_past_the_loader_bound() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            fs.clone(),
            dir,
            &CkptConfig { section_bytes: 4096, ..Default::default() },
            0,
            9,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v3");
        let ns = NsId(16);
        let small = vec![b'a'; 100];
        w.append(&RecordView::StringPostImage { ns, key: b"small", value: &small })
            .expect("append");
        let head = RecordView::StringPostImage { ns, key: b"big", value: b"" }.encoded_len();
        let big = vec![b'b'; ICK_MAX_SECTION_BYTES as usize - head - 3];
        let rec = RecordView::StringPostImage { ns, key: b"big", value: &big };
        assert!(rec.encoded_len() <= ICK_MAX_SECTION_BYTES as usize, "a legal record");
        w.append(&rec).expect("append");
        let summary = w.finish().expect("finish");
        let mut seen = 0usize;
        let (_, audit) =
            read_ick(&fs, &dir.join(ick_file_name(9)), IckReaderConfig::default(), |view| {
                if let RecordView::StringPostImage { value, .. } = view {
                    seen += value.len();
                }
                Ok::<(), ()>(())
            })
            .expect("the writer's own default-configured loader accepts every section");
        assert_eq!(audit, summary);
        assert_eq!(seen, small.len() + big.len());
        assert_eq!(summary.sections, 2, "the big record opened its own section");
    }

    /// F-L03-03 (review 2026-08-30; ADR-0028 A1): the footer probe locates
    /// the footer by the footer's own namespace count, so a durable
    /// namespace with no live entries (`write_sample` names 16 and 17,
    /// stages only 16) no longer defeats it into the dependent hop chain.
    #[test]
    fn the_footer_probe_hits_with_an_empty_namespace() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let summary = write_sample(&fs, dir);
        let path = dir.join(ick_file_name(7));
        let (counts, probe_hit) =
            read_ick_counts_probed(&fs, &path, IckReaderConfig::default()).expect("peek");
        assert_eq!(counts, summary.entries_per_ns);
        assert!(probe_hit, "an empty namespace must not defeat the end-of-file probe");

        // Every namespace populated: the probe hits as before.
        let full = MemFs::new();
        full.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            full.clone(),
            dir,
            &small_cfg(),
            0,
            8,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16, 17, 18],
        )
        .expect("create v3");
        for ns in [16u32, 17, 18] {
            w.append(&RecordView::StringPostImage { ns: NsId(ns), key: b"k", value: b"v" })
                .expect("append");
        }
        let summary = w.finish().expect("finish");
        let (counts, probe_hit) =
            read_ick_counts_probed(&full, &dir.join(ick_file_name(8)), IckReaderConfig::default())
                .expect("peek");
        assert_eq!(counts, summary.entries_per_ns);
        assert!(probe_hit, "a fully populated v3 file probes directly");

        // Trailing bytes still fall back to the hop, honestly reported.
        let bytes = fs.contents(&path).expect("ick bytes");
        let mut padded = bytes.clone();
        padded.extend_from_slice(b"junk");
        let pad = MemFs::new();
        pad.create_dir_all(dir).unwrap();
        use crate::fs::{SegmentFile, SegmentFs as _};
        let mut f = pad.create_meta(&path).expect("create");
        f.write_at(0, &padded).expect("write");
        let (_, probe_hit) =
            read_ick_counts_probed(&pad, &path, IckReaderConfig::default()).expect("hop");
        assert!(!probe_hit, "trailing bytes defeat the probe; the hop finds the footer");
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
        let meta = IdxSidecarMeta {
            ns: 16,
            index_id: 1,
            generation: 1,
            key_encoding_version: 1,
            fixed8: true,
        };
        w.append_idx_entry(&meta, 0, &7u64.to_be_bytes(), 42).expect("idx entry");
        w.append_idx_final(&meta, 1).expect("idx final");
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

    /// The five section classes written by `seal_section`, one each, in
    /// a v2 file (no block padding, so every hop is exact).
    fn write_every_v2_class(fs: &MemFs, dir: &Path) -> (IckSummary, std::path::PathBuf) {
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
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v" })
            .expect("image");
        w.append_ref(16, 4096, 0xfeed_beef, 128).expect("addr ref");
        w.append_live_set(16, 1, 4096, 0, true).expect("live set");
        w.append_blob_ref(16, 100, 7, 4096).expect("blob ref");
        let meta = IdxSidecarMeta {
            ns: 16,
            index_id: 1,
            generation: 1,
            key_encoding_version: 1,
            fixed8: true,
        };
        w.append_idx_entry(&meta, 0, &7u64.to_be_bytes(), 42).expect("idx entry");
        w.append_idx_final(&meta, 1).expect("idx final");
        (w.finish().expect("finish"), dir.join(ick_file_name(21)))
    }

    /// Review L03 (batch 34): one section block costs two dependent
    /// reads — the header, then the rest of the block — for every class.
    /// The pre-batch-34 arms read the tag, the header and the whole block
    /// (three), on the cold recovery path the M2 boot gate measures.
    #[test]
    fn every_section_block_costs_two_dependent_reads() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let (summary, path) = write_every_v2_class(&fs, dir);
        assert_eq!(summary.sections, 5, "one section per class");
        let mut reader = IckReader::open(&fs, &path, IckReaderConfig::default()).expect("open");
        let mut sections = 0u32;
        loop {
            let before = fs.reads();
            let step = reader
                .next_step_hybrid(
                    |_| Ok::<(), ()>(()),
                    |_| Ok(()),
                    |_| Ok(()),
                    |_| Ok(()),
                    |_| Ok(()),
                )
                .expect("step");
            let reads = fs.reads() - before;
            match step {
                IckStep::Section { .. } => {
                    sections += 1;
                    assert_eq!(reads, 2, "section {sections}: header + block, nothing else");
                }
                IckStep::Done(_) => {
                    assert_eq!(reads, 3, "footer: dispatch header + fixed part + block");
                    break;
                }
            }
        }
        assert_eq!(sections, 5);
    }

    /// Review L03 style row 3: the loader bound is one check in one place.
    /// A section header of every class whose body length exceeds the
    /// configured bound is refused before any body byte is read — the
    /// "one arm forgot a check" class the five-arm loader once hit
    /// (ADR-0073 D4's hop arm).
    #[test]
    fn every_section_class_is_bounded_by_the_loader_config() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let (_, path) = write_every_v2_class(&fs, dir);
        let header_len = HEADER_FIXED_LEN + 4 + CRC_LEN;
        let header = fs.contents(&path).expect("ick bytes")[..header_len].to_vec();
        let cfg = IckReaderConfig { max_section_bytes: 64 };
        for tag in
            [BLOCK_SECTION, BLOCK_ADDR_SECTION, BLOCK_LIVESET, BLOCK_BLOBREF, BLOCK_IDXSIDECAR]
        {
            let mut image = header.clone();
            image.push(tag);
            image.extend_from_slice(&65u32.to_le_bytes());
            image.extend_from_slice(&1u32.to_le_bytes());
            image.extend_from_slice(&[0xaa; 65 + CRC_LEN]);
            let bad = MemFs::new();
            bad.create_dir_all(dir).unwrap();
            use crate::fs::{SegmentFile, SegmentFs as _};
            let mut f = bad.create_meta(&path).expect("create");
            f.write_at(0, &image).expect("write");
            let before = bad.reads();
            let err = read_ick_hybrid(
                &bad,
                &path,
                cfg,
                |_| Ok::<(), ()>(()),
                |_| Ok(()),
                |_| Ok(()),
                |_| Ok(()),
                |_| Ok(()),
            )
            .expect_err("an over-bound section is refused");
            assert!(
                matches!(
                    err,
                    IckApplyError::Read(IckReadError::SectionTooLarge { len: 65, max: 64 })
                ),
                "tag {tag:#04x}: {err:?}"
            );
            assert_eq!(
                bad.reads() - before,
                3,
                "tag {tag:#04x}: two header reads + one section header, no body read"
            );
        }
    }

    /// M4.5-S36 (ADR-0088 D3): the v3 container — every block starts on
    /// an `ICK_BLOCK_ALIGN` boundary, the file ends on one, records
    /// round-trip byte-identically, the audit reproduces the summary, the
    /// footer probe finds the padded footer, and `info.version` is 3.
    #[test]
    fn v3_round_trips_on_aligned_blocks_and_audits() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            fs.clone(),
            dir,
            &small_cfg(),
            3,
            7,
            Lsn::new(crate::lsn::SegmentId(2), 4096),
            &[16, 17],
        )
        .expect("create v3");
        for (key, value, exp) in sample_records() {
            let ns = NsId(16);
            w.append(&RecordView::StringPostImage { ns, key: &key, value: &value })
                .expect("append");
            if let Some(at) = exp {
                w.append(&RecordView::ExpireAt { ns, at_unix_ms: at, key: &key }).expect("append");
            }
        }
        let summary = w.finish().expect("finish");
        assert!(summary.sections > 1, "sample must span sections");
        assert_eq!(summary.bytes % ICK_BLOCK_ALIGN as u64, 0, "the file ends on a boundary");
        let path = dir.join(ick_file_name(7));
        let bytes = fs.contents(&path).expect("ick bytes");
        assert_eq!(bytes.len() as u64, summary.bytes);
        // Every block boundary carries a block tag or the magic.
        let mut at = 0usize;
        assert_eq!(&bytes[..8], &ICK_MAGIC);
        at += ick_align_up(HEADER_FIXED_LEN + 2 * 4 + CRC_LEN);
        let mut sections = 0u32;
        while bytes[at] != BLOCK_FOOTER {
            assert_eq!(bytes[at], BLOCK_SECTION, "a section tag on the boundary at {at}");
            let body_len = le_u32(&bytes[at + 1..at + 5]) as usize;
            let block = SECTION_HEADER_LEN + body_len + CRC_LEN;
            assert!(bytes[at + block..at + ick_align_up(block)].iter().all(|b| *b == 0));
            at += ick_align_up(block);
            sections += 1;
        }
        assert_eq!(sections, summary.sections);

        let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let (info, audit) = read_ick(&fs, &path, IckReaderConfig::default(), |view| {
            if let RecordView::StringPostImage { key, value, .. } = view {
                got.push((key.to_vec(), value.to_vec()));
            }
            Ok::<(), ()>(())
        })
        .expect("load");
        assert_eq!(info.version, ICK_VERSION_V3);
        assert_eq!(info.ns_ids, vec![16, 17]);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        let want: Vec<(Vec<u8>, Vec<u8>)> =
            sample_records().into_iter().map(|(k, v, _)| (k, v)).collect();
        assert_eq!(got, want, "records replay in file order, byte-identical");
        let counts = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("probe");
        assert_eq!(counts, summary.entries_per_ns, "the padded footer probe finds the footer");
        // The hop fallback across aligned hops agrees too.
        let mut padded = bytes.clone();
        padded.extend_from_slice(b"junk");
        let pad = MemFs::new();
        pad.create_dir_all(dir).unwrap();
        use crate::fs::{SegmentFile, SegmentFs as _};
        let mut f = pad.create_meta(&path).expect("create");
        f.write_at(0, &padded).expect("write");
        let counts =
            read_ick_counts(&pad, &path, IckReaderConfig::default()).expect("hop fallback");
        assert_eq!(counts, summary.entries_per_ns);
    }

    /// ADR-0088 D3: v3 padding is written, never left over — a non-zero
    /// pad byte is damage (`IckReadError::Padding`), the CRC's class.
    #[test]
    fn v3_refuses_a_non_zero_padding_byte() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            9,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v3");
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v" })
            .expect("image");
        w.finish().expect("finish");
        let path = dir.join(ick_file_name(9));
        let bytes = fs.contents(&path).expect("ick bytes");
        let header_len = HEADER_FIXED_LEN + 4 + CRC_LEN;
        for at in [header_len, ICK_BLOCK_ALIGN - 1, bytes.len() - 1] {
            assert_eq!(bytes[at], 0, "byte {at} is padding");
            let mut damaged = bytes.clone();
            damaged[at] = 0x5A;
            let dmg = MemFs::new();
            dmg.create_dir_all(dir).unwrap();
            use crate::fs::{SegmentFile, SegmentFs as _};
            let mut f = dmg.create_meta(&path).expect("create");
            f.write_at(0, &damaged).expect("write");
            let err = read_ick(&dmg, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
                .expect_err("non-zero padding is damage");
            assert!(
                matches!(err, IckApplyError::Read(IckReadError::Padding { .. })),
                "byte {at}: {err:?}"
            );
        }
    }

    /// ADR-0088 D3: every section class rides aligned blocks, and the v3
    /// reader's hybrid path replays a v2-vocabulary file written as v3.
    #[test]
    fn v3_covers_every_section_class() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            22,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v3");
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v" })
            .expect("image");
        w.append_ref(16, 4096, 0xfeed_beef, 128).expect("addr ref");
        w.append_live_set(16, 1, 4096, 0, true).expect("live set");
        w.append_blob_ref(16, 100, 7, 4096).expect("blob ref");
        let meta = IdxSidecarMeta {
            ns: 16,
            index_id: 1,
            generation: 1,
            key_encoding_version: 1,
            fixed8: true,
        };
        w.append_idx_entry(&meta, 0, &7u64.to_be_bytes(), 42).expect("idx entry");
        w.append_idx_final(&meta, 1).expect("idx final");
        let summary = w.finish().expect("finish");
        assert_eq!(summary.bytes % ICK_BLOCK_ALIGN as u64, 0);
        let path = dir.join(ick_file_name(22));
        let counts = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("probe");
        assert_eq!(counts, summary.entries_per_ns);
        let mut padded = fs.contents(&path).expect("ick bytes");
        padded.extend_from_slice(b"junk");
        let pad = MemFs::new();
        pad.create_dir_all(dir).unwrap();
        use crate::fs::{SegmentFile, SegmentFs as _};
        let mut f = pad.create_meta(&path).expect("create");
        f.write_at(0, &padded).expect("write");
        let counts =
            read_ick_counts(&pad, &path, IckReaderConfig::default()).expect("hop fallback");
        assert_eq!(counts, summary.entries_per_ns, "the aligned hop visits every class");
    }

    /// ADR-0088 D3: the growth case — a record larger than the section
    /// target reallocates the staging `Block`; the sealed block's base is
    /// still aligned and the record round-trips.
    #[test]
    fn v3_block_stays_aligned_when_a_record_outruns_the_section_target() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v3(
            fs.clone(),
            dir,
            &small_cfg(), // 64-byte sections
            0,
            23,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16],
        )
        .expect("create v3");
        let big = vec![b'x'; 40 << 10]; // 40 KiB — ten alignments past the target
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"big", value: &big })
            .expect("image");
        w.append(&RecordView::StringPostImage { ns: NsId(16), key: b"k", value: b"v" })
            .expect("image");
        let summary = w.finish().expect("finish");
        assert_eq!(summary.bytes % ICK_BLOCK_ALIGN as u64, 0);
        let mut got = Vec::new();
        let (_, audit) =
            read_ick(&fs, &dir.join(ick_file_name(23)), IckReaderConfig::default(), |view| {
                if let RecordView::StringPostImage { value, .. } = view {
                    got.push(value.len());
                }
                Ok::<(), ()>(())
            })
            .expect("load");
        assert_eq!(audit, summary);
        assert_eq!(got, vec![40 << 10, 1]);
    }

    /// ADR-0088 D4: the derived interval is inside `[floor, cap]` for
    /// every input, the floor alone before the first checkpoint, the cap
    /// when the dataset outgrows the replay budget, `0` when manual-only;
    /// the record cap and byte cap are the replay rates × the budget.
    #[test]
    fn derived_checkpoint_interval_is_clamped_to_the_recovery_gate() {
        let cfg = CkptConfig::default();
        let floor = cfg.interval_bytes;
        let cap = cfg.cap_bytes();
        assert_eq!(cap, (1 << 30) * 5);
        assert_eq!(cfg.cap_records(), 400_000 * 5);
        assert_eq!(cfg.derive_interval(0), floor, "no prior checkpoint ⇒ the floor");
        assert_eq!(cfg.derive_interval(floor / 4), floor, "small dataset ⇒ the floor");
        assert_eq!(cfg.derive_interval(300 << 20), 600 << 20, "α = 2 above the floor");
        assert_eq!(cfg.derive_interval(cap), cap, "cap binds");
        assert_eq!(cfg.derive_interval(u64::MAX), cap, "saturating, never above the cap");
        let mut seed = 0x5EED_1234_ABCD_EF01u64;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let interval = cfg.derive_interval(seed % (8 << 30));
            assert!((floor..=cap).contains(&interval));
        }
        let manual = CkptConfig { interval_bytes: 0, ..cfg };
        assert_eq!(manual.derive_interval(1 << 40), 0, "manual-only stays manual");
        // A floor above the cap: the floor wins (the operator asked for
        // it; the cap is a derivation, the floor an override).
        let tall = CkptConfig { interval_bytes: cap * 2, ..cfg };
        assert_eq!(tall.derive_interval(0), cap * 2);
        // α = 0 is the pre-S36 fixed trigger.
        let fixed = CkptConfig { alpha: 0, ..cfg };
        assert_eq!(fixed.derive_interval(1 << 40), floor);
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
            |_| panic!("no index-sidecar sections in this image"),
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
            |_| panic!("no index-sidecar sections in this image"),
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
            |_| panic!("no index-sidecar sections in this image"),
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

    /// Index-sidecar sections (tag 0x06, M4.5-S06, ADR-0078 D2/D3/D4):
    /// exact multi-section round trip across both key schemes plus the
    /// zero-entry FINAL shape; the footer-accounting exemption
    /// (`records_total` and the per-ns presize counts both untouched);
    /// the records-only refusal; and the soft damage policy — body
    /// damage delivers `Damaged` and the load *continues*, while damage
    /// to the stored CRC field fail-stops at the footer digest audit.
    #[test]
    fn idx_sidecar_sections_round_trip_and_audit() {
        let fs = MemFs::new();
        let dir = Path::new("/ckpt");
        fs.create_dir_all(dir).unwrap();
        let mut w = SyncIckWriter::create_v2(
            fs.clone(),
            dir,
            &small_cfg(),
            0,
            15,
            Lsn::new(crate::lsn::SegmentId(1), 64),
            &[16, 17],
        )
        .expect("create v2");
        for i in 0..3u32 {
            let key = format!("k{i}").into_bytes();
            w.append(&RecordView::StringPostImage { ns: NsId(16), key: &key, value: b"v" })
                .expect("image");
        }
        // Index A: Fixed8, 5 pairs — the 64-byte section target splits
        // the stream into three sections (2 + 2 + 1-with-FINAL).
        let idx_a = IdxSidecarMeta {
            ns: 16,
            index_id: 1,
            generation: 3,
            key_encoding_version: 1,
            fixed8: true,
        };
        let want_a: Vec<(Vec<u8>, u64)> =
            (0..5u64).map(|i| ((i * 3).to_be_bytes().to_vec(), 100 + i)).collect();
        for (ordinal, (key, entry_ref)) in want_a.iter().enumerate() {
            w.append_idx_entry(&idx_a, ordinal as u64, key, *entry_ref).expect("idx entry");
        }
        w.append_idx_final(&idx_a, want_a.len() as u64).expect("idx final");
        // Index B: VarKey with shared prefixes, >8-byte keys, and a
        // duplicate key under two refs.
        let idx_b = IdxSidecarMeta {
            ns: 16,
            index_id: 2,
            generation: 7,
            key_encoding_version: 1,
            fixed8: false,
        };
        let want_b: Vec<(Vec<u8>, u64)> = vec![
            (b"alpha".to_vec(), 1),
            (b"alpha".to_vec(), 9),
            (b"alphabetically-long-key".to_vec(), 2),
            (b"beta".to_vec(), 3),
        ];
        for (ordinal, (key, entry_ref)) in want_b.iter().enumerate() {
            w.append_idx_entry(&idx_b, ordinal as u64, key, *entry_ref).expect("idx entry");
        }
        w.append_idx_final(&idx_b, want_b.len() as u64).expect("idx final");
        // Index C: the empty converged tree — exactly one zero-entry
        // FINAL section (ADR-0078 D2).
        let idx_c = IdxSidecarMeta {
            ns: 17,
            index_id: 3,
            generation: 1,
            key_encoding_version: 1,
            fixed8: true,
        };
        w.append_idx_final(&idx_c, 0).expect("empty final");
        let summary = w.finish().expect("finish");
        assert_eq!(summary.records, 3, "sidecar pairs stay out of records_total (ADR-0078 D2)");
        assert_eq!(
            summary.entries_per_ns,
            vec![(16, 3)],
            "sidecar pairs stay out of the presize hint; ns 17 has no live entries at all"
        );

        // Exact hybrid round trip: per-index order, contiguity, FINAL
        // totals, and the empty-FINAL shape.
        let path = dir.join(ick_file_name(15));
        type GotSection = (u32, u32, u64, bool, bool, u64, u64, Vec<(Vec<u8>, u64)>);
        let mut got: Vec<GotSection> = Vec::new();
        let mut damaged = 0u64;
        let (info, audit) = read_ick_hybrid(
            &fs,
            &path,
            IckReaderConfig::default(),
            |_| Ok::<(), ()>(()),
            |_| panic!("no addr-ref sections in this image"),
            |_| panic!("no live-set sections in this image"),
            |_| panic!("no blob-ref sections in this image"),
            |step| {
                match step {
                    IckIdxSidecarStep::Section(section) => got.push((
                        section.ns,
                        section.index_id,
                        section.generation,
                        section.fixed8,
                        section.final_section,
                        section.entries_before,
                        section.total_entries,
                        section.iter().map(|(key, r)| (key.to_vec(), r)).collect(),
                    )),
                    IckIdxSidecarStep::Damaged { .. } => damaged += 1,
                }
                Ok::<(), ()>(())
            },
        )
        .expect("hybrid load");
        assert_eq!(info.version, ICK_VERSION_V2);
        assert_eq!(audit, summary, "loader audit reproduces the writer summary");
        assert_eq!(damaged, 0);
        for (id, generation, fixed8, want) in [(1u32, 3u64, true, &want_a), (2, 7, false, &want_b)]
        {
            let sections: Vec<_> = got.iter().filter(|s| s.1 == id).collect();
            let mut replayed = Vec::new();
            let mut expect_before = 0u64;
            for (at, s) in sections.iter().enumerate() {
                assert_eq!((s.0, s.2, s.3), (16, generation, fixed8));
                assert_eq!(s.5, expect_before, "sections arrive ordinal-contiguous");
                assert_eq!(s.4, at == sections.len() - 1, "FINAL marks the last section only");
                assert_eq!(s.6, if s.4 { want.len() as u64 } else { 0 });
                replayed.extend(s.7.iter().cloned());
                expect_before += s.7.len() as u64;
            }
            assert_eq!(&replayed, want, "pairs replay in order, exact");
        }
        let empties: Vec<_> = got.iter().filter(|s| s.1 == 3).collect();
        assert_eq!(empties.len(), 1, "an empty tree is exactly one section");
        assert!(empties[0].4 && empties[0].6 == 0 && empties[0].7.is_empty());
        assert!(got.iter().filter(|s| s.1 == 1).count() >= 3, "the target split index A");

        // The counts probe (both paths) ignores sidecar sections.
        let probe = read_ick_counts(&fs, &path, IckReaderConfig::default()).expect("counts");
        assert_eq!(probe, summary.entries_per_ns);
        let mut padded = fs.contents(&path).expect("image");
        padded.extend_from_slice(b"junk");
        let pad = MemFs::new();
        pad.create_dir_all(dir).unwrap();
        let mut f = pad.create_meta(&path).expect("create");
        f.write_at(0, &padded).expect("write");
        let counts = read_ick_counts(&pad, &path, IckReaderConfig::default()).expect("hop");
        assert_eq!(counts, summary.entries_per_ns, "the hop arm covers 0x06");

        // Loaders without the sidecar arm refuse typed (the ADR-0073 D7
        // downgrade boundary).
        let err = read_ick(&fs, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("records-only load must refuse sidecar sections");
        assert!(matches!(
            err,
            IckApplyError::Read(IckReadError::IdxSidecarSectionUnsupported { .. })
        ));

        // Soft damage (ADR-0078 D4): a flipped entry byte fails the
        // section CRC — delivered as Damaged, and the load *continues*
        // to a clean footer (the digest folds the stored CRC).
        let image = fs.contents(&path).expect("image");
        let sec_at = find_block(&image, BLOCK_IDXSIDECAR);
        let body_len = le_u32(&image[sec_at + 1..sec_at + 5]) as usize;
        let load_with = |image: Vec<u8>| {
            let fs2 = MemFs::new();
            fs2.create_dir_all(dir).unwrap();
            let mut f = fs2.create_segment(&path, 0).unwrap();
            f.write_at(0, &image).unwrap();
            drop(f);
            let mut sections = 0u64;
            let mut damaged = 0u64;
            let result = read_ick_hybrid(
                &fs2,
                &path,
                IckReaderConfig::default(),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |_| Ok::<(), ()>(()),
                |step| {
                    match step {
                        IckIdxSidecarStep::Section(_) => sections += 1,
                        IckIdxSidecarStep::Damaged { .. } => damaged += 1,
                    }
                    Ok::<(), ()>(())
                },
            );
            (result.map(|_| ()), sections, damaged)
        };
        let mut body_damaged = image.clone();
        body_damaged[sec_at + SECTION_HEADER_LEN + IDXSIDECAR_META_LEN + 3] ^= 0x40;
        let (result, sections, damaged) = load_with(body_damaged);
        result.expect("body damage never refuses the boot (L2)");
        assert_eq!(damaged, 1, "the damaged section is counted");
        assert!(sections >= 4, "every other sidecar section still delivers");

        // Semantic damage behind a valid CRC — the writer-bug model: a
        // non-canonical body written with a correct CRC and a footer
        // digest to match (bit-rot cannot produce this; only a buggy
        // writer can, so every checksum is made consistent). The canon
        // audit, not any CRC, must catch it.
        let mut canon_damaged = image.clone();
        let entry0 = sec_at + SECTION_HEADER_LEN + IDXSIDECAR_META_LEN;
        let first: [u8; 16] = canon_damaged[entry0..entry0 + 16].try_into().unwrap();
        canon_damaged[entry0 + 16..entry0 + 32].copy_from_slice(&first);
        let crc_at = sec_at + SECTION_HEADER_LEN + body_len;
        let crc = crc32c(&canon_damaged[sec_at..crc_at]);
        canon_damaged[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        refresh_footer(&mut canon_damaged);
        let (result, _, damaged) = load_with(canon_damaged);
        result.expect("canon damage is body-class too");
        assert_eq!(damaged, 1, "the non-ascending body arrives as Damaged");

        // Damage to the stored CRC *field* is indistinguishable from
        // framing damage — the footer digest audit fail-stops (the
        // ADR-0073 D6 asymmetry, conservative by design).
        let mut crc_damaged = image.clone();
        crc_damaged[crc_at + 1] ^= 0x01;
        let (result, _, _) = load_with(crc_damaged);
        assert!(result.is_err(), "a damaged stored CRC fails the file-level audit");

        // Writer gates: v1 refusal, ordinal gaps, regressions, and
        // entries after FINAL are walker bugs — refused loud.
        for gate in [
            (|| {
                let mut stream = IckStream::new(&small_cfg());
                let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
                stream.release(lease);
                let meta = IdxSidecarMeta {
                    ns: 16,
                    index_id: 1,
                    generation: 1,
                    key_encoding_version: 1,
                    fixed8: true,
                };
                stream.stage_idx_entry(&meta, 0, &1u64.to_be_bytes(), 1);
            }) as fn(),
            || {
                let mut stream = IckStream::new_v2(&small_cfg());
                let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
                stream.release(lease);
                let meta = IdxSidecarMeta {
                    ns: 16,
                    index_id: 1,
                    generation: 1,
                    key_encoding_version: 1,
                    fixed8: true,
                };
                stream.stage_idx_entry(&meta, 0, &1u64.to_be_bytes(), 1);
                stream.stage_idx_entry(&meta, 2, &2u64.to_be_bytes(), 1); // ordinal gap
            },
            || {
                let mut stream = IckStream::new_v2(&small_cfg());
                let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
                stream.release(lease);
                let meta = IdxSidecarMeta {
                    ns: 16,
                    index_id: 1,
                    generation: 1,
                    key_encoding_version: 1,
                    fixed8: true,
                };
                stream.stage_idx_entry(&meta, 0, &2u64.to_be_bytes(), 1);
                stream.stage_idx_entry(&meta, 1, &1u64.to_be_bytes(), 1); // regression
            },
            || {
                let mut stream = IckStream::new_v2(&small_cfg());
                let lease = stream.begin(0, 1, Lsn::new(crate::lsn::SegmentId(0), 0), &[16]);
                stream.release(lease);
                let meta = IdxSidecarMeta {
                    ns: 16,
                    index_id: 1,
                    generation: 1,
                    key_encoding_version: 1,
                    fixed8: true,
                };
                stream.stage_idx_entry(&meta, 0, &1u64.to_be_bytes(), 1);
                stream.stage_idx_final(&meta, 1);
                stream.stage_idx_entry(&meta, 1, &2u64.to_be_bytes(), 1); // after FINAL
            },
        ] {
            assert!(std::panic::catch_unwind(gate).is_err(), "writer gate must panic");
        }
    }

    /// Recomputes the footer's digest (the fold of the header CRC and
    /// every section's *stored* CRC, in order) and its trailing CRC —
    /// the test-side writer-bug forge: content changed with every
    /// checksum made consistent, so only semantic audits can object.
    fn refresh_footer(image: &mut [u8]) {
        let ns_count = le_u32(&image[28..32]) as usize;
        let header_crc_at = HEADER_FIXED_LEN + ns_count * 4;
        let mut digest = fold_digest(DIGEST_SEED, le_u32(&image[header_crc_at..header_crc_at + 4]));
        let mut at = header_crc_at + CRC_LEN;
        while image[at] != BLOCK_FOOTER {
            let body_len = le_u32(&image[at + 1..at + 5]) as usize;
            let crc_at = at + SECTION_HEADER_LEN + body_len;
            digest = fold_digest(digest, le_u32(&image[crc_at..crc_at + 4]));
            at = crc_at + CRC_LEN;
        }
        let block_len = image.len() - at;
        let digest_at = image.len() - CRC_LEN - 8;
        image[digest_at..digest_at + 8].copy_from_slice(&digest.to_le_bytes());
        let crc = crc32c(&image[at..at + block_len - CRC_LEN]);
        let crc_field = image.len() - CRC_LEN;
        image[crc_field..].copy_from_slice(&crc.to_le_bytes());
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
        image[8] = 4; // version 4: unknown to this reader (v3 is M4.5-S36's)
        let fs2 = MemFs::new();
        fs2.create_dir_all(dir).unwrap();
        let mut f = fs2.create_segment(&path, 0).unwrap();
        f.write_at(0, &image).unwrap();
        drop(f);
        let err = read_ick(&fs2, &path, IckReaderConfig::default(), |_| Ok::<(), ()>(()))
            .expect_err("unknown version");
        assert!(matches!(err, IckApplyError::Read(IckReadError::UnsupportedVersion(4))));

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
