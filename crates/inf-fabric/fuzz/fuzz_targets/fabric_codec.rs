//! Fabric codec decode totality (M0-S10): arbitrary bytes must decode or
//! fail with a typed `CodecError` — never panic, never UB. Frames that do
//! decode must re-encode byte-exact (round-trip canonicality).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(op) = inf_fabric::decode(data) {
        let mut reencoded = Vec::with_capacity(data.len());
        inf_fabric::encode(&op, &mut reencoded);
        assert_eq!(reencoded.as_slice(), data, "decode→encode must be byte-exact");
    }
});
