//! CRC32C kernel A/B: dispatched (hardware where available) vs the
//! slicing-by-8 software fallback (M2-S01 AC, L4). Criterion reports
//! bytes/s; the gate value (≥ 10 GB/s) binds on the reference box only —
//! dev-tier runs are correctness/trend evidence (L10).

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use inf_simd::{crc32c, scalar_crc32c_update};
use std::hint::black_box;

/// Deterministic pseudo-random payload (SplitMix64) — no `rand` dependency,
/// same bytes every run.
fn payload(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn bench_crc32c(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    // 64 B ≈ one small record; 4 KiB ≈ small frame; 64 KiB / 1 MiB ≈ big
    // frames and checkpoint sections (throughput regime — the gate's shape).
    for len in [64usize, 4 << 10, 64 << 10, 1 << 20] {
        let data = payload(len);
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::new("dispatched", len), &data, |b, data| {
            b.iter(|| crc32c(black_box(data)));
        });
        group.bench_with_input(BenchmarkId::new("software", len), &data, |b, data| {
            b.iter(|| scalar_crc32c_update(0, black_box(data)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_crc32c);
criterion_main!(benches);
