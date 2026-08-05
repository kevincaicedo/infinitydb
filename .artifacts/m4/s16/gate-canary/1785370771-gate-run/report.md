# M4 gate-run report

date: 1785370771 (unix) · cells: 2 · duration: 2s · replicates: 1 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:937c1283f44031e9 (10648048 bytes)
- --baseline-bin not given: delta rows report PENDING (build the M3 tip commit's infinityd and pass its path)
- write amplification 8.318× supplied externally (milli 8318) — the gate row binds this value; the memory-mode rows above report `n/a` because no tiered namespace exists on a node this harness can build yet (S19/S22 own the tiered rows)
- campaign: CANARY: compaction dead-ratio deliberately mis-tuned to 10% (ADR-0059 D1 default is 50%) — same build, same workload, only the trigger differs; figure from mistuned_dead_ratio_trips_the_write_amp_gate (crates/inf-store/tests/tiered_write_amp.rs), log .artifacts/m4/s16/canary-test-20260729.txt

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
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 8.32 | FAIL |
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
ops = 4018037
errors = 0
elapsed_s = 2.001
ops_per_sec = 2007726
p50_us = 559
p99_us = 735
p999_us = 1007
p9999_us = 6015
max_us = 6206
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 859564
errors = 0
elapsed_s = 2.007
ops_per_sec = 428216
p50_us = 1183
p99_us = 2303
p999_us = 2879
p9999_us = 3583
max_us = 4278
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 3499431
errors = 0
elapsed_s = 2.001
ops_per_sec = 1748700
p50_us = 607
p99_us = 975
p999_us = 2431
p9999_us = 6143
max_us = 6158
```
