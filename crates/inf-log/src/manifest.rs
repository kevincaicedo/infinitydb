//! Per-cell recovery-unit MANIFEST (M2-S11, ADR-0017): `shard-k/MANIFEST`
//! names `{ckpt id, begin-LSN, live segment set, format epoch}` atomically
//! — the §3.2 **MANIFEST schema v1** freeze row. The swap is the
//! [`crate::meta`] protocol class (write-new + fsync + rename + dir-fsync,
//! always), so a reader sees the old recovery unit or the new one, never a
//! blend (§8.4).
//!
//! Semantics:
//! - The manifest is the **only** authority that names a checkpoint
//!   (ADR-0016 D4): a `.ick` not named here is an orphan, garbage-collected
//!   by recovery and the truncation slice.
//! - `begin_lsn.segment` is the **truncation floor**: every segment below
//!   it is fully covered by the named checkpoint and may be deleted.
//!   Publication is gated on `begin_lsn ≤` the durability watermark, so
//!   the floor segment's begin marker is always durable log bytes.
//! - `segments` is the live set at publication (v1 writes the contiguous
//!   range `floor..=active`). Decode enforces strictly-ascending only —
//!   later milestones (M5 retention exemptions) may publish holes;
//!   recovery accepts segments appended after publication and fail-stops
//!   if a *listed* segment is missing.
//! - **Epoch 2 (M4-S12, ADR-0057 D5):** cells owning tiered namespaces
//!   append per-namespace tier sections — `{ns, flushed watermark, file
//!   set}` — so {ckpt, WAL segments, tier files, watermarks} publish as
//!   one atomic unit and a checkpoint's address references can never
//!   outrun the file set that resolves them (the §3.1 corollary). Cells
//!   without tiered namespaces keep writing epoch-1 payloads
//!   byte-identically: the degenerate case is absence (ADR-0051 posture).
//!   The manifest names **logical durable ranges**, not physical files —
//!   a sealed file's physical extent may exceed its manifested range in
//!   the disclosed capacity-seal edge; recovery truncates only unsealed
//!   files (seal is terminal, ADR-0056 D5).
//!
//! ```text
//! payload := magic:   [u8;8] = "INFMAN1\0"
//!            epoch:   u32 LE  — format epoch (1 = no tier sections, 2 = tiered)
//!            ckpt_id: u64 LE  — the named checkpoint (`ckpt-{id}.ick`)
//!            begin:   u64 LE  — packed begin-LSN (Lsn::to_u64)
//!            count:   u32 LE
//!            count × u32 LE   — segment ids, strictly ascending,
//!                               segments[0] == begin.segment (the floor)
//! epoch 2 appends:
//!            tier_ns_count: u32 LE            — ≥ 1 (canonical: an empty
//!                                               set re-encodes as epoch 1)
//!            tier_ns_count × tier_ns
//! tier_ns := ns: u32 LE · flushed: u64 LE (48-bit checked) ·
//!            file_count: u32 LE ·
//!            file_count × (id u32 LE · base u64 LE · durable_len u64 LE)
//! ```
//! Tier canonicality (each violation a named fail-stop error): namespaces
//! strictly ascending; files strictly ascending by id **and** by base;
//! `durable_len ≥ 1` (an unconfirmed file is simply not named); ranges
//! never overlap (ADR-0052 ring-top gaps between them are legal); the last
//! range ends at or below `flushed` (a trailing gap is legal); every
//! address 48-bit. The envelope ([`crate::meta`]) adds magic, length, and
//! CRC32C around this payload; corruption at either layer is a named
//! fail-stop error.

use core::fmt;
use std::io;
use std::path::Path;

use crate::fs::SegmentFs;
use crate::lsn::{Lsn, SegmentId};
use crate::meta::{read_envelope, write_envelope};

