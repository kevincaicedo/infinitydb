//! Review harness: the `INCR` family reaches the record writer without the
//! key-length validation every other write command performs.
//!
//! Goal and method: `RecordSpec::write` documents "Panics on key/value/expiry
//! bounds violations — the command layer validates inputs before reaching the
//! record writer" (`inf-store/src/record.rs:181-184`). `CellStore::set` honors
//! that contract and returns `OpError`; `CellStore::incr_by` and
//! `incr_by_float` do not. These tests drive both with a 256-byte key — one
//! byte past the u8 key-length field — and assert the store returns a typed
//! error rather than panicking. Today the `incr` cases panic, and in the
//! server that panic is a whole-node fail-stop.

use inf_foundation::time::Nanos;
use inf_store::{CellStore, SetOptions, StoreConfig};

fn now() -> Nanos {
    Nanos(1_000_000)
}

/// One byte past `MAX_KEY_LEN` (the record header's u8 key-length field).
fn over_long_key() -> Vec<u8> {
    vec![b'k'; 256]
}

#[test]
fn rv_f_l05_01_set_rejects_an_over_long_key_without_panicking() {
    // The control: `SET` honors the record writer's contract today.
    let mut store = CellStore::new(StoreConfig::default());
    let result = store.set(&over_long_key(), b"v", SetOptions::default(), now());
    assert!(result.is_err(), "SET must reject a 256-byte key with a typed error");
}

#[test]
fn rv_f_l05_01_incr_rejects_an_over_long_key_without_panicking() {
    let mut store = CellStore::new(StoreConfig::default());
    let result = store.incr_by(&over_long_key(), 1, now());
    assert!(
        result.is_err(),
        "INCR on a 256-byte key must return a typed error, not panic the cell"
    );
}

#[test]
fn rv_f_l05_01_incrbyfloat_rejects_an_over_long_key_without_panicking() {
    let mut store = CellStore::new(StoreConfig::default());
    let result = store.incr_by_float(&over_long_key(), 1.5, now());
    assert!(
        result.is_err(),
        "INCRBYFLOAT on a 256-byte key must return a typed error, not panic the cell"
    );
}

#[test]
fn rv_f_l05_01_the_boundary_is_exactly_255_bytes() {
    // 255 bytes must work, so the fix is a bound check and not a smaller cap.
    let mut store = CellStore::new(StoreConfig::default());
    let key = vec![b'k'; 255];
    assert_eq!(store.incr_by(&key, 1, now()).expect("255-byte key is legal"), 1);
}
