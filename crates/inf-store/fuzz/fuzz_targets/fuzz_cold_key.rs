//! Cold-key window fuzz (ADR-0174 D3: a boot settle keeps or removes a
//! slot only on a record whose key hashes to the slot's hash; L9): the
//! settle read's `parse` step. `ColdKey::from_window` is the one constructor a
//! boot settle answers from, so it is total over arbitrary bytes and
//! answers `Ok` exactly when the five checks, recomputed here from the
//! record format alone, hold: the window holds the fixed header, the
//! type tag is one of the three bound tags, the key is whole inside the
//! window, the record's encoded length lies inside `left`, and the key
//! hashes to the slot's hash under the namespace's keyed hash.
//!
//! The input's last 16 bytes are the slot's hash and `left`; a quarter
//! of the inputs take the key's true hash as the slot's, so the `Ok` arm
//! is reached and its fields are checked against the recomputation.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_store::{ColdKey, ColdKeyError, KeyHasher, TypeTag};

const HEADER_LEN: usize = 8;
const TTL_EXT_LEN: usize = 5;
const FLAG_TTL: u8 = 0b0001;

/// The checks, recomputed from the format: `Some((key, record_len,
/// kind))` when the window parses structurally, before the length and
/// hash checks.
fn structural(window: &[u8]) -> Result<(&[u8], u64, TypeTag), ColdKeyError> {
    if window.len() < HEADER_LEN {
        return Err(ColdKeyError::ShortHeader { len: window.len() });
    }
    let bits = window[0] >> 4;
    let kind = match bits {
        1 => TypeTag::String,
        2 => TypeTag::JsonDoc,
        3 => TypeTag::StringExtent,
        _ => return Err(ColdKeyError::TypeTag { bits }),
    };
    let klen = usize::from(window[1]);
    let key_at = HEADER_LEN + if window[0] & FLAG_TTL != 0 { TTL_EXT_LEN } else { 0 };
    let Some(key) = window.get(key_at..key_at + klen) else {
        return Err(ColdKeyError::KeyTruncated { len: window.len() });
    };
    let vlen = u64::from(window[2]) | (u64::from(window[3]) << 8) | (u64::from(window[4]) << 16);
    let record_len = key_at as u64 + klen as u64 + vlen;
    Ok((key, record_len, kind))
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }
    let (window, trailer) = data.split_at(data.len() - 16);
    let mut slot_hash = u64::from_le_bytes(trailer[..8].try_into().expect("8 bytes"));
    let left = u64::from_le_bytes(trailer[8..].try_into().expect("8 bytes"));
    let hasher = KeyHasher::from_seed(0x5E77_1E_C01D);
    let expected = structural(window).and_then(|(key, record_len, kind)| {
        if record_len > left {
            return Err(ColdKeyError::LengthPastFile { record_len, left });
        }
        let key_hash = hasher.hash(key);
        if trailer[0] & 0b11 == 0 {
            // The reachable `Ok` arm: the slot's hash is the key's.
            slot_hash = key_hash;
        }
        if key_hash != slot_hash {
            return Err(ColdKeyError::HashMismatch { slot: slot_hash, key: key_hash });
        }
        Ok((key, record_len, kind))
    });
    let got = ColdKey::from_window(window, left, slot_hash, |key| hasher.hash(key));
    match (expected, got) {
        (Ok((key, record_len, kind)), Ok(cold)) => {
            assert_eq!(cold.key(), key, "the verified key");
            assert_eq!(u64::from(cold.record_len()), record_len, "the exact length");
            assert_eq!(cold.kind(), kind, "the type");
            assert_eq!(cold.hash(), slot_hash, "the slot's hash");
        }
        (Err(want), Err(got)) => assert_eq!(got, want, "the refusal names the first failed check"),
        (want, got) => panic!("recomputed {want:?}, the constructor answered {got:?}"),
    }
});
