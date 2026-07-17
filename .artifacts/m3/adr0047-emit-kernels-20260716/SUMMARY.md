# ADR-0047 emit-kernel slice — levers K1–K3 (dev tier, 2026-07-16)

Environment: i7-13700KF, `performance` governor + EPP, `no_turbo=1`,
bench core pinned `taskset -c 4`; wire rows server 0-7 / memtier 12-23,
3 replicates. Dirty tree disclosed. Nothing citable (L10).

## Session baseline disclosure

The fresh same-day baseline read **gate-1KiB 952 MiB/s / medium
1.016 GiB/s** — materially above the 2026-07-11 slice-2 recorded finals
(683/673 MiB/s) on the same nominal env. Provenance of that
cross-session drift is unknown (box state / code-layout accumulation
across the intervening S16–S24 tree); it is disclosed, not explained.
All lever verdicts below are same-session A/Bs against this fresh
baseline, per the house method. The medium ≥ 1 GB/s floor already
passed at baseline today.

## Lever chain (gate-1KiB row; criterion 100 samples, pinned)

| Arm | gate ns | gate thrpt | disposition |
|---|---|---|---|
| session baseline | 1026.0 | 952 MiB/s | — |
| K1 fused long-string copy (`json_copy_unescaped`) | 958.0 | 1019 MiB/s (+7.1%) | **Accepted** after outline repair — the first cut regressed deep −6.8%/wide −3.6% through `emit_string` inline growth; outlining `emit_string_general` (`#[inline(never)]`) restored them |
| K2 short-string 32-byte kernel (`json_copy_unescaped_short`) | 914.9 | 1042 MiB/s (+1.8%) | **Accepted** — medium +3.1%, deep +2.0%; small pays a tail-slack fallback (−6.3% vs K1 arm, +0.5% net vs baseline) |
| K3 `flush_block` unchecked index writes | **881.9** | **1081 MiB/s (+3.7%)** | **Accepted — every shape improved** (medium +5.7%, large +9.1%, deep +6.1%, small +8.2%, wide +5.3%). Distinct from the slice-2 Rejected slot-flatten: the well-predicted bit-loop stays; only the per-push growth branch goes |

## Cumulative (all six shapes, vs today's baseline)

| Shape | Baseline | Final | Δ |
|---|---|---|---|
| gate-1KiB | 952 MiB/s | **1081 MiB/s (1.081 GiB/s)** | **+16.3%** |
| medium-2KiB | 1.016 GiB/s | 1.125 GiB/s | +10.7% |
| large-64KiB | 1.289 GiB/s | 1.585 GiB/s | +23.0% |
| small-200B | 786 MiB/s* | 850 MiB/s | +8.8% |
| deep-32 | 386 MiB/s* | 407 MiB/s | +5.2% |
| wide-array | 455 MiB/s | 480 MiB/s | +5.6% |

(*from the baseline sweep's time column; thrpt lines were captured for
gate/medium/large directly.)

Ingest seam (`doc_ingest_1kib`, measured at the K1 arm):
`parse_into + json_set` e2e 0.975 µs (was 1.326 µs at the S11
denominator reading); `store_set` 49.5 ns.

## Wire verdict (wire-control.sh, 3 reps)

| Arm | JSON.SET/SET (≥ 0.70) | JSON.GET/GET p50 (≤ 1.5) |
|---|---|---|
| pre-campaign (2026-07-16 a.m.) | 0.6020 | 1.1988 |
| ADR-0046 control | 0.6234 | 1.1587 |
| K1 | 0.6514 | 1.2562 |
| K1+K2+K3 (final) | **0.6556** | **1.0890** |

**The doc-write gate remains red at dev tier: 0.6556 < 0.70.** The
calibrated ratio model (c_set ≈ 1.73 µs from the measured rows) puts the
bar at gate-shape parse ≈ **1.38 GiB/s** — a further ≈ +28% over the
1.081 GiB/s this slice reached. The ADR-0047 D1 estimate (~1.2 GB/s) was
derived against a stale c_set; the D1 bar is re-derived to ~1.4 GB/s in
the ledger entry.

## Post-slice profile (parse/gate, 40k samples, dwarf)

`parse_indexed` 30.1% → grammar loop is now the top bucket ·
`try_fixstr_fast` 18.9% (pre-K2; K2 attacks exactly this) ·
stage-1 `flush_block` 17.5% + `avx2_scan` 9.0% (pre-K3) ·
`parse_number_value` 8.4%.

## Named next steps (in ADR-0047's own staging)

1. **Stage fusion** (D2 item 3): fold structural classification into the
   emit walk — the remaining ≥ 20% class.
2. **D3 escalation** if fusion under-delivers: an unsafe emit region in
   `inf_doc::emit` via an L9 leaf-list amendment ADR.

## Validation

`cargo test -p inf-simd -p inf-doc` green after every lever (48 + full
inf-doc battery; goldens byte-exact, exact error offsets);
`json_differential` release run 250k cases/property green (6.95 s);
`json_parse` fuzz smoke **10,676,781 runs / 301 s / zero findings**;
SAFETY.md K1/K2/K3 sections added; `just check` + `cargo deny check` at
session close.
