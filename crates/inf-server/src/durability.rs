use core::{fmt, mem::size_of};

use inf_foundation::varint::encode_u64;
use inf_log::{
    CheckpointId, FrameMeta, LOG_CONTROL_NAMESPACE, LogCodecError, LogStaging, LogStagingError,
    Lsn, MAX_RECORD_ENCODED_LEN, NamespaceId, RecordKind, RecordRef, record_encoded_len,
};
use inf_store::{MAX_EXPIRE_MS, MutationEffect, MutationSink};

pub const MUTATION_FLAG_RAW: u16 = 0x0001;
pub const MUTATION_FLAG_HAS_EXPIRE: u16 = 0x0002;
pub const CHECKPOINT_BEGIN_PAYLOAD_LEN: usize = 4;
pub const DEFAULT_LOG_STAGING_CAPACITY_BYTES: usize = MAX_RECORD_ENCODED_LEN;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MutationRecordDecodeError {
    UnsupportedKind { kind: RecordKind },
    UnknownFlags { kind: RecordKind, flags: u16 },
    InvalidLength,
    LengthOverflow { len: u64 },
    Truncated { needed: usize, available: usize },
    ExpireOutOfRange { expire_at_ms: u64 },
    TrailingBytes { bytes: usize },
}

impl fmt::Display for MutationRecordDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MutationRecordDecodeError::UnsupportedKind { kind } => {
                write!(f, "unsupported mutation log record kind {kind:?}")
            }
            MutationRecordDecodeError::UnknownFlags { kind, flags } => {
                write!(f, "unknown mutation log record flags {flags:#06x} for {kind:?}")
            }
            MutationRecordDecodeError::InvalidLength => {
                write!(f, "invalid mutation payload length")
            }
            MutationRecordDecodeError::LengthOverflow { len } => {
                write!(f, "mutation payload length {len} does not fit usize")
            }
            MutationRecordDecodeError::Truncated { needed, available } => {
                write!(f, "truncated mutation payload: need {needed} bytes, have {available}")
            }
            MutationRecordDecodeError::ExpireOutOfRange { expire_at_ms } => {
                write!(f, "mutation expiry {expire_at_ms} exceeds record u40 bound")
            }
            MutationRecordDecodeError::TrailingBytes { bytes } => {
                write!(f, "mutation payload has {bytes} trailing bytes")
            }
        }
    }
}

impl std::error::Error for MutationRecordDecodeError {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MutationStageError {
    Log(LogStagingError),
    CheckpointBeginRequiresEmptyStaging {
        staged_record_count: u32,
        staged_bytes: usize,
    },
    ReservationStale {
        expected_bytes: u64,
        actual_bytes: u64,
        expected_record_count: u32,
        actual_record_count: u32,
    },
}

impl fmt::Display for MutationStageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MutationStageError::Log(error) => error.fmt(f),
            MutationStageError::CheckpointBeginRequiresEmptyStaging {
                staged_record_count,
                staged_bytes,
            } => write!(
                f,
                "checkpoint begin requires empty log staging, found {staged_record_count} record(s) and {staged_bytes} bytes"
            ),
            MutationStageError::ReservationStale {
                expected_bytes,
                actual_bytes,
                expected_record_count,
                actual_record_count,
            } => write!(
                f,
                "log staging reservation stale: expected {expected_bytes} bytes/{expected_record_count} records, got {actual_bytes} bytes/{actual_record_count} records"
            ),
        }
    }
}

impl std::error::Error for MutationStageError {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointBeginRecordDecodeError {
    UnsupportedKind { kind: RecordKind },
    UnknownFlags { flags: u16 },
    InvalidNamespace { namespace: NamespaceId },
    InvalidLength { len: usize },
    InvalidCheckpointId { raw: u32 },
}

impl fmt::Display for CheckpointBeginRecordDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointBeginRecordDecodeError::UnsupportedKind { kind } => {
                write!(f, "unsupported checkpoint-begin log record kind {kind:?}")
            }
            CheckpointBeginRecordDecodeError::UnknownFlags { flags } => {
                write!(f, "checkpoint-begin log record has unknown flags {flags:#06x}")
            }
            CheckpointBeginRecordDecodeError::InvalidNamespace { namespace } => write!(
                f,
                "checkpoint-begin log record uses namespace {}, expected {}",
                namespace.get(),
                LOG_CONTROL_NAMESPACE.get()
            ),
            CheckpointBeginRecordDecodeError::InvalidLength { len } => {
                write!(f, "checkpoint-begin payload is {len} bytes")
            }
            CheckpointBeginRecordDecodeError::InvalidCheckpointId { raw } => {
                write!(f, "checkpoint-begin checkpoint id {raw} exceeds v1 max")
            }
        }
    }
}

impl std::error::Error for CheckpointBeginRecordDecodeError {}

#[derive(Debug)]
pub struct DurabilityCell {
    staging: LogStaging,
}

impl DurabilityCell {
    pub fn new() -> DurabilityCell {
        DurabilityCell::with_capacity(DEFAULT_LOG_STAGING_CAPACITY_BYTES)
            .expect("default log staging capacity fits one max encoded record")
    }

