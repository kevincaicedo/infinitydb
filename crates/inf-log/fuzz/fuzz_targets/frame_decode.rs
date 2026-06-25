//! Log frame decoder totality (M2-S01): arbitrary bytes must decode or fail
//! with a typed error. Frames that decode must expose records only after full
//! validation and re-encode byte-exact.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(frame) = inf_log::decode_batch_frame(data) {
        let first_lsn = frame.first_lsn();
        assert!(first_lsn.offset() >= inf_log::FRAME_HEADER_LEN as u32);

        let frame_start = inf_log::Lsn::new(
            first_lsn.segment(),
            first_lsn.offset() - inf_log::FRAME_HEADER_LEN as u32,
        );
        let records: Vec<_> = frame.records().map(|record| record.record()).collect();
        let mut encoded = Vec::with_capacity(data.len());
        inf_log::encode_batch_frame(frame_start, &records, &mut encoded)
            .expect("decoded frame must re-encode");
        assert_eq!(encoded.as_slice(), data, "decode->encode must be byte-exact");
    }
});
