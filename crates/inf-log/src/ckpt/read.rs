//! The `.ick` reader: the footer probe, the bounded section decoder
//! (`IckReader` — iterative, one block per step, every section class
//! capped by `IckReaderConfig`), and the audit helpers recovery drives.

use super::*;

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
    /// The load path cannot apply index sidecars (a loader without the
    /// sidecar arm opened a v2 checkpoint carrying tag 0x06 — the
    /// ADR-0073 D7 downgrade boundary, typed).
    IdxSidecarSectionUnsupported {
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
    /// A v3 block's alignment padding is not zero (ADR-0088 D3).
    Padding {
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
            IckReadError::IdxSidecarSectionUnsupported { at } => {
                write!(
                    f,
                    "ick index-sidecar section at offset {at} but the load path has no sidecar arm"
                )
            }
            IckReadError::FooterMismatch { field } => {
                write!(f, "ick footer disagrees with sections: {field}")
            }
            IckReadError::TrailingData { at } => {
                write!(f, "trailing bytes after ick footer ({at})")
            }
            IckReadError::Padding { at } => {
                write!(f, "ick v3 block padding is not zero at offset {at}")
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
        IckReaderConfig { max_section_bytes: ICK_MAX_SECTION_BYTES }
    }
}

/// v3 block padding must be zero (ADR-0088 D3): every padding byte was
/// written by the sealer, so a non-zero one is damage, not slack.
fn verify_padding<File: SegmentFile>(file: &File, at: u64, len: usize) -> Result<(), IckReadError> {
    if len == 0 {
        return Ok(());
    }
    debug_assert!(len < ICK_BLOCK_ALIGN);
    let mut pad = [0u8; ICK_BLOCK_ALIGN];
    read_exact_at(file, at, &mut pad[..len])?;
    if pad[..len].iter().any(|b| *b != 0) {
        return Err(IckReadError::Padding { at });
    }
    Ok(())
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

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("2 bytes"))
}

/// The end-of-file footer probe (ADR-0028 D3 as amended): one read of the
/// longest footer block the header's namespace count allows, then each
/// candidate count from that maximum down names a footer start; the
/// first whose tag and own count agree is CRC-checked once. `None` =
/// the hop chain locates the footer (trailing bytes, damage, or a file
/// too short for the probe).
fn probe_footer<File: SegmentFile>(
    file: &File,
    file_size: u64,
    sections_at: u64,
    header_ns: usize,
    aligned: bool,
) -> Result<Option<Vec<(u32, u64)>>, IckReadError> {
    let hop = |len: usize| if aligned { ick_align_up(len) } else { len };
    let footer_len = |ns: usize| FOOTER_FIXED_LEN + ns * 12 + 8 + CRC_LEN;
    let max_block = hop(footer_len(header_ns));
    if file_size < sections_at + max_block as u64 {
        return Ok(None);
    }
    let tail_at = file_size - max_block as u64;
    let mut tail = vec![0u8; max_block];
    if read_exact_at(file, tail_at, &mut tail).is_err() {
        return Ok(None);
    }
    for ns in (0..=header_ns).rev() {
        let len = footer_len(ns);
        let start = max_block - hop(len);
        let block = &tail[start..];
        if block[0] != BLOCK_FOOTER || le_u32(&block[13..17]) as usize != ns {
            continue;
        }
        let hit = crc32c(&block[..len - CRC_LEN]) == le_u32(&block[len - CRC_LEN..len])
            && block[len..].iter().all(|b| *b == 0);
        if !hit {
            return Ok(None);
        }
        return Ok(Some(
            block[FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + ns * 12]
                .chunks_exact(12)
                .map(|chunk| (le_u32(&chunk[0..4]), le_u64(&chunk[4..12])))
                .collect(),
        ));
    }
    Ok(None)
}

pub(super) fn le_u32(bytes: &[u8]) -> u32 {
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
    read_ick_counts_probed(fs, path, cfg).map(|(counts, _)| counts)
}

