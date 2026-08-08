# ADR-0046 remediation A/B — document residency v2 (dev tier, 2026-07-16)

Candidate arm: this tree (`doc_morph_bytes_min` default → `usize::MAX`;
ingest never builds the tree form). Baseline arm: the same-day, same-box,
same-harness pre-campaign rows in `../s25-dev-20260716/` (tree at ≥ 4 KiB).
Environment: `env.txt`. Nothing here is citable (dev tier, L10).

## Corpus finding that re-baselines the comparator axis (corpus v2)

The v1 harness loaded **one generated file per shape into N keys** —
identical bytes on every key. Pinned RedisJSON exploits cross-key
identical strings: 2,000 identical 64 KiB docs cost it 36.3 KB/doc
(0.556× serialized) vs **92.9 KB/doc (1.418×) for unique docs**
(measured: `used_memory` delta 185,891,696 B / 2,000 unique docs,
DBSIZE-verified). No real workload hands the comparator that redundancy,
and InfinityDB stores per-key either way — so the v1 corpus mis-measured
the ≤ 0.7×-RedisJSON axis by construction. Corpus v2
(`inf-bench doc-corpus --pipe --counts …`) emits per-index-unique
documents from the same pinned generator/seed; the RSS gate binds on it
(ADR-0046 D3).

## Binding row — mixed corpus v2 (71,200 docs, 207,450,429 serialized B)

| Replicate | RSS/serialized (≤ 1.5) | vs redis-stack (≤ 0.7) |
|---|---|---|
| r1 | 1.043 | 0.381 |
| r2 | 1.043 | 0.381 |
| r3 | 1.025 | 0.375 |

**PASS both axes** (was 2.155 / 0.949 with tree residency on corpus v1).
Both engines verified at 71,200 documents per replicate (DBSIZE +
`used_memory` captured in each `v2-*/` dir).

## Per-shape diagnostics (corpus v2, candidate)

| Shape | RSS/serialized | vs stack | v1-baseline (tree) RSS/serialized |
|---|---|---|---|
| small-200B ×40k | 1.471 ✓ | 0.535 ✓ | 1.488 |
| gate-1KiB ×40k | 1.173 ✓ | 0.561 ✓ | 1.174 |
| large-64KiB ×2k | 1.019 ✓ | 0.743 ✗ (diagnostic) | 1.854 |
| wide-array ×300 | 1.028 ✓ | 0.289 ✓ | 3.288 |

The large-shape 0.743 sits 6% over the per-shape line against a
comparator that stores that shape at 1.371× serialized; the binding
mixed row carries 0.381. Named follow-ups if a per-shape bar is ever
adopted: interning reserve (−31.6% stored bytes on this shape,
ADR-0038) or blob size-class tuning.

Continuity row (corpus v1, identical docs): candidate large-64KiB
1.027× serialized / 1.849× stack — the stack side of that ratio is the
dedup artifact above, recorded for cross-session comparability
(`shape-large-64KiB/verdict.txt`).

## Wire no-regression controls (candidate binary, 3 reps)

Gate shapes are ≤ 4 KiB — placement unchanged — so ratios must match the
baseline session within noise, and do:

| Row | Candidate | Baseline (s25-dev) |
|---|---|---|
| JSON.SET/SET (1 KiB, pipelined) | 0.6234 (rsd ≤ 2.4%) | 0.6020 |
| JSON.GET/GET p50 (1 KiB) | 1.1587 | 1.1988 |

Absolute ops/s ran 6–20% below the baseline session on **all** rows
(both plain-KV and JSON alike — box-state drift, not a candidate
effect); the ratios are the controls. The doc-write gate remains red
(0.62 < 0.70) — owned by the separate ADR-0047 lever, untouched here.

## Verdict

Document-memory STOP gate: **remediated at dev tier — PASS both axes
with margin** (1.04× serialized vs ≤ 1.5; 0.38× redis-stack vs ≤ 0.7).
Reference-box re-run rides S25 and owns the §7 gate flip.

Scripts: `rss-v2.sh` (corpus-v2 A/B, any count vector), `rss-shape-v2.sh`
(v1-corpus continuity), `wire-control.sh`. Validation: workspace tests
green after the placement change; `just check` + `cargo deny check` at
session close (see ledger).
