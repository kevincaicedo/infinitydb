# M4 gate-run report

date: 1786063451 (unix) · cells: 4 · duration: 10s · replicates: 4 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/0ff70128-f0f2-4226-9508-f52e3e4de141/scratchpad/bins/infinityd-s27: hash64:76e226a50381d437 (12688280 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/0ff70128-f0f2-4226-9508-f52e3e4de141/scratchpad/s27-baseline/target/release/infinityd: hash64:458d8ce79c0cac03 (12620880 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 2842293 ops/s (spread 1.83%) vs m4 2841735 ops/s (spread 2.61%) — signed ops delta -0.02% · p999 959 → 911 µs (-5.01%) · peak-RSS 186998784 → 187088896 B (+0.05%)
- unpipelined 512-conn (M0 gate mix): m3 687600 ops/s (spread 4.49%) vs m4 678735 ops/s (spread 2.14%) — signed ops delta -1.29% · p999 1919 → 1919 µs (+0.00%) · peak-RSS 116457472 → 116133888 B (-0.28%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2423243 ops/s (spread 7.06%) vs m4 2431644 ops/s (spread 1.28%) — signed ops delta +0.35% · p999 3199 → 2623 µs (-18.01%) · peak-RSS 236765184 → 236990464 B (+0.10%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.02 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 1.29 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.10 | PASS (DEV-TIER, non-binding) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | — | PENDING (tooling) |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | — | PENDING (tooling) |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | — | PENDING (tooling) |
| Memory honesty: RSS slope over the 24 h endurance run | < 0.5 pct per 24 h (storm-resistant first/last-5% medians) | — | PENDING (tooling) |
| Endurance: zero crashes over the full 24 h run | <= 0 crashes | — | PENDING (tooling) |
| M3 regression: worst M3 gate delta on memory-mode namespaces | <= 5 pct vs M3 baseline artifact, worst gate | — | PENDING (tooling) |
| Recovery with tiering on: replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| Recovery with tiering on: 10 GB boot | < 15 s | — | PENDING (tooling) |
| Never-none invariant: zero violations in the 10k-seed DST sweep | <= 0 violations | — | PENDING (tooling) |
| Crash + ENOSPC matrices: all fault points green | <= 0 failing rows | — | PENDING (tooling) |
| Foreground protection: p99.9 during demotion + compaction storms | < 2 ms | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 28594073
errors = 0
elapsed_s = 10.001
ops_per_sec = 2859083
p50_us = 343
p99_us = 671
p999_us = 975
p9999_us = 10751
max_us = 11792
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 28425933
errors = 0
elapsed_s = 10.001
ops_per_sec = 2842293
p50_us = 351
p99_us = 655
p999_us = 959
p9999_us = 10751
max_us = 11248
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 28212849
errors = 0
elapsed_s = 10.001
ops_per_sec = 2820952
p50_us = 351
p99_us = 671
p999_us = 927
p9999_us = 10751
max_us = 11548
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 28420505
errors = 0
elapsed_s = 10.001
ops_per_sec = 2841735
p50_us = 351
p99_us = 655
p999_us = 895
p9999_us = 10751
max_us = 11400
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 28093902
errors = 0
elapsed_s = 10.001
ops_per_sec = 2809065
p50_us = 351
p99_us = 671
p999_us = 895
p9999_us = 10751
max_us = 11739
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 27907483
errors = 0
elapsed_s = 10.001
ops_per_sec = 2790416
p50_us = 351
p99_us = 671
p999_us = 943
p9999_us = 10495
max_us = 10860
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 28426839
errors = 0
elapsed_s = 10.001
ops_per_sec = 2842376
p50_us = 351
p99_us = 655
p999_us = 959
p9999_us = 10751
max_us = 12515
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 27853870
errors = 0
elapsed_s = 10.001
ops_per_sec = 2785025
p50_us = 351
p99_us = 687
p999_us = 911
p9999_us = 10751
max_us = 11209
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3398605
errors = 0
elapsed_s = 5.007
ops_per_sec = 678735
p50_us = 719
p99_us = 1535
p999_us = 1887
p9999_us = 3583
max_us = 5780
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3493941
errors = 0
elapsed_s = 5.007
ops_per_sec = 697768
p50_us = 703
p99_us = 1503
p999_us = 1887
p9999_us = 3391
max_us = 4304
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3442823
errors = 0
elapsed_s = 5.007
ops_per_sec = 687600
p50_us = 703
p99_us = 1535
p999_us = 1919
p9999_us = 3583
max_us = 4276
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3379130
errors = 0
elapsed_s = 5.008
ops_per_sec = 674698
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 4479
max_us = 6467
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3374765
errors = 0
elapsed_s = 5.007
ops_per_sec = 673945
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 3839
max_us = 6943
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3361594
errors = 0
elapsed_s = 5.008
ops_per_sec = 671214
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 4031
max_us = 7651
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3339380
errors = 0
elapsed_s = 5.007
ops_per_sec = 666893
p50_us = 735
p99_us = 1535
p999_us = 1919
p9999_us = 4095
max_us = 5901
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3447320
errors = 0
elapsed_s = 5.007
ops_per_sec = 688461
p50_us = 719
p99_us = 1535
p999_us = 1887
p9999_us = 3583
max_us = 5579
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 24319439
errors = 0
elapsed_s = 10.001
ops_per_sec = 2431644
p50_us = 399
p99_us = 799
p999_us = 3199
p9999_us = 16383
max_us = 17542
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 23546547
errors = 0
elapsed_s = 10.001
ops_per_sec = 2354339
p50_us = 415
p99_us = 815
p999_us = 2367
p9999_us = 13311
max_us = 14540
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 24235064
errors = 0
elapsed_s = 10.001
ops_per_sec = 2423243
p50_us = 399
p99_us = 783
p999_us = 3199
p9999_us = 16895
max_us = 17591
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 24097875
errors = 0
elapsed_s = 10.001
ops_per_sec = 2409558
p50_us = 399
p99_us = 815
p999_us = 2623
p9999_us = 15103
max_us = 15844
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 24225064
errors = 0
elapsed_s = 10.001
ops_per_sec = 2422247
p50_us = 399
p99_us = 783
p999_us = 2495
p9999_us = 14847
max_us = 16686
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 25225758
errors = 0
elapsed_s = 10.001
ops_per_sec = 2522283
p50_us = 383
p99_us = 735
p999_us = 1631
p9999_us = 14335
max_us = 15043
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 23515012
errors = 0
elapsed_s = 10.001
ops_per_sec = 2351243
p50_us = 407
p99_us = 847
p999_us = 3839
p9999_us = 16127
max_us = 23555
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24409381
errors = 0
elapsed_s = 10.001
ops_per_sec = 2440639
p50_us = 399
p99_us = 799
p999_us = 1951
p9999_us = 14079
max_us = 15228
```
