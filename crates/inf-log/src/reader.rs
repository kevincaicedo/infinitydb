use core::fmt;

use crate::{
    DecodedFrame, FRAME_HEADER_LEN, FRAME_MAGIC, FRAME_TRAILER_LEN, LogCodecError, Lsn,
    MAX_FRAME_LEN, SegmentId, decode_batch_frame,
};

pub const DEFAULT_SEGMENT_READ_CHUNK_BYTES: u32 = 1024 * 1024;

/// How to handle bytes after the last complete frame in a segment image.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SegmentTailPolicy {
    /// A sealed segment image must contain only complete frames.
    Sealed,
    /// The active tail may end in preallocated zeros or one partial final frame.
    ActiveTail,
}

/// Why a sequential segment read stopped without an error.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SegmentReadTerminal {
    /// The segment ended exactly after the last complete frame.
    CompleteEof { segment: SegmentId, offset: u32 },
    /// The active segment reached preallocated zero bytes after the last frame.
    ActiveZeroTail { segment: SegmentId, offset: u32, len: usize },
    /// The active segment ended with one incomplete final frame.
    ActivePartialFrame { segment: SegmentId, offset: u32, needed: usize, available: usize },
}

impl SegmentReadTerminal {
    #[inline]
    pub const fn segment(self) -> SegmentId {
        match self {
            SegmentReadTerminal::CompleteEof { segment, .. }
            | SegmentReadTerminal::ActiveZeroTail { segment, .. }
            | SegmentReadTerminal::ActivePartialFrame { segment, .. } => segment,
        }
    }

    #[inline]
    pub const fn offset_bytes(self) -> u32 {
        match self {
            SegmentReadTerminal::CompleteEof { offset, .. }
            | SegmentReadTerminal::ActiveZeroTail { offset, .. }
            | SegmentReadTerminal::ActivePartialFrame { offset, .. } => offset,
        }
    }
}

/// One CRC-valid frame yielded from a segment image.
#[derive(Copy, Clone, Debug)]
pub struct SegmentFrame<'a> {
    frame_start: Lsn,
    frame_end: Lsn,
    frame: DecodedFrame<'a>,
}

impl<'a> SegmentFrame<'a> {
    #[inline]
    pub const fn frame_start(self) -> Lsn {
        self.frame_start
    }

    #[inline]
    pub const fn frame_end(self) -> Lsn {
        self.frame_end
    }

    #[inline]
    pub const fn frame(self) -> DecodedFrame<'a> {
        self.frame
    }
}

/// Sequential frame iterator over one segment image.
///
/// The caller supplies bytes read from disk; this module performs no I/O. That
/// keeps recovery, topics, and replication on one byte-validating path without
/// letting `inf-log` know about filesystems, RESP, or store state.
#[derive(Clone, Debug)]
pub struct SegmentFrameIter<'a> {
    segment: SegmentId,
    bytes: &'a [u8],
    offset: usize,
    tail_policy: SegmentTailPolicy,
    finished: bool,
    terminal: Option<SegmentReadTerminal>,
}

impl<'a> Iterator for SegmentFrameIter<'a> {
    type Item = Result<SegmentFrame<'a>, SegmentFrameError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        match self.next_frame() {
            Ok(Some(frame)) => Some(Ok(frame)),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

impl<'a> SegmentFrameIter<'a> {
    #[inline]
    pub const fn terminal(&self) -> Option<SegmentReadTerminal> {
        self.terminal
    }

    fn next_frame(&mut self) -> Result<Option<SegmentFrame<'a>>, SegmentFrameError> {
        if self.offset == self.bytes.len() {
            self.terminal = Some(SegmentReadTerminal::CompleteEof {
                segment: self.segment,
                offset: offset_u32(self.segment, self.offset)?,
            });
            return Ok(None);
        }

        let tail = &self.bytes[self.offset..];
        if is_zero_tail(tail) {
            return self.zero_tail();
        }

        let available = tail.len();
        let minimum = FRAME_HEADER_LEN + FRAME_TRAILER_LEN;
        if available < minimum {
            return self.partial_tail(minimum, available);
        }

