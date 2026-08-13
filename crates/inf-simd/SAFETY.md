# inf-simd SAFETY

`inf-simd` is one of the four crates allowed `unsafe` (milestone M0 §3.3).
All unsafe code is platform intrinsics in `crlf.rs`, `group16.rs`, and
`crc32c.rs`; `swar.rs` is fully safe (64-bit integer tricks only).

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

## `json.rs` — JSON stage-1 structural scan (M3-S05)

- **Bounds**: the AVX2 path's two unaligned 32-byte loads per block are
  guarded by the enclosing `offset + 64 <= input.len()` loop bound; the
  SSE2 quarter load reads exactly the 16 bytes of a `chunks_exact(16)`
  slice (length debug-asserted); the tail is copied into a stack
  `[u8; 64]` padded with spaces and classified bytewise (no vector load).
- **Feature availability**: SSE2/NEON are baseline; the AVX2 path is
  reachable only behind cached `is_x86_feature_detected!("avx2")` (the
  crlf.rs AtomicU8 pattern) and is annotated `#[target_feature]`.
- **No aliasing games**: intrinsics read the input slice only; all
  results land in plain `u64` masks; the escape/string-span arithmetic is
  fully safe integer code shared by every tier.
- **Oracle**: `scalar_json_scan_structurals` is an independent per-byte
  state machine (not the bit tricks); the equivalence proptests drive all
  tiers (dispatched, forced-SSE2, scalar) over JSON-ish and arbitrary
  bytes and require identical output — including deliberately invalid
  inputs, so the context-free escape semantics match exactly.

## `json.rs` — fused string-content copies + index writes (ADR-0047 K1/K2/K3)

- **Bounds (loads)**: full-block loads are guarded by `i + 32 <= len`;
  the final block loads `len - 32 .. len`, in bounds because the
  dispatcher sends the AVX2 tier only `src.len() >= 32` (debug-asserted
  again in the tier).
- **Bounds (stores + `set_len`)**: `out.reserve(len)` runs before any
  raw write; every `_mm256_storeu_si256` lands inside
  `[out.len(), out.len() + len)` of that reservation. `set_len(base +
  len)` executes only on the no-special path, where the full-block
  stores plus the backward-overlapped tail store have initialized
  exactly those `len` bytes. On the `Some` return, `set_len` never runs
  — the partially written bytes remain spare capacity and are never
  exposed.
- **Overlap argument**: the final block re-covers `len - 32 .. i`; those
  bytes were already classified clean by earlier full blocks (same bytes,
  same predicate), so the first set mask bit is at or past `i`
  (debug-asserted). Re-storing the clean prefix rewrites identical bytes.
- **Feature availability**: AVX2 behind the cached
  `is_x86_feature_detected!` AtomicU8 pattern, `#[target_feature]`
  annotated; the portability tier (`scalar_json_copy_unescaped`) is
  fully safe SWAR and is also the Miri path (`cfg(not(miri))` on the
  dispatch arm).
- **Oracle**: an independent per-byte `position` scan; the boundary
  sweep plants every special at every position across word/block
  boundaries, and the proptests drive arbitrary + string-ish bytes
  through every tier requiring verdict *and* appended-byte equality.
- **K2 `json_copy_unescaped_short`** (fixstr companion, `1 <= len <= 31`):
  the single 32-byte load is guarded by the entry `assert!(window.len()
  >= 32)`; the special mask is ANDed with `(1 << len) - 1`, so bytes past
  the live length can never veto; the 32-byte store goes into
  `reserve(32)` spare capacity and `set_len(base + len)` exposes only the
  `len` initialized bytes. Exhaustive sweep test: every length × every
  special position inside and outside the live window, both tiers.
- **K2b `json_copy_unescaped_fixstr`** (header-fused K2, stage-fusion
  slice): identical load/mask discipline to K2; `reserve(33)` covers the
  one header byte plus the 32-byte store, and `set_len(base + 1 + len)`
  exposes the header plus the `len` initialized payload bytes. The
  `false` return runs before any store bookkeeping, leaving `out`
  untouched. Covered by the same exhaustive sweep (header byte + payload
  equality asserted per tier).
- **K3 `flush_block` unchecked index writes**: `reserve(64)` precedes the
  bit-loop (a 64-bit emit mask can set at most 64 bits), every write
  lands at `out.len() + k`, `k < 64`, and `set_len` exposes exactly the
  written prefix. The existing tier-equivalence proptests (dispatched,
  forced-SSE2, scalar oracle) cover it on arbitrary and JSON-ish bytes.
