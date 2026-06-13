//! SCAN cursor totality (M1 §5): arbitrary cursor bytes through the decoder
//! and arbitrary u64 cursors through the table's reverse-binary iteration
//! must never panic, and the cursor chain from 0 must terminate (the SCAN
//! guarantee's liveness half — proptest owns the coverage half).
#![no_main]

use inf_foundation::time::Nanos;
use inf_store::{CellStore, SetOptions, StoreConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Decoder totality: Redis `strtoull` shape, never panics.
    let parsed = inf_server::parse_cursor(data);

    // A small store seeded from the input so table sizes vary.
    let now = Nanos(1);
    let mut store = CellStore::new(StoreConfig::default());
    let keys = data.first().copied().unwrap_or(0) as usize % 64;
    for i in 0..keys {
        let key = format!("k:{i}");
        let _ = store.set(key.as_bytes(), b"v", SetOptions::default(), now);
    }

    // Arbitrary cursors (parsed, raw little-endian, and a high-bit pattern)
    // must be safe to resume from.
    let mut raw = [0u8; 8];
    for (slot, byte) in raw.iter_mut().zip(data.iter().rev()) {
        *slot = *byte;
    }
    for cursor in [parsed.unwrap_or(0), u64::from_le_bytes(raw), u64::MAX >> 1] {
        let _ = store.scan(cursor, 8, now, |_| {});
    }

    // Liveness: the chain from 0 terminates well within table bounds.
    let mut cursor = 0u64;
    for _ in 0..10_000 {
        cursor = store.scan(cursor, 8, now, |_| {});
        if cursor == 0 {
            return;
        }
    }
    panic!("SCAN cursor chain failed to terminate");
});
