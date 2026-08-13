//! M4.5-S02 storm ACs (deterministic, the S01 `ordered_storm` shape):
//! 10⁶ order-preservation pairs and 10⁶ cross-type coercion cases
//! against the ADR-0074 truth table, with the special-value corpus
//! salted in (NaN, ±∞, ±0.0, the 2⁵³/2⁶³ boundaries, NUL-bearing and
//! cap-adjacent strings). A closing integration sweep proves the
//! composition with S01: encoded-key order through an [`OrderedMap`]
//! cursor equals typed order.

use std::cmp::Ordering;

use inf_store::{
    DecodedIndexKey, Fixed8, IndexKeyBuf, IndexKeyType, IndexScalar, KeySkip, OrderedCursor,
    OrderedMap, VarKey, compare_i64_f64, index_key_decode, index_key_encode, index_scalar_coerce,
};

const PAIRS: usize = 1_000_000;

/// splitmix64 — the storm's only randomness source (L7: fixed seeds,
/// reproducible failures; print the seed on assert via the pair index).
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A salted i64: uniform words mixed with the boundary corpus.
fn gen_i64(rng: &mut SplitMix) -> i64 {
    const P53: i64 = 1 << 53;
    match rng.next() % 8 {
        0 => [i64::MIN, i64::MAX, 0, -1, 1][(rng.next() % 5) as usize],
        1 => P53 + (rng.next() % 8) as i64 - 4,
        2 => (1 << 60) + (rng.next() % 8) as i64 - 4,
        3 => (rng.next() % 2048) as i64 - 1024,
        _ => rng.next() as i64,
    }
}

/// A salted f64: finite doubles across magnitudes plus the specials.
/// NaN is deliberately included — the table must skip it, never panic.
fn gen_f64(rng: &mut SplitMix) -> f64 {
    match rng.next() % 8 {
        0 => [0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN][(rng.next() % 5) as usize],
        1 => gen_i64(rng) as f64,
        2 => (gen_i64(rng) as f64) + 0.5,
        3 => f64::from_bits(rng.next()), // arbitrary bit patterns (often NaN/huge)
        _ => (rng.next() as i64 as f64) / 1024.0,
    }
}

/// A salted string: byte soup with NULs and 0xFF, occasionally
/// cap-adjacent lengths (the D3 boundary).
fn gen_string(rng: &mut SplitMix) -> String {
    let len = match rng.next() % 16 {
        0 => 0,
        1 => 500 + (rng.next() % 30) as usize,
        _ => (rng.next() % 48) as usize,
    };
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        let c = match rng.next() % 8 {
            0 => '\0',
            1 => 'ÿ',        // 0xC3 0xBF in UTF-8: exercises high bytes
            2 => '\u{2603}', // three UTF-8 bytes
            _ => (b'a' + (rng.next() % 26) as u8) as char,
        };
        s.push(c);
    }
    s
}

fn encode(key_type: IndexKeyType, value: IndexScalar<'_>) -> Result<Vec<u8>, KeySkip> {
    let mut buf = IndexKeyBuf::new();
    index_key_encode(key_type, value, &mut buf).map(|()| buf.as_bytes().to_vec())
}

/// Typed order for admitted f64 values (never NaN once admitted).
fn f64_order(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).expect("admitted values are never NaN")
}

