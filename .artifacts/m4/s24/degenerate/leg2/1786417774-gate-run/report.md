# M4 gate-run report

date: 1786417774 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3202360 ops/s (spread 4.77%) vs m4 3201889 ops/s (spread 4.80%) — signed ops delta -0.01% · p999 623 → 607 µs (-2.57%) · peak-RSS 188329984 → 188727296 B (+0.21%)
- unpipelined 512-conn (M0 gate mix): m3 787209 ops/s (spread 2.46%) vs m4 786633 ops/s (spread 2.24%) — signed ops delta -0.07% · p999 1183 → 1183 µs (+0.00%) · peak-RSS 119427072 → 119648256 B (+0.19%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2725518 ops/s (spread 7.11%) vs m4 2728567 ops/s (spread 5.09%) — signed ops delta +0.11% · p999 4095 → 3903 µs (-4.69%) · peak-RSS 243896320 → 244367360 B (+0.19%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.01 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.07 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.21 | PASS |
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
ops = 30613441
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3060947
p50_us = 327
p99_us = 591
p999_us = 703
p9999_us = 10495
max_us = 11702
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 30547661
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3054408
p50_us = 319
p99_us = 639
p999_us = 751
p9999_us = 10239
max_us = 11170
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 32028299
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3202360
p50_us = 319
p99_us = 527
p999_us = 623
p9999_us = 10239
max_us = 10842
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 31965729
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3196210
p50_us = 319
p99_us = 511
p999_us = 591
p9999_us = 10239
max_us = 11106
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 31159555
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3115601
p50_us = 319
p99_us = 575
p999_us = 671
p9999_us = 10239
max_us = 20756
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 31246073
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3124155
p50_us = 327
p99_us = 543
p999_us = 623
p9999_us = 10239
max_us = 21006
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 32074421
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3207051
p50_us = 319
p99_us = 511
p999_us = 591
p9999_us = 10239
max_us = 10853
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 32151713
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3214758
p50_us = 311
p99_us = 511
p999_us = 607
p9999_us = 10239
max_us = 10714
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 32055153
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3205097
p50_us = 319
p99_us = 503
p999_us = 591
p9999_us = 10239
max_us = 10770
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 31715789
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3171214
p50_us = 319
p99_us = 559
p999_us = 655
p9999_us = 10239
max_us = 10822
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 32046341
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3204186
p50_us = 319
p99_us = 495
p999_us = 575
p9999_us = 10239
max_us = 10596
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 32022997
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3201889
p50_us = 319
p99_us = 503
p999_us = 575
p9999_us = 10239
max_us = 10911
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3940595
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 786633
p50_us = 639
p99_us = 1007
p999_us = 1247
p9999_us = 3263
max_us = 4162
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3947913
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 788359
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3199
max_us = 4159
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3936395
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 786003
p50_us = 639
p99_us = 1023
p999_us = 1247
p9999_us = 3327
max_us = 4463
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3938101
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 786178
p50_us = 639
p99_us = 1023
p999_us = 1151
p9999_us = 3327
max_us = 4171
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3928181
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784319
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3263
max_us = 5540
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3933446
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 785246
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3327
max_us = 4372
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3926025
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 783745
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3263
max_us = 4356
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3937792
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 786323
p50_us = 639
p99_us = 1007
p999_us = 1151
p9999_us = 3199
max_us = 4167
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3949904
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 788788
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3327
max_us = 4293
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3941493
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 787209
p50_us = 639
p99_us = 1023
p999_us = 1151
p9999_us = 3327
max_us = 4409
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 4023203
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 803134
p50_us = 623
p99_us = 1007
p999_us = 1151
p9999_us = 3199
max_us = 4049
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 4015972
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 801934
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3327
max_us = 5305
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27289828
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2728567
p50_us = 359
p99_us = 623
p999_us = 879
p9999_us = 15359
max_us = 15918
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 25405067
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2540152
p50_us = 383
p99_us = 687
p999_us = 4607
p9999_us = 17407
max_us = 18048
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27258894
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2725518
p50_us = 367
p99_us = 607
p999_us = 831
p9999_us = 15103
max_us = 15751
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 26425779
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2642248
p50_us = 367
p99_us = 671
p999_us = 4863
p9999_us = 23039
max_us = 23546
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 27403093
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2739925
p50_us = 359
p99_us = 607
p999_us = 1695
p9999_us = 17407
max_us = 17840
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 26865936
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2686282
p50_us = 367
p99_us = 591
p999_us = 4351
p9999_us = 17407
max_us = 18333
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 26438243
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2643522
p50_us = 367
p99_us = 735
p999_us = 2943
p9999_us = 17919
max_us = 18746
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 26926433
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2692304
p50_us = 367
p99_us = 607
p999_us = 3903
p9999_us = 16895
max_us = 21157
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 27319747
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2731627
p50_us = 359
p99_us = 607
p999_us = 1279
p9999_us = 15615
max_us = 16509
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 27282715
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2727954
p50_us = 359
p99_us = 559
p999_us = 3839
p9999_us = 15103
max_us = 17586
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 27343245
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2733944
p50_us = 359
p99_us = 623
p999_us = 4095
p9999_us = 20479
max_us = 21199
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 26014930
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2601092
p50_us = 383
p99_us = 639
p999_us = 5119
p9999_us = 18431
max_us = 19445
```
