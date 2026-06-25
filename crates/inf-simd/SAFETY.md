# inf-simd SAFETY

`inf-simd` is one of the four crates allowed `unsafe` (milestone M0 §3.3).
All unsafe code is platform intrinsics in `crlf.rs`, `group16.rs`, and
`crc32c.rs`; `swar.rs` is fully safe (64-bit integer tricks only).

## `crc32c.rs` — log-frame checksum acceleration (M2-S01)

- **Feature availability**: x86-64 calls the SSE4.2 `crc32` intrinsics only
  after `is_x86_feature_detected!("sse4.2")`; aarch64 calls the CRC extension
  intrinsics only after `is_aarch64_feature_detected!("crc")`.
- **Bounds**: the hardware paths read from a borrowed slice using explicit
  `offset + 8 <= data.len()` and `offset < data.len()` loop guards before
  constructing little-endian words or loading tail bytes.
- **Fallback**: unsupported CPUs use the safe scalar CRC32C table. The scalar
  path is public as `scalar_crc32c` so benchmarks and tests have a fixed oracle.
- **Incremental state**: `Crc32c` carries only the unfinalized `u32` CRC state;
  hardware and scalar updates consume borrowed byte slices under the same bounds
  checks as the one-shot path, and `finish()` performs the final xor exactly
  once.
- **Verification**: fixed CRC32C reference vectors and proptests compare the
  runtime-dispatched path to the scalar oracle on arbitrary byte strings; split
  proptests verify incremental updates are byte-equivalent to one-shot CRCs.

## `crlf.rs` — SIMD loads and feature-gated paths

- **Bounds**: every unaligned vector load (`_mm_loadu_si128`,
  `_mm256_loadu_si256`, `vld1q_u8`) is guarded by
  `offset + CHUNK <= buf.len()` in the enclosing loop condition; the
  remainder is handled by the scalar tail.
- **Feature availability**: SSE2 and NEON are baseline on x86-64/aarch64
  respectively. The AVX2 path is reachable only behind cached
  `is_x86_feature_detected!("avx2")` runtime dispatch and is annotated
  `#[target_feature(enable = "avx2")]`.
- **No aliasing games**: intrinsics read the input slice only; results land
  in plain Rust values.

## `group16.rs` — Swiss-table group probes (M0-S14)

- **Bounds**: all vector loads (`_mm_loadu_si128`, `vld1q_u8`) read exactly
  16 bytes from a borrowed `&[u8; 16]` — the type carries the bound; no
  loop arithmetic involved.
- **Feature availability**: SSE2/NEON baseline only (no AVX2 path yet —
  the 32-way probe is an A/B-measured follow-up).
- **`prefetch_read`**: `_mm_prefetch` is a pure hint and cannot fault on
  any pointer value (the safe wrapper is therefore sound for arbitrary
  pointers); the aarch64 body is a no-op (intrinsics unstable).

## Verification

Every SIMD path is property-tested against its scalar oracle
(`scalar_scan_crlf`, `scalar_eq_mask16`, `scalar_high_bit_mask16`) on
arbitrary inputs (1000 cases per run, plus fixed chunk-boundary/edge corpora
ported from `vortex-proto`). The aarch64 NEON paths (new in this port —
Vortex used nightly `std::simd`) are covered by the same equivalence suites;
the x86 SSE2 paths were runtime-verified on Linux on 2026-06-11.