/// The committed manifest file name (lives in the shard dir, beside
/// `log/` and `ckpt/` — the §3.1 storage layout).
pub const MANIFEST_FILE: &str = "MANIFEST";
/// The staging name; debris from a crashed swap is cleared by the next
/// write and never read.
pub const MANIFEST_STAGING_FILE: &str = "MANIFEST.new";
/// Payload magic; the trailing `1` tags the payload family alongside
/// `epoch` (epoch, not magic, is the version — ADR-0057 D5).
pub const MANIFEST_MAGIC: [u8; 8] = *b"INFMAN1\0";
/// Format epoch for cells without tiered namespaces (schema v1).
pub const MANIFEST_EPOCH: u32 = 1;
/// Format epoch once tier sections are present (M4-S12, ADR-0057 D5).
pub const MANIFEST_EPOCH_V2: u32 = 2;

/// Defensive bound on the decoded segment-set size: 2²⁰ segments × 256 MiB
/// is a 256 TiB cell log — corruption, not configuration.
const MAX_SEGMENTS: usize = 1 << 20;
/// Defensive bounds on tier sections (namespaces per cell) and files per
/// namespace: 2²⁰ files × 1 GiB is a 1 EiB tier — corruption.
const MAX_TIER_NS: usize = 1 << 16;
const MAX_TIER_FILES: usize = 1 << 20;
/// Logical addresses are 48-bit (§3.2 freeze).
const ADDR_LIMIT: u64 = 1 << 48;

const FIXED_LEN: usize = 8 + 4 + 8 + 8 + 4;
const TIER_NS_FIXED_LEN: usize = 4 + 8 + 4;
const TIER_FILE_LEN: usize = 4 + 8 + 8;

/// One tier file's manifested logical range: `[base, base + durable_len)`
/// of address space is durably resolvable through file `id`
/// (`tier-{id:06}.itier`). `durable_len` is the *manifested* durable
/// prefix — for the at-most-one unsealed file it is the recovery
/// truncation length (ADR-0056 D5); a sealed file's physical bytes may
/// exceed it in the disclosed capacity-seal edge (excess is inert).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierFileRange {
    /// File id (`tier-{id:06}.itier` under `shard-k/cold/`).
    pub id: u32,
    /// First logical address of the manifested range.
    pub base: u64,
    /// Manifested durable bytes (≥ 1; unconfirmed files are not named).
    pub durable_len: u64,
}

impl TierFileRange {
    /// One past the last manifested address of this range.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.base + self.durable_len
    }
}

/// One tiered namespace's durable tier state: the flushed watermark (the
/// next boot life's origin — ADR-0057 D6) and the file ranges that,
/// together with ADR-0052 ring-top gaps, tile `[0, flushed)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierNsManifest {
    /// Owning namespace id.
    pub ns: u32,
    /// The manifested flushed watermark (48-bit).
    pub flushed: u64,
    /// Manifested file ranges, strictly ascending by id and base.
    pub files: Vec<TierFileRange>,
}

/// One decoded recovery-unit manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The named checkpoint: `ckpt-{ckpt_id}.ick` in the cell's ckpt dir.
    pub ckpt_id: u64,
    /// The checkpoint's begin LSN — tail replay starts here; segments
    /// below `begin_lsn.segment` are truncatable.
    pub begin_lsn: Lsn,
    /// Live segment set at publication, strictly ascending;
    /// `segments[0] == begin_lsn.segment`.
    pub segments: Vec<SegmentId>,
    /// Tier sections (M4-S12, ADR-0057 D5), strictly ascending by ns.
    /// Empty for cells without tiered namespaces — such a manifest
    /// encodes as epoch 1, byte-identical to M2 (degenerate = absence).
    pub tiers: Vec<TierNsManifest>,
}

impl Manifest {
    /// A tierless recovery unit — the M2 shape (encodes as epoch 1).
    /// Cells that own tiered namespaces populate `tiers` and encode as
    /// epoch 2 (ADR-0057 D5).
    #[must_use]
    pub fn v1(ckpt_id: u64, begin_lsn: Lsn, segments: Vec<SegmentId>) -> Manifest {
        Manifest { ckpt_id, begin_lsn, segments, tiers: Vec::new() }
    }

