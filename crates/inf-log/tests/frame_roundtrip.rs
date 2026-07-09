//! M2-S01 AC: arbitrary record sequences round-trip frame encode/decode
//! byte-exact, including frame-boundary edge cases; corruption and
//! truncation are always detected, never silently absorbed.

use inf_log::{
    DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN, FRAME_TRAILER_LEN, FrameBuilder, FrameIter,
    FrameStamp, Lsn, NsId, RecordView, SegmentId, decode_frame,
};
use proptest::prelude::*;

/// Canonical v2 stamp for hand-built test frames (epoch 1, covered 0 —
/// attests nothing). `seq` matters only where a test builds sequential
/// frames the recovery policy will walk; readers/scanners ignore it.
fn stamp(seq: u64) -> FrameStamp {
    FrameStamp { epoch: 1, seq, covered_lsn: 0 }
}

/// Owned mirror of `RecordView` for proptest generation.
#[derive(Clone, Debug)]
enum OwnedRecord {
    String { ns: u32, key: Vec<u8>, value: Vec<u8> },
    Delete { ns: u32, key: Vec<u8> },
    ExpireAt { ns: u32, at: u64, key: Vec<u8> },
    NsOp { ns: u32, payload: Vec<u8> },
    CkptBegin { ns: u32, id: u64 },
}

impl OwnedRecord {
    fn view(&self) -> RecordView<'_> {
        match self {
            OwnedRecord::String { ns, key, value } => {
                RecordView::StringPostImage { ns: NsId(*ns), key, value }
            }
            OwnedRecord::Delete { ns, key } => RecordView::Delete { ns: NsId(*ns), key },
            OwnedRecord::ExpireAt { ns, at, key } => {
                RecordView::ExpireAt { ns: NsId(*ns), at_unix_ms: *at, key }
            }
            OwnedRecord::NsOp { ns, payload } => RecordView::NsOp { ns: NsId(*ns), payload },
            OwnedRecord::CkptBegin { ns, id } => {
                RecordView::CkptBegin { ns: NsId(*ns), ckpt_id: *id }
            }
        }
    }
}

fn record_strategy() -> impl Strategy<Value = OwnedRecord> {
    let bytes = || proptest::collection::vec(any::<u8>(), 0..48);
    prop_oneof![
        (any::<u32>(), bytes(), bytes()).prop_map(|(ns, key, value)| OwnedRecord::String {
            ns,
            key,
            value
        }),
        (any::<u32>(), bytes()).prop_map(|(ns, key)| OwnedRecord::Delete { ns, key }),
        (any::<u32>(), any::<u64>(), bytes()).prop_map(|(ns, at, key)| OwnedRecord::ExpireAt {
            ns,
            at,
            key
        }),
        (any::<u32>(), bytes()).prop_map(|(ns, payload)| OwnedRecord::NsOp { ns, payload }),
        (any::<u32>(), any::<u64>()).prop_map(|(ns, id)| OwnedRecord::CkptBegin { ns, id }),
    ]
}

fn frames_strategy() -> impl Strategy<Value = Vec<Vec<OwnedRecord>>> {
    proptest::collection::vec(proptest::collection::vec(record_strategy(), 1..17), 1..8)
}

/// Encode `frames` into a contiguous segment image starting at offset 0,
/// returning the image and the expected (lsn, record) sequence.
fn build_image(
    segment: SegmentId,
    frames: &[Vec<OwnedRecord>],
) -> (Vec<u8>, Vec<(Lsn, OwnedRecord)>) {
    let mut image = Vec::new();
    let mut expected = Vec::new();
    let mut builder = FrameBuilder::new();
    for (index, frame) in frames.iter().enumerate() {
        builder.reset();
        let base = Lsn::new(segment, u32::try_from(image.len()).expect("image fits u32"));
        for record in frame {
            // Next record lands right after the bytes staged so far.
            let staged = builder.frame_len() as usize - FRAME_TRAILER_LEN;
            expected.push((base.advance(staged as u32), record.clone()));
            builder.append(&record.view());
        }
        image.extend_from_slice(
            builder.finalize(base.advance(FRAME_HEADER_LEN as u32), stamp(index as u64 + 1)),
        );
    }
    (image, expected)
}

