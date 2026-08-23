# S39b campaign G verdict — ADR-0090 D9 (the bounded pool wait): **Revised** by the predeclared rule; mechanism kept; the one-slot default confirmed

Engine `c284c48`, clean tree, reference box (governor `performance`,
env-check OK), 2026-08-22 21:11–21:49. Run: `run/1787448954-gate-run/
report.md`; raw legs in `campaign.log`. Rules: `README.md` (written before
the run). No `fstrim` (no sudo) — the drive carried the day's campaigns
D/E/F and S40; the arms interleave (ABBA) so both see the same drive state.

## Per-replicate pairs (A = one slot + `--recycle-wait off`, B = one slot + `quarter`)

| rep | order | warmed zero-fill/log A → B | waits B (sat/exp) | host/log A → B | device/log A → B | p50 B/A | p99 B/A | reads B/A | recovery A → B (s) |
|---|---|---|---|---|---|---|---|---|---|
| 0 | A,B | 0.257 → **0.111** | 34 / 4 | 1.438 → 1.302 | 1.442 → 1.306 | 0.994 | 0.773 | 1.057 | 5.055 → 5.144 |
| 1 | B,A | 0.203 → **0.104** | 34 / 4 | 1.388 → 1.289 | 1.392 → 1.296 | 0.992 | 0.909 | 1.007 | 3.725 → 3.695 |
| 2 | A,B | 0.335 → **0.103** | 36 / 4 | 1.517 → 1.290 | 1.523 → 1.294 | 0.992 | 0.926 | 0.987 | 3.330 → 3.592 |
| **median** | | **0.257 → 0.104** | **0.89 satisfied** | **1.438 → 1.290** | 1.442 → 1.296 | **0.99** | **0.91** | **1.01** | **1.02** |

On B in every replicate: `rotations_unzeroed = 0`, `inline_preallocs = 0`,
`prealloc_failures = 0`, `recycle_pool_full = 0` (A: 2 / 1 / 5), warmed
deficit 0 / 1 / 0 (A: 3 / 2 / 3), padding 51.5–51.8 % on both arms
(Δ ≤ 0.3 pt), rotations per cell ≥ 10, every first boot completed.
Accounted host bytes and device sectors agree to 0.5 %.

## Gate reading (the predeclared rule, ADR-0090 A9)

- `s39b_warmed_zero_fill_share` ≤ 0.10: **0.104 — FAIL by 0.004.**
- `s39b_wait_satisfied_share_arm` ≥ 0.5: **0.89 — PASS.**
- `rotations_unzeroed` / `inline_preallocs` / `prealloc_failures` on B,
  worst replicate: **0 / 0 / 0 — PASS.**
- p50 ≤ 1.05 (0.99), p99 ≤ 1.1 (0.91), reads ≥ 0.98 (1.01), padding ≤ 3 pt
  (0.11), deficit ≤ 2 (0), rotations ≥ 8 (10): **PASS.**
- recovery ≤ 1.05 (1.02): PASS — a control here, both arms recycle; not a
  decision input (S39d owns attribution).

**Disposition: D9 `Revised`** — the third branch of the rule: the share is
above 0.10, satisfied waits dominate, no latency/read/padding control
failed. The mechanism ships as built (`--recycle-wait quarter` stays the
default), the ≤ 0.10 number is `Evidence-pending`, and **the one-slot
default is confirmed** on the already-measured reduction plus this row's.

## Where the residual 0.104 comes from

Every B leg has exactly **four expired waits (the sum over four cells) and
a warmed zero-fill of exactly 1 GiB = 4 × 256 MiB** — the counters say four
fallbacks per leg, each one segment; the row's scrape is the cell sum, so
"one per cell" is the consistent reading, not a per-cell observation. The
mechanism: the first waiting generation on a cell (generation 3 — the first
with a sealed pre-zeroed segment to wait for) expires because its feed is
the cell's first checkpoint cycle, which publishes later than a quarter
segment into that generation's life; every later wait (34–36 per leg) is
fed before the bound. That one fallback lands
inside the warmed window (the trigger fires at 15 s, before generation 3
ends), so the warmed share is one segment per cell over the window's
frames — 0.104 at 150 s, ≈ 0.05 at 300 s by arithmetic (not measured), and
**zero after the first expired wait**. The number is a window artefact of a
per-boot cost, not a steady-state term; the next measurement that could
move the gate is a 300 s leg or a window that starts after the first
expired wait — by amendment, not tonight.

## What this row adds to C41

Chained across campaigns (different hours of the same day, same box, same
binary lineage): recycling-off 2.20 (F baseline) → one slot, wait off 1.42
(F arm) / 1.44 (G baseline, the same configuration) → one slot, quarter
wait **1.29** (G arm) accounted host bytes per log byte — **≈ 41 % fewer
host writes than recycling-off**, p99 0.20 × (F) × 0.91 (G). The directly
paired figure of this campaign is 1.44 → 1.29 (−10 %).
