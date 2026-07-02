//! Sequential log read path (M2-S04): a CRC-validating, frame-at-a-time
//! reader over one segment — sealed segments and the active tail alike —
//! written once here and reused by recovery (M2-E5), topics (M5), and
//! replication (M9) (L2).
//!
//! Mechanics: the reader pulls **large sequential reads** (a read-ahead
//! window, default 1 MiB) through the injected [`SegmentFs`] seam so the
//! DST tier can fault every read (L7; migration of these reads onto
//! `BackendDriver` file ops rides M2-S05 — recorded in ADR-0012). A
//! header-only peek sizes the window so each frame is CRC-validated
//! exactly once ([`decode_frame`] never runs twice over the same bytes).
//! Every frame's stored `first_lsn` is cross-checked against its physical
//! offset — a misdirected write is a named error, not a silent replay of
//! bytes in the wrong place (ADR-0011 Decision 2).
//!
//! End-of-log policy is deliberately **facts, not policy** (M2-S14 owns
//! the torn-final vs interior-corruption split): a clean end is
//! [`ReadEnd::ZeroTail`] (preallocated, never-written bytes) or
//! [`ReadEnd::FileEnd`]; anything else surfaces as a typed [`ReadError`]
//! with the exact segment offset. `ReadEnd::at` is the byte after the
//! last valid frame — precisely the `tail_offset` that
//! [`SegmentRotor::open_existing`](crate::SegmentRotor::open_existing)
//! resumes appending at.

use core::fmt;
use std::io;
use std::path::Path;

use crate::frame::{
    FRAME_HEADER_LEN, FRAME_MAGIC, FrameDecodeError, FrameRef, MIN_FRAME_LEN, decode_frame,
};
use crate::fs::{SegmentFile, SegmentFs};
use crate::lsn::{Lsn, SegmentId};
use crate::segment::segment_file_name;

/// Default read-ahead window (bytes): large sequential reads amortize the
/// per-read syscall/fault cost (L3) while keeping the reader's resident
/// memory bounded and attributed.
pub const DEFAULT_READ_CHUNK: usize = 1 << 20;

/// Reader configuration.
#[derive(Copy, Clone, Debug)]
pub struct ReaderConfig {
    /// Read-ahead window size. The window grows (once) to the largest
    /// frame it encounters, bounded by `max_frame_len`.
    pub chunk_bytes: usize,
    /// Upper bound accepted for a single frame — must be ≥ the writer's
    /// staging capacity or valid frames become unreadable.
    pub max_frame_len: u32,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        ReaderConfig {
            chunk_bytes: DEFAULT_READ_CHUNK,
            max_frame_len: crate::frame::DEFAULT_MAX_FRAME_LEN,
        }
    }
}

/// How the written portion of a segment ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ReadEnd {
    /// Preallocated, never-written bytes begin at `at` (zero magic) — the
    /// normal end of the active segment and of any sealed segment whose
    /// last frame did not land exactly on the preallocation boundary.
    ZeroTail { at: u32 },
    /// The file ends exactly at `at` on a frame boundary.
    FileEnd { at: u32 },
}

impl ReadEnd {
    /// Byte offset one past the last valid frame — the recovered
    /// `tail_offset` for reopening the segment as the active tail.
    #[must_use]
    pub fn at(self) -> u32 {
        match self {
            ReadEnd::ZeroTail { at } | ReadEnd::FileEnd { at } => at,
        }
    }
}

/// Typed read-path failures. Every variant names the segment and the exact
/// offset — the inputs S14's torn-tail/corruption taxonomy classifies.
#[derive(Debug)]
pub enum ReadError {
    Io {
        segment: SegmentId,
        offset: u32,
        source: io::Error,
    },
    /// A frame at `offset` failed validation (bad magic/length, CRC
    /// mismatch, zero record count, or bytes ending mid-frame).
    Frame {
        segment: SegmentId,
        offset: u32,
        error: FrameDecodeError,
    },
    /// The frame's stored first-record LSN disagrees with its physical
    /// position: a misdirected write, never applied.
    LsnMismatch {
        segment: SegmentId,
        offset: u32,
        stored: Lsn,
        expected: Lsn,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io { segment, offset, source } => {
                write!(f, "log read I/O error on {segment} at {offset:#x}: {source}")
            }
            ReadError::Frame { segment, offset, error } => {
                write!(f, "invalid frame in {segment} at {offset:#x}: {error}")
            }
            ReadError::LsnMismatch { segment, offset, stored, expected } => write!(
                f,
                "misdirected frame in {segment} at {offset:#x}: stored LSN {stored}, \
                 physical position implies {expected}"
            ),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Io { source, .. } => Some(source),
            ReadError::Frame { error, .. } => Some(error),
            ReadError::LsnMismatch { .. } => None,
        }
    }
}

