//! Tail-region scan mechanics for torn-final vs interior-corruption
//! classification (M2-S14, §8.4, L9; extended by M2.5-S12/ADR-0031).
//!
//! A torn *final* write is expected physics: a power cut mid-write leaves
//! a partial frame, garbage remnants, or a zero hole at the tail — none of
//! it fsync-covered (a completed fdatasync would have made it whole), so
//! none of it acked. A **validating frame beyond a segment's data end**
//! needs evidence-based classification (ADR-0031 D4): it is either real
//! log data the replay cannot reach (covered bytes the disk lost — the
//! fail-stop [`LogCorruption`] class) or the surviving remainder of a
//! reorder hole in the un-covered suffix (legal torn-tail physics, safe to
//! truncate). Frame format v2's stamp is what discriminates: any surviving
//! frame whose `covered_lsn` attests coverage at or past the data end
//! proves the gap sat in covered territory; absent such attestation, the
//! hole is consistent only with un-covered loss. v1 frames attest nothing
//! and stay in the fail-stop class (the pre-ADR-0031 rule).
//!
//! What "validating" means here is strict: the frame decodes (magic,
//! length, CRC32C) **and self-locates** — its stored first-record LSN
//! matches the physical offset it was found at (the ADR-0011 misdirected-
//! write check). Remnant frames from a previous, truncated life fail the
//! LSN check at any shifted offset; payload bytes that happen to contain
//! the magic fail the CRC or the LSN. Only a frame written *for exactly
//! this position* classifies as data.
//!
//! **Foreign-segment frames** (M4.5-S39b, ADR-0090 D2 as amended): a frame
//! that decodes in full and sits at its stored *offset* but is stamped
//! for *another segment id* is the residue a recycled file carries from
//! its previous life. It is counted apart (`RegionEvidence::foreign_
//! frames`, `max_foreign_epoch`), contributes no attestation, epoch or
//! hole evidence (it attests another life's coverage in another file),
//! and is skipped by its padded extent **only because its CRC passed**:
//! a stale header over a body this life partly overwrote fails the CRC
//! and is scanned byte-wise, so a same-segment validating frame can never
//! hide behind a foreign length field. The recovery policy reads "no
//! self-located frame, ≥ 1 foreign frame" as proven residue — never a
//! hole, never torn.
//!
//! This module owns the facts ([`scan_region`], [`RegionScan`],
//! [`scan_region_evidence`], [`RegionEvidence`]) and the fatal type; the
//! recovery *policy* — which segments to scan, the attestation rule,
//! torn-tail truncation, sealed-slack residue tolerance, trailing-segment
//! GC, the begin-LSN guard, epoch derivation — lives in
//! `inf-server::recover` (ADR-0018/0031), the same facts-here/policy-there
//! split as the S04 reader.
//!
//! Soundness of resuming over remnants (ADR-0031 D5): a torn-tail resume
//! appends new frames over the remnant region under a **fresh epoch**
//! strictly above every epoch observed this boot — including the epochs of
//! the validating beyond-frames this scan aggregates. New frames validate
//! at their own positions; a hybrid of new bytes and remnant bytes fails
//! the CRC; a durably-whole remnant that later resurfaces at the new
//! life's data end carries its old, lower epoch and is rejected by the
//! replay prefix's epoch-monotonicity rule — it can never re-enter a
//! prefix. Durably-torn remnants can never validate at all.

use core::fmt;
use std::io;
use std::path::Path;

use crate::frame::{FrameStamp, decode_frame, frame_shape};
use crate::fs::{SegmentFile, SegmentFs};
use crate::lsn::{Lsn, SegmentId};
use crate::reader::ReaderConfig;
use crate::segment::segment_file_name;

/// What one region scan found, strongest evidence first.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegionScan {
    /// A frame decodes and self-locates at `offset`: real log data.
    ValidFrame { offset: u32 },
    /// Nonzero bytes but no validating frame — torn-write remnants;
    /// `first_nonzero` is the lowest nonzero offset seen.
    Garbage { first_nonzero: u32 },
    /// Entirely zero: pristine preallocated bytes.
    AllZero,
}