    pub fn with_capacity(capacity_bytes: usize) -> Result<DurabilityCell, LogStagingError> {
        Ok(DurabilityCell { staging: LogStaging::with_capacity(capacity_bytes)? })
    }

    #[inline]
    pub fn log_staging_bytes(&self) -> u64 {
        self.staging.len_bytes() as u64
    }

    #[inline]
    pub fn log_staging_capacity_bytes(&self) -> u64 {
        self.staging.capacity_bytes() as u64
    }

    #[inline]
    pub fn pending_frame_len_bytes(&self) -> Option<u32> {
        self.staging.pending_frame_len_bytes()
    }

    pub fn reserve_mutation_effect_capacity(
        &self,
        effect: MutationEffect<'_>,
    ) -> Result<(), MutationStageError> {
        reserve_mutation_effect_capacity(&self.staging, effect)
    }

    pub fn reserve_mutation_effect(
        &self,
        effect: MutationEffect<'_>,
    ) -> Result<MutationReservation, MutationStageError> {
        reserve_mutation_effect(&self.staging, effect)
    }

    pub fn reserved_mutation_sink(
        &mut self,
        namespace: NamespaceId,
        reservation: MutationReservation,
    ) -> Result<ReservedMutationSink<'_>, MutationStageError> {
        if reservation.start_len_bytes != self.log_staging_bytes()
            || reservation.start_record_count != self.staging.record_count()
        {
            return Err(MutationStageError::ReservationStale {
                expected_bytes: reservation.start_len_bytes,
                actual_bytes: self.log_staging_bytes(),
                expected_record_count: reservation.start_record_count,
                actual_record_count: self.staging.record_count(),
            });
        }

        Ok(ReservedMutationSink {
            cell: self,
            namespace,
            reserved_record_len: reservation.record_len,
            staged: false,
        })
    }

    pub fn reserve_mutation_effect_batch(
        &self,
        record_count: u32,
        record_len_bytes: usize,
    ) -> Result<MutationBatchReservation, MutationStageError> {
        reserve_mutation_effect_batch(&self.staging, record_count, record_len_bytes)
    }

    pub fn reserved_mutation_batch_sink(
        &mut self,
        namespace: NamespaceId,
        reservation: MutationBatchReservation,
    ) -> Result<ReservedMutationBatchSink<'_>, MutationStageError> {
        if reservation.start_len_bytes != self.log_staging_bytes()
            || reservation.start_record_count != self.staging.record_count()
        {
            return Err(MutationStageError::ReservationStale {
                expected_bytes: reservation.start_len_bytes,
                actual_bytes: self.log_staging_bytes(),
                expected_record_count: reservation.start_record_count,
                actual_record_count: self.staging.record_count(),
            });
        }

        Ok(ReservedMutationBatchSink {
            cell: self,
            namespace,
            reserved_record_count: reservation.record_count,
            reserved_len_bytes: reservation.record_len_bytes,
            staged_record_count: 0,
            staged_len_bytes: 0,
        })
    }

    pub fn stage_mutation_effect(
        &mut self,
        namespace: NamespaceId,
        effect: MutationEffect<'_>,
    ) -> Result<(), MutationStageError> {
        stage_mutation_effect(&mut self.staging, namespace, effect)
    }

    pub fn stage_checkpoint_begin(
        &mut self,
        checkpoint: CheckpointId,
    ) -> Result<(), MutationStageError> {
        stage_checkpoint_begin(&mut self.staging, checkpoint)
    }

    pub fn drain_frame(
        &mut self,
        frame_start: Lsn,
        out: &mut Vec<u8>,
    ) -> Result<Option<FrameMeta>, MutationStageError> {
        self.staging.drain_frame(frame_start, out).map_err(MutationStageError::Log)
    }
}

#[derive(Debug)]
#[must_use = "a batch reservation only protects a command if it is consumed by a batch sink"]
pub struct MutationBatchReservation {
    record_count: u32,
    record_len_bytes: usize,
    start_len_bytes: u64,
    start_record_count: u32,
}

impl MutationBatchReservation {
    #[inline]
    pub fn record_count(&self) -> u32 {
        self.record_count
    }

    #[inline]
    pub fn record_len_bytes(&self) -> usize {
        self.record_len_bytes
    }
}

#[derive(Debug)]
#[must_use = "a mutation reservation only protects a command if it is consumed by a reserved sink"]
pub struct MutationReservation {
    record_len: usize,
    start_len_bytes: u64,
    start_record_count: u32,
}

impl MutationReservation {
    #[inline]
    pub fn record_len(&self) -> usize {
        self.record_len
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MutationBatchOutcome {
    pub record_count: u32,
    pub len_bytes: usize,
}

#[derive(Debug)]
#[must_use = "finish the reserved batch sink after store mutation to verify staged effects"]
pub struct ReservedMutationBatchSink<'a> {
    cell: &'a mut DurabilityCell,
    namespace: NamespaceId,
    reserved_record_count: u32,
    reserved_len_bytes: usize,
    staged_record_count: u32,
    staged_len_bytes: usize,
}

impl ReservedMutationBatchSink<'_> {
    #[inline]
    pub fn finish(self) -> MutationBatchOutcome {
        MutationBatchOutcome {
            record_count: self.staged_record_count,
            len_bytes: self.staged_len_bytes,
        }
    }
}