/// Why a batch-apply pass stopped early.
#[derive(Debug)]
pub enum ApplyError<E> {
    Read(ReadError),
    /// The apply callback failed on the frame whose first record is `at`.
    Apply {
        at: Lsn,
        error: E,
    },
}

impl<E: fmt::Display> fmt::Display for ApplyError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Read(err) => err.fmt(f),
            ApplyError::Apply { at, error } => write!(f, "frame apply failed at {at}: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ApplyError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApplyError::Read(err) => Some(err),
            ApplyError::Apply { error, .. } => Some(error),
        }
    }
}

/// Header-only classification of the bytes at a frame boundary — sizes the
/// window so [`decode_frame`] (and its CRC pass) runs exactly once per
/// frame. Mirrors `decode_frame`'s header checks; the round-trip proptests
/// pin the two against each other.
enum Peek {
    /// Zero magic: preallocated tail begins here.
    ZeroTail,
    /// A full frame of this many bytes is in the window.
    Ready(usize),
    /// The window needs this many bytes (from the frame boundary) to
    /// classify further.
    NeedMore(usize),
    /// The header itself is invalid.
    Bad(FrameDecodeError),
}

fn peek(window: &[u8], max_frame_len: u32) -> Peek {
    if window.len() < 4 {
        return Peek::NeedMore(4);
    }
    let magic: [u8; 4] = window[0..4].try_into().expect("4-byte slice");
    if magic == [0; 4] {
        return Peek::ZeroTail;
    }
    if magic != FRAME_MAGIC {
        return Peek::Bad(FrameDecodeError::BadMagic { found: magic });
    }
    if window.len() < FRAME_HEADER_LEN {
        return Peek::NeedMore(FRAME_HEADER_LEN);
    }
    let frame_len = u32::from_le_bytes(window[4..8].try_into().expect("4-byte slice"));
    if frame_len < MIN_FRAME_LEN || frame_len > max_frame_len {
        return Peek::Bad(FrameDecodeError::BadLength { len: frame_len });
    }
    let frame_len = frame_len as usize;
    if window.len() < frame_len { Peek::NeedMore(frame_len) } else { Peek::Ready(frame_len) }
}

/// Sequential frame reader over one segment file.
pub struct SegmentReader<File: SegmentFile> {
    file: File,
    segment: SegmentId,
    cfg: ReaderConfig,
    buf: Vec<u8>,
    /// Window bounds within `buf`: `buf[start..valid]` are unconsumed
    /// file bytes; `buf[start]` sits at segment offset `next_offset`.
    start: usize,
    valid: usize,
    next_offset: u32,
    /// Next file offset to read from.
    file_pos: u64,
    hit_eof: bool,
    end: Option<ReadEnd>,
    failed: bool,
}

impl<File: SegmentFile> fmt::Debug for SegmentReader<File> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentReader")
            .field("segment", &self.segment)
            .field("next_offset", &self.next_offset)
            .field("window", &(self.valid - self.start))
            .field("end", &self.end)
            .finish()
    }
}

impl<File: SegmentFile> SegmentReader<File> {
    /// Read `segment`'s frames from offset 0 through `file`.
    #[must_use]
    pub fn new(file: File, segment: SegmentId, cfg: ReaderConfig) -> SegmentReader<File> {
        SegmentReader {
            file,
            segment,
            cfg,
            buf: vec![0; cfg.chunk_bytes.max(FRAME_HEADER_LEN)],
            start: 0,
            valid: 0,
            next_offset: 0,
            file_pos: 0,
            hit_eof: false,
            end: None,
            failed: false,
        }
    }

    /// Open `segment` read-only under `log_dir` and read it.
    pub fn open<F: SegmentFs<File = File>>(
        fs: &F,
        log_dir: &Path,
        segment: SegmentId,
        cfg: ReaderConfig,
    ) -> Result<SegmentReader<File>, ReadError> {
        let path = log_dir.join(segment_file_name(segment));
        let file =
            fs.open_read(&path).map_err(|source| ReadError::Io { segment, offset: 0, source })?;
        Ok(SegmentReader::new(file, segment, cfg))
    }

