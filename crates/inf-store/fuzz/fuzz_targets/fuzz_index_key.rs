//! Index-key encoding fuzz (M4.5-S02, ADR-0074 D7, L9). Two law sets:
//!
//! - **Decoder laws** (arbitrary bytes): `index_key_decode` never
//!   panics; every accepted byte string is canonical — re-encoding the
//!   decoded value reproduces the input byte-for-byte.
//! - **Order laws** (fuzz-chosen scalars): where the ADR-0074 truth
//!   table admits two values into one index type, encoded byte order
//!   equals typed order (`compare_i64_f64` across the numeric pair),
//!   and encode's admission verdict always equals `index_scalar_coerce`'s
//!   (one truth table, provably one).

#![no_main]

use std::cmp::Ordering;

use libfuzzer_sys::fuzz_target;

use inf_store::{
    DecodedIndexKey, IndexKeyBuf, IndexKeyType, IndexScalar, compare_i64_f64, index_key_decode,
    index_key_encode, index_scalar_coerce,
};

const KEY_TYPES: [IndexKeyType; 4] =
    [IndexKeyType::Utf8, IndexKeyType::I64, IndexKeyType::F64, IndexKeyType::Bool];

fn encode(key_type: IndexKeyType, value: IndexScalar<'_>) -> Option<Vec<u8>> {
    let mut buf = IndexKeyBuf::new();
    let admitted = index_key_encode(key_type, value, &mut buf).is_ok();
    // One truth table: encode admits exactly when coerce admits.
    assert_eq!(admitted, index_scalar_coerce(key_type, value).is_ok());
    admitted.then(|| buf.as_bytes().to_vec())
}

/// Arbitrary bytes into the debug decoder: total, and canonical-strict —
/// `Ok` implies re-encode reproduces the input exactly (ADR-0074 D7).
fn decoder_laws(key_type: IndexKeyType, bytes: &[u8]) {
    let Ok(decoded) = index_key_decode(key_type, bytes) else {
        return; // rejection is a fine outcome; panics are the bug
    };
    let string;
    let value = match &decoded {
        DecodedIndexKey::I64(v) => IndexScalar::I64(*v),
        DecodedIndexKey::F64(f) => IndexScalar::F64(*f),
        DecodedIndexKey::Bool(b) => IndexScalar::Bool(*b),
        DecodedIndexKey::Utf8(s) => {
            string = s.clone();
            IndexScalar::Utf8(&string)
        }
    };
    let reencoded = encode(key_type, value).expect("canonical decode re-admits");
    assert_eq!(reencoded, bytes, "decode is canonical-strict: {decoded:?}");
}

/// Same-type and cross-numeric order agreement for fuzz-chosen scalars.
fn order_laws(selector: u8, payload: &[u8]) {
    if payload.len() < 16 {
        return;
    }
    let (word_a, word_b) = payload.split_at(8);
    let word_a = u64::from_le_bytes(word_a[..8].try_into().expect("split_at(8)"));
    let word_b = u64::from_le_bytes(word_b[..8].try_into().expect("8 checked above"));
    match selector % 4 {
        0 => {
            let (a, b) = (word_a as i64, word_b as i64);
            let ka = encode(IndexKeyType::I64, IndexScalar::I64(a)).expect("i64 admits");
            let kb = encode(IndexKeyType::I64, IndexScalar::I64(b)).expect("i64 admits");
            assert_eq!(ka.cmp(&kb), a.cmp(&b));
        }
        1 => {
            let (a, b) = (f64::from_bits(word_a), f64::from_bits(word_b));
            let (ka, kb) = (
                encode(IndexKeyType::F64, IndexScalar::F64(a)),
                encode(IndexKeyType::F64, IndexScalar::F64(b)),
            );
            assert_eq!(ka.is_none(), a.is_nan(), "only NaN skips an f64 index");
            if let (Some(ka), Some(kb)) = (ka, kb) {
                assert_eq!(ka.cmp(&kb), a.partial_cmp(&b).expect("non-NaN"));
            }
        }
        2 => {
            // Cross-numeric: the ADR-0074 D5 consistency law under both
            // declared index types.
            let (a, b) = (word_a as i64, f64::from_bits(word_b));
            for key_type in [IndexKeyType::I64, IndexKeyType::F64] {
                let (ka, kb) = (
                    encode(key_type, IndexScalar::I64(a)),
                    encode(key_type, IndexScalar::F64(b)),
                );
                if let (Some(ka), Some(kb)) = (ka, kb) {
                    let verdict = compare_i64_f64(a, b);
                    assert_eq!(ka.cmp(&kb), verdict, "{key_type:?}: {a} vs {b}");
                    if verdict == Ordering::Equal {
                        assert_eq!(ka, kb, "equal numerics collide byte-identically");
                    }
                }
            }
        }
        _ => {
            // Strings from the remaining payload, split in two: order +
            // prefix safety (enc(s) starts with escape(p) ⟺ p prefix).
            let rest = &payload[16..];
            let cut = (word_a as usize) % (rest.len() + 1);
            let a = String::from_utf8_lossy(&rest[..cut]);
            let b = String::from_utf8_lossy(&rest[cut..]);
            let (ka, kb) = (
                encode(IndexKeyType::Utf8, IndexScalar::Utf8(&a)),
                encode(IndexKeyType::Utf8, IndexScalar::Utf8(&b)),
            );
            if let (Some(ka), Some(kb)) = (&ka, &kb) {
                assert_eq!(ka.cmp(kb), a.as_bytes().cmp(b.as_bytes()));
            }
            if let (Some(ka), Some(kab)) = (
                &ka,
                encode(IndexKeyType::Utf8, IndexScalar::Utf8(&format!("{a}{b}"))),
            ) {
                let escape_a = &ka[..ka.len() - 1];
                assert!(kab.starts_with(escape_a), "escape is prefix-safe (S09 begins_with)");
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode < 4 {
        decoder_laws(KEY_TYPES[mode as usize], rest);
    } else {
        order_laws(mode % 4, rest);
    }
});