    /// The truncation floor: the first live segment. Everything below is
    /// covered by the named checkpoint.
    #[must_use]
    pub fn floor(&self) -> SegmentId {
        self.begin_lsn.segment
    }

    /// The tier section for `ns`, if this manifest carries one.
    #[must_use]
    pub fn tier_ns(&self, ns: u32) -> Option<&TierNsManifest> {
        self.tiers.iter().find(|t| t.ns == ns)
    }

    /// Canonical payload bytes: epoch 1 when `tiers` is empty (bit-exact
    /// M2 output), epoch 2 otherwise.
    ///
    /// # Panics
    /// Panics when a tier section violates its own invariants — the
    /// writer-side half of the §3.1 corollary check (a manifest that
    /// cannot decode canonically must never be staged).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let tier_len: usize =
            self.tiers.iter().map(|t| TIER_NS_FIXED_LEN + t.files.len() * TIER_FILE_LEN).sum();
        let mut out = Vec::with_capacity(FIXED_LEN + self.segments.len() * 4 + 4 + tier_len);
        out.extend_from_slice(&MANIFEST_MAGIC);
        let epoch = if self.tiers.is_empty() { MANIFEST_EPOCH } else { MANIFEST_EPOCH_V2 };
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&self.ckpt_id.to_le_bytes());
        out.extend_from_slice(&self.begin_lsn.to_u64().to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.segments.len()).expect("segment count").to_le_bytes(),
        );
        for seg in &self.segments {
            out.extend_from_slice(&seg.0.to_le_bytes());
        }
        if self.tiers.is_empty() {
            return out;
        }
        out.extend_from_slice(
            &u32::try_from(self.tiers.len()).expect("tier ns count").to_le_bytes(),
        );
        let mut prev_ns: Option<u32> = None;
        for tier in &self.tiers {
            assert!(prev_ns.is_none_or(|p| tier.ns > p), "tier namespaces ascend");
            assert!(tier.flushed < ADDR_LIMIT, "flushed watermark is 48-bit");
            out.extend_from_slice(&tier.ns.to_le_bytes());
            out.extend_from_slice(&tier.flushed.to_le_bytes());
            out.extend_from_slice(
                &u32::try_from(tier.files.len()).expect("tier file count").to_le_bytes(),
            );
            let mut prev_end = 0u64;
            let mut prev_id: Option<u32> = None;
            for file in &tier.files {
                assert!(file.durable_len >= 1, "unconfirmed files are not named");
                assert!(file.base >= prev_end, "tier ranges must not overlap");
                assert!(prev_id.is_none_or(|p| file.id > p), "tier file ids ascend");
                assert!(file.end() <= tier.flushed, "ranges tile inside [0, flushed)");
                out.extend_from_slice(&file.id.to_le_bytes());
                out.extend_from_slice(&file.base.to_le_bytes());
                out.extend_from_slice(&file.durable_len.to_le_bytes());
                prev_end = file.end();
                prev_id = Some(file.id);
            }
            prev_ns = Some(tier.ns);
        }
        out
    }

    /// Decode and validate one payload (epoch 1 or 2). Canonical:
    /// trailing bytes, a non-ascending set, an empty set, a floor
    /// mismatch, or any tier-section violation (epoch 2 with no
    /// sections, unordered namespaces/files, overlapping or
    /// past-`flushed` ranges, zero-length ranges, non-48-bit addresses)
    /// are all named errors — recovery never guesses (§8.4).
    ///
    /// # Errors
    /// [`ManifestDecodeError`] naming the exact violation.
    pub fn decode(payload: &[u8]) -> Result<Manifest, ManifestDecodeError> {
        if payload.len() < FIXED_LEN {
            return Err(ManifestDecodeError::Truncated { at: payload.len() });
        }
        if payload[..8] != MANIFEST_MAGIC {
            let mut got = [0u8; 8];
            got.copy_from_slice(&payload[..8]);
            return Err(ManifestDecodeError::BadMagic { got });
        }
        let epoch = u32::from_le_bytes(payload[8..12].try_into().expect("4 bytes"));
        if epoch != MANIFEST_EPOCH && epoch != MANIFEST_EPOCH_V2 {
            return Err(ManifestDecodeError::UnsupportedEpoch { epoch });
        }
        let ckpt_id = u64::from_le_bytes(payload[12..20].try_into().expect("8 bytes"));
        let begin_lsn =
            Lsn::from_u64(u64::from_le_bytes(payload[20..28].try_into().expect("8 bytes")));
        let count = u32::from_le_bytes(payload[28..32].try_into().expect("4 bytes")) as usize;
        if count == 0 {
            return Err(ManifestDecodeError::NoSegments);
        }
        if count > MAX_SEGMENTS {
            return Err(ManifestDecodeError::TooManySegments { count });
        }
        let segments_end = FIXED_LEN + count * 4;
        if payload.len() < segments_end {
            return Err(ManifestDecodeError::Truncated { at: payload.len() });
        }
        let segments: Vec<SegmentId> = payload[FIXED_LEN..segments_end]
            .chunks_exact(4)
            .map(|c| SegmentId(u32::from_le_bytes(c.try_into().expect("4 bytes"))))
            .collect();
        for (i, pair) in segments.windows(2).enumerate() {
            if pair[1] <= pair[0] {
                return Err(ManifestDecodeError::SegmentsNotAscending { index: i + 1 });
            }
        }
        if segments[0] != begin_lsn.segment {
            return Err(ManifestDecodeError::FloorMismatch {
                first: segments[0],
                begin_segment: begin_lsn.segment,
            });
        }
        let tiers = if epoch == MANIFEST_EPOCH {
            if payload.len() > segments_end {
                return Err(ManifestDecodeError::TrailingBytes {
                    extra: payload.len() - segments_end,
                });
            }
            Vec::new()
        } else {
            Self::decode_tiers(payload, segments_end)?
        };
        Ok(Manifest { ckpt_id, begin_lsn, segments, tiers })
    }

    fn decode_tiers(
        payload: &[u8],
        mut at: usize,
    ) -> Result<Vec<TierNsManifest>, ManifestDecodeError> {
        let take = |at: &mut usize, n: usize| -> Result<&[u8], ManifestDecodeError> {
            let end = at.checked_add(n).ok_or(ManifestDecodeError::Truncated { at: *at })?;
            let slice = payload
                .get(*at..end)
                .ok_or(ManifestDecodeError::Truncated { at: payload.len() })?;
            *at = end;
            Ok(slice)
        };
        let ns_count = u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes")) as usize;
        if ns_count == 0 {
            // Canonical: an empty tier set re-encodes as epoch 1.
            return Err(ManifestDecodeError::NoTierSections);
        }
        if ns_count > MAX_TIER_NS {
            return Err(ManifestDecodeError::TooManyTierSections { count: ns_count });
        }
        let mut tiers: Vec<TierNsManifest> = Vec::with_capacity(ns_count.min(64));
        let mut prev_ns: Option<u32> = None;
        for _ in 0..ns_count {
            let head = take(&mut at, TIER_NS_FIXED_LEN)?;
            let ns = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes"));
            let flushed = u64::from_le_bytes(head[4..12].try_into().expect("8 bytes"));
            let file_count = u32::from_le_bytes(head[12..16].try_into().expect("4 bytes")) as usize;
            if prev_ns.is_some_and(|p| ns <= p) {
                return Err(ManifestDecodeError::TierNsNotAscending { ns });
            }
            if flushed >= ADDR_LIMIT {
                return Err(ManifestDecodeError::TierAddrOutOfRange { ns, value: flushed });
            }
            if file_count > MAX_TIER_FILES {
                return Err(ManifestDecodeError::TooManyTierFiles { ns, count: file_count });
            }
            let mut files: Vec<TierFileRange> = Vec::with_capacity(file_count.min(1024));
            let mut prev_end = 0u64;
            let mut prev_id: Option<u32> = None;
            for _ in 0..file_count {
                let raw = take(&mut at, TIER_FILE_LEN)?;
                let file = TierFileRange {
                    id: u32::from_le_bytes(raw[0..4].try_into().expect("4 bytes")),
                    base: u64::from_le_bytes(raw[4..12].try_into().expect("8 bytes")),
                    durable_len: u64::from_le_bytes(raw[12..20].try_into().expect("8 bytes")),
                };
                if file.durable_len == 0 {
                    return Err(ManifestDecodeError::TierRangeEmpty { ns, id: file.id });
                }
                let Some(end) = file.base.checked_add(file.durable_len) else {
                    return Err(ManifestDecodeError::TierAddrOutOfRange { ns, value: file.base });
                };
                if end > flushed || file.base < prev_end {
                    return Err(ManifestDecodeError::TierRangeBroken { ns, id: file.id });
                }
                if prev_id.is_some_and(|p| file.id <= p) {
                    return Err(ManifestDecodeError::TierFilesNotAscending { ns, id: file.id });
                }
                prev_end = end;
                prev_id = Some(file.id);
                files.push(file);
            }
            prev_ns = Some(ns);
            tiers.push(TierNsManifest { ns, flushed, files });
        }
        if at != payload.len() {
            return Err(ManifestDecodeError::TrailingBytes { extra: payload.len() - at });
        }
        Ok(tiers)
    }
}

