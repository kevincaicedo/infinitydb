# M3-S05 optimization slice 3 — SIMD UTF-8 validation kernel (dev-tier, 2026-07-11)

- Env: `env.txt` (binding env re-verified same session). Baseline: fresh
  same-day run on this tree (which carries the S02 traversal slice) —
  gate **659.6** / medium **666.3** / small 653.5 / large **414.0** /
  deep 632.7 / wide 456.9 MiB/s. The slice-2 artifact's final rows
  (680/673) are a *different binary from a different day* — the ±3.5–4%
  cross-binary layout spread documented there applies; today's baseline
  is the honest arm.
- Tier: **dev** — no public claim (L10).

## Lever U — `inf_simd::utf8_is_valid` (Keiser–Lemire lookup, AVX2)

One 32-byte-block classifier (three nibble shuffles + 3/4-byte
continuation cross-check), per-block ASCII fast path (one movemask),
zero-padded tail block; `std::str::from_utf8` stays the portability tier
and the oracle. `inf-doc` consequence: the parser pipeline moves from
`&str` to `&[u8]` end-to-end (the `&str` token was what forced std's
validation pass), `decode_string` scratch becomes `Vec<u8>` — deleting
the escape walk's per-slice `&str` char-boundary checks — and the exact
error offset comes from re-running std on the (cold) reject path, which
also makes a kernel false-negative harmless by construction.

**Same-binary isolation A/B** (both validators in one binary, pinned
core — the box's rule for sub-4% effects):

| input | kernel | std | speedup |
|---|---|---|---|
| ascii-200B | 12.1 ns (16.5 GB/s) | 14.3 ns (14.0 GB/s) | 1.18× |
| ascii-1KiB | 22.1 ns (46.3 GB/s) | 38.4 ns (26.7 GB/s) | 1.74× |
| ascii-64KiB | 0.96 µs (68.3 GB/s) | 1.53 µs (42.7 GB/s) | 1.60× |
| mixed-1KiB (non-ASCII) | 78.0 ns (13.1 GB/s) | 519.9 ns (2.0 GB/s) | 6.67× |

**e2e rows** (shipped binary, r1–r3 same-binary replicates):

| row | baseline (today) | shipped | Δ |
|---|---|---|---|
| gate-1KiB | 659.6 | 645.8–659.3 | **−2.1% … 0.0%** (inside spread) |
| medium-2KiB | 666.3 | 652.4–659.3 | **−2.1% … −1.1%** (inside spread) |
| small-200B | 653.5 | 668.3 | +2.3% |
| **large-64KiB** | 414.0 | **452.2–453.5** | **+9.3–9.6%** |
| deep-32 | 632.7 | 648.8 | +2.5% |
| wide-array | 456.9 | 461.0 | +0.9% |

## Disposition — **Accepted, with the honest split recorded**

The budget rows did not move measurably: validation was ~38 ns of a
~1212 ns gate parse (3.2%), so the kernel's 1.74× there predicts ~+1.3%
e2e — **below this box's noise floor**, and the readings (−2…0%) sit
inside the documented cross-binary spread (< 4% ⇒ unproven, the box
rule). The lever is accepted on the same grounds as slice 2's lever G:
the mechanism is proven positive in a same-binary arm on every shape,
4/6 e2e rows improve — large-64KiB +9.6% is beyond noise (escape-heavy
strings: the `&str`-boundary-check deletion in `decode_string` plus the
validation saving) — and the whole-input `&str` framing the slice-2
entry named as the lever's second payoff is gone. Anyone re-litigating
the budget rows re-runs r2/r3 on one binary.

Recorded limitation: no SSE2/NEON utf8 tier — non-AVX2 x86 and aarch64
fall back to std (itself word-optimized). An unmeasured port would be
L4 theater; the S25 reference box has AVX2.

## Slice-3 close-out of the S05 named-lever list

1. ~~SIMD UTF-8 kernel~~ — **done (this slice)**.
2. Stage-1 typed index — still deprioritized (slice-2 reasons stand).
3. The unsafe-emit-core ADR question — still gated on S11's e2e
   denominator, per the risk-row rule (never gate narrowing).

Safe-Rust local levers are now exhausted at dev tier: the remaining
gate-shape profile is grammar dispatch + emit floor + stage-1 share,
each already at its measured safe-Rust floor per the slice-1/2/3
artifacts. The throughput ACs stay **Evidence-pending**; the next
decision point is S11's e2e `JSON.SET` vs `SET` rows, exactly as the
plan's risk row prescribes.

## Validation (this slice)

- `cargo test -p inf-simd` (kernel: adversarial corpus, block-boundary
  shifts, incomplete-before-ASCII, arbitrary + mutated-text equivalence
  proptests) and `cargo test -p inf-doc`, both feature lanes — green.
- `cargo clippy` clean on both crates; SAFETY.md gains the `utf8.rs`
  inventory section.
- 10⁶-doc differential release run green on the final code
  (`PROPTEST_CASES=250000`, 4 properties, 7.0 s — serde_json would
  reject any UTF-8 false-accept, so this run also exercises the kernel's
  accept direction).
- Fuzz smoke (`json_parse`, corpus-seeded, with the fixpoint +
  tight-limits oracles) runs in the session-close validation set.