impl MutationSink for ReservedMutationBatchSink<'_> {
    fn push(&mut self, effect: MutationEffect<'_>) {
        assert!(self.staged_record_count < self.reserved_record_count);

        let before = self.cell.log_staging_bytes();
        self.cell
            .stage_mutation_effect(self.namespace, effect)
            .expect("reserved mutation batch effect must fit in log staging");
        let written = (self.cell.log_staging_bytes() - before) as usize;
        self.staged_record_count += 1;
        self.staged_len_bytes += written;
        assert!(self.staged_len_bytes <= self.reserved_len_bytes);
    }
}

#[derive(Debug)]
#[must_use = "finish the reserved sink after store mutation to observe whether an effect was staged"]
pub struct ReservedMutationSink<'a> {
    cell: &'a mut DurabilityCell,
    namespace: NamespaceId,
    reserved_record_len: usize,
    staged: bool,
}

impl ReservedMutationSink<'_> {
    #[inline]
    pub fn staged(&self) -> bool {
        self.staged
    }

    #[inline]
    pub fn finish(self) -> bool {
        self.staged
    }
}

impl MutationSink for ReservedMutationSink<'_> {
    fn push(&mut self, effect: MutationEffect<'_>) {
        assert!(!self.staged, "reserved mutation sink received more than one effect");

        let before = self.cell.log_staging_bytes();
        self.cell
            .stage_mutation_effect(self.namespace, effect)
            .expect("reserved mutation effect must fit in log staging");
        let written = (self.cell.log_staging_bytes() - before) as usize;
        assert_eq!(
            written, self.reserved_record_len,
            "reserved mutation effect length drifted between admission and staging"
        );
        self.staged = true;
    }
}

impl Default for DurabilityCell {
    fn default() -> DurabilityCell {
        DurabilityCell::new()
    }
}

pub fn reserve_mutation_effect_capacity(
    staging: &LogStaging,
    effect: MutationEffect<'_>,
) -> Result<(), MutationStageError> {
    reserve_mutation_effect(staging, effect).map(|_| ())
}

pub fn reserve_mutation_effect(
    staging: &LogStaging,
    effect: MutationEffect<'_>,
) -> Result<MutationReservation, MutationStageError> {
    if staging.record_count() == u32::MAX {
        return Err(MutationStageError::Log(LogStagingError::RecordCountOverflow));
    }

    let needed = mutation_effect_record_len(effect)?;
    let available = staging.available_bytes();
    if needed > available {
        return Err(MutationStageError::Log(LogStagingError::Full { needed, available }));
    }
    Ok(MutationReservation {
        record_len: needed,
        start_len_bytes: staging.len_bytes() as u64,
        start_record_count: staging.record_count(),
    })
}

pub fn reserve_mutation_effect_batch(
    staging: &LogStaging,
    record_count: u32,
    record_len_bytes: usize,
) -> Result<MutationBatchReservation, MutationStageError> {
    if record_count > u32::MAX - staging.record_count() {
        return Err(MutationStageError::Log(LogStagingError::RecordCountOverflow));
    }

    let available = staging.available_bytes();
    if record_len_bytes > available {
        return Err(MutationStageError::Log(LogStagingError::Full {
            needed: record_len_bytes,
            available,
        }));
    }
    Ok(MutationBatchReservation {
        record_count,
        record_len_bytes,
        start_len_bytes: staging.len_bytes() as u64,
        start_record_count: staging.record_count(),
    })
}

pub fn mutation_effect_record_len(effect: MutationEffect<'_>) -> Result<usize, MutationStageError> {
    let payload_len = mutation_effect_payload_len(effect)?;
    record_encoded_len(payload_len)
        .map_err(|error| MutationStageError::Log(LogStagingError::Codec(error)))
}

pub fn stage_mutation_effect(
    staging: &mut LogStaging,
    namespace: NamespaceId,
    effect: MutationEffect<'_>,
) -> Result<(), MutationStageError> {
    let payload_len = mutation_effect_payload_len(effect)?;
    let (kind, flags) = mutation_effect_record_meta(effect);
    staging
        .try_push_payload(kind, namespace, flags, payload_len, |out| {
            encode_mutation_payload(effect, out);
        })
        .map_err(MutationStageError::Log)
}

pub fn checkpoint_begin_record_len() -> Result<usize, MutationStageError> {
    record_encoded_len(CHECKPOINT_BEGIN_PAYLOAD_LEN)
        .map_err(|error| MutationStageError::Log(LogStagingError::Codec(error)))
}

pub fn stage_checkpoint_begin(
    staging: &mut LogStaging,
    checkpoint: CheckpointId,
) -> Result<(), MutationStageError> {
    if !staging.is_empty() {
        return Err(MutationStageError::CheckpointBeginRequiresEmptyStaging {
            staged_record_count: staging.record_count(),
            staged_bytes: staging.len_bytes(),
        });
    }
    staging
        .try_push_payload(
            RecordKind::CheckpointBegin,
            LOG_CONTROL_NAMESPACE,
            0,
            CHECKPOINT_BEGIN_PAYLOAD_LEN,
            |out| out.extend_from_slice(&checkpoint.get().to_le_bytes()),
        )
        .map_err(MutationStageError::Log)
}