/// Named decode failures — every one is fail-stop for recovery (the
/// envelope CRC already passed, so damage here is disk lying or a
/// writer bug, never line noise).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestDecodeError {
    BadMagic {
        got: [u8; 8],
    },
    UnsupportedEpoch {
        epoch: u32,
    },
    Truncated {
        at: usize,
    },
    TrailingBytes {
        extra: usize,
    },
    NoSegments,
    TooManySegments {
        count: usize,
    },
    SegmentsNotAscending {
        index: usize,
    },
    FloorMismatch {
        first: SegmentId,
        begin_segment: SegmentId,
    },
    /// Epoch 2 with zero tier sections — re-encodes as epoch 1, so the
    /// form is non-canonical by construction (ADR-0057 D5).
    NoTierSections,
    TooManyTierSections {
        count: usize,
    },
    TierNsNotAscending {
        ns: u32,
    },
    TooManyTierFiles {
        ns: u32,
        count: usize,
    },
    TierFilesNotAscending {
        ns: u32,
        id: u32,
    },
    /// A named range is zero-length (unconfirmed files are never named).
    TierRangeEmpty {
        ns: u32,
        id: u32,
    },
    /// A range overlaps its predecessor or ends past `flushed` — the
    /// §3.1 tiling corollary violated on disk.
    TierRangeBroken {
        ns: u32,
        id: u32,
    },
    /// A watermark or range breaches the 48-bit address space.
    TierAddrOutOfRange {
        ns: u32,
        value: u64,
    },
}