        let offset = offset_u32(self.segment, self.offset)?;
        let frame_len = read_frame_len(self.segment, offset, tail)?;
        if frame_len < minimum {
            return Err(SegmentFrameError::FrameLengthTooSmall {
                segment: self.segment,
                offset,
                frame_len,
            });
        }
        if frame_len > MAX_FRAME_LEN {
            return Err(SegmentFrameError::FrameLengthTooLarge {
                segment: self.segment,
                offset,
                frame_len,
            });
        }
        if frame_len > available {
            return self.partial_tail(frame_len, available);
        }

        let image = &tail[..frame_len];
        let frame = self.decode_frame(image)?;
        self.offset += frame_len;
        Ok(Some(frame))
    }

    fn zero_tail(&mut self) -> Result<Option<SegmentFrame<'a>>, SegmentFrameError> {
        let offset = offset_u32(self.segment, self.offset)?;
        match self.tail_policy {
            SegmentTailPolicy::ActiveTail => {
                self.terminal = Some(SegmentReadTerminal::ActiveZeroTail {
                    segment: self.segment,
                    offset,
                    len: self.bytes.len() - self.offset,
                });
                Ok(None)
            }
            SegmentTailPolicy::Sealed => Err(SegmentFrameError::ZeroTailInSealedSegment {
                segment: self.segment,
                offset,
                len: self.bytes.len() - self.offset,
            }),
        }
    }

    fn partial_tail(
        &mut self,
        needed: usize,
        available: usize,
    ) -> Result<Option<SegmentFrame<'a>>, SegmentFrameError> {
        let offset = offset_u32(self.segment, self.offset)?;
        match self.tail_policy {
            SegmentTailPolicy::ActiveTail => {
                self.terminal = Some(SegmentReadTerminal::ActivePartialFrame {
                    segment: self.segment,
                    offset,
                    needed,
                    available,
                });
                Ok(None)
            }
            SegmentTailPolicy::Sealed => Err(SegmentFrameError::PartialFrame {
                segment: self.segment,
                offset,
                needed,
                available,
            }),
        }
    }

    fn decode_frame(&self, image: &'a [u8]) -> Result<SegmentFrame<'a>, SegmentFrameError> {
        let offset = offset_u32(self.segment, self.offset)?;
        decode_segment_frame_at(self.segment, offset, image)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SegmentReadConfig {
    chunk_bytes: u32,
}

impl SegmentReadConfig {
    pub const DEFAULT: SegmentReadConfig =
        SegmentReadConfig { chunk_bytes: DEFAULT_SEGMENT_READ_CHUNK_BYTES };

    pub fn new(chunk_bytes: u32) -> Result<SegmentReadConfig, SegmentReadConfigError> {
        if chunk_bytes == 0 {
            return Err(SegmentReadConfigError::ZeroChunkBytes);
        }
        Ok(SegmentReadConfig { chunk_bytes })
    }

    #[inline]
    pub const fn chunk_bytes(self) -> u32 {
        self.chunk_bytes
    }
}

impl Default for SegmentReadConfig {
    fn default() -> SegmentReadConfig {
        SegmentReadConfig::DEFAULT
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SegmentReadConfigError {
    ZeroChunkBytes,
}

impl fmt::Display for SegmentReadConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentReadConfigError::ZeroChunkBytes => {
                write!(f, "segment read chunk must be nonzero")
            }
        }
    }
}

impl std::error::Error for SegmentReadConfigError {}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SegmentReadRequest {
    offset_bytes: u64,
    len: u32,
}

impl SegmentReadRequest {
    #[inline]
    pub const fn offset_bytes(self) -> u64 {
        self.offset_bytes
    }

    #[inline]
    pub const fn len_bytes(self) -> u32 {
        self.len
    }
}

/// Callback invoked for every complete CRC-valid frame in a chunked reader.
///
/// The frame borrows the reader's internal pending buffer and must be consumed
/// before the callback returns. This keeps recovery and topic scanners from
/// copying frame bodies just to cross an iterator boundary.
pub trait SegmentFrameSink {
    type Error;

    fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error>;
}

/// Chunked segment reader driven by `FileReadAt`-shaped completions.
///
/// `inf-log` owns the state machine because frame validation is part of the
/// log byte contract. It deliberately does not name `BackendDriver`: callers
/// issue [`SegmentReadRequest`]s through whatever runtime backend owns the fd,
/// then feed the returned bytes back through [`SegmentFileReader::push_read`].
#[derive(Clone, Debug)]
pub struct SegmentFileReader {
    segment: SegmentId,
    tail_policy: SegmentTailPolicy,
    config: SegmentReadConfig,
    next_read_offset_bytes: u64,
    pending_offset_bytes: u64,
    pending: Vec<u8>,
    frames_read: u64,
    records_read: u64,
    finished: bool,
    terminal: Option<SegmentReadTerminal>,
}

impl SegmentFileReader {
    pub fn new(
        segment: SegmentId,
        tail_policy: SegmentTailPolicy,
        config: SegmentReadConfig,
    ) -> SegmentFileReader {
        SegmentFileReader {
            segment,
            tail_policy,
            config,
            next_read_offset_bytes: 0,
            pending_offset_bytes: 0,
            pending: Vec::new(),
            frames_read: 0,
            records_read: 0,
            finished: false,
            terminal: None,
        }
    }

    #[inline]
    pub const fn segment(&self) -> SegmentId {
        self.segment
    }

    #[inline]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    #[inline]
    pub const fn terminal(&self) -> Option<SegmentReadTerminal> {
        self.terminal
    }

    #[inline]
    pub const fn frames_read(&self) -> u64 {
        self.frames_read
    }

    #[inline]
    pub const fn records_read(&self) -> u64 {
        self.records_read
    }

    #[inline]
    pub const fn next_read_offset_bytes(&self) -> u64 {
        self.next_read_offset_bytes
    }

    #[inline]
    pub fn next_request(&self) -> Option<SegmentReadRequest> {
        if self.finished {
            return None;
        }
        Some(SegmentReadRequest {
            offset_bytes: self.next_read_offset_bytes,
            len: self.config.chunk_bytes,
        })
    }

    pub fn push_read<S>(
        &mut self,
        offset_bytes: u64,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<(), SegmentReadError<S::Error>>
    where
        S: SegmentFrameSink,
    {
        if self.finished {
            return Err(SegmentReadError::ReadAfterFinished { segment: self.segment });
        }
        if offset_bytes != self.next_read_offset_bytes {
            return Err(SegmentReadError::UnexpectedReadOffset {
                segment: self.segment,
                expected: self.next_read_offset_bytes,
                got: offset_bytes,
            });
        }
        if bytes.len() > self.config.chunk_bytes as usize {
            return Err(SegmentReadError::ReadLenTooLarge {
                segment: self.segment,
                len: bytes.len(),
                max: self.config.chunk_bytes,
            });
        }

        if bytes.is_empty() {
            return self.drain_ready(true, sink);
        }

        self.next_read_offset_bytes =
            self.next_read_offset_bytes
                .checked_add(bytes.len() as u64)
                .ok_or(SegmentReadError::ReadOffsetOverflow { segment: self.segment })?;
        self.pending.try_reserve(bytes.len()).map_err(|_| SegmentReadError::PendingReserve {
            segment: self.segment,
            additional: bytes.len(),
        })?;
        self.pending.extend_from_slice(bytes);
        self.drain_ready(false, sink)
    }

    fn drain_ready<S>(&mut self, eof: bool, sink: &mut S) -> Result<(), SegmentReadError<S::Error>>
    where
        S: SegmentFrameSink,
    {
        let mut consumed = 0usize;
        while consumed < self.pending.len() {
            let tail = &self.pending[consumed..];
            let frame_offset = self.frame_offset(consumed)?;
            if is_zero_tail(tail) {
                consumed = self.handle_zero_tail(frame_offset, tail.len())?;
                break;
            }

            let minimum = FRAME_HEADER_LEN + FRAME_TRAILER_LEN;
            let available = tail.len();
            if available < minimum {
                if eof {
                    consumed = self.handle_partial_tail(frame_offset, minimum, available)?;
                }
                break;
            }

            let offset = read_offset_u32(self.segment, frame_offset)?;
            let frame_len =
                read_frame_len(self.segment, offset, tail).map_err(SegmentReadError::Frame)?;
            if frame_len < minimum {
                return Err(SegmentReadError::Frame(SegmentFrameError::FrameLengthTooSmall {
                    segment: self.segment,
                    offset,
                    frame_len,
                }));
            }
            if frame_len > MAX_FRAME_LEN {
                return Err(SegmentReadError::Frame(SegmentFrameError::FrameLengthTooLarge {
                    segment: self.segment,
                    offset,
                    frame_len,
                }));
            }
            if frame_len > available {
                if eof {
                    consumed = self.handle_partial_tail(frame_offset, frame_len, available)?;
                }
                break;
            }

            let frame = decode_segment_frame_at(self.segment, offset, &tail[..frame_len])
                .map_err(SegmentReadError::Frame)?;
            sink.push_frame(frame).map_err(SegmentReadError::Sink)?;
            self.frames_read += 1;
            self.records_read += u64::from(frame.frame().record_count());
            consumed += frame_len;
        }

        self.compact_pending(consumed)?;
        if eof && self.pending.is_empty() && !self.finished {
            let offset = read_offset_u32(self.segment, self.pending_offset_bytes)?;
            self.terminal =
                Some(SegmentReadTerminal::CompleteEof { segment: self.segment, offset });
            self.finished = true;
        }
        Ok(())
    }

    fn frame_offset<E>(&self, consumed: usize) -> Result<u64, SegmentReadError<E>> {
        self.pending_offset_bytes
            .checked_add(consumed as u64)
            .ok_or(SegmentReadError::ReadOffsetOverflow { segment: self.segment })
    }

    fn handle_zero_tail<E>(
        &mut self,
        frame_offset: u64,
        len: usize,
    ) -> Result<usize, SegmentReadError<E>> {
        let offset = read_offset_u32(self.segment, frame_offset)?;
        match self.tail_policy {
            SegmentTailPolicy::ActiveTail => {
                self.terminal = Some(SegmentReadTerminal::ActiveZeroTail {
                    segment: self.segment,
                    offset,
                    len,
                });
                self.finished = true;
                Ok(self.pending.len())
            }
            SegmentTailPolicy::Sealed => {
                Err(SegmentReadError::Frame(SegmentFrameError::ZeroTailInSealedSegment {
                    segment: self.segment,
                    offset,
                    len,
                }))
            }
        }
    }

    fn handle_partial_tail<E>(
        &mut self,
        frame_offset: u64,
        needed: usize,
        available: usize,
    ) -> Result<usize, SegmentReadError<E>> {
        let offset = read_offset_u32(self.segment, frame_offset)?;
        match self.tail_policy {
            SegmentTailPolicy::ActiveTail => {
                self.terminal = Some(SegmentReadTerminal::ActivePartialFrame {
                    segment: self.segment,
                    offset,
                    needed,
                    available,
                });
                self.finished = true;
                Ok(self.pending.len())
            }
            SegmentTailPolicy::Sealed => {
                Err(SegmentReadError::Frame(SegmentFrameError::PartialFrame {
                    segment: self.segment,
                    offset,
                    needed,
                    available,
                }))
            }
        }
    }

    fn compact_pending<E>(&mut self, consumed: usize) -> Result<(), SegmentReadError<E>> {
        if consumed == 0 {
            return Ok(());
        }
        self.pending_offset_bytes = self
            .pending_offset_bytes
            .checked_add(consumed as u64)
            .ok_or(SegmentReadError::ReadOffsetOverflow { segment: self.segment })?;
        if consumed == self.pending.len() {
            self.pending.clear();
        } else {
            self.pending.drain(..consumed);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentReadError<E> {
    UnexpectedReadOffset { segment: SegmentId, expected: u64, got: u64 },
    ReadLenTooLarge { segment: SegmentId, len: usize, max: u32 },
    ReadAfterFinished { segment: SegmentId },
    ReadOffsetOverflow { segment: SegmentId },
    PendingReserve { segment: SegmentId, additional: usize },
    Frame(SegmentFrameError),
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for SegmentReadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentReadError::UnexpectedReadOffset { segment, expected, got } => write!(
                f,
                "segment {} read offset mismatch: expected {expected}, got {got}",
                segment.file_name()
            ),
            SegmentReadError::ReadLenTooLarge { segment, len, max } => write!(
                f,
                "segment {} read returned {len} bytes, above requested max {max}",
                segment.file_name()
            ),
            SegmentReadError::ReadAfterFinished { segment } => {
                write!(f, "segment {} read arrived after reader finished", segment.file_name())
            }
            SegmentReadError::ReadOffsetOverflow { segment } => {
                write!(f, "segment {} read offset overflowed", segment.file_name())
            }
            SegmentReadError::PendingReserve { segment, additional } => write!(
                f,
                "segment {} could not reserve {additional} pending read bytes",
                segment.file_name()
            ),
            SegmentReadError::Frame(error) => error.fmt(f),
            SegmentReadError::Sink(error) => write!(f, "segment frame sink failed: {error}"),
        }
    }
}

impl<E> std::error::Error for SegmentReadError<E> where E: std::error::Error + 'static {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SegmentFrameError {
    ZeroTailInSealedSegment { segment: SegmentId, offset: u32, len: usize },
    PartialFrame { segment: SegmentId, offset: u32, needed: usize, available: usize },
    FrameLengthTooSmall { segment: SegmentId, offset: u32, frame_len: usize },
    FrameLengthTooLarge { segment: SegmentId, offset: u32, frame_len: usize },
    FrameLsnMismatch { segment: SegmentId, offset: u32, expected: Lsn, actual: Lsn },
    OffsetTooLarge { segment: SegmentId, offset: usize },
    Codec { segment: SegmentId, offset: u32, source: LogCodecError },
}

impl fmt::Display for SegmentFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentFrameError::ZeroTailInSealedSegment { segment, offset, len } => write!(
                f,
                "sealed segment {} has {len} zero tail bytes at offset {offset}",
                segment.file_name()
            ),
            SegmentFrameError::PartialFrame { segment, offset, needed, available } => write!(
                f,
                "sealed segment {} has partial frame at offset {offset}: need {needed}, have {available}",
                segment.file_name()
            ),
            SegmentFrameError::FrameLengthTooSmall { segment, offset, frame_len } => write!(
                f,
                "segment {} frame at offset {offset} has invalid length {frame_len}",
                segment.file_name()
            ),
            SegmentFrameError::FrameLengthTooLarge { segment, offset, frame_len } => write!(
                f,
                "segment {} frame at offset {offset} exceeds max length: {frame_len}",
                segment.file_name()
            ),
            SegmentFrameError::FrameLsnMismatch { segment, offset, expected, actual } => write!(
                f,
                "segment {} frame at offset {offset} first LSN mismatch: expected {expected}, got {actual}",
                segment.file_name()
            ),
            SegmentFrameError::OffsetTooLarge { segment, offset } => write!(
                f,
                "segment {} offset {offset} exceeds the v1 u32 LSN range",
                segment.file_name()
            ),
            SegmentFrameError::Codec { segment, offset, source } => write!(
                f,
                "segment {} frame at offset {offset} is invalid: {source}",
                segment.file_name()
            ),
        }
    }
}