/// Aggregate facts about every validating, self-located frame in a region
/// (ADR-0031 D4). O(1) memory by construction: an adversarial image could
/// pack thousands of validating frames into slack, so the policy consumes
/// maxima, never a frame list.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct RegionEvidence {
    /// Validating self-located frames found (0 = none).
    pub valid_frames: u64,
    /// Offset of the first validating frame (the refusal evidence anchor).
    pub first_valid: Option<u32>,
    /// Strongest coverage attestation among v2 frames (`covered_lsn` max).
    pub max_covered_lsn: u64,
    /// Highest epoch among v2 frames (the ADR-0031 D5 derivation input).
    pub max_epoch: u32,
    /// A v1 frame validated here — it attests nothing, so the conservative
    /// pre-ADR-0031 classification applies.
    pub any_v1: bool,
    /// Lowest nonzero offset seen (garbage/remnant indicator).
    pub first_nonzero: Option<u32>,
    /// Decoded, offset-located frames stamped for **another segment id**
    /// (recycled-life residue, ADR-0090 D2 as amended). Never evidence
    /// of this life; counted so the policy can prove a slack is residue.
    pub foreign_frames: u64,
    /// Highest epoch among foreign-segment frames — folded into the
    /// resume-epoch derivation so "tops every epoch observed this boot"
    /// (ADR-0031 D5) is literal.
    pub max_foreign_epoch: u32,
    /// Bytes the scan read from the file (M4.5-S39d): the audit's device
    /// cost, so a boot's recovery time decomposes into phases with their
    /// bytes beside them. Zero-run skipping consumes bytes it still had
    /// to read, so this is the read extent, never the decoded one.
    pub bytes_read: u64,
}

impl RegionEvidence {
    /// The M2-S14 three-way summary this evidence collapses to.
    #[must_use]
    pub fn summary(&self) -> RegionScan {
        if let Some(offset) = self.first_valid {
            return RegionScan::ValidFrame { offset };
        }
        match self.first_nonzero {
            Some(first_nonzero) => RegionScan::Garbage { first_nonzero },
            None => RegionScan::AllZero,
        }
    }

    /// Fold another region's evidence in (the resume region spans the tail
    /// segment and its trailing segments; offsets keep per-segment meaning
    /// so only the aggregates merge).
    pub fn absorb(&mut self, other: &RegionEvidence) {
        self.valid_frames += other.valid_frames;
        self.max_covered_lsn = self.max_covered_lsn.max(other.max_covered_lsn);
        self.max_epoch = self.max_epoch.max(other.max_epoch);
        self.any_v1 |= other.any_v1;
        self.foreign_frames += other.foreign_frames;
        self.max_foreign_epoch = self.max_foreign_epoch.max(other.max_foreign_epoch);
        self.bytes_read += other.bytes_read;
    }

    /// Proven recycled-life residue (ADR-0090 D2 as amended): no frame
    /// of this segment validates in the region, at least one foreign-
    /// segment frame does. Garbage beside it does not change the
    /// verdict (a torn tail over residue is indistinguishable from
    /// residue by construction, and nothing acked can sit past a data
    /// end).
    #[must_use]
    pub fn is_recycled_residue(&self) -> bool {
        self.valid_frames == 0 && self.foreign_frames > 0
    }
}

/// The fatal taxonomy entry (M2-S14): interior corruption, named exactly.
/// The process must refuse to start; there is no safe truncation point.
#[derive(Clone, Debug)]
pub struct LogCorruption {
    /// Where the segment's replayable data ended (the corruption point).
    pub segment: SegmentId,
    pub offset: u32,
    /// The validating frame found beyond it — the evidence that bytes
    /// between `offset` and here were lost or corrupted after covering.
    pub evidence_segment: SegmentId,
    pub evidence_offset: u32,
    /// What the bytes at the corruption point looked like (a rendered
    /// [`ReadError`](crate::ReadError), or a description of the gap).
    pub detail: String,
}

impl fmt::Display for LogCorruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "log corruption in {} at {:#x}: {}; a validating frame follows at {} offset {:#x} — \
             interior data would be lost by truncation, refusing to start (§8.4)",
            self.segment, self.offset, self.detail, self.evidence_segment, self.evidence_offset
        )
    }
}

impl std::error::Error for LogCorruption {}

/// Scan `segment` from byte `from` for the strongest evidence present:
/// a validating self-located frame beats remnant garbage beats zeros.
/// Stops at the first validating frame — the sealed-slack policy needs
/// existence, not aggregates ([`scan_region_evidence`] is the exhaustive
/// form). Boot-path only — the scan streams in `cfg.chunk_bytes` windows
/// and never holds more than one frame (`cfg.max_frame_len`) resident;
/// zero regions of preallocated-but-unwritten extents read at metadata
/// speed (holes / unwritten extents never touch the device).
///
/// # Errors
/// Open/read failures on the segment file.
pub fn scan_region<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    segment: SegmentId,
    from: u32,
    cfg: ReaderConfig,
) -> io::Result<RegionScan> {
    Ok(scanner(fs, log_dir, segment, from, cfg)?.run(true)?.summary())
}