impl fmt::Display for ManifestDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestDecodeError::BadMagic { got } => write!(f, "MANIFEST bad magic {got:02x?}"),
            ManifestDecodeError::UnsupportedEpoch { epoch } => {
                write!(f, "MANIFEST unsupported format epoch {epoch}")
            }
            ManifestDecodeError::Truncated { at } => {
                write!(f, "MANIFEST payload truncated at {at} bytes")
            }
            ManifestDecodeError::TrailingBytes { extra } => {
                write!(f, "MANIFEST payload has {extra} trailing bytes")
            }
            ManifestDecodeError::NoSegments => write!(f, "MANIFEST names an empty segment set"),
            ManifestDecodeError::TooManySegments { count } => {
                write!(f, "MANIFEST segment count {count} exceeds the sanity bound")
            }
            ManifestDecodeError::SegmentsNotAscending { index } => {
                write!(f, "MANIFEST segment set not strictly ascending at index {index}")
            }
            ManifestDecodeError::FloorMismatch { first, begin_segment } => write!(
                f,
                "MANIFEST floor mismatch: first segment {first}, begin-LSN segment {begin_segment}"
            ),
            ManifestDecodeError::NoTierSections => {
                write!(f, "MANIFEST epoch 2 with no tier sections (non-canonical)")
            }
            ManifestDecodeError::TooManyTierSections { count } => {
                write!(f, "MANIFEST tier section count {count} exceeds the sanity bound")
            }
            ManifestDecodeError::TierNsNotAscending { ns } => {
                write!(f, "MANIFEST tier namespaces not strictly ascending at ns {ns}")
            }
            ManifestDecodeError::TooManyTierFiles { ns, count } => {
                write!(f, "MANIFEST ns {ns} tier file count {count} exceeds the sanity bound")
            }
            ManifestDecodeError::TierFilesNotAscending { ns, id } => {
                write!(f, "MANIFEST ns {ns} tier file ids not strictly ascending at id {id}")
            }
            ManifestDecodeError::TierRangeEmpty { ns, id } => {
                write!(f, "MANIFEST ns {ns} tier file {id} names an empty range")
            }
            ManifestDecodeError::TierRangeBroken { ns, id } => {
                write!(f, "MANIFEST ns {ns} tier file {id} range overlaps or ends past flushed")
            }
            ManifestDecodeError::TierAddrOutOfRange { ns, value } => {
                write!(f, "MANIFEST ns {ns} address {value} breaches the 48-bit space")
            }
        }
    }
}

