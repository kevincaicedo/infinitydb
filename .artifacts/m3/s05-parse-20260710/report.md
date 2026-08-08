# M3-S05 — JSON parse throughput + SIMD A/B + gate-shape projection (dev-tier, 2026-07-10)

- Box: HomeLab i7-13700KF, kernel 7.0.0-27-generic, governor=performance,
  `no_turbo=1` (binding env, 3.4 GHz P-core base). Pinned `taskset -c 4`;
  the concurrent `json_parse` 1 h fuzz ran on E-cores 16–23 (disclosed;
  criterion replicate spread stayed < 1%).
- Tree: post-S03/S04 + the S05 working tree (the change under test).
- Raw logs under `raw/` (`parse-r1` = first cut, `parse-r2-post-levers`,
  `parse-final-r{1,2}`, `ingest-r1`, `ingest-final`).
- Tier: **dev** — no public claim exists from this artifact (L10).

## Parse throughput (final, per corpus shape)

| row | ns | throughput | budget |
|---|---|---|---|
| parse/small-200B | 595 | 338 MiB/s | — |
| **parse/gate-1KiB** | **2043** | **383 MiB/s** | **≥ 2.5 GB/s — MISSED (≈ 6.5×)** |
| **parse/medium-2KiB** | **3364** | **395 MiB/s** | **≥ 1 GB/s floor — MISSED (≈ 2.6×)** |
| parse/large-64KiB | 151 µs | ~271 MiB/s | — |
| parse/deep-32 | 4.23 µs | 375 MiB/s | — |
| parse/wide-array | 1.33 ms | 285 MiB/s | — |

Both §4.1 budget rows **fail at dev tier** — this is the plan's own
week-1/2 early-warning check firing, by design, four weeks before the
S25 campaign. Stage 1 is NOT the problem (below); stage 2 dominates.

## SIMD stage-1 A/B (L4)

| row | throughput | delta |
|---|---|---|
| scan/simd (AVX2, medium shape) | **3.03 GiB/s** | — |
| scan/scalar (state machine) | 479 MiB/s | SIMD 6.3× faster |
| parse full, SIMD stage 1 (gate) | 383 MiB/s | — |
| parse full, scalar stage 1 (gate) | 217 MiB/s | SIMD +77% end-to-end |

**Disposition: Accepted** — the SIMD stage 1 is a clear win and the
scalar tier remains as portability fallback + oracle.

## Optimization log (profile-driven, each re-measured)

Baseline first cut: gate 330 MiB/s. Perf: `parse_indexed` 36%,
`decode_string`+`from_utf8` 28%, `emit_str` 9%, stage 1 ~11%.

1. **Whole-input UTF-8 hoisting** (validate once, slice `&str` by ASCII
   quote offsets — the simdjson trick in safe Rust) + SWAR
   special-byte scan replacing the bytewise closure: **Accepted**
   (largest single win of the set).
2. **`#[inline]` on hot TapeBuilder methods** (emit_str/i64/begin/end
   failed to inline even at cgu=1 + thin LTO): **Accepted** (folded into
   the same measurement step as 1: combined +9–15% by shape).
3. **Hash-free duplicate detection** (key spans + u16 length prefilter
   recorded at emit; close-time sort for > 256-entry objects; per-key
   hash64 removed): **Accepted** (+6–6.5% on the budget rows;
   `key_bytes_at` bucket 10.6% → gone).

Final: gate **383 MiB/s** (+17% total), medium **395 MiB/s**. Remaining
profile: `parse_indexed` (grammar dispatch + number/literal parsing +
per-value builder plumbing) ~38%, `decode_string` ~11%, `emit_str` ~9% —
structural, not micro.

## Gate-shape projection row (the §4.1 arithmetic, measured)

| row | cost |
|---|---|
| parse gate-1KiB | 2.09 µs |
| `CellStore::set` 1 KiB (same run) | 48.3 ns |
| `json_set` prebuilt idoc (same run) | 63.6 ns |
| parse + `json_set` e2e | 2.19 µs |

Store-level SET is a size-class in-place overwrite (~48 ns): the record
write is negligible, so the SET gate's denominator is wire + log
staging. Two arithmetic anchors:

- **Plan §4.1 assumption** (server SET ≈ 0.5–1.0 µs at 1 KiB):
  JSON.SET ≈ 0.75/(0.75+2.09) ≈ **26% of SET** — gate lost decisively.
- **Re-derived from measured M2.5 rows** (4c natural 2.85 M ops/s ⇒
  ~1.4 µs/op/cell at 16 B/64 B values; 1 KiB adds copy + wire ⇒
  ~1.5–2 µs): JSON.SET ≈ 1.75/(1.75+2.09) ≈ **45% of SET** — still
  below 70%. Under this anchor the parse bar for 70% is ≈ 0.75 µs ≈
  **1.05 GB/s** on the gate shape — 2.7× from here, not 6.5×.

The real denominator is settled by S11's end-to-end `JSON.SET` vs `SET`
rows; the anchor spread is recorded so the S25 target is derived from
measurement, not the plan's pre-measurement sketch.

## Disposition and named levers

Throughput ACs stay **open (Evidence-pending)**; correctness ACs closed
(differential, edges, fuzz — see ledger). Named levers for the follow-up
optimization slice (each an L4 hypothesis with its own A/B):

1. Fuse tape emission into the parser: direct canonical byte emission
   with batched size checks, dropping per-value `Result`/claim plumbing
   through `TapeBuilder` (the ~38% + ~9% buckets).
2. Batched/SWAR unescape and string copy (the ~11% bucket).
3. Short-decimal f64 fast path ahead of `dec2flt` (~3%).
4. Stage 1 emitting (offset, class) pairs to skip stage-2 byte
   re-classification.
5. If safe-Rust levers exhaust short of the S11-measured bar: an ADR
   weighing a vetted unsafe emit core in `inf-doc` (breaks
   `#![forbid(unsafe_code)]` — an L9/§3.3 decision), or an
   evidence-based gate re-derivation at S25.