impl std::error::Error for SegmentFrameError {}

#[inline]
pub fn iter_segment_frames(
    segment: SegmentId,
    bytes: &[u8],
    tail_policy: SegmentTailPolicy,
) -> SegmentFrameIter<'_> {
    SegmentFrameIter { segment, bytes, offset: 0, tail_policy, finished: false, terminal: None }
}

#[inline]
pub fn find_frame_magic_offset(bytes: &[u8]) -> Option<usize> {
    bytes.windows(core::mem::size_of::<u32>()).position(|window| window == FRAME_MAGIC_BYTES)
}

const FRAME_MAGIC_BYTES: [u8; 4] = FRAME_MAGIC.to_le_bytes();

fn read_frame_len(
    segment: SegmentId,
    offset: u32,
    bytes: &[u8],
) -> Result<usize, SegmentFrameError> {
    let magic = read_u32(bytes, 0);
    if magic != FRAME_MAGIC {
        return Err(SegmentFrameError::Codec {
            segment,
            offset,
            source: LogCodecError::BadMagic { got: magic },
        });
    }
    Ok(read_u32(bytes, 4) as usize)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

fn is_zero_tail(bytes: &[u8]) -> bool {
    debug_assert!(!bytes.is_empty());
    bytes.iter().all(|byte| *byte == 0)
}

fn decode_segment_frame_at(
    segment: SegmentId,
    offset: u32,
    image: &[u8],
) -> Result<SegmentFrame<'_>, SegmentFrameError> {
    let frame_start = Lsn::new(segment.get(), offset);
    let expected_first = frame_start.checked_add_bytes(FRAME_HEADER_LEN).map_err(|source| {
        SegmentFrameError::Codec { segment, offset: frame_start.offset(), source }
    })?;
    let frame = decode_batch_frame(image).map_err(|source| SegmentFrameError::Codec {
        segment,
        offset: frame_start.offset(),
        source,
    })?;
    if frame.first_lsn() != expected_first {
        return Err(SegmentFrameError::FrameLsnMismatch {
            segment,
            offset: frame_start.offset(),
            expected: expected_first,
            actual: frame.first_lsn(),
        });
    }
    let frame_end = frame_start.checked_add_bytes(image.len()).map_err(|source| {
        SegmentFrameError::Codec { segment, offset: frame_start.offset(), source }
    })?;
    Ok(SegmentFrame { frame_start, frame_end, frame })
}