    /// The next validated frame, or `None` once the written portion ends
    /// cleanly ([`read_end`](Self::read_end) then reports how). Errors are
    /// terminal: the reader yields nothing after one.
    pub fn next_frame(&mut self) -> Result<Option<FrameRef<'_>>, ReadError> {
        if self.end.is_some() || self.failed {
            return Ok(None);
        }
        let frame_len = loop {
            let window = &self.buf[self.start..self.valid];
            match peek(window, self.cfg.max_frame_len) {
                Peek::Ready(frame_len) => break frame_len,
                Peek::ZeroTail => {
                    self.end = Some(ReadEnd::ZeroTail { at: self.next_offset });
                    return Ok(None);
                }
                Peek::NeedMore(needed) if self.hit_eof => {
                    if window.is_empty() {
                        self.end = Some(ReadEnd::FileEnd { at: self.next_offset });
                        return Ok(None);
                    }
                    if window.iter().all(|&b| b == 0) {
                        // Shorter than a magic word but all zeros: still
                        // the preallocated tail, same as decode_frame.
                        self.end = Some(ReadEnd::ZeroTail { at: self.next_offset });
                        return Ok(None);
                    }
                    self.failed = true;
                    return Err(ReadError::Frame {
                        segment: self.segment,
                        offset: self.next_offset,
                        error: FrameDecodeError::Truncated { needed, available: window.len() },
                    });
                }
                Peek::NeedMore(needed) => self.refill(needed)?,
                Peek::Bad(error) => {
                    self.failed = true;
                    return Err(ReadError::Frame {
                        segment: self.segment,
                        offset: self.next_offset,
                        error,
                    });
                }
            }
        };

        // The window holds the whole frame: decode (single CRC pass).
        let at = self.next_offset;
        let window = &self.buf[self.start..self.valid];
        match decode_frame(window, self.cfg.max_frame_len) {
            Ok((frame, consumed)) => {
                debug_assert_eq!(consumed, frame_len, "peek and decode_frame disagree");
                let expected = Lsn::new(self.segment, at + FRAME_HEADER_LEN as u32);
                if frame.first_lsn() != expected {
                    self.failed = true;
                    return Err(ReadError::LsnMismatch {
                        segment: self.segment,
                        offset: at,
                        stored: frame.first_lsn(),
                        expected,
                    });
                }
                self.start += consumed;
                self.next_offset += consumed as u32;
                Ok(Some(frame))
            }
            Err(error) => {
                self.failed = true;
                Err(ReadError::Frame { segment: self.segment, offset: at, error })
            }
        }
    }

    /// Batch-apply every remaining frame (the replay shape: validate, then
    /// apply per frame — L3). Returns how the segment ended.
    pub fn apply_frames<E>(
        &mut self,
        mut apply: impl FnMut(FrameRef<'_>) -> Result<(), E>,
    ) -> Result<ReadEnd, ApplyError<E>> {
        loop {
            match self.next_frame() {
                Ok(Some(frame)) => {
                    let at = frame.first_lsn();
                    apply(frame).map_err(|error| ApplyError::Apply { at, error })?;
                }
                Ok(None) => {
                    return Ok(self.end.expect("clean exhaustion always records an end"));
                }
                Err(err) => return Err(ApplyError::Read(err)),
            }
        }
    }

    /// How the segment's written portion ended — `Some` only after
    /// [`next_frame`](Self::next_frame) has returned `Ok(None)`.
    #[must_use]
    pub fn read_end(&self) -> Option<ReadEnd> {
        self.end
    }

    /// Segment offset of the next unconsumed byte.
    #[must_use]
    pub fn offset(&self) -> u32 {
        self.next_offset
    }

    /// Ensure the window holds `needed` bytes from the current frame
    /// boundary (or end at EOF), compacting and reading ahead in
    /// `chunk_bytes` strides. The buffer grows only when one frame
    /// exceeds it — bounded by `max_frame_len`.
    fn refill(&mut self, needed: usize) -> Result<(), ReadError> {
        self.buf.copy_within(self.start..self.valid, 0);
        self.valid -= self.start;
        self.start = 0;
        let target = needed.max(self.cfg.chunk_bytes);
        if self.buf.len() < target {
            self.buf.resize(target, 0);
        }
        while self.valid < target && !self.hit_eof {
            let read = self
                .file
                .read_at(self.file_pos, &mut self.buf[self.valid..target])
                .map_err(|source| ReadError::Io {
                    segment: self.segment,
                    offset: self.next_offset,
                    source,
                })?;
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