proptest! {
    /// Byte-exact round trip across whole segment images, LSNs included.
    #[test]
    fn frames_round_trip_byte_exact(frames in frames_strategy()) {
        let segment = SegmentId(7);
        let (image, expected) = build_image(segment, &frames);

        let mut iter = FrameIter::new(&image, DEFAULT_MAX_FRAME_LEN);
        let mut decoded = Vec::new();
        let mut frame_count = 0;
        for result in &mut iter {
            let (frame_offset, frame) = result.expect("clean image decodes");
            prop_assert_eq!(
                frame.first_lsn(),
                Lsn::new(segment, frame_offset as u32 + FRAME_HEADER_LEN as u32)
            );
            for record in frame.records() {
                let (lsn, view) = record.expect("CRC-valid frame yields records");
                decoded.push((lsn, view_to_owned(&view)));
            }
            frame_count += 1;
        }
        prop_assert_eq!(iter.offset(), image.len(), "iterator consumes the whole image");
        prop_assert_eq!(frame_count, frames.len());
        prop_assert_eq!(decoded.len(), expected.len());
        for ((got_lsn, got), (want_lsn, want)) in decoded.iter().zip(&expected) {
            prop_assert_eq!(got_lsn, want_lsn);
            prop_assert_eq!(got.view(), want.view());
        }

        // Re-encoding the decoded sequence reproduces the image bit-for-bit
        // (one value, one encoding — L7).
        let owned: Vec<Vec<OwnedRecord>> = split_like(&frames, &decoded);
        let (reencoded, _) = build_image(segment, &owned);
        prop_assert_eq!(reencoded, image);
    }

    /// Any single corrupted byte in a frame is detected.
    #[test]
    fn single_byte_corruption_is_detected(
        records in proptest::collection::vec(record_strategy(), 1..9),
        corrupt in any::<prop::sample::Index>(),
        flip in 1u8..=255,
    ) {
        let mut builder = FrameBuilder::new();
        for record in &records {
            builder.append(&record.view());
        }
        let base = Lsn::new(SegmentId(0), 0);
        let mut image = builder.finalize(base.advance(FRAME_HEADER_LEN as u32), stamp(1)).to_vec();
        let at = corrupt.index(image.len());
        image[at] ^= flip;
        match decode_frame(&image, DEFAULT_MAX_FRAME_LEN) {
            Err(_) => {}
            Ok((frame, _)) => {
                // Header+body corruption always fails the CRC before this
                // point; a flip that still decodes would be a CRC32C
                // collision — treat as failure.
                let all_valid = frame.records().all(|r| r.is_ok());
                prop_assert!(!all_valid, "corrupted frame decoded cleanly (byte {at})");
            }
        }
    }

    /// Every proper prefix of a frame fails as truncated (torn tail), never
    /// as a clean decode.
    #[test]
    fn truncation_never_decodes(records in proptest::collection::vec(record_strategy(), 1..5)) {
        let mut builder = FrameBuilder::new();
        for record in &records {
            builder.append(&record.view());
        }
        let base = Lsn::new(SegmentId(0), 0);
        let image = builder.finalize(base.advance(FRAME_HEADER_LEN as u32), stamp(1)).to_vec();
        for cut in 0..image.len() {
            prop_assert!(
                decode_frame(&image[..cut], DEFAULT_MAX_FRAME_LEN).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
    }
}

fn view_to_owned(view: &RecordView<'_>) -> OwnedRecord {
    match *view {
        RecordView::StringPostImage { ns, key, value } => {
            OwnedRecord::String { ns: ns.0, key: key.to_vec(), value: value.to_vec() }
        }
        RecordView::Delete { ns, key } => OwnedRecord::Delete { ns: ns.0, key: key.to_vec() },
        RecordView::ExpireAt { ns, at_unix_ms, key } => {
            OwnedRecord::ExpireAt { ns: ns.0, at: at_unix_ms, key: key.to_vec() }
        }
        RecordView::NsOp { ns, payload } => {
            OwnedRecord::NsOp { ns: ns.0, payload: payload.to_vec() }
        }
        RecordView::CkptBegin { ns, ckpt_id } => OwnedRecord::CkptBegin { ns: ns.0, id: ckpt_id },
    }
}

/// Regroup the flat decoded sequence into the original frame shapes.
fn split_like(shape: &[Vec<OwnedRecord>], flat: &[(Lsn, OwnedRecord)]) -> Vec<Vec<OwnedRecord>> {
    let mut out = Vec::with_capacity(shape.len());
    let mut cursor = flat.iter();
    for frame in shape {
        out.push(cursor.by_ref().take(frame.len()).map(|(_, r)| r.clone()).collect());
    }
    out
}

/// Frame-boundary edge case pinned explicitly: a minimal frame (single
/// empty-key delete) decodes and re-encodes byte-exact.
#[test]
fn minimal_frame_round_trips() {
    let mut builder = FrameBuilder::new();
    builder.append(&RecordView::Delete { ns: NsId(0), key: b"" });
    let base = Lsn::new(SegmentId(0), 0);
    let image = builder.finalize(base.advance(FRAME_HEADER_LEN as u32), stamp(1)).to_vec();
    let (frame, consumed) = decode_frame(&image, DEFAULT_MAX_FRAME_LEN).expect("decodes");
    assert_eq!(consumed, image.len());
    assert_eq!(frame.record_count(), 1);
    let records: Vec<_> = frame.records().collect::<Result<_, _>>().expect("valid");
    assert_eq!(records[0].1, RecordView::Delete { ns: NsId(0), key: b"" });
}

/// Builder reuse (`reset`) produces the same bytes as a fresh builder —
/// the buffer-reuse path cannot leak prior-iteration state.
#[test]
fn builder_reuse_is_clean() {
    let base = Lsn::new(SegmentId(3), 96);
    let first_lsn = base.advance(FRAME_HEADER_LEN as u32);

    let mut reused = FrameBuilder::new();
    reused.append(&RecordView::Delete { ns: NsId(1), key: b"first-frame-key" });
    let _ = reused.finalize(first_lsn, stamp(1));
    reused.reset();
    reused.append(&RecordView::NsOp { ns: NsId(2), payload: b"second" });
    let from_reused = reused.finalize(first_lsn, stamp(2)).to_vec();

    let mut fresh = FrameBuilder::new();
    fresh.append(&RecordView::NsOp { ns: NsId(2), payload: b"second" });
    assert_eq!(fresh.finalize(first_lsn, stamp(2)), from_reused.as_slice());
}
