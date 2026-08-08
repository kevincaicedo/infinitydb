# CRC32C kernel A/B — dispatched (SSE4.2) vs software slicing-by-8

- **Story:** M2-S01 (AC: kernel ≥ 10 GB/s on the reference box, A/B vs software fallback, L4)
- **Date:** 2026-07-02 · **Tier:** dev box (Linux, governor not pinned for this run) — **indicative only, non-citable (L10)**; the gate value binds on the reference box (M0-R2)
- **Command:** `cargo bench -p inf-simd --bench crc32c` (criterion raw output: `criterion-output.txt`; detailed estimates under `target/criterion/crc32c/`)
- **Tree:** M2-E1 working tree (S01/S02), inf-simd `crc32c.rs` @ SSE4.2 serial `crc32q` path

| Input | Dispatched (SSE4.2) | Software (slicing-by-8) | Speedup |
|-------|--------------------:|------------------------:|--------:|
| 64 B | 22.47 GiB/s | 3.16 GiB/s | 7.1× |
| 4 KiB | 10.29 GiB/s | 1.58 GiB/s | 6.5× |
| 64 KiB | 8.50 GiB/s | 1.57 GiB/s | 5.4× |
| 1 MiB | 8.23 GiB/s | 1.57 GiB/s | 5.3× |

## Reading

- The hardware path wins 5–7× at every size — the fallback exists for the
  sim tier and as the proptest oracle, not as a viable production path.
- Large-buffer throughput plateaus ≈ 8.2–8.5 GiB/s: the serial `crc32q`
  dependency chain (3-cycle latency, 8 B/instruction) is the ceiling, as
  expected. Frame-sized inputs (≤ 4 KiB) exceed 10 GiB/s.
- **Disposition: Evidence-pending** for the ≥ 10 GB/s AC (reference box,
  pinned governor). If the reference box misses at large sizes, the
  recorded remedy is 3-way stream interleaving + CRC-combine (ADR-0011),
  to ship only with its own A/B.
