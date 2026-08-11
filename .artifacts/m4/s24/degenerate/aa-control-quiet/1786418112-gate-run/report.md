# M4 gate-run report

date: 1786418112 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m4: hash64:2b50b03e54378a6f (12722784 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3185301 ops/s (spread 6.21%) vs m4 3175433 ops/s (spread 2.27%) — signed ops delta -0.31% · p999 639 → 639 µs (+0.00%) · peak-RSS 188702720 → 188620800 B (-0.04%)
- unpipelined 512-conn (M0 gate mix): m3 806505 ops/s (spread 0.54%) vs m4 805268 ops/s (spread 1.00%) — signed ops delta -0.15% · p999 1119 → 1087 µs (-2.86%) · peak-RSS 120205312 → 120156160 B (-0.04%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2633811 ops/s (spread 12.79%) vs m4 2719729 ops/s (spread 5.78%) — signed ops delta +3.26% · p999 4351 → 4223 µs (-2.94%) · peak-RSS 244162560 → 244191232 B (+0.01%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.31 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.15 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.01 | PASS |
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
ops = 31204989
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3120108
p50_us = 319
p99_us = 559
p999_us = 655
p9999_us = 5503
max_us = 11774
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 30027999
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3002350
p50_us = 343
p99_us = 655
p999_us = 735
p9999_us = 10495
max_us = 10967
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 31309664
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3130576
p50_us = 319
p99_us = 559
p999_us = 639
p9999_us = 5503
max_us = 11984
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 31696487
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3169268
p50_us = 327
p99_us = 575
p999_us = 671
p9999_us = 10239
max_us = 10636
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 31925022
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3192086
p50_us = 311
p99_us = 543
p999_us = 639
p9999_us = 10239
max_us = 10826
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 31661486
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3165730
p50_us = 319
p99_us = 559
p999_us = 639
p9999_us = 10239
max_us = 10906
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 31976895
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3197286
p50_us = 311
p99_us = 543
p999_us = 639
p9999_us = 10239
max_us = 10871
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 31924120
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3191994
p50_us = 319
p99_us = 527
p999_us = 607
p9999_us = 10239
max_us = 20864
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 31757735
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3175433
p50_us = 319
p99_us = 543
p999_us = 639
p9999_us = 10239
max_us = 10906
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 31856526
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3185301
p50_us = 319
p99_us = 527
p999_us = 607
p9999_us = 10239
max_us = 10779
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 32006420
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3200229
p50_us = 319
p99_us = 527
p999_us = 607
p9999_us = 10239
max_us = 10934
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 31674504
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3167040
p50_us = 319
p99_us = 559
p999_us = 639
p9999_us = 10239
max_us = 10729
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 4025657
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 803774
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3199
max_us = 5379
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 4042422
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 807343
p50_us = 623
p99_us = 1007
p999_us = 1151
p9999_us = 3327
max_us = 4339
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 4033618
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 805257
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3263
max_us = 4881
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 4031762
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 804867
p50_us = 623
p99_us = 1007
p999_us = 1151
p9999_us = 3263
max_us = 4045
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 4035524
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 805652
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3327
max_us = 4607
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 4039649
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 806804
p50_us = 623
p99_us = 1007
p999_us = 1183
p9999_us = 3263
max_us = 4063
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 4039663
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806505
p50_us = 623
p99_us = 1007
p999_us = 1119
p9999_us = 3263
max_us = 4081
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 4033393
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 805268
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3263
max_us = 5762
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 4045164
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807531
p50_us = 623
p99_us = 1007
p999_us = 1087
p9999_us = 3199
max_us = 3973
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 4029267
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 804698
p50_us = 623
p99_us = 1007
p999_us = 1119
p9999_us = 3263
max_us = 4005
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 4020929
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 802980
p50_us = 623
p99_us = 1007
p999_us = 1119
p9999_us = 3199
max_us = 5567
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 4003107
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 799447
p50_us = 623
p99_us = 1023
p999_us = 1183
p9999_us = 3199
max_us = 4013
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27441355
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2743804
p50_us = 359
p99_us = 591
p999_us = 3007
p9999_us = 19967
max_us = 20517
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 26082843
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2607975
p50_us = 367
p99_us = 719
p999_us = 4351
p9999_us = 17407
max_us = 18366
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27541377
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2753821
p50_us = 359
p99_us = 623
p999_us = 2943
p9999_us = 17919
max_us = 18983
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 26575827
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2657259
p50_us = 367
p99_us = 607
p999_us = 4351
p9999_us = 15615
max_us = 16625
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 27200499
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2719729
p50_us = 359
p99_us = 623
p999_us = 1439
p9999_us = 16127
max_us = 17162
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 26342032
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2633811
p50_us = 367
p99_us = 671
p999_us = 4351
p9999_us = 18431
max_us = 19091
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 27290796
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2728747
p50_us = 359
p99_us = 591
p999_us = 2751
p9999_us = 20991
max_us = 21372
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 25868601
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2586527
p50_us = 391
p99_us = 655
p999_us = 4735
p9999_us = 17919
max_us = 18672
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 27435139
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2743203
p50_us = 351
p99_us = 655
p999_us = 2687
p9999_us = 17407
max_us = 18396
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24171622
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2416891
p50_us = 415
p99_us = 799
p999_us = 10751
p9999_us = 15615
max_us = 18759
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25422048
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2541856
p50_us = 407
p99_us = 751
p999_us = 10751
p9999_us = 18431
max_us = 19321
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 26288838
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2628558
p50_us = 375
p99_us = 719
p999_us = 4223
p9999_us = 18431
max_us = 19169
```