impl std::error::Error for ManifestDecodeError {}

/// The full envelope image [`write_manifest`] stages — exposed for the
/// reactor-tier asynchronous swap (M2-S12, ADR-0017): the plane writes
/// these bytes to `MANIFEST.new` itself and drives the fdatasync /
/// rename / dir-fsync steps through `BackendDriver`, so publication never
/// blocks the loop on a device barrier. Same bytes, same step order, same
/// crash windows as the synchronous protocol.
#[must_use]
pub fn manifest_envelope(m: &Manifest) -> Vec<u8> {
    crate::meta::encode_envelope(&m.encode()).expect("manifest payload bounded far below u32")
}

/// Durably replace `shard_dir/MANIFEST` — the [`crate::meta`] swap class.
/// On return the new recovery unit survives power loss; a crash at any
/// earlier step leaves the previous manifest (or its absence) intact.
///
/// # Errors
/// Any swap step's I/O failure. The caller owns the policy: the M2
/// truncation slice counts the failure and keeps the old recovery unit —
/// nothing was acked against the new manifest (ADR-0017; deliberately the
/// checkpoint-abort class, not §8.4 fail-stop).
pub fn write_manifest<F: SegmentFs>(fs: &F, shard_dir: &Path, m: &Manifest) -> io::Result<()> {
    write_envelope(fs, shard_dir, MANIFEST_STAGING_FILE, MANIFEST_FILE, &m.encode())
}

