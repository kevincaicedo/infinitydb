use core::fmt;

use inf_foundation::varint::encode_u64;
use inf_simd::crc32c;

use crate::{
    FRAME_HEADER_LEN, FRAME_TRAILER_LEN, FrameMeta, LogCodecError, Lsn, MAX_FRAME_LEN, NamespaceId,
    RECORD_HEADER_LEN, RecordKind, RecordRef, encode_record, record_encoded_len, write_header,
};

pub const MAX_STAGING_BODY_LEN: usize = MAX_FRAME_LEN - FRAME_HEADER_LEN - FRAME_TRAILER_LEN;

#[derive(Clone, Debug)]
pub struct LogStaging {
    bytes: Vec<u8>,
    record_count: u32,
}

impl LogStaging {
    pub fn with_capacity(capacity_bytes: usize) -> Result<LogStaging, LogStagingError> {
        if capacity_bytes == 0 {
            return Err(LogStagingError::ZeroCapacity);
        }
        if capacity_bytes > MAX_STAGING_BODY_LEN {
            return Err(LogStagingError::CapacityTooLarge {
                capacity_bytes,
                max_bytes: MAX_STAGING_BODY_LEN,
            });
        }
        Ok(LogStaging { bytes: Vec::with_capacity(capacity_bytes), record_count: 0 })
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    #[inline]
    pub fn len_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn capacity_bytes(&self) -> usize {
        self.bytes.capacity()
    }

    #[inline]
    pub fn record_count(&self) -> u32 {
        self.record_count
    }

    /// Encoded log-record bytes staged for the next frame, without a frame
    /// header/trailer. Checkpoint record sections reuse this exact byte
    /// sequence so recovery can apply snapshot entries through the normal
    /// mutation record decoder.
    #[inline]
    pub fn record_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[inline]
    pub fn pending_frame_len_bytes(&self) -> Option<u32> {
        if self.is_empty() {
            return None;
        }

        let len_bytes = FRAME_HEADER_LEN + self.bytes.len() + FRAME_TRAILER_LEN;
        Some(u32::try_from(len_bytes).expect("bounded staging frame length fits u32"))
    }

    #[inline]
    pub fn available_bytes(&self) -> usize {
        self.bytes.capacity() - self.bytes.len()
    }

    pub fn try_push(&mut self, record: RecordRef<'_>) -> Result<(), LogStagingError> {
        if self.record_count == u32::MAX {
            return Err(LogStagingError::RecordCountOverflow);
        }

        let needed = record.encoded_len();
        let available = self.available_bytes();
        if needed > available {
            return Err(LogStagingError::Full { needed, available });
        }

        let start_len = self.bytes.len();
        encode_record(record, &mut self.bytes).map_err(LogStagingError::Codec)?;
        debug_assert!(self.bytes.len() <= self.bytes.capacity());
        debug_assert!(self.bytes.len() - start_len == needed);
        self.record_count += 1;
        Ok(())
    }

    pub fn try_push_payload(
        &mut self,
        kind: RecordKind,
        namespace: NamespaceId,
        flags: u16,
        payload_len: usize,
        encode_payload: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), LogStagingError> {
        if self.record_count == u32::MAX {
            return Err(LogStagingError::RecordCountOverflow);
        }

        let needed = record_encoded_len(payload_len).map_err(LogStagingError::Codec)?;
        let available = self.available_bytes();
        if needed > available {
            return Err(LogStagingError::Full { needed, available });
        }

        let start_len = self.bytes.len();
        let body_len = RECORD_HEADER_LEN + payload_len;
        encode_u64(body_len as u64, &mut self.bytes);
        self.bytes.push(kind.tag());
        self.bytes.extend_from_slice(&flags.to_le_bytes());
        self.bytes.extend_from_slice(&namespace.get().to_le_bytes());

        let payload_start = self.bytes.len();
        encode_payload(&mut self.bytes);
        let actual_payload_len = self.bytes.len() - payload_start;
        if actual_payload_len != payload_len {
            self.bytes.truncate(start_len);
            return Err(LogStagingError::PayloadLengthMismatch {
                expected: payload_len,
                actual: actual_payload_len,
            });
        }

        debug_assert!(self.bytes.len() <= self.bytes.capacity());
        debug_assert!(self.bytes.len() - start_len == needed);
        self.record_count += 1;
        Ok(())
    }

    pub fn drain_frame(
        &mut self,
        frame_start: Lsn,
        out: &mut Vec<u8>,
    ) -> Result<Option<FrameMeta>, LogStagingError> {
        if self.is_empty() {
            return Ok(None);
        }

        let start_len = out.len();
        match encode_staged_frame(frame_start, self.record_count, &self.bytes, out) {
            Ok(meta) => {
                self.clear();
                Ok(Some(meta))
            }
            Err(error) => {
                out.truncate(start_len);
                Err(LogStagingError::Codec(error))
            }
        }
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.record_count = 0;
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogStagingError {
    ZeroCapacity,
    CapacityTooLarge { capacity_bytes: usize, max_bytes: usize },
    Full { needed: usize, available: usize },
    RecordCountOverflow,
    PayloadLengthMismatch { expected: usize, actual: usize },
    Codec(LogCodecError),
}

impl fmt::Display for LogStagingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogStagingError::ZeroCapacity => write!(f, "log staging capacity must be nonzero"),
            LogStagingError::CapacityTooLarge { capacity_bytes, max_bytes } => write!(
                f,
                "log staging capacity {capacity_bytes} exceeds frame body max {max_bytes}"
            ),
            LogStagingError::Full { needed, available } => {
                write!(f, "log staging full: need {needed} bytes, have {available}")
            }
            LogStagingError::RecordCountOverflow => write!(f, "log staging record count overflow"),
            LogStagingError::PayloadLengthMismatch { expected, actual } => write!(
                f,
                "log staging payload length mismatch: expected {expected} bytes, got {actual}"
            ),
            LogStagingError::Codec(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LogStagingError {}

fn encode_staged_frame(
    frame_start: Lsn,
    record_count: u32,
    body: &[u8],
    out: &mut Vec<u8>,
) -> Result<FrameMeta, LogCodecError> {
    debug_assert!(record_count > 0);
    let frame_len = FRAME_HEADER_LEN + body.len() + FRAME_TRAILER_LEN;
    if frame_len > MAX_FRAME_LEN {
        return Err(LogCodecError::FrameTooLarge);
    }
    let frame_len_u32 = u32::try_from(frame_len).map_err(|_| LogCodecError::FrameTooLarge)?;
    let first_lsn = frame_start.checked_add_bytes(FRAME_HEADER_LEN)?;

    let frame_at = out.len();
    out.resize(frame_at + FRAME_HEADER_LEN, 0);
    out.extend_from_slice(body);
    write_header(
        &mut out[frame_at..frame_at + FRAME_HEADER_LEN],
        frame_len_u32,
        record_count,
        first_lsn,
    );
    let crc = crc32c(&out[frame_at..]);
    out.extend_from_slice(&crc.to_le_bytes());

    let frame_end = frame_start.checked_add_bytes(frame_len)?;
    Ok(FrameMeta { frame_start, first_lsn, frame_end, frame_len: frame_len_u32, record_count })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NamespaceId, RecordKind, decode_batch_frame, decode_record_sequence};

    fn record(payload: &'static [u8]) -> RecordRef<'static> {
        RecordRef::new(RecordKind::StringPostImage, NamespaceId::new(7), 0, payload).unwrap()
    }

    #[test]
    fn drains_staged_records_into_one_valid_frame() {
        let mut staging = LogStaging::with_capacity(128).unwrap();
        staging.try_push(record(b"a")).unwrap();
        staging.try_push(record(b"bc")).unwrap();

        let mut frame = Vec::new();
        let meta = staging.drain_frame(Lsn::new(3, 64), &mut frame).unwrap().unwrap();
        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();

        assert!(staging.is_empty());
        assert_eq!(meta.record_count(), 2);
        assert_eq!(decoded.first_lsn(), Lsn::new(3, 64 + FRAME_HEADER_LEN as u32));
        assert_eq!(records[0].record().payload(), b"a");
        assert_eq!(records[1].record().payload(), b"bc");
    }

    #[test]
    fn full_staging_is_backpressure_not_growth() {
        let first = record(b"abcd");
        let second = record(b"ef");
        let mut staging = LogStaging::with_capacity(first.encoded_len()).unwrap();
        let capacity = staging.capacity_bytes();

        staging.try_push(first).unwrap();
        assert_eq!(
            staging.try_push(second),
            Err(LogStagingError::Full { needed: second.encoded_len(), available: 0 })
        );

        assert_eq!(staging.capacity_bytes(), capacity);
        assert_eq!(staging.record_count(), 1);
        assert_eq!(staging.len_bytes(), first.encoded_len());
    }

    #[test]
    fn empty_drain_is_noop() {
        let mut staging = LogStaging::with_capacity(16).unwrap();
        let mut out = vec![1, 2, 3];

        assert_eq!(staging.pending_frame_len_bytes(), None);
        assert_eq!(staging.drain_frame(Lsn::new(0, 0), &mut out).unwrap(), None);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn pending_frame_len_reports_encoded_frame_size_without_draining() {
        let mut staging = LogStaging::with_capacity(128).unwrap();
        staging.try_push(record(b"a")).unwrap();

        let frame_len = staging.pending_frame_len_bytes().unwrap();
        let staged_bytes = staging.len_bytes();
        assert_eq!(frame_len as usize, FRAME_HEADER_LEN + staged_bytes + FRAME_TRAILER_LEN);

        let mut frame = Vec::new();
        let meta = staging.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();
        assert_eq!(meta.frame_len(), frame_len);
        assert_eq!(frame.len(), frame_len as usize);
    }

    #[test]
    fn append_path_keeps_allocated_capacity() {
        let mut staging = LogStaging::with_capacity(128).unwrap();
        let capacity = staging.capacity_bytes();

        for payload in [b"a".as_slice(), b"bb".as_slice(), b"ccc".as_slice()] {
            staging.try_push(record(payload)).unwrap();
            assert_eq!(staging.capacity_bytes(), capacity);
        }
    }

    #[test]
    fn direct_payload_append_keeps_capacity_and_decodes() {
        let payload = b"direct";
        let needed = record(payload).encoded_len();
        let mut staging = LogStaging::with_capacity(needed).unwrap();
        let capacity = staging.capacity_bytes();

        staging
            .try_push_payload(
                RecordKind::StringPostImage,
                NamespaceId::new(7),
                0x55aa,
                payload.len(),
                |out| out.extend_from_slice(payload),
            )
            .unwrap();

        assert_eq!(staging.capacity_bytes(), capacity);
        assert_eq!(staging.len_bytes(), needed);
        assert_eq!(staging.record_bytes().len(), needed);
        let mut sequence_payloads = Vec::new();
        let sequence_count =
            decode_record_sequence(staging.record_bytes(), Lsn::new(0, 0), |rec| {
                sequence_payloads.push(rec.record().payload().to_vec());
            })
            .unwrap();
        assert_eq!(sequence_count, 1);
        assert_eq!(sequence_payloads, [payload.to_vec()]);
        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();
        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record().payload(), payload);
        assert_eq!(records[0].record().flags(), 0x55aa);
    }

    #[test]
    fn direct_payload_append_rejects_length_mismatch_without_record() {
        let mut staging = LogStaging::with_capacity(64).unwrap();

        assert_eq!(
            staging.try_push_payload(
                RecordKind::StringPostImage,
                NamespaceId::new(7),
                0,
                4,
                |out| out.extend_from_slice(b"abc"),
            ),
            Err(LogStagingError::PayloadLengthMismatch { expected: 4, actual: 3 })
        );
        assert!(staging.is_empty());
        assert_eq!(staging.len_bytes(), 0);
    }

    #[test]
    fn invalid_capacity_is_named() {
        assert_eq!(LogStaging::with_capacity(0).unwrap_err(), LogStagingError::ZeroCapacity);
        assert_eq!(
            LogStaging::with_capacity(MAX_STAGING_BODY_LEN + 1).unwrap_err(),
            LogStagingError::CapacityTooLarge {
                capacity_bytes: MAX_STAGING_BODY_LEN + 1,
                max_bytes: MAX_STAGING_BODY_LEN,
            }
        );
    }
}