/// Exhaustive form of [`scan_region`] (ADR-0031 D4): walks the whole
/// region, aggregating every validating frame's stamp facts — the
/// attestation/epoch inputs the resume-region policy consumes. Same
/// bounded-memory contract as `scan_region`.
///
/// # Errors
/// Open/read failures on the segment file.
pub fn scan_region_evidence<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    segment: SegmentId,
    from: u32,
    cfg: ReaderConfig,
) -> io::Result<RegionEvidence> {
    scanner(fs, log_dir, segment, from, cfg)?.run(false)
}

fn scanner<F: SegmentFs>(
    fs: &F,
    log_dir: &Path,
    segment: SegmentId,
    from: u32,
    cfg: ReaderConfig,
) -> io::Result<RegionScanner<F::File>> {
    let file = fs.open_read(&log_dir.join(segment_file_name(segment)))?;
    Ok(RegionScanner {
        file,
        segment,
        cfg,
        buf: vec![0; cfg.chunk_bytes.max(crate::frame::FRAME_HEADER_LEN)],
        start: 0,
        valid: 0,
        offset: from,
        file_pos: u64::from(from),
        hit_eof: false,
    })
}

/// Length of the zero-byte run at the head of `w` (which starts with a
/// zero byte), scanned 16 bytes at a time.
fn zero_run(w: &[u8]) -> usize {
    let mut i = 0;
    while i + 16 <= w.len() {
        let chunk: [u8; 16] = w[i..i + 16].try_into().expect("16-byte slice");
        if u128::from_le_bytes(chunk) != 0 {
            break;
        }
        i += 16;
    }
    while i < w.len() && w[i] == 0 {
        i += 1;
    }
    i
}

/// Where a decoded frame says it belongs relative to where it was found.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Placement {
    /// Stored LSN == physical position: a validating frame of this file.
    SelfLocated,
    /// Stored offset == physical offset, stored segment ≠ this file:
    /// recycled-life residue (ADR-0090 D2 as amended).
    ForeignSegment,
}

/// A decoded frame the scanner probed at the current offset: its
/// on-device length (to skip — the aligned extent for v3, ADR-0086 D3),
/// its stamp facts, and its placement. A frame at a shifted offset is
/// never probed as a frame at all (it is garbage at this position).
struct ProbedFrame {
    frame_len: usize,
    stamp: Option<FrameStamp>,
    placement: Placement,
}

struct RegionScanner<File: SegmentFile> {
    file: File,
    segment: SegmentId,
    cfg: ReaderConfig,
    /// `buf[start..valid]` are unconsumed file bytes; `buf[start]` sits at
    /// segment offset `offset`.
    buf: Vec<u8>,
    start: usize,
    valid: usize,
    offset: u32,
    file_pos: u64,
    hit_eof: bool,
}

impl<File: SegmentFile> RegionScanner<File> {
    /// Walk the region. With `stop_at_first_valid` the walk returns at the
    /// first validating frame (the [`scan_region`] contract); otherwise it
    /// aggregates every validating frame, skipping each frame's own bytes
    /// (interior sub-frames of a validated frame are body payload, not
    /// writer output — they carry no independent evidence).
    fn run(&mut self, stop_at_first_valid: bool) -> io::Result<RegionEvidence> {
        let from = self.file_pos;
        let mut evidence = self.walk(stop_at_first_valid)?;
        evidence.bytes_read = self.file_pos - from;
        Ok(evidence)
    }

    fn walk(&mut self, stop_at_first_valid: bool) -> io::Result<RegionEvidence> {
        let mut evidence = RegionEvidence::default();
        loop {
            if self.window().is_empty() {
                if self.hit_eof {
                    break;
                }
                self.refill(1)?;
                continue;
            }
            let window = self.window();
            if window[0] == 0 {
                // Skip the zero run in this window, word-wise: the common
                // case is hundreds of MiB of preallocated zeros, and a
                // byte-wise scan was the measured floor of the every-boot
                // audit (S13 rehearsal `slack-floor` row, ADR-0018).
                self.consume(zero_run(window));
                continue;
            }
            evidence.first_nonzero.get_or_insert(self.offset);
            if !is_magic_first_byte(window[0]) {
                // Skip garbage up to the next possible magic or zero run.
                let run = window
                    .iter()
                    .position(|&b| b == 0 || is_magic_first_byte(b))
                    .unwrap_or(window.len());
                self.consume(run);
                continue;
            }
            if let Some(probed) = self.try_frame()? {
                match probed.placement {
                    Placement::SelfLocated => {
                        evidence.valid_frames += 1;
                        evidence.first_valid.get_or_insert(self.offset);
                        match probed.stamp {
                            Some(stamp) => {
                                evidence.max_covered_lsn =
                                    evidence.max_covered_lsn.max(stamp.covered_lsn);
                                evidence.max_epoch = evidence.max_epoch.max(stamp.epoch);
                            }
                            None => evidence.any_v1 = true,
                        }
                        if stop_at_first_valid {
                            return Ok(evidence);
                        }
                    }
                    Placement::ForeignSegment => {
                        // Residue of another life of this file: no
                        // attestation, no hole, no v1 conservatism — only
                        // the count and the epoch bound (module docs).
                        evidence.foreign_frames += 1;
                        if let Some(stamp) = probed.stamp {
                            evidence.max_foreign_epoch =
                                evidence.max_foreign_epoch.max(stamp.epoch);
                        }
                    }
                }
                self.consume(probed.frame_len);
                continue;
            }
            self.consume(1);
        }
        Ok(evidence)
    }

