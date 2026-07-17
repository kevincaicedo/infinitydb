# M3-S05 optimization slice 2 — parse throughput levers (dev-tier, 2026-07-11)

- Box: HomeLab i7-13700KF, kernel 7.0.0-27-generic, governor=performance,
  EPP=performance, `no_turbo=1` (binding env, 3.4 GHz P-core base, re-verified
  at session start — `env.txt`), pinned `taskset -c 4`. No concurrent load
  during bench runs; perf captures deleted after extraction.
- Tree: post-slice-1 commit eb9c211 + this slice's working tree (dirty file
  at baseline = this artifact dir only).
- Baseline: fresh same-day run of the eb9c211 binary (`raw/baseline-r1.txt`)
  — gate **517.8 MiB/s**, medium **516.7 MiB/s** (within the ±3.5%
  rebuild-layout spread of slice 1's quoted 525.6/519.5 final pair).
- Method: levers land and measure separately, cumulative chain; a losing
  lever (or losing half of one) is recorded and reverted (M0-S14). Output
  identity enforced at every step: goldens byte-exact, 12 edge suites,
  10⁶-doc differential on the final code, `json_parse` fuzz oracles.
- Tier: **dev** — no public claim exists from this artifact (L10).

## Lever chain (gate-1KiB / medium-2KiB, cumulative)

| lever | gate | medium | disposition |
|---|---|---|---|
| baseline (eb9c211) | 517.8 | 516.7 | — |
| **E** fixstr fused scan+copy (≤ 31 B strings: word loads feed word stores, tag push + mask-tail; general path keeps escapes/errors) | 602.7 (+16.4%) | 588.3 (+14.0%) | **Accepted** (large read −5.5% cross-binary; recovered by F — see below) |
| **F** fused entry/element loops (`key:value,` and `value,` runs cost no dispatch round trip; container kind static in the separator check) | 602.0 (+0.0%) | 597.6 (+1.5%) | **Accepted** (large **+18.2%** → 417; wide +4.2%; the E-step large dip was layout, cured by the restructure) |
| **G** `parse_into` caller-owned output buffer (the S03/S11 ingest seam; post-rejection memory observable for S07) | 593.1 (−1.5%) | 592.9 (−0.8%) | **Accepted as API, perf-recorded below** — the alloc-reuse win shows on the final binary's same-run rows, not on the fresh-alloc `parse/` chain rows |
| **H** dup-scan first-byte prefilter (no per-key build cost — the fix for slice 1's rejected lever D) | 643.9 (+8.5%) | 651.0 (+9.6%) | **Accepted** (small +20.8%, large +11.4%, wide +6.0%) |
| **I** stage-1 slot-style flatten + SIMD-classified tail | 634.3 (−1.5%) | 635.9 (−2.3%) | **split** ↓ |
| **I-split** SIMD-classified tail + count-returning scan API only (slot flatten reverted) | 668.4 (+7.1% vs I) | 658.6 (+3.8% vs I) | **Accepted**; slot flatten **Rejected on the budget rows** (−1.5%/−2.3% there vs +4–6% small/large; push loop predicts well at real densities — recorded in `flush_block`'s comment, `raw/leverI-r1/r2.txt`) |

## Final rows (shipped binary, `final-r2`/`final-r3`)

| row | slice-1 final | slice-2 final | Δ | budget |
|---|---|---|---|---|
| **parse/gate-1KiB** | 525.6 MiB/s | **680.1–683.8 MiB/s** | **+29.4%** | ≥ 2.5 GB/s as written — MISSED (≈ 3.8×); ≈ 1.1 GB/s re-derived — ≈ 1.65× |
| **parse/medium-2KiB** | 519.5 MiB/s | **672.9–673.4 MiB/s** | **+29.5%** | ≥ 1 GB/s floor — MISSED (≈ 1.52×; ≈ 1.46× on the ingest-seam row) |
| parse/small-200B | 440.4 | 664.0 | +50.8% | — |
| parse/large-64KiB | 371.5 | 455.3 | +22.6% | — |
| parse/deep-32 | 529.5 | 650.5 | +22.9% | — |
| parse/wide-array | 400.9 | 467.2 | +16.6% | — |

vs the pre-optimization S05 first cut (383/395 MiB/s): **1.78× / 1.70×**.

`parse_into` (the ingest-seam arm, same run r2/r3 — lever G's measured value):
small **700.8** (+5.5% vs `parse`), gate **672.3–696.5** (−1.1%…+1.9%),
medium **703.2–707.5** (+4.4%), large 457.1 (+0.4%), deep 664.9 (+2.2%),
wide 467.4 (0%). The G hypothesis (~2–3%) is confirmed on 4/6 shapes on the
final binary; gate sits inside run spread.

## Projection row (`raw/ingest-final.txt`, same session)

| row | slice 1 | now |
|---|---|---|
| parse gate-1KiB (in-store harness) | 1.456 µs | **1.264 µs** (−13.2%) |
| `CellStore::set` 1 KiB | 47.4 ns | 47.9 ns |
| `json_set` prebuilt idoc | 62.5 ns | 62.8 ns |
| parse + `json_set` e2e | 1.587 µs | 1.340 µs |
| `parse_into` + `json_set` e2e (new row) | — | **1.326 µs** |

Under the measured-M2.5 server-SET anchor (~1.75 µs at 1 KiB): JSON.SET
projects to ≈ 1.75/(1.75+1.28) ≈ **58% of SET** (slice 1: 55%; original: 45%).
The 70%-gate parse bar stays ≈ 0.75 µs ≈ **1.1 GB/s** on this shape — the
shipped code is ≈ 1.65× short at dev tier. S11's e2e rows fix the real
denominator; S25 re-runs at reference tier.

## Remaining profile (gate shape, final code, perf 4999 Hz dwarf)

`parse_indexed` 30.7% (grammar machine + dup bookkeeping; `memcmp` down to
1.6% after H) · `emit_string` 26.9% (fixstr fast path is the floor of safe
word-at-a-time copy) · stage 1 ~19.4% (`flush_block` 14.9 + `avx2_scan` 4.5
— share ceiling at this e2e rate) · `parse_number` 10.9% + `emit_i64` 2.1 ·
`from_utf8` 2.4%.

## Remaining named levers (expected-value order)

1. SIMD UTF-8 validation kernel in `inf-simd` (~2%, plus removes the
   whole-input `&str` framing).
2. Stage-1 typed `(class, offset)` index — deprioritized this slice: at
   gate scale the input byte reload is L1-resident and the grammar branch
   pattern (the real cost) stays data-dependent either way; u24-offset
   packing also needs the S07 text cap ≤ 16 MiB first. Re-examine only if
   a profile shows stage-2 stalled on input loads.
3. The ADR question (vetted unsafe emit core in `inf-doc` — L9/§3.3
   decision) **iff** S11's e2e rows put the bar out of safe-Rust reach.

## Validation (this artifact's run set)

- `cargo test -p inf-doc -p inf-simd` green after every lever (goldens
  byte-exact, 12 edge suites, cross-tier scan equivalence, differential).
- 10⁶-doc differential release run green on the final code
  (`PROPTEST_CASES=250000`, 4 properties, 7.0 s — bit-exact f64).
- `just check` green from clean at session close (757 tests, 0 failed —
  the tree also contains S06/S07 by then); `cargo deny check` green;
  `json_parse` fuzz smoke 600 s / 16,033,321 runs / zero findings with
  the S06 fixpoint + S07 tight-limits oracles live (the 1 h accrual is
  the nightly job's — per-PR tier is the smoke).
- Same-binary replicate spread ≤ ~1.5% (r2 vs r3); cross-binary layout
  spread ±3.5% still applies (memory note) — per-lever chain steps are
  directional, the shipped binary's r2/r3 pair is the quotable number.