/// [`read_ick_counts`] plus whether the end-of-file footer probe hit
/// (`false` = the dependent hop chain located the footer).
///
/// # Errors
/// As [`read_ick_counts`].
pub fn read_ick_counts_probed<F: SegmentFs>(
    fs: &F,
    path: &Path,
    cfg: IckReaderConfig,
) -> Result<(Vec<(u32, u64)>, bool), IckReadError> {
    let file = fs.open_read(path).map_err(IckReadError::Io)?;
    let mut fixed = [0u8; HEADER_FIXED_LEN];
    read_exact_at(&file, 0, &mut fixed)?;
    if fixed[0..8] != ICK_MAGIC {
        return Err(IckReadError::BadMagic);
    }
    let version = u16::from_le_bytes([fixed[8], fixed[9]]);
    if !(ICK_VERSION..=ICK_VERSION_V3).contains(&version) {
        return Err(IckReadError::UnsupportedVersion(version));
    }
    let ns_count = le_u32(&fixed[28..32]) as usize;
    if ns_count > (1 << 20) {
        return Err(IckReadError::Truncated { at: 28 });
    }
    let file_size = file.file_size()?;
    let aligned = version >= ICK_VERSION_V3;
    let hop = |len: usize| if aligned { ick_align_up(len) } else { len };
    let header_len = HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN;
    if aligned {
        verify_padding(&file, header_len as u64, hop(header_len) - header_len)?;
    }
    let mut offset = hop(header_len) as u64;
    // Direct footer probe (M2.5-S08): a well-formed `.ick` ends exactly at
    // its footer — two reads instead of hopping every section header (a
    // chain of *dependent* small reads; cold, each hop is a synchronous
    // page fault — measured as the dominant cold ick cost). The footer's
    // namespace count is its own, not the header's (ADR-0028 A1): a
    // namespace with no live entries is absent from the footer, so the
    // probe reads the longest possible footer block and locates the
    // footer by the count each candidate length implies. The footer CRC
    // validates the probe; any mismatch falls back to the hop below, and
    // a wrong hint could only ever cost memory geometry (the streaming
    // pass re-audits). Under v3 the footer block is padded: the footer
    // sits at the head of its aligned block (ADR-0088 D3).
    if let Some(counts) = probe_footer(&file, file_size, offset, ns_count, aligned)? {
        return Ok((counts, true));
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
            // The 0x03/0x04/0x05/0x06 tags are a v2 vocabulary — in a v1
            // file they are corruption. Every tag `seal_section` can emit
            // must appear here or the footer-probe fallback misdiagnoses
            // a valid file as corrupt (the ADR-0073 D4 three-site rule —
            // 0x05 was missing until M4.5-S00's audit).
            BLOCK_SECTION | BLOCK_ADDR_SECTION | BLOCK_LIVESET | BLOCK_BLOBREF
            | BLOCK_IDXSIDECAR => {
                if head[0] != BLOCK_SECTION && version < ICK_VERSION_V2 {
                    return Err(IckReadError::UnknownBlock { tag: head[0], at: offset });
                }
                let body_len = le_u32(&head[1..5]);
                if body_len > cfg.max_section_bytes {
                    return Err(IckReadError::SectionTooLarge {
                        len: body_len,
                        max: cfg.max_section_bytes,
                    });
                }
                offset += hop(SECTION_HEADER_LEN + body_len as usize + CRC_LEN) as u64;
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
                if aligned {
                    verify_padding(&file, offset + block_len as u64, hop(block_len) - block_len)?;
                }
                return Ok((
                    block[FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + footer_ns * 12]
                        .chunks_exact(12)
                        .map(|chunk| (le_u32(&chunk[0..4]), le_u64(&chunk[4..12])))
                        .collect(),
                    false,
                ));
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

/// The per-section index-sidecar handler (M4.5-S06) — same per-section
/// dyn dispatch shape; receives damaged-body notifications too (the
/// soft class must stay countable, L10).
type IdxSidecarHandler<'a, E> = &'a mut dyn FnMut(IckIdxSidecarStep<'_>) -> Result<(), E>;

/// One index-sidecar delivery (M4.5-S06, ADR-0078 D4): tag 0x06 is the
/// file's only *soft* body class — a section whose body fails its CRC
/// or canon arrives as `Damaged` (counted by the applier; per-index
/// outcomes resolve through the loader's completeness rules) and the
/// read continues. Everything else about the file stays fail-stop.
#[derive(Debug)]
pub enum IckIdxSidecarStep<'a> {
    /// A validated section: shape, flags, key bounds, and the strictly-
    /// ascending canon all passed — the applier trusts the shape.
    Section(IckIdxSidecarSection<'a>),
    /// A well-framed section whose body failed (CRC or canon). The body
    /// is untrusted, so the damage is deliberately unattributed.
    Damaged { at: u64 },
}

/// One validated index-sidecar section (v2, ADR-0078 D2/D3): one
/// converged index's `(typed key bytes, entry_ref)` pairs, strictly
/// ascending — per-section dispatch, tight per-entry loop (the
/// [`IckRefSection`] posture).
#[derive(Debug)]
pub struct IckIdxSidecarSection<'a> {
    /// Owning namespace.
    pub ns: u32,
    /// The index's node-unique id.
    pub index_id: u32,
    /// The generation the pairs were maintained under — the loader
    /// discards on any mismatch with the seeded registry (ADR-0073
    /// D5.1).
    pub generation: u64,
    /// The S02 key-encoding version the key bytes were produced by.
    pub key_encoding_version: u16,
    /// True: `Fixed8` entries; false: length-prefixed `VarKey` entries.
    pub fixed8: bool,
    /// This is the index's last section; `total_entries` is meaningful.
    pub final_section: bool,
    /// Ordinal of this section's first pair within the index's whole
    /// emission (the loader's contiguity check).
    pub entries_before: u64,
    /// Whole-stream cardinality (0 unless `final_section`).
    pub total_entries: u64,
    entries: &'a [u8],
}

impl IckIdxSidecarSection<'_> {
    /// Entry count (audited against the section header at decode).
    #[must_use]
    pub fn len(&self) -> usize {
        if self.fixed8 {
            return self.entries.len() / IDXSIDECAR_FIXED_ENTRY_LEN;
        }
        self.iter().count()
    }

    /// True for the zero-entry FINAL shape (the empty converged tree —
    /// legal for tag 0x06 exactly there, ADR-0078 D2).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(typed key bytes, entry_ref)` pairs in file (= ascending) order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u64)> + '_ {
        let mut rest = self.entries;
        let fixed8 = self.fixed8;
        std::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            // Decode audited the shape; the arithmetic below cannot go
            // out of bounds on a delivered section.
            let key_len = if fixed8 { 8 } else { le_u16(&rest[0..2]) as usize };
            let key_at = if fixed8 { 0 } else { 2 };
            let key = &rest[key_at..key_at + key_len];
            let entry_ref = le_u64(&rest[key_at + key_len..key_at + key_len + 8]);
            rest = &rest[key_at + key_len + 8..];
            Some((key, entry_ref))
        })
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

/// Body audit for a well-framed, CRC-clean 0x06 section (ADR-0078 D3):
/// meta shape, known flags, scheme, key bounds, entry-count agreement,
/// and the strictly-ascending canon — iterative, bounded by the body
/// length (L9). `None` is a **body-class** verdict: the caller delivers
/// [`IckIdxSidecarStep::Damaged`], never a read error (the soft class).
fn parse_idx_sidecar_body(body: &[u8], record_count: u32) -> Option<IckIdxSidecarSection<'_>> {
    if body.len() < IDXSIDECAR_META_LEN {
        return None;
    }
    let fixed8 = match body[18] {
        IDXSIDECAR_SCHEME_FIXED8 => true,
        IDXSIDECAR_SCHEME_VAR => false,
        _ => return None,
    };
    let flags = body[19];
    if flags & !IDXSIDECAR_FLAG_FINAL != 0 {
        return None;
    }
    let final_section = flags & IDXSIDECAR_FLAG_FINAL != 0;
    let total_entries = le_u64(&body[28..36]);
    if !final_section && (total_entries != 0 || record_count == 0) {
        return None;
    }
    // Entry audit: exact count and the strictly-ascending pair canon.
    let entries = &body[IDXSIDECAR_META_LEN..];
    let mut rest = entries;
    let mut decoded: u32 = 0;
    let mut prev: Option<(&[u8], u64)> = None;
    while !rest.is_empty() {
        let key_len = if fixed8 {
            8
        } else {
            if rest.len() < 2 {
                return None;
            }
            le_u16(&rest[0..2]) as usize
        };
        let key_at = if fixed8 { 0 } else { 2 };
        if key_len > IDXSIDECAR_KEY_MAX || rest.len() < key_at + key_len + 8 {
            return None;
        }
        let key = &rest[key_at..key_at + key_len];
        let entry_ref = le_u64(&rest[key_at + key_len..key_at + key_len + 8]);
        if prev.is_some_and(|p| (key, entry_ref) <= p) {
            return None;
        }
        prev = Some((key, entry_ref));
        decoded = decoded.checked_add(1)?;
        rest = &rest[key_at + key_len + 8..];
    }
    if decoded != record_count {
        return None;
    }
    Some(IckIdxSidecarSection {
        ns: le_u32(&body[0..4]),
        index_id: le_u32(&body[4..8]),
        generation: le_u64(&body[8..16]),
        key_encoding_version: le_u16(&body[16..18]),
        fixed8,
        final_section,
        entries_before: le_u64(&body[20..28]),
        total_entries,
        entries,
    })
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
        if !(ICK_VERSION..=ICK_VERSION_V3).contains(&version) {
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
        let header_len = HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN;
        if version >= ICK_VERSION_V3 {
            // The header block's padding is zero like every other block's
            // (ADR-0088 D3).
            verify_padding(&file, header_len as u64, ick_align_up(header_len) - header_len)?;
        }
        Ok(IckReader {
            file,
            cfg,
            info: IckInfo { version, cell, ckpt_id, begin_lsn, ns_ids },
            file_size,
            offset: if version >= ICK_VERSION_V3 {
                ick_align_up(HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN) as u64
            } else {
                (HEADER_FIXED_LEN + ns_count * 4 + CRC_LEN) as u64
            },
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

    /// The distance to the next block: the exact length on v1/v2, the
    /// aligned length on v3 — whose padding must read back as zeros
    /// (ADR-0088 D3; a non-zero pad byte is the CRC's fail-stop class).
    /// Read-ahead hint for the blocks after the current one (M2.5-S08):
    /// four blocks deep, aimed at the *next block start* — on v3 that is
    /// the aligned offset, not the padding (review L03, batch 33).
    /// Hint-only; EOF-safe.
    fn advise_next_blocks(&self, block_len: usize) {
        let next =
            if self.info.version < ICK_VERSION_V3 { block_len } else { ick_align_up(block_len) };
        self.file.advise_read_ahead(self.offset + next as u64, 4 * next as u64);
    }

    fn hop(&self, block_len: usize) -> Result<u64, IckReadError> {
        if self.info.version < ICK_VERSION_V3 {
            return Ok(block_len as u64);
        }
        let padded = ick_align_up(block_len);
        verify_padding(&self.file, self.offset + block_len as u64, padded - block_len)?;
        Ok(padded as u64)
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
        self.step_inner(&mut apply, None, None, None, None)
    }

    /// [`next_step`](Self::next_step) with the v2 arms: ref, live-set,
    /// blob-ref, and index-sidecar sections arrive whole, post-audit
    /// (shape, CRC, per-entry invariants), one callback per section
    /// (M4-S12/S14/S17, M4.5-S06 — ADR-0057 D3/D6, ADR-0058 D3,
    /// ADR-0061 D6, ADR-0078 D3/D4).
    ///
    /// # Errors
    /// As [`next_step`](Self::next_step).
    pub fn next_step_hybrid<E>(
        &mut self,
        mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
        mut on_refs: impl FnMut(IckRefSection<'_>) -> Result<(), E>,
        mut on_live_set: impl FnMut(IckLiveSetSection<'_>) -> Result<(), E>,
        mut on_blob_refs: impl FnMut(IckBlobRefSection<'_>) -> Result<(), E>,
        mut on_idx_sidecar: impl FnMut(IckIdxSidecarStep<'_>) -> Result<(), E>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.step_inner(
            &mut apply,
            Some(&mut on_refs),
            Some(&mut on_live_set),
            Some(&mut on_blob_refs),
            Some(&mut on_idx_sidecar),
        )
    }

    fn step_inner<E>(
        &mut self,
        apply: &mut dyn FnMut(RecordView<'_>) -> Result<(), E>,
        refs: Option<RefHandler<'_, E>>,
        live_set: Option<LiveSetHandler<'_, E>>,
        blob_refs: Option<BlobRefHandler<'_, E>>,
        idx_sidecar: Option<IdxSidecarHandler<'_, E>>,
    ) -> Result<IckStep, IckApplyError<E>> {
        assert!(!self.done, "IckReader stepped past its footer");
        if self.offset == self.file_size {
            return Err(IckReadError::MissingFooter.into());
        }
        // One header read serves the tag dispatch and the section frame
        // (review L03, batch 34): a footer's tag sits in the same bytes.
        let mut head = [0u8; SECTION_HEADER_LEN];
        read_exact_at(&self.file, self.offset, &mut head)?;
        let BlockTag::Section(class) = self.classify(head[0])? else {
            return self.step_footer().map_err(IckApplyError::Read);
        };
        let frame = self.read_block(head)?;
        match class {
            SectionTag::Records => self.step_records(frame, apply),
            SectionTag::Refs => self.step_refs(frame, refs),
            SectionTag::LiveSet => self.step_live_set(frame, live_set),
            SectionTag::BlobRefs => self.step_blob_refs(frame, blob_refs),
            SectionTag::IdxSidecar => self.step_idx_sidecar(frame, idx_sidecar),
        }
    }

    /// The block class a tag names under the file's version: the v2
    /// vocabulary (0x03–0x06) is corruption in a v1 file. Every tag
    /// `seal_section` can emit is listed here and in the counts hop
    /// (the ADR-0073 D4 rule).
    fn classify(&self, tag: u8) -> Result<BlockTag, IckReadError> {
        let v2 = self.info.version >= ICK_VERSION_V2;
        Ok(match tag {
            BLOCK_SECTION => BlockTag::Section(SectionTag::Records),
            BLOCK_FOOTER => BlockTag::Footer,
            BLOCK_ADDR_SECTION if v2 => BlockTag::Section(SectionTag::Refs),
            BLOCK_LIVESET if v2 => BlockTag::Section(SectionTag::LiveSet),
            BLOCK_BLOBREF if v2 => BlockTag::Section(SectionTag::BlobRefs),
            BLOCK_IDXSIDECAR if v2 => BlockTag::Section(SectionTag::IdxSidecar),
            tag => return Err(IckReadError::UnknownBlock { tag, at: self.offset }),
        })
    }

    /// The shared section preamble: bound the body by the loader config,
    /// read the rest of the block behind the header already in hand, hint
    /// the read-ahead. Two dependent reads per section block (was three —
    /// review L03). The CRC is *not* checked here: tag 0x06 folds it
    /// before verification (the soft class).
    fn read_block(&mut self, head: [u8; SECTION_HEADER_LEN]) -> Result<SectionFrame, IckReadError> {
        let body_len = le_u32(&head[1..5]);
        if body_len > self.cfg.max_section_bytes {
            return Err(IckReadError::SectionTooLarge {
                len: body_len,
                max: self.cfg.max_section_bytes,
            });
        }
        let record_count = le_u32(&head[5..9]);
        let block_len = SECTION_HEADER_LEN + body_len as usize + CRC_LEN;
        self.block.resize(block_len, 0);
        self.block[..SECTION_HEADER_LEN].copy_from_slice(&head);
        read_exact_at(
            &self.file,
            self.offset + SECTION_HEADER_LEN as u64,
            &mut self.block[SECTION_HEADER_LEN..],
        )?;
        // Read-ahead the next blocks (M2.5-S08): their device reads
        // overlap this section's CRC + decode + apply. Four blocks deep —
        // sections share the staging capacity class and are small enough
        // that one-ahead loses the race against the prefetcher's wakeup
        // latency. Hint-only; EOF-safe.
        self.advise_next_blocks(block_len);
        let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
        Ok(SectionFrame { record_count, block_len, stored_crc })
    }

    /// The hard CRC audit: a mismatch is fail-stop, a match folds into
    /// the footer digest.
    fn audit_crc(&mut self, frame: SectionFrame) -> Result<(), IckReadError> {
        if !frame.crc_ok(&self.block) {
            return Err(IckReadError::SectionCrc { index: self.sections, at: self.offset });
        }
        self.digest = fold_digest(self.digest, frame.stored_crc);
        Ok(())
    }

    /// Accounts one validated section and hops to the next block.
    fn finish_section(
        &mut self,
        frame: SectionFrame,
        records: u64,
    ) -> Result<IckStep, IckReadError> {
        self.sections += 1;
        self.records_total += records;
        let hop = self.hop(frame.block_len)?;
        self.offset += hop;
        Ok(IckStep::Section { bytes: hop })
    }

    fn apply_err<E>(&self, error: E) -> IckApplyError<E> {
        IckApplyError::Apply { section: self.sections, error }
    }

    fn step_records<E>(
        &mut self,
        frame: SectionFrame,
        apply: &mut dyn FnMut(RecordView<'_>) -> Result<(), E>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.audit_crc(frame)?;
        let section = self.sections;
        let mut body = frame.body(&self.block);
        let mut decoded = 0u32;
        while !body.is_empty() {
            let (view, consumed) =
                decode_record(body).map_err(|error| IckReadError::Record { section, error })?;
            if let RecordView::StringPostImage { ns, .. } | RecordView::DocFull { ns, .. } = view {
                count_entries(&mut self.entries_seen, ns.0, 1);
            }
            apply(view).map_err(|error| IckApplyError::Apply { section, error })?;
            decoded += 1;
            body = &body[consumed..];
        }
        if decoded != frame.record_count {
            return Err(IckReadError::FooterMismatch { field: "section record_count" }.into());
        }
        Ok(self.finish_section(frame, u64::from(frame.record_count))?)
    }

    fn step_refs<E>(
        &mut self,
        frame: SectionFrame,
        refs: Option<RefHandler<'_, E>>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.audit_crc(frame)?;
        let malformed = IckReadError::RefSectionMalformed { index: self.sections, at: self.offset };
        let (ns, walk_watermark, entries) =
            audit_ref_body(frame.body(&self.block), frame.record_count).ok_or(malformed)?;
        // Watermark audit (the §3.1 corollary's decode half) before the
        // applier sees a single entry.
        if entries.chunks_exact(ADDR_REF_ENTRY_LEN).any(|e| ref_addr(&e[8..]) >= walk_watermark) {
            return Err(
                IckReadError::RefBeyondWatermark { index: self.sections, at: self.offset }.into()
            );
        }
        let Some(on_refs) = refs else {
            return Err(IckReadError::RefSectionUnsupported { at: self.offset }.into());
        };
        count_entries(&mut self.entries_seen, ns, u64::from(frame.record_count));
        on_refs(IckRefSection { ns, walk_watermark, entries }).map_err(|e| self.apply_err(e))?;
        Ok(self.finish_section(frame, u64::from(frame.record_count))?)
    }

    fn step_live_set<E>(
        &mut self,
        frame: SectionFrame,
        live_set: Option<LiveSetHandler<'_, E>>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.audit_crc(frame)?;
        let malformed =
            IckReadError::LiveSetSectionMalformed { index: self.sections, at: self.offset };
        let (ns, entries) =
            audit_live_set_body(frame.body(&self.block), frame.record_count).ok_or(malformed)?;
        let Some(on_live_set) = live_set else {
            return Err(IckReadError::LiveSetSectionUnsupported { at: self.offset }.into());
        };
        // Deliberately NOT entries_seen: the per-ns counts presize the
        // index at recovery, and a file entry is not an index entry
        // (mirrors the writer).
        on_live_set(IckLiveSetSection { ns, entries }).map_err(|e| self.apply_err(e))?;
        Ok(self.finish_section(frame, u64::from(frame.record_count))?)
    }

    fn step_blob_refs<E>(
        &mut self,
        frame: SectionFrame,
        blob_refs: Option<BlobRefHandler<'_, E>>,
    ) -> Result<IckStep, IckApplyError<E>> {
        self.audit_crc(frame)?;
        let malformed =
            IckReadError::BlobRefSectionMalformed { index: self.sections, at: self.offset };
        let (ns, entries) =
            audit_blob_ref_body(frame.body(&self.block), frame.record_count).ok_or(malformed)?;
        let Some(on_blob_refs) = blob_refs else {
            return Err(IckReadError::BlobRefSectionUnsupported { at: self.offset }.into());
        };
        // Deliberately NOT entries_seen: a cold blob record's index slot
        // was already counted by its 0x03 ref entry — this section is
        // bookkeeping, not index content (mirrors the writer).
        on_blob_refs(IckBlobRefSection { ns, entries }).map_err(|e| self.apply_err(e))?;
        Ok(self.finish_section(frame, u64::from(frame.record_count))?)
    }

    fn step_idx_sidecar<E>(
        &mut self,
        frame: SectionFrame,
        idx_sidecar: Option<IdxSidecarHandler<'_, E>>,
    ) -> Result<IckStep, IckApplyError<E>> {
        let Some(on_idx_sidecar) = idx_sidecar else {
            return Err(IckReadError::IdxSidecarSectionUnsupported { at: self.offset }.into());
        };
        // The stored CRC folds into the digest BEFORE verification
        // (ADR-0073 D3.3/D6): the file-level audit and the body verdict
        // are independent — that is what makes 0x06 the file's only soft
        // body class. Damage to the CRC *field* itself fails the footer
        // digest audit ⇒ fail-stop, conservatively (the D6 asymmetry).
        self.digest = fold_digest(self.digest, frame.stored_crc);
        let step = if frame.crc_ok(&self.block) {
            match parse_idx_sidecar_body(frame.body(&self.block), frame.record_count) {
                Some(section) => IckIdxSidecarStep::Section(section),
                None => IckIdxSidecarStep::Damaged { at: self.offset },
            }
        } else {
            IckIdxSidecarStep::Damaged { at: self.offset }
        };
        on_idx_sidecar(step).map_err(|e| self.apply_err(e))?;
        // Sidecar entries join neither `records_total` nor the per-ns
        // counts (ADR-0078 D2, mirroring the writer): no soft-class body
        // byte may be load-bearing for the footer audit.
        Ok(self.finish_section(frame, 0)?)
    }

    fn step_footer(&mut self) -> Result<IckStep, IckReadError> {
        let mut fixed = [0u8; FOOTER_FIXED_LEN];
        read_exact_at(&self.file, self.offset, &mut fixed)?;
        let footer_sections = le_u32(&fixed[1..5]);
        let footer_records = le_u64(&fixed[5..13]);
        let footer_ns = le_u32(&fixed[13..17]) as usize;
        if footer_ns > (1 << 20) {
            return Err(IckReadError::Truncated { at: self.offset + 13 });
        }
        let tail_len = footer_ns * 12 + 8 + CRC_LEN;
        let block_len = FOOTER_FIXED_LEN + tail_len;
        self.block.resize(block_len, 0);
        read_exact_at(&self.file, self.offset, &mut self.block)?;
        let stored_crc = le_u32(&self.block[block_len - CRC_LEN..]);
        if crc32c(&self.block[..block_len - CRC_LEN]) != stored_crc {
            return Err(IckReadError::FooterCrc { at: self.offset });
        }
        let stored_digest = le_u64(&self.block[block_len - CRC_LEN - 8..block_len - CRC_LEN]);
        if footer_sections != self.sections {
            return Err(IckReadError::FooterMismatch { field: "section_count" });
        }
        if footer_records != self.records_total {
            return Err(IckReadError::FooterMismatch { field: "records_total" });
        }
        if stored_digest != self.digest {
            return Err(IckReadError::FooterMismatch { field: "digest" });
        }
        let footer_entries: Vec<(u32, u64)> = self.block
            [FOOTER_FIXED_LEN..FOOTER_FIXED_LEN + footer_ns * 12]
            .chunks_exact(12)
            .map(|chunk| (le_u32(&chunk[0..4]), le_u64(&chunk[4..12])))
            .collect();
        let mut seen_sorted = self.entries_seen.clone();
        seen_sorted.sort_unstable();
        let mut footer_sorted = footer_entries.clone();
        footer_sorted.sort_unstable();
        if seen_sorted != footer_sorted {
            return Err(IckReadError::FooterMismatch { field: "entries_per_ns" });
        }
        let end = self.offset + self.hop(block_len)?;
        if end != self.file_size {
            return Err(IckReadError::TrailingData { at: end });
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
}

/// What a block header's tag names on the read side (see
/// [`IckReader::classify`]); the writer's `SectionClass` carries the
/// staged namespace, this carries only the tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BlockTag {
    Section(SectionTag),
    Footer,
}

/// The five section classes `seal_section` emits, by tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SectionTag {
    Records,
    Refs,
    LiveSet,
    BlobRefs,
    IdxSidecar,
}

/// One section block read into `IckReader::block`: the header's counts and
/// the block geometry every arm needs.
#[derive(Copy, Clone, Debug)]
struct SectionFrame {
    record_count: u32,
    block_len: usize,
    stored_crc: u32,
}

impl SectionFrame {
    fn body<'a>(&self, block: &'a [u8]) -> &'a [u8] {
        &block[SECTION_HEADER_LEN..self.block_len - CRC_LEN]
    }

    fn crc_ok(&self, block: &[u8]) -> bool {
        crc32c(&block[..self.block_len - CRC_LEN]) == self.stored_crc
    }
}

/// The per-ns presize count fold (the footer audits it).
fn count_entries(seen: &mut Vec<(u32, u64)>, ns: u32, n: u64) {
    match seen.iter_mut().find(|(id, _)| *id == ns) {
        Some((_, count)) => *count += n,
        None => seen.push((ns, n)),
    }
}

/// A 48-bit little-endian address at the head of `bytes`.
fn ref_addr(bytes: &[u8]) -> u64 {
    let mut addr = [0u8; 8];
    addr[..6].copy_from_slice(&bytes[..6]);
    u64::from_le_bytes(addr)
}

/// Shape audit shared by the fixed-entry classes: `meta_len` bytes of
/// meta, then exactly `record_count` entries of `entry_len`; never empty
/// (the writer only seals non-empty sections).
fn fixed_entries(
    body: &[u8],
    meta_len: usize,
    entry_len: usize,
    record_count: u32,
) -> Option<&[u8]> {
    let entry_bytes = body.len().checked_sub(meta_len)?;
    (record_count != 0
        && entry_bytes.is_multiple_of(entry_len)
        && entry_bytes / entry_len == record_count as usize)
        .then(|| &body[meta_len..])
}

/// Addr-ref body (ADR-0057 D3): `{ns, walk_watermark}` + entries; the
/// watermark is a legal address.
fn audit_ref_body(body: &[u8], record_count: u32) -> Option<(u32, u64, &[u8])> {
    let entries = fixed_entries(body, ADDR_SECTION_META_LEN, ADDR_REF_ENTRY_LEN, record_count)?;
    let walk_watermark = le_u64(&body[4..12]);
    (walk_watermark < ADDR_LIMIT).then(|| (le_u32(&body[0..4]), walk_watermark, entries))
}

/// Live-set body (ADR-0058 D3): `ns` + entries; unknown flag bits are
/// fail-stop within the frozen version, and a dead count above the
/// file's data bytes is the over-count the D4 sound-direction rule
/// exists to make unrepresentable.
fn audit_live_set_body(body: &[u8], record_count: u32) -> Option<(u32, &[u8])> {
    let entries = fixed_entries(body, LIVESET_META_LEN, LIVESET_ENTRY_LEN, record_count)?;
    let sound = entries.chunks_exact(LIVESET_ENTRY_LEN).all(|entry| {
        entry[20] & !LIVESET_FLAG_BYTE_EXACT == 0 && le_u64(&entry[12..20]) <= le_u64(&entry[4..12])
    });
    sound.then(|| (le_u32(&body[0..4]), entries))
}

/// Blob-ref body (ADR-0061 D6): `ns` + entries; a zero-length reference
/// and out-of-order addresses are non-canonical — fail-stop within the
/// frozen version. (Addresses are 48-bit by encoding: six bytes cannot
/// exceed the limit.)
fn audit_blob_ref_body(body: &[u8], record_count: u32) -> Option<(u32, &[u8])> {
    let entries = fixed_entries(body, BLOBREF_META_LEN, BLOBREF_ENTRY_LEN, record_count)?;
    let mut prev_addr: Option<u64> = None;
    for entry in entries.chunks_exact(BLOBREF_ENTRY_LEN) {
        let addr = ref_addr(entry);
        if le_u64(&entry[14..22]) == 0 || prev_addr.is_some_and(|p| addr <= p) {
            return None;
        }
        prev_addr = Some(addr);
    }
    Some((le_u32(&body[0..4]), entries))
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

/// [`read_ick`] with the v2 arms (M4-S12/S14/S17, M4.5-S06 — ADR-0057
/// D3/D6, ADR-0058 D3, ADR-0061 D6, ADR-0078 D3/D4): the hybrid load
/// recovery drives — records through `apply`, validated ref sections
/// through `on_refs`, validated live-set sections through
/// `on_live_set`, blob refs through `on_blob_refs`, and index-sidecar
/// deliveries (validated or damaged-soft) through `on_idx_sidecar`.
/// Same audit, same fuzz surface.
///
/// # Errors
/// As [`read_ick`].
#[allow(clippy::too_many_arguments)] // one handler per v2 section class, deliberately flat
pub fn read_ick_hybrid<F: SegmentFs, E>(
    fs: &F,
    path: &Path,
    cfg: IckReaderConfig,
    mut apply: impl FnMut(RecordView<'_>) -> Result<(), E>,
    mut on_refs: impl FnMut(IckRefSection<'_>) -> Result<(), E>,
    mut on_live_set: impl FnMut(IckLiveSetSection<'_>) -> Result<(), E>,
    mut on_blob_refs: impl FnMut(IckBlobRefSection<'_>) -> Result<(), E>,
    mut on_idx_sidecar: impl FnMut(IckIdxSidecarStep<'_>) -> Result<(), E>,
) -> Result<(IckInfo, IckSummary), IckApplyError<E>> {
    let mut reader = IckReader::open(fs, path, cfg)?;
    loop {
        match reader.next_step_hybrid(
            &mut apply,
            &mut on_refs,
            &mut on_live_set,
            &mut on_blob_refs,
            &mut on_idx_sidecar,
        )? {
            IckStep::Section { .. } => {}
            IckStep::Done(summary) => return Ok((reader.info, summary)),
        }
    }
}