pub fn decode_checkpoint_begin_record(
    record: RecordRef<'_>,
) -> Result<CheckpointId, CheckpointBeginRecordDecodeError> {
    if record.kind() != RecordKind::CheckpointBegin {
        return Err(CheckpointBeginRecordDecodeError::UnsupportedKind { kind: record.kind() });
    }
    if record.flags() != 0 {
        return Err(CheckpointBeginRecordDecodeError::UnknownFlags { flags: record.flags() });
    }
    if record.namespace() != LOG_CONTROL_NAMESPACE {
        return Err(CheckpointBeginRecordDecodeError::InvalidNamespace {
            namespace: record.namespace(),
        });
    }
    let payload = record.payload();
    if payload.len() != CHECKPOINT_BEGIN_PAYLOAD_LEN {
        return Err(CheckpointBeginRecordDecodeError::InvalidLength { len: payload.len() });
    }
    let raw = u32::from_le_bytes(payload.try_into().expect("payload length checked"));
    CheckpointId::new(raw).ok_or(CheckpointBeginRecordDecodeError::InvalidCheckpointId { raw })
}

pub fn decode_mutation_record(
    record: RecordRef<'_>,
) -> Result<MutationEffect<'_>, MutationRecordDecodeError> {
    let mut payload = record.payload();
    let kind = record.kind();
    let flags = record.flags();
    let effect = match kind {
        RecordKind::StringPostImage => {
            reject_unknown_flags(kind, flags, MUTATION_FLAG_RAW | MUTATION_FLAG_HAS_EXPIRE)?;
            let key = take_len_prefixed(&mut payload)?;
            let value = take_len_prefixed(&mut payload)?;
            let expire_at_ms = take_optional_expire(flags, &mut payload)?;
            MutationEffect::StringPostImage {
                key,
                value,
                expire_at_ms,
                raw: flags & MUTATION_FLAG_RAW != 0,
            }
        }
        RecordKind::Delete => {
            reject_unknown_flags(kind, flags, 0)?;
            let key = take_len_prefixed(&mut payload)?;
            MutationEffect::Delete { key }
        }
        RecordKind::ExpireAt => {
            reject_unknown_flags(kind, flags, MUTATION_FLAG_HAS_EXPIRE)?;
            let key = take_len_prefixed(&mut payload)?;
            let expire_at_ms = take_optional_expire(flags, &mut payload)?;
            MutationEffect::ExpireAt { key, expire_at_ms }
        }
        RecordKind::Namespace | RecordKind::CheckpointBegin => {
            return Err(MutationRecordDecodeError::UnsupportedKind { kind });
        }
    };
    if !payload.is_empty() {
        return Err(MutationRecordDecodeError::TrailingBytes { bytes: payload.len() });
    }
    Ok(effect)
}

fn reject_unknown_flags(
    kind: RecordKind,
    flags: u16,
    allowed: u16,
) -> Result<(), MutationRecordDecodeError> {
    if flags & !allowed != 0 {
        return Err(MutationRecordDecodeError::UnknownFlags { kind, flags });
    }
    Ok(())
}

fn take_len_prefixed<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], MutationRecordDecodeError> {
    let (len, prefix_len) = inf_foundation::varint::decode_u64(input)
        .ok_or(MutationRecordDecodeError::InvalidLength)?;
    let len =
        usize::try_from(len).map_err(|_| MutationRecordDecodeError::LengthOverflow { len })?;
    let needed = prefix_len
        .checked_add(len)
        .ok_or(MutationRecordDecodeError::LengthOverflow { len: len as u64 })?;
    if input.len() < needed {
        return Err(MutationRecordDecodeError::Truncated { needed, available: input.len() });
    }
    let bytes = &input[prefix_len..needed];
    *input = &input[needed..];
    Ok(bytes)
}

fn take_optional_expire(
    flags: u16,
    input: &mut &[u8],
) -> Result<Option<u64>, MutationRecordDecodeError> {
    if flags & MUTATION_FLAG_HAS_EXPIRE == 0 {
        return Ok(None);
    }
    let needed = size_of::<u64>();
    if input.len() < needed {
        return Err(MutationRecordDecodeError::Truncated { needed, available: input.len() });
    }
    let expire_at_ms = u64::from_le_bytes(
        input[..needed].try_into().expect("slice length checked against u64 width"),
    );
    if expire_at_ms > MAX_EXPIRE_MS {
        return Err(MutationRecordDecodeError::ExpireOutOfRange { expire_at_ms });
    }
    *input = &input[needed..];
    Ok(Some(expire_at_ms))
}

