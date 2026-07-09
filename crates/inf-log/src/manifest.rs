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
//!   later milestones (M5 retention exemptions, M7 tiering) may publish
//!   holes; recovery accepts segments appended after publication and
//!   fail-stops if a *listed* segment is missing.
//!
//! ```text
//! payload := magic:   [u8;8] = "INFMAN1\0"
//!            epoch:   u32 LE  — format epoch (1)
//!            ckpt_id: u64 LE  — the named checkpoint (`ckpt-{id}.ick`)
//!            begin:   u64 LE  — packed begin-LSN (Lsn::to_u64)
//!            count:   u32 LE
//!            count × u32 LE   — segment ids, strictly ascending,
//!                               segments[0] == begin.segment (the floor)
//! ```
//! The envelope ([`crate::meta`]) adds magic, length, and CRC32C around
//! this payload; corruption at either layer is a named fail-stop error.

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
/// Payload magic; the trailing `1` tags schema v1 alongside `epoch`.
pub const MANIFEST_MAGIC: [u8; 8] = *b"INFMAN1\0";
/// Format epoch this writer emits (schema v1).
pub const MANIFEST_EPOCH: u32 = 1;

/// Defensive bound on the decoded segment-set size: 2²⁰ segments × 256 MiB
/// is a 256 TiB cell log — corruption, not configuration.
const MAX_SEGMENTS: usize = 1 << 20;

const FIXED_LEN: usize = 8 + 4 + 8 + 8 + 4;

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
}

impl Manifest {
    /// The truncation floor: the first live segment. Everything below is
    /// covered by the named checkpoint.
    #[must_use]
    pub fn floor(&self) -> SegmentId {
        self.begin_lsn.segment
    }

    /// Canonical schema-v1 payload bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED_LEN + self.segments.len() * 4);
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&MANIFEST_EPOCH.to_le_bytes());
        out.extend_from_slice(&self.ckpt_id.to_le_bytes());
        out.extend_from_slice(&self.begin_lsn.to_u64().to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.segments.len()).expect("segment count").to_le_bytes(),
        );
        for seg in &self.segments {
            out.extend_from_slice(&seg.0.to_le_bytes());
        }
        out
    }

    /// Decode and validate one schema-v1 payload. Canonical: trailing
    /// bytes, a non-ascending set, an empty set, or a floor mismatch are
    /// all named errors — recovery never guesses (§8.4).
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
        if epoch != MANIFEST_EPOCH {
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
        let expected = FIXED_LEN + count * 4;
        if payload.len() < expected {
            return Err(ManifestDecodeError::Truncated { at: payload.len() });
        }
        if payload.len() > expected {
            return Err(ManifestDecodeError::TrailingBytes { extra: payload.len() - expected });
        }
        let segments: Vec<SegmentId> = payload[FIXED_LEN..]
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
        Ok(Manifest { ckpt_id, begin_lsn, segments })
    }
}

/// Named schema-v1 decode failures — every one is fail-stop for recovery
/// (the envelope CRC already passed, so damage here is disk lying or a
/// writer bug, never line noise).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestDecodeError {
    BadMagic { got: [u8; 8] },
    UnsupportedEpoch { epoch: u32 },
    Truncated { at: usize },
    TrailingBytes { extra: usize },
    NoSegments,
    TooManySegments { count: usize },
    SegmentsNotAscending { index: usize },
    FloorMismatch { first: SegmentId, begin_segment: SegmentId },
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
        }
    }

    #[test]
    fn roundtrip_is_byte_exact() {
        let m = sample();
        let payload = m.encode();
        assert_eq!(Manifest::decode(&payload).expect("valid"), m);
        assert_eq!(Manifest::decode(&payload).expect("valid").encode(), payload);
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