/// AC 1 — order preservation: 10⁶ same-type pairs, byte order ≡ typed
/// order for every type; skips agree between coerce and encode.
#[test]
fn storm_order_preservation_one_million_pairs() {
    let mut rng = SplitMix(0x5EED_0201);
    for pair in 0..PAIRS {
        match pair % 4 {
            0 => {
                let (a, b) = (gen_i64(&mut rng), gen_i64(&mut rng));
                let ka = encode(IndexKeyType::I64, IndexScalar::I64(a)).expect("i64 admits");
                let kb = encode(IndexKeyType::I64, IndexScalar::I64(b)).expect("i64 admits");
                assert_eq!(ka.cmp(&kb), a.cmp(&b), "pair {pair}: {a} vs {b}");
            }
            1 => {
                let (a, b) = (gen_f64(&mut rng), gen_f64(&mut rng));
                let (ka, kb) = (
                    encode(IndexKeyType::F64, IndexScalar::F64(a)),
                    encode(IndexKeyType::F64, IndexScalar::F64(b)),
                );
                assert_eq!(ka.is_err(), a.is_nan(), "pair {pair}: only NaN skips");
                assert_eq!(kb.is_err(), b.is_nan(), "pair {pair}: only NaN skips");
                if let (Ok(ka), Ok(kb)) = (ka, kb) {
                    assert_eq!(ka.cmp(&kb), f64_order(a, b), "pair {pair}: {a} vs {b}");
                }
            }
            2 => {
                let (a, b) = (gen_string(&mut rng), gen_string(&mut rng));
                let (ka, kb) = (
                    encode(IndexKeyType::Utf8, IndexScalar::Utf8(&a)),
                    encode(IndexKeyType::Utf8, IndexScalar::Utf8(&b)),
                );
                if let (Ok(ka), Ok(kb)) = (&ka, &kb) {
                    assert_eq!(
                        ka.cmp(kb),
                        a.as_bytes().cmp(b.as_bytes()),
                        "pair {pair}: {a:?} vs {b:?}"
                    );
                }
                // The only string skip is the D3 cap.
                for (key, value) in [(&ka, &a), (&kb, &b)] {
                    if let Err(skip) = key {
                        assert_eq!(*skip, KeySkip::TooLong, "pair {pair}: {value:?}");
                    }
                }
            }
            _ => {
                let (a, b) = (rng.next().is_multiple_of(2), rng.next().is_multiple_of(2));
                let ka = encode(IndexKeyType::Bool, IndexScalar::Bool(a)).expect("bool admits");
                let kb = encode(IndexKeyType::Bool, IndexScalar::Bool(b)).expect("bool admits");
                assert_eq!(ka.cmp(&kb), a.cmp(&b), "pair {pair}: {a} vs {b}");
            }
        }
    }
}

/// AC 2 — cross-type coercion: 10⁶ mixed i64/f64 cases — membership and
/// VM comparison verdicts agree with the shared table and each other,
/// and coerced twins collide byte-identically.
#[test]
fn storm_cross_type_coercion_one_million_cases() {
    let mut rng = SplitMix(0x5EED_0202);
    for case in 0..PAIRS {
        let a = gen_i64(&mut rng);
        let b = gen_f64(&mut rng);
        for key_type in [IndexKeyType::I64, IndexKeyType::F64] {
            // Membership: encode outcome ≡ coerce outcome (one table).
            let coerce_a = index_scalar_coerce(key_type, IndexScalar::I64(a));
            let coerce_b = index_scalar_coerce(key_type, IndexScalar::F64(b));
            let key_a = encode(key_type, IndexScalar::I64(a));
            let key_b = encode(key_type, IndexScalar::F64(b));
            assert_eq!(coerce_a.err(), key_a.as_ref().err().copied(), "case {case} {key_type:?}");
            assert_eq!(coerce_b.err(), key_b.as_ref().err().copied(), "case {case} {key_type:?}");
            // Agreement: both admitted ⇒ byte order ≡ exact VM compare.
            if let (Ok(key_a), Ok(key_b)) = (key_a, key_b) {
                assert!(!b.is_nan(), "case {case}: NaN can never admit");
                assert_eq!(
                    key_a.cmp(&key_b),
                    compare_i64_f64(a, b),
                    "case {case} {key_type:?}: {a} vs {b}"
                );
                // Equal verdict ⇒ the twins collide byte-identically
                // (the §3.1 `10` ≡ `10.0` rule, storm-scale).
                if compare_i64_f64(a, b) == Ordering::Equal {
                    assert_eq!(key_a, key_b, "case {case} {key_type:?}: {a} vs {b}");
                }
            }
        }
    }
}