fn mutation_effect_record_meta(effect: MutationEffect<'_>) -> (RecordKind, u16) {
    match effect {
        MutationEffect::StringPostImage { expire_at_ms, raw, .. } => {
            let mut flags = if raw { MUTATION_FLAG_RAW } else { 0 };
            apply_optional_expire_flag(expire_at_ms, &mut flags);
            (RecordKind::StringPostImage, flags)
        }
        MutationEffect::Delete { .. } => (RecordKind::Delete, 0),
        MutationEffect::ExpireAt { expire_at_ms, .. } => {
            let mut flags = 0;
            apply_optional_expire_flag(expire_at_ms, &mut flags);
            (RecordKind::ExpireAt, flags)
        }
    }
}

fn encode_mutation_payload(effect: MutationEffect<'_>, out: &mut Vec<u8>) {
    match effect {
        MutationEffect::StringPostImage { key, value, expire_at_ms, raw: _ } => {
            encode_key(key, out);
            encode_u64(value.len() as u64, out);
            out.extend_from_slice(value);
            encode_optional_expire(expire_at_ms, out);
        }
        MutationEffect::Delete { key } => encode_key(key, out),
        MutationEffect::ExpireAt { key, expire_at_ms } => {
            encode_key(key, out);
            encode_optional_expire(expire_at_ms, out);
        }
    }
}

fn mutation_effect_payload_len(effect: MutationEffect<'_>) -> Result<usize, MutationStageError> {
    match effect {
        MutationEffect::StringPostImage { key, value, expire_at_ms, raw: _ } => {
            let len = checked_len_add(
                len_prefixed_bytes_len(key.len())?,
                len_prefixed_bytes_len(value.len())?,
            )?;
            checked_len_add(len, optional_expire_len(expire_at_ms))
        }
        MutationEffect::Delete { key } => len_prefixed_bytes_len(key.len()),
        MutationEffect::ExpireAt { key, expire_at_ms } => {
            checked_len_add(len_prefixed_bytes_len(key.len())?, optional_expire_len(expire_at_ms))
        }
    }
}

fn len_prefixed_bytes_len(len: usize) -> Result<usize, MutationStageError> {
    checked_len_add(varint_len_usize(len), len)
}

fn optional_expire_len(expire_at_ms: Option<u64>) -> usize {
    if expire_at_ms.is_some() { size_of::<u64>() } else { 0 }
}

fn checked_len_add(lhs: usize, rhs: usize) -> Result<usize, MutationStageError> {
    lhs.checked_add(rhs).ok_or_else(record_too_large_error)
}

fn record_too_large_error() -> MutationStageError {
    MutationStageError::Log(LogStagingError::Codec(LogCodecError::RecordTooLarge))
}

fn varint_len_usize(mut value: usize) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn encode_key(key: &[u8], out: &mut Vec<u8>) {
    encode_u64(key.len() as u64, out);
    out.extend_from_slice(key);
}

fn apply_optional_expire_flag(expire_at_ms: Option<u64>, flags: &mut u16) {
    if expire_at_ms.is_some() {
        *flags |= MUTATION_FLAG_HAS_EXPIRE;
    }
}

