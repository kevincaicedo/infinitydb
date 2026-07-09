//! Tail-region scan mechanics for torn-final vs interior-corruption
//! classification (M2-S14, §8.4, L9).
//!
//! A torn *final* write is expected physics: a power cut mid-write leaves
//! a partial frame, garbage remnants, or a zero hole at the tail — none of
//! it fsync-covered (a completed fdatasync would have made it whole), so
//! none of it acked. A **validating frame beyond a segment's data end** is
//! not physics — it is real log data the replay cannot reach (a dropped-
//! write gap before it, or covered bytes the disk corrupted): that is
//! fail-stop with a named [`LogCorruption`], never silent truncation of
//! interior data.
//!
//! What "validating" means here is strict: the frame decodes (magic,
//! length, CRC32C) **and self-locates** — its stored first-record LSN
//! matches the physical offset it was found at (the ADR-0011 misdirected-
//! write check). Remnant frames from a previous, truncated life fail the
//! LSN check at any shifted offset; payload bytes that happen to contain
//! the magic fail the CRC or the LSN. Only a frame written *for exactly
//! this position* classifies as data.
//!
//! This module owns the facts ([`scan_region`], [`RegionScan`]) and the
//! fatal type; the recovery *policy* — which segments to scan, torn-tail
//! truncation, sealed-slack residue tolerance, trailing-segment GC, the
//! begin-LSN guard — lives in `inf-server::recover` (ADR-0018), the same
//! facts-here/policy-there split as the S04 reader.
//!
//! Soundness of resuming over non-validating remnants: a torn-tail resume
//! appends new frames over the remnant region. New frames validate at
//! their own positions; a hybrid of new bytes and remnant bytes fails the
//! CRC; and remnant frames beyond the new tail were verified
//! non-validating by this scan before the resume was allowed. Appends only
//! ever shrink the remnant region, so no future boot can find a validating
//! frame there that this boot did not.

use core::fmt;
use std::io;
use std::path::Path;

use crate::frame::{FRAME_HEADER_LEN, FRAME_MAGIC, MIN_FRAME_LEN, decode_frame};
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
/// Boot-path only — the scan streams in `cfg.chunk_bytes` windows and
/// never holds more than one frame (`cfg.max_frame_len`) resident; zero
/// regions of preallocated-but-unwritten extents read at metadata speed
/// (holes / unwritten extents never touch the device).
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
    let file = fs.open_read(&log_dir.join(segment_file_name(segment)))?;
    let mut scanner = RegionScanner {
        file,
        segment,
        cfg,
        buf: vec![0; cfg.chunk_bytes.max(FRAME_HEADER_LEN)],
        start: 0,
        valid: 0,
        offset: from,
        file_pos: u64::from(from),
        hit_eof: false,
    };
    scanner.run()
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
    fn run(&mut self) -> io::Result<RegionScan> {
        let mut first_nonzero: Option<u32> = None;
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
            first_nonzero.get_or_insert(self.offset);
            if window[0] != FRAME_MAGIC[0] {
                // Skip garbage up to the next possible magic or zero run.
                let run = window
                    .iter()
                    .position(|&b| b == 0 || b == FRAME_MAGIC[0])
                    .unwrap_or(window.len());
                self.consume(run);
                continue;
            }
            if self.try_frame()? {
                return Ok(RegionScan::ValidFrame { offset: self.offset });
            }
            self.consume(1);
        }
        Ok(match first_nonzero {
            Some(first_nonzero) => RegionScan::Garbage { first_nonzero },
            None => RegionScan::AllZero,
        })
    }

    /// Whether a validating, self-located frame starts at the current
    /// offset. `false` is never terminal — the caller keeps scanning.
    fn try_frame(&mut self) -> io::Result<bool> {
        if self.window().len() < FRAME_HEADER_LEN && !self.hit_eof {
            self.refill(FRAME_HEADER_LEN)?;
        }
        let window = self.window();
        if window.len() < FRAME_HEADER_LEN || window[0..4] != FRAME_MAGIC {
            return Ok(false);
        }
        let frame_len = u32::from_le_bytes(window[4..8].try_into().expect("4-byte slice"));
        if frame_len < MIN_FRAME_LEN || frame_len > self.cfg.max_frame_len {
            return Ok(false);
        }
        let frame_len = frame_len as usize;
        if self.window().len() < frame_len {
            if !self.hit_eof {
                self.refill(frame_len)?;
            }
            if self.window().len() < frame_len {
                return Ok(false);
            }
        }
        match decode_frame(self.window(), self.cfg.max_frame_len) {
            Ok((frame, _)) => {
                let expected = Lsn::new(self.segment, self.offset + FRAME_HEADER_LEN as u32);
                Ok(frame.first_lsn() == expected)
            }
            Err(_) => Ok(false),
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