/// Round-trip law at storm scale: decode ∘ encode is the identity on
/// the coerced value, for every type (the fuzz target's first law).
#[test]
fn storm_decode_round_trip() {
    let mut rng = SplitMix(0x5EED_0203);
    for case in 0..100_000usize {
        let key_type = match case % 4 {
            0 => IndexKeyType::I64,
            1 => IndexKeyType::F64,
            2 => IndexKeyType::Utf8,
            _ => IndexKeyType::Bool,
        };
        let string = gen_string(&mut rng);
        let value = match key_type {
            IndexKeyType::I64 => IndexScalar::I64(gen_i64(&mut rng)),
            IndexKeyType::F64 => IndexScalar::F64(gen_f64(&mut rng)),
            IndexKeyType::Utf8 => IndexScalar::Utf8(&string),
            IndexKeyType::Bool => IndexScalar::Bool(rng.next().is_multiple_of(2)),
        };
        let Ok(bytes) = encode(key_type, value) else { continue };
        let decoded = index_key_decode(key_type, &bytes).expect("canonical bytes decode");
        let admitted = index_scalar_coerce(key_type, value).expect("encoded implies admitted");
        match (decoded, admitted) {
            (DecodedIndexKey::I64(d), IndexScalar::I64(v)) => assert_eq!(d, v, "case {case}"),
            (DecodedIndexKey::Bool(d), IndexScalar::Bool(v)) => assert_eq!(d, v, "case {case}"),
            (DecodedIndexKey::Utf8(d), IndexScalar::Utf8(v)) => assert_eq!(d, v, "case {case}"),
            (DecodedIndexKey::F64(d), IndexScalar::F64(v)) => {
                // -0.0 canonicalizes to +0.0; otherwise bit-exact.
                let canonical = if v == 0.0 { 0.0 } else { v };
                assert_eq!(d.to_bits(), canonical.to_bits(), "case {case}");
            }
            (decoded, admitted) => panic!("case {case}: type drift {decoded:?} vs {admitted:?}"),
        }
    }
}

/// Composition with S01: encoded keys inserted into the real trees come
/// back in typed order through the cursor (Fixed8 for f64, VarKey for
/// strings) — the property every S09 range scan stands on.
#[test]
fn storm_tree_iteration_matches_typed_order() {
    let mut rng = SplitMix(0x5EED_0204);
    // f64 through the Fixed8 tree.
    let mut fixed: OrderedMap<Fixed8, 32> = OrderedMap::new();
    let mut admitted_f64: Vec<f64> = Vec::new();
    for i in 0..10_000u64 {
        let value = gen_f64(&mut rng);
        if let Ok(bytes) = encode(IndexKeyType::F64, IndexScalar::F64(value))
            && fixed.insert(&bytes, i).expect("capacity")
        {
            admitted_f64.push(value);
        }
    }
    admitted_f64.sort_by(|a, b| f64_order(*a, *b));
    let mut cursor = OrderedCursor::from_start();
    let mut walked = 0usize;
    let mut previous: Option<Vec<u8>> = None;
    while let Some((key, _)) = cursor.next(&fixed) {
        let DecodedIndexKey::F64(decoded) =
            index_key_decode(IndexKeyType::F64, key).expect("tree keys decode")
        else {
            panic!("f64 tree yielded a non-f64 key")
        };
        // Duplicates (e.g. -0.0/0.0, coerced twins) collapse to one
        // key with many refs — compare against the sorted multiset by
        // walking forward past equal values.
        assert_eq!(f64_order(decoded, admitted_f64[walked]), Ordering::Equal, "slot {walked}");
        if let Some(previous) = &previous {
            assert!(previous.as_slice() <= key, "cursor must walk ascending bytes");
        }
        previous = Some(key.to_vec());
        walked += 1;
    }
    assert_eq!(walked, admitted_f64.len(), "every admitted pair walks");

    // Strings through the VarKey tree.
    let mut var: OrderedMap<VarKey, 32> = OrderedMap::new();
    let mut admitted_strings: Vec<String> = Vec::new();
    for i in 0..10_000u64 {
        let value = gen_string(&mut rng);
        if let Ok(bytes) = encode(IndexKeyType::Utf8, IndexScalar::Utf8(&value))
            && var.insert(&bytes, i).expect("capacity")
        {
            admitted_strings.push(value);
        }
    }
    admitted_strings.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut cursor = OrderedCursor::from_start();
    let mut walked = 0usize;
    while let Some((key, _)) = cursor.next(&var) {
        let DecodedIndexKey::Utf8(decoded) =
            index_key_decode(IndexKeyType::Utf8, key).expect("tree keys decode")
        else {
            panic!("utf8 tree yielded a non-utf8 key")
        };
        assert_eq!(decoded, admitted_strings[walked], "slot {walked}");
        walked += 1;
    }
    assert_eq!(walked, admitted_strings.len(), "every admitted pair walks");
}