fn read_offset_u32<E>(segment: SegmentId, offset_bytes: u64) -> Result<u32, SegmentReadError<E>> {
    let offset = usize::try_from(offset_bytes)
        .map_err(|_| SegmentReadError::ReadOffsetOverflow { segment })?;
    offset_u32(segment, offset).map_err(SegmentReadError::Frame)
}

fn offset_u32(segment: SegmentId, offset: usize) -> Result<u32, SegmentFrameError> {
    u32::try_from(offset).map_err(|_| SegmentFrameError::OffsetTooLarge { segment, offset })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NamespaceId, RecordKind, RecordRef, encode_batch_frame};
    use core::convert::Infallible;
    use proptest::prelude::*;

    #[derive(Debug, Default)]
    struct PayloadSink {
        frames: Vec<(Lsn, Lsn)>,
        payloads: Vec<Vec<u8>>,
    }

    impl SegmentFrameSink for PayloadSink {
        type Error = Infallible;

        fn push_frame(&mut self, frame: SegmentFrame<'_>) -> Result<(), Self::Error> {
            self.frames.push((frame.frame_start(), frame.frame_end()));
            self.payloads
                .extend(frame.frame().records().map(|record| record.record().payload().to_vec()));
            Ok(())
        }
    }

    fn record(payload: &[u8]) -> RecordRef<'_> {
        RecordRef::new(RecordKind::StringPostImage, NamespaceId::new(1), 0, payload).unwrap()
    }

    fn append_frame(out: &mut Vec<u8>, segment: SegmentId, payloads: &[Vec<u8>]) {
        let offset = out.len() as u32;
        let refs: Vec<_> = payloads.iter().map(|payload| record(payload)).collect();
        encode_batch_frame(Lsn::new(segment.get(), offset), &refs, out).unwrap();
    }

    fn drive_chunked_reader(
        segment: SegmentId,
        bytes: &[u8],
        tail_policy: SegmentTailPolicy,
        chunk_bytes: u32,
    ) -> Result<(SegmentFileReader, PayloadSink), SegmentReadError<Infallible>> {
        let config = SegmentReadConfig::new(chunk_bytes).unwrap();
        let mut reader = SegmentFileReader::new(segment, tail_policy, config);
        let mut sink = PayloadSink::default();
        let mut cursor = 0usize;

        while !reader.is_finished() {
            let request = reader.next_request().unwrap();
            assert_eq!(request.offset_bytes(), cursor as u64);
            if cursor == bytes.len() {
                reader.push_read(request.offset_bytes(), &[], &mut sink)?;
                break;
            }

            let read_len = (request.len_bytes() as usize).min(bytes.len() - cursor);
            let end = cursor + read_len;
            reader.push_read(request.offset_bytes(), &bytes[cursor..end], &mut sink)?;
            cursor = end;
        }

        Ok((reader, sink))
    }

    #[test]
    fn iterates_complete_sealed_segment_frames() {
        let segment = SegmentId::new(3).unwrap();
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"a".to_vec(), b"bb".to_vec()]);
        append_frame(&mut bytes, segment, &[b"ccc".to_vec()]);

        let frames: Vec<_> = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .map(Result::unwrap)
            .collect();
        let payloads: Vec<_> = frames
            .iter()
            .flat_map(|frame| frame.frame().records().map(|record| record.record().payload()))
            .collect();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame_start(), Lsn::new(3, 0));
        assert_eq!(frames[0].frame_end(), frames[1].frame_start());
        assert_eq!(payloads, vec![b"a".as_slice(), b"bb".as_slice(), b"ccc".as_slice()]);
    }

    #[test]
    fn active_tail_stops_at_preallocated_zeros() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"live".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(&[0; 128]);

        let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::ActiveTail);
        let frames: Vec<_> = iter.by_ref().map(Result::unwrap).collect();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame().record_count(), 1);
        assert_eq!(
            iter.terminal(),
            Some(SegmentReadTerminal::ActiveZeroTail { segment, offset: tail_offset, len: 128 })
        );
    }

    #[test]
    fn active_tail_stops_at_partial_final_frame() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"ok".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::ActiveTail);
        let frames: Vec<_> = iter.by_ref().map(Result::unwrap).collect();

        assert_eq!(frames.len(), 1);
        assert_eq!(
            iter.terminal(),
            Some(SegmentReadTerminal::ActivePartialFrame {
                segment,
                offset: tail_offset,
                needed: FRAME_HEADER_LEN + FRAME_TRAILER_LEN,
                available: 4,
            })
        );
    }

    #[test]
    fn frame_magic_search_finds_split_candidates_by_offset() {
        let mut bytes = b"prefix".to_vec();
        let magic_offset = bytes.len();
        bytes.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        bytes.extend_from_slice(b"suffix");

        assert_eq!(find_frame_magic_offset(&bytes), Some(magic_offset));
        assert_eq!(find_frame_magic_offset(b"no magic here"), None);
    }

    #[test]
    fn sealed_segment_rejects_zero_tail() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"sealed".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(&[0; 4]);

        let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed);
        assert!(iter.next().unwrap().is_ok());
        assert_eq!(
            iter.next().unwrap().unwrap_err(),
            SegmentFrameError::ZeroTailInSealedSegment { segment, offset: tail_offset, len: 4 }
        );
    }

    #[test]
    fn sealed_segment_rejects_partial_tail() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"sealed".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed);
        assert!(iter.next().unwrap().is_ok());
        assert_eq!(
            iter.next().unwrap().unwrap_err(),
            SegmentFrameError::PartialFrame {
                segment,
                offset: tail_offset,
                needed: FRAME_HEADER_LEN + FRAME_TRAILER_LEN,
                available: 4,
            }
        );
    }

    #[test]
    fn iterator_rejects_corrupt_interior_frame() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"ok".to_vec()]);
        bytes.extend_from_slice(b"not a frame but not zero");

        let mut iter = iter_segment_frames(segment, &bytes, SegmentTailPolicy::ActiveTail);
        assert!(iter.next().unwrap().is_ok());
        assert!(matches!(iter.next().unwrap().unwrap_err(), SegmentFrameError::Codec { .. }));
        assert!(iter.next().is_none());
    }

    #[test]
    fn iterator_rejects_frame_lsn_mismatch() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        let payloads = [b"wrong-lsn".to_vec()];
        let refs: Vec<_> = payloads.iter().map(|payload| record(payload)).collect();
        encode_batch_frame(Lsn::new(segment.get(), 32), &refs, &mut bytes).unwrap();

        let error = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
            .next()
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, SegmentFrameError::FrameLsnMismatch { .. }));
    }

    #[test]
    fn chunked_reader_yields_frames_across_tiny_reads() {
        let segment = SegmentId::new(5).unwrap();
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"a".to_vec(), b"bb".to_vec()]);
        append_frame(&mut bytes, segment, &[b"ccc".to_vec(), b"dddd".to_vec()]);

        let (reader, sink) =
            drive_chunked_reader(segment, &bytes, SegmentTailPolicy::Sealed, 3).unwrap();

        assert!(reader.is_finished());
        assert_eq!(
            reader.terminal(),
            Some(SegmentReadTerminal::CompleteEof { segment, offset: bytes.len() as u32 })
        );
        assert_eq!(reader.frames_read(), 2);
        assert_eq!(reader.records_read(), 4);
        assert_eq!(sink.frames[0].0, Lsn::new(5, 0));
        assert_eq!(sink.frames[0].1, sink.frames[1].0);
        assert_eq!(
            sink.payloads,
            vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec(), b"dddd".to_vec()]
        );
    }

    #[test]
    fn chunked_reader_stops_active_tail_at_preallocated_zeroes() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"live".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(&[0; 256]);

        let (reader, sink) =
            drive_chunked_reader(segment, &bytes, SegmentTailPolicy::ActiveTail, 17).unwrap();

        assert!(reader.is_finished());
        assert_eq!(reader.next_request(), None);
        assert!(matches!(
            reader.terminal(),
            Some(SegmentReadTerminal::ActiveZeroTail { segment: actual, offset, .. })
                if actual == segment && offset == tail_offset
        ));
        assert_eq!(sink.payloads, vec![b"live".to_vec()]);
    }

    #[test]
    fn chunked_reader_stops_active_partial_tail_at_eof() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"ok".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let (reader, sink) =
            drive_chunked_reader(segment, &bytes, SegmentTailPolicy::ActiveTail, 5).unwrap();

        assert!(reader.is_finished());
        assert_eq!(
            reader.terminal(),
            Some(SegmentReadTerminal::ActivePartialFrame {
                segment,
                offset: tail_offset,
                needed: FRAME_HEADER_LEN + FRAME_TRAILER_LEN,
                available: 4,
            })
        );
        assert_eq!(reader.frames_read(), 1);
        assert_eq!(sink.payloads, vec![b"ok".to_vec()]);
    }

    #[test]
    fn chunked_reader_rejects_sealed_partial_tail_at_eof() {
        let segment = SegmentId::ZERO;
        let mut bytes = Vec::new();
        append_frame(&mut bytes, segment, &[b"sealed".to_vec()]);
        let tail_offset = bytes.len() as u32;
        bytes.extend_from_slice(b"ILG1");

        let error =
            drive_chunked_reader(segment, &bytes, SegmentTailPolicy::Sealed, 5).unwrap_err();

        assert_eq!(
            error,
            SegmentReadError::Frame(SegmentFrameError::PartialFrame {
                segment,
                offset: tail_offset,
                needed: FRAME_HEADER_LEN + FRAME_TRAILER_LEN,
                available: 4,
            })
        );
    }

    #[test]
    fn chunked_reader_rejects_out_of_order_read_completion() {
        let segment = SegmentId::new(7).unwrap();
        let config = SegmentReadConfig::new(64).unwrap();
        let mut reader = SegmentFileReader::new(segment, SegmentTailPolicy::Sealed, config);
        let mut sink = PayloadSink::default();

        let error = reader.push_read(1, b"", &mut sink).unwrap_err();

        assert_eq!(error, SegmentReadError::UnexpectedReadOffset { segment, expected: 0, got: 1 });
    }

    #[test]
    fn chunked_reader_rejects_zero_chunk_config() {
        assert_eq!(SegmentReadConfig::new(0), Err(SegmentReadConfigError::ZeroChunkBytes));
    }

    proptest! {
        #[test]
        fn reader_yields_the_encoded_record_sequence(
            frames in prop::collection::vec(
                prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 1..8),
                1..32,
            )
        ) {
            let segment = SegmentId::new(11).unwrap();
            let mut bytes = Vec::new();
            for frame in &frames {
                append_frame(&mut bytes, segment, frame);
            }

            let expected: Vec<Vec<u8>> = frames.iter().flatten().cloned().collect();
            let actual: Vec<Vec<u8>> = iter_segment_frames(segment, &bytes, SegmentTailPolicy::Sealed)
                .map(Result::unwrap)
                .flat_map(|frame| {
                    frame
                        .frame()
                        .records()
                        .map(|record| record.record().payload().to_vec())
                        .collect::<Vec<_>>()
                })
                .collect();

            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn chunked_reader_matches_whole_image_iterator(
            frames in prop::collection::vec(
                prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 1..8),
                1..32,
            ),
            chunk_bytes in 1u32..128,
        ) {
            let segment = SegmentId::new(13).unwrap();
            let mut bytes = Vec::new();
            for frame in &frames {
                append_frame(&mut bytes, segment, frame);
            }

            let expected: Vec<Vec<u8>> = iter_segment_frames(
                segment,
                &bytes,
                SegmentTailPolicy::Sealed,
            )
            .map(Result::unwrap)
            .flat_map(|frame| {
                frame
                    .frame()
                    .records()
                    .map(|record| record.record().payload().to_vec())
                    .collect::<Vec<_>>()
            })
            .collect();

            let (reader, sink) =
                drive_chunked_reader(segment, &bytes, SegmentTailPolicy::Sealed, chunk_bytes)
                    .unwrap();
            let expected_len = expected.len();

            prop_assert!(reader.is_finished());
            prop_assert_eq!(sink.payloads, expected);
            prop_assert_eq!(reader.records_read() as usize, expected_len);
        }
    }
}