fn encode_optional_expire(expire_at_ms: Option<u64>, out: &mut Vec<u8>) {
    if let Some(ms) = expire_at_ms {
        out.extend_from_slice(&ms.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_foundation::time::Nanos;
    use inf_foundation::varint::decode_u64;
    use inf_log::{Lsn, MAX_RECORD_PAYLOAD_LEN, MAX_STAGING_BODY_LEN, decode_batch_frame};
    use inf_store::{
        CellStore, MAX_KEY_LEN, MAX_VAL_LEN, SetCond, SetOptions, SetOutcome, StoreConfig,
    };

    fn decode_len_prefixed(input: &[u8]) -> (&[u8], &[u8]) {
        let (len, prefix) = decode_u64(input).expect("length");
        let len = len as usize;
        (&input[prefix..prefix + len], &input[prefix + len..])
    }

    #[test]
    fn stages_string_post_image_payload_shape() {
        let mut staging = LogStaging::with_capacity(128).unwrap();
        let effect = MutationEffect::StringPostImage {
            key: b"k",
            value: b"value",
            expire_at_ms: Some(42),
            raw: true,
        };

        stage_mutation_effect(&mut staging, NamespaceId::new(9), effect).unwrap();
        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();

        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();
        assert_eq!(records.len(), 1);
        let record = records[0].record();
        assert_eq!(record.kind(), RecordKind::StringPostImage);
        assert_eq!(record.namespace(), NamespaceId::new(9));
        assert_eq!(record.flags(), MUTATION_FLAG_RAW | MUTATION_FLAG_HAS_EXPIRE);

        let (key, rest) = decode_len_prefixed(record.payload());
        let (value, rest) = decode_len_prefixed(rest);
        assert_eq!(key, b"k");
        assert_eq!(value, b"value");
        assert_eq!(rest, 42u64.to_le_bytes());
    }

    #[test]
    fn stages_delete_and_persist_expire_shapes() {
        let mut staging = LogStaging::with_capacity(128).unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::Delete { key: b"dead" },
        )
        .unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::ExpireAt { key: b"ttl", expire_at_ms: None },
        )
        .unwrap();

        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();
        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();
        assert_eq!(records.len(), 2);

        let delete = records[0].record();
        let (key, rest) = decode_len_prefixed(delete.payload());
        assert_eq!(delete.kind(), RecordKind::Delete);
        assert_eq!(delete.flags(), 0);
        assert_eq!(key, b"dead");
        assert!(rest.is_empty());

        let expire = records[1].record();
        let (key, rest) = decode_len_prefixed(expire.payload());
        assert_eq!(expire.kind(), RecordKind::ExpireAt);
        assert_eq!(expire.flags(), 0);
        assert_eq!(key, b"ttl");
        assert!(rest.is_empty());
    }

    #[test]
    fn decodes_staged_mutation_records() {
        let mut staging = LogStaging::with_capacity(256).unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::StringPostImage {
                key: b"k",
                value: b"value",
                expire_at_ms: Some(42),
                raw: true,
            },
        )
        .unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::Delete { key: b"dead" },
        )
        .unwrap();
        stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::ExpireAt { key: b"ttl", expire_at_ms: None },
        )
        .unwrap();

        let mut frame = Vec::new();
        staging.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();
        let decoded = decode_batch_frame(&frame).unwrap();
        let effects: Vec<_> = decoded
            .records()
            .map(|record| decode_mutation_record(record.record()).unwrap())
            .collect();

        assert_eq!(
            effects,
            vec![
                MutationEffect::StringPostImage {
                    key: b"k",
                    value: b"value",
                    expire_at_ms: Some(42),
                    raw: true,
                },
                MutationEffect::Delete { key: b"dead" },
                MutationEffect::ExpireAt { key: b"ttl", expire_at_ms: None },
            ]
        );
    }

    #[test]
    fn mutation_decoder_rejects_unknown_flags() {
        let record = inf_log::RecordRef::new(
            RecordKind::Delete,
            NamespaceId::new(1),
            MUTATION_FLAG_RAW,
            &[1, b'k'],
        )
        .unwrap();

        assert_eq!(
            decode_mutation_record(record),
            Err(MutationRecordDecodeError::UnknownFlags {
                kind: RecordKind::Delete,
                flags: MUTATION_FLAG_RAW,
            })
        );
    }

    #[test]
    fn max_store_string_post_image_fits_log_record_bound() {
        let payload_len = checked_len_add(
            checked_len_add(
                len_prefixed_bytes_len(MAX_KEY_LEN).unwrap(),
                len_prefixed_bytes_len(MAX_VAL_LEN).unwrap(),
            )
            .unwrap(),
            optional_expire_len(Some(0)),
        )
        .unwrap();
        let record_len = record_encoded_len(payload_len).unwrap();

        assert_eq!(payload_len, MAX_RECORD_PAYLOAD_LEN);
        assert!(record_len <= MAX_STAGING_BODY_LEN);
    }

    #[test]
    fn durability_cell_default_holds_one_max_record() {
        let cell = DurabilityCell::new();

        assert_eq!(cell.log_staging_bytes(), 0);
        assert!(cell.log_staging_capacity_bytes() as usize >= MAX_RECORD_ENCODED_LEN);
    }

    #[test]
    fn durability_cell_gauges_track_append_and_drain() {
        let effect = MutationEffect::StringPostImage {
            key: b"k",
            value: b"value",
            expire_at_ms: Some(42),
            raw: true,
        };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed).unwrap();
        let capacity = cell.log_staging_capacity_bytes();

        cell.reserve_mutation_effect_capacity(effect).unwrap();
        cell.stage_mutation_effect(NamespaceId::new(4), effect).unwrap();
        assert_eq!(cell.log_staging_bytes(), needed as u64);
        assert_eq!(cell.log_staging_capacity_bytes(), capacity);

        let mut frame = Vec::new();
        let meta = cell.drain_frame(Lsn::new(0, 0), &mut frame).unwrap().unwrap();
        assert_eq!(meta.record_count(), 1);
        assert_eq!(cell.log_staging_bytes(), 0);
        assert_eq!(cell.log_staging_capacity_bytes(), capacity);
    }

    #[test]
    fn stages_checkpoint_begin_as_isolated_control_record() {
        let checkpoint = CheckpointId::new(7).unwrap();
        let needed = checkpoint_begin_record_len().unwrap();
        let mut cell = DurabilityCell::with_capacity(needed).unwrap();

        cell.stage_checkpoint_begin(checkpoint).unwrap();
        assert_eq!(cell.log_staging_bytes(), needed as u64);

        let mut frame = Vec::new();
        let meta = cell.drain_frame(Lsn::new(2, 0), &mut frame).unwrap().unwrap();
        assert_eq!(meta.record_count(), 1);
        assert_eq!(meta.first_lsn(), Lsn::new(2, inf_log::FRAME_HEADER_LEN as u32));

        let decoded = decode_batch_frame(&frame).unwrap();
        let records: Vec<_> = decoded.records().collect();
        let record = records[0].record();
        assert_eq!(record.kind(), RecordKind::CheckpointBegin);
        assert_eq!(record.namespace(), inf_log::LOG_CONTROL_NAMESPACE);
        assert_eq!(record.flags(), 0);
        assert_eq!(decode_checkpoint_begin_record(record), Ok(checkpoint));
    }

    #[test]
    fn checkpoint_begin_rejects_nonempty_staging() {
        let checkpoint = CheckpointId::new(7).unwrap();
        let effect = MutationEffect::Delete { key: b"k" };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell =
            DurabilityCell::with_capacity(needed + checkpoint_begin_record_len().unwrap()).unwrap();

        cell.stage_mutation_effect(NamespaceId::new(1), effect).unwrap();
        assert_eq!(
            cell.stage_checkpoint_begin(checkpoint),
            Err(MutationStageError::CheckpointBeginRequiresEmptyStaging {
                staged_record_count: 1,
                staged_bytes: needed,
            })
        );
    }

    #[test]
    fn checkpoint_begin_decoder_rejects_noncanonical_record_shape() {
        let checkpoint = CheckpointId::new(7).unwrap();
        let checkpoint_bytes = checkpoint.get().to_le_bytes();
        let bad_kind = RecordRef::new(
            RecordKind::Delete,
            inf_log::LOG_CONTROL_NAMESPACE,
            0,
            &checkpoint_bytes,
        )
        .unwrap();
        assert_eq!(
            decode_checkpoint_begin_record(bad_kind),
            Err(CheckpointBeginRecordDecodeError::UnsupportedKind { kind: RecordKind::Delete })
        );

        let bad_namespace =
            RecordRef::new(RecordKind::CheckpointBegin, NamespaceId::new(1), 0, &checkpoint_bytes)
                .unwrap();
        assert_eq!(
            decode_checkpoint_begin_record(bad_namespace),
            Err(CheckpointBeginRecordDecodeError::InvalidNamespace {
                namespace: NamespaceId::new(1),
            })
        );

        let bad_flags = RecordRef::new(
            RecordKind::CheckpointBegin,
            inf_log::LOG_CONTROL_NAMESPACE,
            MUTATION_FLAG_RAW,
            &checkpoint_bytes,
        )
        .unwrap();
        assert_eq!(
            decode_checkpoint_begin_record(bad_flags),
            Err(CheckpointBeginRecordDecodeError::UnknownFlags { flags: MUTATION_FLAG_RAW })
        );

        let bad_len =
            RecordRef::new(RecordKind::CheckpointBegin, inf_log::LOG_CONTROL_NAMESPACE, 0, &[0])
                .unwrap();
        assert_eq!(
            decode_checkpoint_begin_record(bad_len),
            Err(CheckpointBeginRecordDecodeError::InvalidLength { len: 1 })
        );
    }

    #[test]
    fn reserve_matches_actual_staged_length() {
        let effects = [
            MutationEffect::StringPostImage {
                key: b"k",
                value: b"value",
                expire_at_ms: Some(42),
                raw: true,
            },
            MutationEffect::StringPostImage {
                key: b"rawless",
                value: b"v",
                expire_at_ms: None,
                raw: false,
            },
            MutationEffect::Delete { key: b"dead" },
            MutationEffect::ExpireAt { key: b"ttl", expire_at_ms: Some(9) },
            MutationEffect::ExpireAt { key: b"persist", expire_at_ms: None },
        ];

        for effect in effects {
            let needed = mutation_effect_record_len(effect).unwrap();
            let mut staging = LogStaging::with_capacity(needed).unwrap();
            let capacity = staging.capacity_bytes();

            reserve_mutation_effect_capacity(&staging, effect).unwrap();
            stage_mutation_effect(&mut staging, NamespaceId::new(3), effect).unwrap();

            assert_eq!(staging.capacity_bytes(), capacity);
            assert_eq!(staging.len_bytes(), needed);
            assert_eq!(staging.record_count(), 1);
        }
    }

    #[test]
    fn reserve_rejects_before_mutation_when_full() {
        let staging = LogStaging::with_capacity(1).unwrap();
        let effect = MutationEffect::Delete { key: b"too-large" };
        let needed = mutation_effect_record_len(effect).unwrap();
        let result = reserve_mutation_effect_capacity(&staging, effect);

        assert_eq!(
            result,
            Err(MutationStageError::Log(LogStagingError::Full { needed, available: 1 }))
        );
        assert_eq!(staging.len_bytes(), 0);
        assert_eq!(staging.record_count(), 0);
    }

    #[test]
    fn reserved_sink_stages_after_store_applies() {
        let effect = MutationEffect::StringPostImage {
            key: b"k",
            value: b"value",
            expire_at_ms: None,
            raw: false,
        };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed).unwrap();
        let reservation = cell.reserve_mutation_effect(effect).unwrap();
        assert_eq!(reservation.record_len(), needed);

        let mut store = CellStore::new(StoreConfig::default());
        let mut sink = cell.reserved_mutation_sink(NamespaceId::new(2), reservation).unwrap();
        let outcome = store
            .set_with_effect(b"k", b"value", SetOptions::default(), Nanos(1_000_000), &mut sink)
            .unwrap();

        assert_eq!(outcome, SetOutcome::Applied { old: None });
        assert!(sink.staged());
        assert!(sink.finish());
        assert_eq!(cell.log_staging_bytes(), needed as u64);
    }

    #[test]
    fn reserved_sink_allows_store_skip_without_staging() {
        let effect = MutationEffect::StringPostImage {
            key: b"k",
            value: b"ignored",
            expire_at_ms: None,
            raw: false,
        };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed).unwrap();
        let mut store = CellStore::new(StoreConfig::default());
        let now = Nanos(1_000_000);

        store.set(b"k", b"v", SetOptions::default(), now).unwrap();
        let reservation = cell.reserve_mutation_effect(effect).unwrap();
        let mut sink = cell.reserved_mutation_sink(NamespaceId::new(2), reservation).unwrap();
        let outcome = store
            .set_with_effect(
                b"k",
                b"ignored",
                SetOptions { cond: SetCond::IfAbsent, ..Default::default() },
                now,
                &mut sink,
            )
            .unwrap();

        assert_eq!(outcome, SetOutcome::Skipped { old: None });
        assert!(!sink.finish());
        assert_eq!(cell.log_staging_bytes(), 0);
    }

    #[test]
    fn reservation_rejects_before_store_mutation_when_full() {
        let effect = MutationEffect::StringPostImage {
            key: b"k",
            value: b"value",
            expire_at_ms: None,
            raw: false,
        };
        let needed = mutation_effect_record_len(effect).unwrap();
        let cell = DurabilityCell::with_capacity(needed - 1).unwrap();
        let mut store = CellStore::new(StoreConfig::default());
        let now = Nanos(1_000_000);

        assert!(matches!(
            cell.reserve_mutation_effect(effect),
            Err(MutationStageError::Log(LogStagingError::Full { .. }))
        ));
        assert_eq!(store.get(b"k", now), None);
    }

    #[test]
    fn stale_reservation_is_rejected_before_sink_creation() {
        let effect = MutationEffect::Delete { key: b"k" };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed * 2).unwrap();
        let reservation = cell.reserve_mutation_effect(effect).unwrap();

        cell.stage_mutation_effect(NamespaceId::new(2), effect).unwrap();
        assert_eq!(
            cell.reserved_mutation_sink(NamespaceId::new(2), reservation).unwrap_err(),
            MutationStageError::ReservationStale {
                expected_bytes: 0,
                actual_bytes: needed as u64,
                expected_record_count: 0,
                actual_record_count: 1,
            }
        );
    }

    #[test]
    fn reserved_batch_sink_stages_multiple_effects_after_whole_command_reservation() {
        let first = MutationEffect::Delete { key: b"a" };
        let second = MutationEffect::ExpireAt { key: b"b", expire_at_ms: Some(9) };
        let needed = mutation_effect_record_len(first).unwrap()
            + mutation_effect_record_len(second).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed).unwrap();
        let reservation = cell.reserve_mutation_effect_batch(2, needed).unwrap();

        assert_eq!(reservation.record_count(), 2);
        assert_eq!(reservation.record_len_bytes(), needed);

        let mut sink = cell.reserved_mutation_batch_sink(NamespaceId::new(2), reservation).unwrap();
        sink.push(first);
        sink.push(second);
        let outcome = sink.finish();

        assert_eq!(outcome, MutationBatchOutcome { record_count: 2, len_bytes: needed });
        assert_eq!(cell.log_staging_bytes(), needed as u64);
    }

    #[test]
    fn batch_reservation_rejects_before_any_staging_when_full() {
        let first = MutationEffect::Delete { key: b"a" };
        let second = MutationEffect::Delete { key: b"b" };
        let needed = mutation_effect_record_len(first).unwrap()
            + mutation_effect_record_len(second).unwrap();
        let cell = DurabilityCell::with_capacity(needed - 1).unwrap();

        assert!(matches!(
            cell.reserve_mutation_effect_batch(2, needed),
            Err(MutationStageError::Log(LogStagingError::Full { .. }))
        ));
        assert_eq!(cell.log_staging_bytes(), 0);
    }

    #[test]
    fn stale_batch_reservation_is_rejected_before_sink_creation() {
        let effect = MutationEffect::Delete { key: b"k" };
        let needed = mutation_effect_record_len(effect).unwrap();
        let mut cell = DurabilityCell::with_capacity(needed * 2).unwrap();
        let reservation = cell.reserve_mutation_effect_batch(1, needed).unwrap();

        cell.stage_mutation_effect(NamespaceId::new(2), effect).unwrap();
        assert_eq!(
            cell.reserved_mutation_batch_sink(NamespaceId::new(2), reservation).unwrap_err(),
            MutationStageError::ReservationStale {
                expected_bytes: 0,
                actual_bytes: needed as u64,
                expected_record_count: 0,
                actual_record_count: 1,
            }
        );
    }

    #[test]
    fn staging_full_returns_backpressure_error() {
        let mut staging = LogStaging::with_capacity(1).unwrap();
        let result = stage_mutation_effect(
            &mut staging,
            NamespaceId::new(1),
            MutationEffect::Delete { key: b"too-large" },
        );

        assert!(matches!(result, Err(MutationStageError::Log(LogStagingError::Full { .. }))));
    }
}
