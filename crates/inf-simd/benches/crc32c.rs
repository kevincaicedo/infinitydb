//! M2-S01 CRC32C A/B harness.
//!
//! Binding evidence must be collected on the Linux reference box. This bench
//! exists so that run uses the same scalar-vs-dispatch workload every time.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const SIZES: [usize; 3] = [4 * 1024, 64 * 1024, 1024 * 1024];

fn bench_crc32c(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    for size in SIZES {
        let data = data_for_size(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("scalar", size), data.as_slice(), |b, input| {
            b.iter(|| black_box(inf_simd::scalar_crc32c(black_box(input))));
        });
        group.bench_with_input(BenchmarkId::new("dispatch", size), data.as_slice(), |b, input| {
            b.iter(|| black_box(inf_simd::crc32c(black_box(input))));
        });
    }
    group.finish();
}

fn data_for_size(size: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ size as u64;
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(size);
    out
}

criterion_group!(benches, bench_crc32c);
criterion_main!(benches);
