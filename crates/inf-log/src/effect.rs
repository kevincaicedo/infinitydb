//! `MutationEffect` — the typed-effect seam between the mutation path and
//! the log spine (M2-S03; freezes at M2 exit — milestone §3.2). The store
//! layer describes *what became true* (a value was set, a key deleted, an
//! expiry armed); [`effect_record`](MutationEffect::record) is the encoder
//! registry that maps each effect onto the record-v1 vocabulary. M3 adds
//! document full/delta variants here — new effects, new record tags, zero
//! changes to the frame spine (L2).
//!
//! Placement (recorded in ADR-0012): the enum lives in `inf-log`, not
//! `inf-store`, because the log spine is the *consumer* of the seam
//! (milestone §3.3) and the dep-DAG arrow points `inf-store → inf-log` —
//! the store imports this vocabulary when M2-S08 wires durable namespaces
//! into the mutation path. `inf-log` still never sees keyspace semantics:
//! an effect is bytes, a namespace id, and a deadline.

use crate::record::{DocLineage, NsId, RecordView};

/// One durable fact emitted by the mutation path during EXECUTE, borrowing
/// the caller's bytes — staging encodes it in place, no intermediate copy.
///
/// Variants map 1:1 onto record v1 today; the mapping is allowed to become
/// 1:N (one effect, several records) for later engines without touching
/// callers of [`StagingRing::stage`](crate::StagingRing::stage).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MutationEffect<'a> {
    /// A string key now holds `value` (SET family, INCR results, APPEND
    /// results — always the full post-image; ADR-0011 Decision 4).
    StringSet { ns: NsId, key: &'a [u8], value: &'a [u8] },
    /// A key was removed (DEL, expired-on-write, eviction in a durable ns).
    Delete { ns: NsId, key: &'a [u8] },
    /// A key's expiry is now the absolute unix-milliseconds deadline
    /// (relative TTLs are resolved by the store before emission — L7).
    ExpireAt { ns: NsId, at_unix_ms: u64, key: &'a [u8] },
    /// Namespace DDL (payload vocabulary owned by M2-S08).
    NsOp { ns: NsId, payload: &'a [u8] },
    /// Checkpoint-begin marker (M2-S10, ADR-0016 D3): not a mutation — the
    /// checkpoint slice stages it through the same ring so the
    /// one-frame-per-iteration rule holds and its LSN resolves through the
    /// ordinary `FrameLease::lsn_of` path. Cell-scoped (records as ns 0).
    CkptBegin { ckpt_id: u64 },
    /// Version-guarded document path mutation (ADR-0043 D3). Fields are
    /// encoded directly into staging; `inf-log` does not interpret them.
    DocDelta {
        ns: NsId,
        key: &'a [u8],
        lineage: DocLineage,
        base_version: u32,
        match_count: u32,
        post_len: u32,
        opcode: u8,
        program: &'a [u8],
        operand: &'a [u8],
    },
    /// Canonical full-document post-image.
    DocFull { ns: NsId, key: &'a [u8], lineage: DocLineage, version: u32, idoc: &'a [u8] },
    /// A string key now holds a value stored out of line in a blob
    /// extent (M4-S17, ADR-0061 D2/D3). Constructed only from a
    /// [`SealedExtent`](crate::SealedExtent) — the extent's fdatasync
    /// preceded this effect's existence, so "extent durable before the
    /// referencing ack" holds by construction.
    StringSetExtent { ns: NsId, key: &'a [u8], extent_id: u64, offset: u64, len: u64 },
}

impl<'a> MutationEffect<'a> {
    /// The record-v1 encoding of this effect — the encoder registry of the
    /// milestone §3.2 seam.
    #[must_use]
    pub fn record(&self) -> RecordView<'a> {
        match *self {
            MutationEffect::StringSet { ns, key, value } => {
                RecordView::StringPostImage { ns, key, value }
            }
            MutationEffect::Delete { ns, key } => RecordView::Delete { ns, key },
            MutationEffect::ExpireAt { ns, at_unix_ms, key } => {
                RecordView::ExpireAt { ns, at_unix_ms, key }
            }
            MutationEffect::NsOp { ns, payload } => RecordView::NsOp { ns, payload },
            MutationEffect::CkptBegin { ckpt_id } => RecordView::CkptBegin { ns: NsId(0), ckpt_id },
            MutationEffect::DocDelta {
                ns,
                key,
                lineage,
                base_version,
                match_count,
                post_len,
                opcode,
                program,
                operand,
            } => RecordView::DocDelta {
                ns,
                key,
                lineage,
                base_version,
                match_count,
                post_len,
                opcode,
                program,
                operand,
            },
            MutationEffect::DocFull { ns, key, lineage, version, idoc } => {
                RecordView::DocFull { ns, key, lineage, version, idoc }
            }
            MutationEffect::StringSetExtent { ns, key, extent_id, offset, len } => {
                RecordView::StringExtentRef { ns, key, extent_id, offset, len }
            }
        }
    }

    /// Exact encoded size in the log, length prefix included — what the
    /// staging admission check and `log_staging_bytes` accounting use (L5).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.record().encoded_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::decode_record;

    #[test]
    fn every_effect_round_trips_through_its_record() {
        let effects = [
            MutationEffect::StringSet { ns: NsId(3), key: b"user:1", value: b"v" },
            MutationEffect::Delete { ns: NsId(0), key: b"gone" },
            MutationEffect::ExpireAt { ns: NsId(9), at_unix_ms: 1_780_000_000_123, key: b"s" },
            MutationEffect::NsOp { ns: NsId(1), payload: b"create" },
            MutationEffect::CkptBegin { ckpt_id: 42 },
            MutationEffect::DocDelta {
                ns: NsId(7),
                key: b"doc",
                lineage: DocLineage::FIRST,
                base_version: 9,
                match_count: 1,
                post_len: 64,
                opcode: 4,
                program: b"program",
                operand: b"operand",
            },
            MutationEffect::DocFull {
                ns: NsId(7),
                key: b"doc",
                lineage: DocLineage::FIRST,
                version: 10,
                idoc: b"idoc",
            },
            MutationEffect::StringSetExtent {
                ns: NsId(5),
                key: b"big",
                extent_id: 42,
                offset: 0,
                len: 1 << 30,
            },
        ];
        for effect in effects {
            let record = effect.record();
            assert_eq!(effect.encoded_len(), record.encoded_len());
            let mut buf = Vec::new();
            record.encode_into(&mut buf);
            let (decoded, consumed) = decode_record(&buf).expect("canonical bytes");
            assert_eq!(consumed, buf.len());
            assert_eq!(decoded, record);
        }
    }
}
