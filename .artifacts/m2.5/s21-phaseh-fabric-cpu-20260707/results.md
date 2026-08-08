# S21 Phase-H fabric-CPU lever slice — binding campaign results (2026-07-07)

Env: reference box (i7-13700KF), governor performance, turbo off (3.4 GHz
P-core base), env-check OK every leg, 4 cells pinned (cpus 4/6/8/10),
loadgen on disjoint cpus, Gen3 DRAM-less NVMe (memory-mode m0 rows — device
not in play). Binaries: baseline `1886263`, lever tree `c3dfa96`. Evidence
committed between legs (dirty-tree rule); ABA arm order.

## Binding legs (gate-run m0 --reference-box, n=3 replicates each)

| leg | arm | natural ops/s | all-local ops/s | penalty | anchor (Dragonfly in-run) | artifact |
|---|---|---|---|---|---|---|
| baseline (2026-07-06) | `1886263` | 2,461k | 6,212k | **60.4%** | 1.44× (df ~1.709M) | `m0/1783391461` (n=5) |
| 2 | levers A+B, prefetch off | 2,745k | ~6,660k | **58.8%** | 1.61× | `m0/1783400906` |
| 1 | levers + `--fabric-apply-prefetch` | 2,945k | 6,644k | **55.7%** | 1.74× (df 1.693M) | `m0/1783400704` |
| 3 | levers + prefetch (repeat) | 2,984k | 6,572k | **54.6%** | 1.78× (df 1.693M) | `m0/1783401176` |

Zero replicate overlap between arms. Natural row +19.6–21.2% binding over
baseline; anchor up 1.44× → 1.74–1.78× (the comparator is stable at
~1.69M in-run across legs). All other m0 gate rows unchanged (RSS 0.610×,
unpipelined 3.10× Redis, p99.9 975 µs class, loop p999, sqes/submit —
see the reports).

## Verdicts

- **Fabric-apply staged prefetch: Accepted, binding** (58.8% → 54.6–55.7%,
  natural +7.3–8.7% on top of levers, anchor +0.13–0.17×). Ships
  **default-on** in `infinityd` (`--no-fabric-apply-prefetch` = A/B off
  arm) per ADR-0030 decision 2 — this supersedes ADR-0005 clause 2 for the
  fabric-apply path.
- **Levers A+B (int-hash gates, allocation-free deferral/dispatch):
  Accepted, binding** (60.4% → 58.8%, natural +11.5%; unconditional,
  behavior-neutral).
- **≤ 40% staged gate: open** — 54.6% ≥ 40%. Residuals named (ADR-0030):
  plane dispatch machinery ~580 cyc/op-mix (de-async the
  almost-never-suspending send path), codec+mesh ~244, kernel ~915/op-mix
  **fixed-rate** (amortizes with further throughput). Next slice also owns
  the local-leg parse-batch prefetch (ADR-0029 second lever, the 8-cell
  ≥ 6M re-read).

## Cycle attribution (dev-tier perf on pinned cells, this dir)

- Natural mix 5,159 → ~4,280 cyc/op (lever tree + prefetch, dev legs
  `perf-lever-pf-run1`): store bucket 15.26% → **5.31%** of leg cycles
  (~739 → ~227 cyc/op-mix) — the owner-side walk is prefetch-hidden;
  DefaultHasher symbols gone; allocator bucket per-op down with the
  OwnedCmd/argv/extract-keys allocations removed.
- Local leg unchanged within noise (2,122 → ~2,105 cyc/op) — the slice
  targeted the deferred path only, as the hypothesis note budgeted.

## Deviations

- Replicates n=3 per leg (baseline row was n=5).
- Leg 2's run window overlapped uncommitted S11 source edits in the
  working tree (uncompiled; running binaries were from clean `c3dfa96`) —
  disclosed in the evidence commit, §19.
- Dev-tier sanity legs (`sanity-run1`, ABAB prefetch legs) and the perf
  re-attribution are dev tier; every number in the table above is binding.
