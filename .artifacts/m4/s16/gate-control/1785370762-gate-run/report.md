# M4 gate-run report

date: 1785370762 (unix) · cells: 2 · duration: 2s · replicates: 1 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:937c1283f44031e9 (10648048 bytes)
- --baseline-bin not given: delta rows report PENDING (build the M3 tip commit's infinityd and pass its path)
- write amplification 1.730× supplied externally (milli 1730) — the gate row binds this value; the memory-mode rows above report `n/a` because no tiered namespace exists on a node this harness can build yet (S19/S22 own the tiered rows)
- campaign: WA measured on device by the S16 reconciliation churn leg at the ADR-0059 D1 default 50% dead-ratio trigger (real NVMe, O_DIRECT, skewed overwrite churn + retirement, checkpointing quiesced): .artifacts/m4/s16/reconcile-replicate-{1,2,3}.txt

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | — | PENDING (tooling) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.73 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 3672360
errors = 0
elapsed_s = 2.001
ops_per_sec = 1835047
p50_us = 591
p99_us = 943
p999_us = 1087
p9999_us = 6143
max_us = 6361
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 876990
errors = 0
elapsed_s = 2.007
ops_per_sec = 437011
p50_us = 1151
p99_us = 2367
p999_us = 2687
p9999_us = 3711
max_us = 4379
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 3466249
errors = 0
elapsed_s = 2.001
ops_per_sec = 1732006
p50_us = 607
p99_us = 1023
p999_us = 2367
p9999_us = 7551
max_us = 7645
```
