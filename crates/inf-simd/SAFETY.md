# inf-simd SAFETY

`inf-simd` is one of the four crates allowed `unsafe` (milestone M0 §3.3).
All unsafe code is platform intrinsics in `crlf.rs` and `group16.rs`;
`swar.rs` is fully safe (64-bit integer tricks only).

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