- **Stage fusion (`avx2_classify_blocks`, slice 2)**: the per-block
  64-byte reads follow the same `offset + 64 <= len` bound as
  `avx2_scan` (padded stack tail identical); mask stores go through a
  raw cursor over one `reserve(div_ceil(len, 64))` reservation with
  `set_len` exposing exactly the blocks written (a `Vec::push` loop
  variant was repaired out after LLVM SLP-vectorized it into a
  `vpinsrb` storm — see the stage-fusion artifact).

## `utf8.rs` — UTF-8 validation kernel (M3-S05 slice 3)

- **Bounds**: the AVX2 path's one unaligned 32-byte load per block reads a
  `chunks_exact(32)` slice (length guaranteed); the trailing partial block
  is copied into a zero-padded stack `[u8; 32]` and loaded from there —
  no vector load ever touches past the input.
- **Feature availability**: the AVX2 path is reachable only behind cached
  `is_x86_feature_detected!("avx2")` (the crlf.rs AtomicU8 pattern); every
  helper is `#[target_feature(enable = "avx2")] unsafe fn`. There is no
  SSE2/NEON tier — the fallback is `std::str::from_utf8` itself.
- **No aliasing games**: intrinsics read the input slice only; all state
  lives in `__m256i` locals.
- **Verdict-only contract**: the kernel returns a boolean; callers derive
  error offsets by re-running std on the reject path and defer to std's
  verdict there, so a kernel false-negative cannot produce a wrong answer.
  The false-accept direction is property-tested (below) and cross-checked
  by the `json_parse` fuzz differential (serde_json validates UTF-8).
- **Oracle**: `std::str::from_utf8` — the equivalence proptests drive
  arbitrary bytes and boundary-mutated valid text of every width class,
  plus a fixed corpus of every error class (overlongs, surrogates,
  truncations, > U+10FFFF, stray continuations) shifted across the 32-byte
  block boundary and to end-of-input.

## `crc32c.rs` — CRC32C kernel (M2-S01)

- **No memory unsafety at all**: both hardware paths feed the CRC
  instructions (`_mm_crc32_u64`/`_mm_crc32_u8`, `__crc32cd`/`__crc32cb`)
  *values* produced by safe `chunks_exact(8)` + `from_le_bytes` — there are
  no pointer loads. The only obligation is the `target_feature` contract.
- **Feature availability**: the SSE4.2 path is reachable only behind cached
  `is_x86_feature_detected!("sse4.2")` (same AtomicU8 pattern as `crlf.rs`);
  the aarch64 path only behind `is_aarch64_feature_detected!("crc")`.
- **Fallback**: slicing-by-8 with const-built tables, fully safe; it is the
  sim/dev tier and the proptest oracle.

## `lower_bound.rs` — sorted-prefix lower bound (M4.5-S01)

- **No unsafe code.** The explicit AVX2/SSE4.2 kernel (sign-flip
  `pcmpgtq` + movemask popcount behind the `crlf.rs` dispatch pattern)
  was built, measured, and **rejected by the S01 A/B**
  (`.artifacts/m4.5/s01/` — it lost every probe row to the plain
  count-loop, which LLVM auto-vectorizes and inlines while
  `#[target_feature]` blocks inlining). Per the M0-S14 rule the losing
  kernel is recorded, not merged: the module ships one safe branchless
  loop, proptested against `slice::partition_point`. Any future
  explicit kernel (AVX-512, reference box) re-enters through a new A/B
  and re-adds its inventory entry here.

## Verification

Every SIMD path is property-tested against its scalar oracle
(`scalar_scan_crlf`, `scalar_eq_mask16`, `scalar_high_bit_mask16`,
`scalar_crc32c_update`, `scalar_lower_bound_u64`) on arbitrary inputs (1000 cases per run, plus fixed
chunk-boundary/edge corpora ported from `vortex-proto`; CRC32C additionally
pins the RFC 3720 iSCSI vectors). The aarch64 NEON paths (new in this port —
Vortex used nightly `std::simd`) are covered by the same equivalence suites;
the x86 SSE2 paths were runtime-verified on Linux on 2026-06-11 and the
SSE4.2 CRC path on 2026-07-02. The CRC32C hardware/software agreement is
also asserted continuously by the `frame_decode` fuzz target (inf-log).