    /// Whether a fully decoding frame (either format) starts at the
    /// current offset **at its stored offset** — self-located, or stamped
    /// for another segment id (foreign). A frame at a shifted offset is
    /// `None` like any garbage. `None` is never terminal — the caller
    /// keeps scanning.
    fn try_frame(&mut self) -> io::Result<Option<ProbedFrame>> {
        const PROBE_HEADER: usize = crate::frame::FRAME_HEADER_LEN;
        if self.window().len() < PROBE_HEADER && !self.hit_eof {
            self.refill(PROBE_HEADER)?;
        }
        let window = self.window();
        if window.len() < 4 {
            return Ok(None);
        }
        let magic: [u8; 4] = window[0..4].try_into().expect("4-byte slice");
        let Ok(Some(shape)) = frame_shape(magic) else {
            return Ok(None);
        };
        if window.len() < shape.header_len {
            return Ok(None);
        }
        let frame_len = u32::from_le_bytes(window[4..8].try_into().expect("4-byte slice"));
        if frame_len < shape.min_frame_len || frame_len > self.cfg.max_frame_len {
            return Ok(None);
        }
        let frame_len = frame_len as usize;
        if self.window().len() < frame_len {
            if !self.hit_eof {
                self.refill(frame_len)?;
            }
            if self.window().len() < frame_len {
                return Ok(None);
            }
        }
        match decode_frame(self.window(), self.cfg.max_frame_len) {
            Ok((frame, _)) => {
                let expected = Lsn::new(self.segment, self.offset + frame.header_len() as u32);
                let stored = frame.first_lsn();
                let placement = if stored == expected {
                    Placement::SelfLocated
                } else if stored.offset == expected.offset {
                    // Planted-bug canary (ADR-0090 D5): the segment-blind
                    // scanner takes residue for this life's frames.
                    #[cfg(inf_canary_foreign_segment)]
                    {
                        Placement::SelfLocated
                    }
                    #[cfg(not(inf_canary_foreign_segment))]
                    {
                        Placement::ForeignSegment
                    }
                } else {
                    return Ok(None);
                };
                // Skip the padded extent: the padding is the frame's own
                // write, never independent evidence. A window shorter
                // than the padding is consumed like any other skip. The
                // skip is sound only because `decode_frame` passed the
                // CRC above (module docs) — never skip on a header alone.
                let skip = (frame.padded_len() as usize).min(self.window().len());
                Ok(Some(ProbedFrame { frame_len: skip, stamp: frame.stamp(), placement }))
            }
            Err(_) => Ok(None),
        }
    }

    fn window(&self) -> &[u8] {
        &self.buf[self.start..self.valid]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        self.offset += u32::try_from(n).expect("window fits u32");
    }

    /// Ensure the window holds `needed` bytes from the current offset (or
    /// EOF intervenes), compacting and reading in `chunk_bytes` strides.
    fn refill(&mut self, needed: usize) -> io::Result<()> {
        self.buf.copy_within(self.start..self.valid, 0);
        self.valid -= self.start;
        self.start = 0;
        let target = needed.max(self.cfg.chunk_bytes);
        if self.buf.len() < target {
            self.buf.resize(target, 0);
        }
        while self.valid < target && !self.hit_eof {
            let read = self.file.read_at(self.file_pos, &mut self.buf[self.valid..target])?;
            if read == 0 {
                self.hit_eof = true;
            } else {
                self.valid += read;
                self.file_pos += read as u64;
            }
        }
        Ok(())
    }
}

/// Both frame magics start with `b'I'` — the garbage-skip probe byte.
fn is_magic_first_byte(b: u8) -> bool {
    b == b'I'
}
