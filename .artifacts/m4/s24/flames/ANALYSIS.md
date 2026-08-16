# S24 phase 7 — profiling windows (2026-08-15, binding env)

Two 20 s `perf record -F 1997 -g --call-graph dwarf,16384 -C 4-7` windows
on the four pinned cells, one per row class, taken inside a live `ycsb`
leg. `kptr_restrict=1`, so kernel frames are one unresolved bucket and are
read as a bucket, not split (runbook rule). `perf.data` deleted after
report extraction.

| class | leg | samples |
|---|---|---|
| `ycsb-hot-ram` | `--mem-budget-mb 1024 --dataset-multiple 1`, workload a zipfian | 72 K |
| `ycsb-cold-flood` | same at `--dataset-multiple 10` | 50 K |

## Per-cell CPU, top symbols

| symbol | hot-ram (cell-0 / cell-1) | cold-flood (cell-0 / cell-1) |
|---|---|---|
| `ServerPlane::maintain` | 9.43% / 9.73% | **13.47% / 10.73%** |
| `ServerPlane::tier_maintain` | 5.02% / 6.20% | 6.08% / 5.84% |
| `CellLoop::run_iteration` | 2.88% / 2.94% | 3.42% / 2.68% |
| `UringDriver::submit_and_reap` | below 0.5% | 0.62% (cell-1) |
| unresolved kernel (I/O path) | below the 0.5% limit | 0.69–1.02% per cell, plus 1.71% `swapper` |

## What this says about the hot-set gate

The phase-4 hot-set result was that the tiered leg's memory-hit **tail** is
workload-*independent* (167–195 µs on every row) where the RAM-resident
reference's tail tracks its workload (39–155 µs), while p50 is *faster* on
the tiered node. The inference recorded there was "background work sharing
the cell, not a per-operation tiering regression". These windows support
that and sharpen it:

- **`tier_maintain` is not the discriminator.** It costs 5.0–6.2% on
  *both* legs, because the reference leg is also a tiered namespace
  (ADR-0064 D4) — nothing demotes, but the maintenance tick still runs.
  A reader expecting the tiering machinery to show up only under load
  would misread the gate.
- **What actually grows is `maintain` itself** (9.4–9.7% → 10.7–13.5%)
  **and the kernel I/O bucket**, which is below the 0.5% report limit on
  the RAM-resident leg and becomes 0.7–1.0% per cell plus a 1.7%
  `swapper` share under cold flood. `submit_and_reap` only surfaces on the
  cold leg at all.
- Combined per-cell maintenance share goes from **14.5–15.9%** to
  **16.6–19.6%**. That is a real increase but not a doubling, which fits a
  *tail* effect rather than a throughput effect — and matches the measured
  gate exactly: p50 unharmed (in fact better), p99/p99.9 pushed to a floor
  every workload inherits.

**No new hot-path cost appears on the tiered node.** There is no tiering
lookup, hash, or indirection symbol in the cold-flood profile that is
absent from the RAM-resident one; the delta is entirely in cell-resident
*maintenance and I/O completion* work that foreground commands queue
behind on a run-to-completion executor. That is the same mechanism the
S20 cache-isolation deviation reports, measured a second way.

## Cited by

- M4 §7 hot set (phase 4 gate leg) — this is its named flamegraph.
- M4 §7 cold reads (phase 4 loaded leg) — the I/O bucket here is the
  completion-side cost that row's latency reflects.
- M4-S20 cache isolation deviation — same head-of-line mechanism.
