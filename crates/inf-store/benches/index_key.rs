#![allow(
    clippy::disallowed_methods,
    reason = "bench target: the wall clock is the instrument, not cell code"
)]
//! M4.5-S02 budget bench (§4.1): typed index-key encode ≤ 30 ns typical
//! scalar. Steady-state sweeps over shuffled pre-generated corpora
//! (the `ordered` bench harness precedent), medians over ROUNDS; the
//! checksum accumulator keeps the encode from being optimized away.
//!
//! Rows: numeric encodes (i64, f64, the coerced i64→f64 arm), bool,
//! strings at 16 B / 64 B (fast path) and a NUL-bearing 16 B corpus
//! (escape slow path), plus `compare_i64_f64` (the VM's cross-numeric
//! compare — informational, no budget of its own).
//!
//! Run: `taskset -c 4 cargo bench -p inf-store --bench index_key`
//! Artifact: 3 replicates recorded under `.artifacts/m4.5/s02/`.

use std::hint::black_box;
use std::time::Instant;

use inf_store::{IndexKeyBuf, IndexKeyType, IndexScalar, compare_i64_f64, index_key_encode};

const VALUES: usize = 100_000;
const ROUNDS: usize = 15;

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

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN rounds"));
    xs[xs.len() / 2]
}

/// Median ns/op over ROUNDS sweeps of one corpus through `encode_one`.
fn sweep<T, F: FnMut(&T, &mut IndexKeyBuf) -> u64>(
    row: &str,
    corpus: &[T],
    mut encode_one: F,
) -> f64 {
    let mut buf = IndexKeyBuf::new();
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut checksum = 0u64;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for value in corpus {
            checksum = checksum.wrapping_add(encode_one(black_box(value), &mut buf));
        }
        rounds.push(started.elapsed().as_nanos() as f64 / corpus.len() as f64);
    }
    black_box(checksum);
    let ns = median(rounds);
    println!("row={row} n={VALUES} ns_per_op={ns:.2}");
    ns
}

fn key_checksum(buf: &IndexKeyBuf) -> u64 {
    let bytes = buf.as_bytes();
    bytes.len() as u64 + u64::from(bytes[0])
}

fn string_corpus(len: usize, with_nul: bool, seed: u64) -> Vec<String> {
    let mut rng = SplitMix(seed);
    (0..VALUES)
        .map(|_| {
            let mut s = String::with_capacity(len);
            for i in 0..len {
                if with_nul && i.is_multiple_of(5) {
                    s.push('\0');
                } else {
                    s.push((b'a' + (rng.next() % 26) as u8) as char);
                }
            }
            s
        })
        .collect()
}

fn main() {
    let mut rng = SplitMix(0xBEC0_0002);

    let i64_corpus: Vec<i64> = (0..VALUES).map(|_| rng.next() as i64).collect();
    sweep("encode_i64", &i64_corpus, |&v, buf| {
        index_key_encode(IndexKeyType::I64, IndexScalar::I64(v), buf).expect("i64 admits");
        key_checksum(buf)
    });

    let f64_corpus: Vec<f64> = (0..VALUES).map(|_| (rng.next() as i64 as f64) / 1024.0).collect();
    sweep("encode_f64", &f64_corpus, |&v, buf| {
        index_key_encode(IndexKeyType::F64, IndexScalar::F64(v), buf).expect("finite admits");
        key_checksum(buf)
    });

    // The coerced arm: exactly-representable i64 values into an f64
    // index (the truth-table fast path S04 pays on mixed documents).
    let small_i64: Vec<i64> = (0..VALUES).map(|_| (rng.next() % (1 << 50)) as i64).collect();
    sweep("encode_f64_from_i64", &small_i64, |&v, buf| {
        index_key_encode(IndexKeyType::F64, IndexScalar::I64(v), buf).expect("exact admits");
        key_checksum(buf)
    });

    let bool_corpus: Vec<bool> = (0..VALUES).map(|_| rng.next().is_multiple_of(2)).collect();
    sweep("encode_bool", &bool_corpus, |&v, buf| {
        index_key_encode(IndexKeyType::Bool, IndexScalar::Bool(v), buf).expect("bool admits");
        key_checksum(buf)
    });

    for (row, len, with_nul) in [
        ("encode_utf8_16", 16, false),
        ("encode_utf8_64", 64, false),
        ("encode_utf8_16_nul", 16, true),
    ] {
        let corpus = string_corpus(len, with_nul, 0xBEC0_0003 + len as u64);
        sweep(row, &corpus, |s, buf| {
            index_key_encode(IndexKeyType::Utf8, IndexScalar::Utf8(s), buf).expect("fits cap");
            key_checksum(buf)
        });
    }

    // Informational: the VM's exact cross-numeric compare.
    let pairs: Vec<(i64, f64)> =
        (0..VALUES).map(|_| (rng.next() as i64, (rng.next() as i64 as f64) / 1024.0)).collect();
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut checksum = 0i64;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for &(a, b) in &pairs {
            checksum += compare_i64_f64(black_box(a), black_box(b)) as i64;
        }
        rounds.push(started.elapsed().as_nanos() as f64 / pairs.len() as f64);
    }
    black_box(checksum);
    println!("row=compare_i64_f64 n={VALUES} ns_per_op={:.2}", median(rounds));
}