/// Read and validate `shard_dir/MANIFEST`. `Ok(None)` = no manifest (no
/// checkpoint has ever been published — recovery replays the whole log).
///
/// # Errors
/// `InvalidData` on envelope or schema corruption — fail-stop for the
/// caller, never treated as absent.
pub fn read_manifest<F: SegmentFs>(fs: &F, shard_dir: &Path) -> io::Result<Option<Manifest>> {
    let Some(payload) = read_envelope(fs, &shard_dir.join(MANIFEST_FILE))? else {
        return Ok(None);
    };
    Manifest::decode(&payload)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            ckpt_id: 7,
            begin_lsn: Lsn::new(SegmentId(3), 0x40),
            segments: vec![SegmentId(3), SegmentId(4), SegmentId(5)],
            tiers: Vec::new(),
        }
    }

    fn sample_v2() -> Manifest {
        Manifest {
            tiers: vec![
                TierNsManifest {
                    ns: 16,
                    flushed: 9000,
                    files: vec![
                        TierFileRange { id: 0, base: 0, durable_len: 4000 },
                        // A ring-top gap [4000, 4200) between files is legal.
                        TierFileRange { id: 1, base: 4200, durable_len: 4800 },
                    ],
                },
                TierNsManifest { ns: 17, flushed: 0, files: vec![] },
            ],
            ..sample()
        }
    }

    #[test]
    fn roundtrip_is_byte_exact() {
        let m = sample();
        let payload = m.encode();
        assert_eq!(u32::from_le_bytes(payload[8..12].try_into().unwrap()), MANIFEST_EPOCH);
        assert_eq!(Manifest::decode(&payload).expect("valid"), m);
        assert_eq!(Manifest::decode(&payload).expect("valid").encode(), payload);
    }

    /// A tier-carrying manifest encodes as epoch 2 and round-trips byte-
    /// exact; a tierless one stays epoch-1 byte-identical to the M2
    /// writer (the degenerate case is absence — ADR-0057 D5).
    #[test]
    fn v2_roundtrip_and_v1_byte_identity() {
        let m2 = sample_v2();
        let payload = m2.encode();
        assert_eq!(u32::from_le_bytes(payload[8..12].try_into().unwrap()), MANIFEST_EPOCH_V2);
        assert_eq!(Manifest::decode(&payload).expect("valid"), m2);
        assert_eq!(Manifest::decode(&payload).expect("valid").encode(), payload);
        assert_eq!(Manifest::decode(&payload).unwrap().tier_ns(16).unwrap().files.len(), 2);
        assert!(Manifest::decode(&payload).unwrap().tier_ns(99).is_none());
        // The epoch-1 prefix of the v2 payload is exactly the v1 encoding.
        let v1 = sample().encode();
        assert_eq!(&payload[12..v1.len()], &v1[12..], "shared fields are byte-identical");
        // Truncation anywhere in the tier region is named, never a panic.
        for cut in v1.len()..payload.len() {
            let err = Manifest::decode(&payload[..cut]).expect_err("truncation must fail");
            assert!(
                matches!(
                    err,
                    ManifestDecodeError::Truncated { .. } | ManifestDecodeError::NoTierSections
                ),
                "cut {cut}: got {err:?}"
            );
        }
    }

    #[test]
    fn every_tier_violation_is_named() {
        // Epoch 2 with zero sections is non-canonical.
        let mut empty = sample_v2().encode();
        let seg_end = empty.len() - 4 - 2 * TIER_NS_FIXED_LEN - 2 * TIER_FILE_LEN;
        empty.truncate(seg_end);
        empty.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(Manifest::decode(&empty), Err(ManifestDecodeError::NoTierSections));

        // Namespaces must strictly ascend.
        let mut payload = sample_v2().encode();
        let dup_at = payload.len() - TIER_NS_FIXED_LEN;
        payload[dup_at..dup_at + 4].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(
            Manifest::decode(&payload),
            Err(ManifestDecodeError::TierNsNotAscending { ns: 16 })
        );

        // Overlapping ranges, empty ranges, ranges past flushed, and
        // non-ascending ids are each named (mutate the second file entry).
        let base = sample_v2();
        let encode_with = |mutate: fn(&mut TierNsManifest)| {
            let mut m = base.clone();
            mutate(&mut m.tiers[0]);
            m
        };
        let overlap = encode_with(|t| t.files[1].base = 3999);
        assert!(std::panic::catch_unwind(|| overlap.encode()).is_err(), "writer half refuses");
        let mut raw = base.encode();
        // files[1] entry sits TIER_NS_FIXED_LEN + TIER_FILE_LEN before ns 17's section.
        let f1_at = raw.len() - TIER_NS_FIXED_LEN - TIER_FILE_LEN;
        raw[f1_at + 4..f1_at + 12].copy_from_slice(&3999u64.to_le_bytes());
        assert_eq!(
            Manifest::decode(&raw),
            Err(ManifestDecodeError::TierRangeBroken { ns: 16, id: 1 })
        );
        let mut raw = base.encode();
        raw[f1_at + 12..f1_at + 20].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            Manifest::decode(&raw),
            Err(ManifestDecodeError::TierRangeEmpty { ns: 16, id: 1 })
        );
        let mut raw = base.encode();
        raw[f1_at + 12..f1_at + 20].copy_from_slice(&50_000u64.to_le_bytes());
        assert_eq!(
            Manifest::decode(&raw),
            Err(ManifestDecodeError::TierRangeBroken { ns: 16, id: 1 })
        );
        let mut raw = base.encode();
        raw[f1_at..f1_at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Manifest::decode(&raw),
            Err(ManifestDecodeError::TierFilesNotAscending { ns: 16, id: 0 })
        );

        // Trailing bytes after the tier sections are refused.
        let mut raw = base.encode();
        raw.push(0);
        assert_eq!(Manifest::decode(&raw), Err(ManifestDecodeError::TrailingBytes { extra: 1 }));

        // A 48-bit breach in flushed is refused.
        let mut m = base.clone();
        m.tiers[1].flushed = ADDR_LIMIT;
        assert!(std::panic::catch_unwind(move || m.encode()).is_err(), "writer half refuses");
        let mut raw = base.encode();
        let ns17_at = raw.len() - TIER_NS_FIXED_LEN;
        raw[ns17_at + 4..ns17_at + 12].copy_from_slice(&ADDR_LIMIT.to_le_bytes());
        assert_eq!(
            Manifest::decode(&raw),
            Err(ManifestDecodeError::TierAddrOutOfRange { ns: 17, value: ADDR_LIMIT })
        );
    }

    #[test]
    fn every_field_violation_is_named() {
        let good = sample().encode();

        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert!(matches!(Manifest::decode(&bad), Err(ManifestDecodeError::BadMagic { .. })));

        let mut bad = good.clone();
        bad[8] = 9; // epoch
        assert!(matches!(
            Manifest::decode(&bad),
            Err(ManifestDecodeError::UnsupportedEpoch { epoch: 9 })
        ));

        for cut in 0..good.len() {
            let err = Manifest::decode(&good[..cut]).expect_err("truncation must fail");
            assert!(matches!(err, ManifestDecodeError::Truncated { .. }), "cut {cut}: got {err:?}");
        }

        let mut bad = good.clone();
        bad.push(0);
        assert!(matches!(
            Manifest::decode(&bad),
            Err(ManifestDecodeError::TrailingBytes { extra: 1 })
        ));

        let mut none = sample();
        none.segments.clear();
        let mut payload = none.encode();
        assert!(matches!(Manifest::decode(&payload), Err(ManifestDecodeError::NoSegments)));
        // A count field larger than the body: truncated, not a wild alloc.
        payload[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Manifest::decode(&payload),
            Err(ManifestDecodeError::TooManySegments { .. })
        ));

        let mut dup = sample();
        dup.segments = vec![SegmentId(3), SegmentId(3)];
        assert!(matches!(
            Manifest::decode(&dup.encode()),
            Err(ManifestDecodeError::SegmentsNotAscending { index: 1 })
        ));

        let mut off = sample();
        off.segments = vec![SegmentId(4), SegmentId(5)];
        assert!(matches!(
            Manifest::decode(&off.encode()),
            Err(ManifestDecodeError::FloorMismatch { .. })
        ));
    }
}
