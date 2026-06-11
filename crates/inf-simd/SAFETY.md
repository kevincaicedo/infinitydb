# inf-simd SAFETY

`inf-simd` is one of the four crates allowed `unsafe` (milestone M0 §3.3).
All unsafe code is platform intrinsics in `crlf.rs`; `swar.rs` is fully safe
(64-bit integer tricks only).

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

## Verification

Every SIMD path is property-tested against the scalar oracle
(`scalar_scan_crlf`) on arbitrary inputs (1000 cases per run, plus fixed
chunk-boundary/edge corpora ported from `vortex-proto`). The aarch64 NEON
path (new in this port — Vortex used nightly `std::simd`) is covered by the
same equivalence suite.
