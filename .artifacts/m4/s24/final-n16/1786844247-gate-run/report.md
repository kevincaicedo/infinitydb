# M4 gate-run report

date: 1786844247 (unix) · cells: 4 · duration: 10s · replicates: 16 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 2990903 ops/s (spread 4.62%) vs m4 3011819 ops/s (spread 7.50%) — signed ops delta +0.70% · p999 671 → 655 µs (-2.38%) · peak-RSS 187359232 → 188035072 B (+0.36%)
- unpipelined 512-conn (M0 gate mix): m3 784805 ops/s (spread 0.60%) vs m4 783776 ops/s (spread 0.72%) — signed ops delta -0.13% · p999 1215 → 1215 µs (+0.00%) · peak-RSS 119009280 → 119586816 B (+0.49%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2543991 ops/s (spread 6.55%) vs m4 2524788 ops/s (spread 4.76%) — signed ops delta -0.75% · p999 4031 → 4351 µs (+7.94%) · peak-RSS 239017984 → 239198208 B (+0.08%)
- write amplification 1.890× supplied externally (milli 1890) — the gate row binds this value; the memory-mode rows above report `n/a` because no tiered namespace exists on a node this harness can build yet (S19/S22 own the tiered rows)
- campaign: S24 degenerate hard sub-gate, higher-n adjudication (2026-08-15): n=6 produced ttl-heavy p99.9 +17.65% while the same-night A/A control read +2.86%; the row's per-replicate p999 spans 4x, so this leg raises n to 16

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.13 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.75 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 7.94 | FAIL |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.49 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.89 | PASS |
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
ops = 30354543
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3035105
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10495
max_us = 11075
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 29913049
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2990903
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 10961
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 30342958
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3033948
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10495
max_us = 11030
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 30121918
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3011819
p50_us = 351
p99_us = 591
p999_us = 687
p9999_us = 10495
max_us = 11017
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 30336758
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3033334
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11142
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 30246943
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3024373
p50_us = 335
p99_us = 511
p999_us = 607
p9999_us = 10239
max_us = 10868
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 29962153
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2995876
p50_us = 335
p99_us = 591
p999_us = 655
p9999_us = 10495
max_us = 11048
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 28096562
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2809315
p50_us = 359
p99_us = 703
p999_us = 783
p9999_us = 10751
max_us = 11232
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 29218383
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2921522
p50_us = 343
p99_us = 607
p999_us = 687
p9999_us = 10751
max_us = 11121
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 29948225
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2994465
p50_us = 335
p99_us = 591
p999_us = 671
p9999_us = 10495
max_us = 11117
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 29517729
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2951442
p50_us = 359
p99_us = 623
p999_us = 719
p9999_us = 10495
max_us = 11273
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 29837556
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2983404
p50_us = 343
p99_us = 591
p999_us = 655
p9999_us = 10495
max_us = 10986
```

## pipelined 1:10 (M0 gate mix) m4 rep 6

```
ops = 30131838
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3012825
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11049
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 6

```
ops = 29725189
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2972188
p50_us = 343
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 10792
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 7

```
ops = 29496837
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2949343
p50_us = 343
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11015
```

## pipelined 1:10 (M0 gate mix) m4 rep 7

```
ops = 30119940
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3011668
p50_us = 343
p99_us = 543
p999_us = 607
p9999_us = 10495
max_us = 11318
```

## pipelined 1:10 (M0 gate mix) m4 rep 8

```
ops = 30354408
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3035081
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10495
max_us = 11076
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 8

```
ops = 30322626
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3031936
p50_us = 335
p99_us = 495
p999_us = 575
p9999_us = 10495
max_us = 10817
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 9

```
ops = 28959579
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2895625
p50_us = 351
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11031
```

## pipelined 1:10 (M0 gate mix) m4 rep 9

```
ops = 30325545
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3032213
p50_us = 335
p99_us = 527
p999_us = 591
p9999_us = 10495
max_us = 11256
```

## pipelined 1:10 (M0 gate mix) m4 rep 10

```
ops = 29861980
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2985857
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11380
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 10

```
ops = 29830241
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2982687
p50_us = 335
p99_us = 591
p999_us = 655
p9999_us = 10495
max_us = 11056
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 11

```
ops = 29784995
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2978154
p50_us = 343
p99_us = 591
p999_us = 719
p9999_us = 10495
max_us = 11148
```

## pipelined 1:10 (M0 gate mix) m4 rep 11

```
ops = 30069998
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3006600
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 11104
```

## pipelined 1:10 (M0 gate mix) m4 rep 12

```
ops = 29008054
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2900488
p50_us = 343
p99_us = 639
p999_us = 719
p9999_us = 10495
max_us = 11195
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 12

```
ops = 29933645
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2992991
p50_us = 359
p99_us = 575
p999_us = 687
p9999_us = 10495
max_us = 10935
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 13

```
ops = 29807333
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2980384
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11841
```

## pipelined 1:10 (M0 gate mix) m4 rep 13

```
ops = 29677185
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2967372
p50_us = 335
p99_us = 623
p999_us = 687
p9999_us = 10495
max_us = 11053
```

## pipelined 1:10 (M0 gate mix) m4 rep 14

```
ops = 30283228
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3027976
p50_us = 335
p99_us = 527
p999_us = 607
p9999_us = 10495
max_us = 10734
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 14

```
ops = 29787684
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2978388
p50_us = 335
p99_us = 591
p999_us = 671
p9999_us = 10239
max_us = 11008
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 15

```
ops = 30245458
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3024123
p50_us = 335
p99_us = 543
p999_us = 623
p9999_us = 10239
max_us = 11870
```

## pipelined 1:10 (M0 gate mix) m4 rep 15

```
ops = 30336360
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3033303
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10751
max_us = 10982
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3909649
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 780869
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4129
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3925933
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784038
p50_us = 639
p99_us = 1023
p999_us = 1311
p9999_us = 3327
max_us = 4159
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3936882
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786218
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3263
max_us = 4120
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3925076
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783776
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4365
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3920309
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 782879
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 4315
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3924408
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783697
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3391
max_us = 4151
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3920547
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783014
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 5526
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3917833
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 782396
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4160
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3921005
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783044
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3455
max_us = 4426
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3915294
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 781954
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3327
max_us = 4106
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3917585
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 782383
p50_us = 639
p99_us = 1087
p999_us = 1311
p9999_us = 3391
max_us = 4302
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3916286
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 781996
p50_us = 639
p99_us = 1087
p999_us = 1311
p9999_us = 3327
max_us = 4277
```

## unpipelined 512-conn (M0 gate mix) m4 rep 6

```
ops = 3939498
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 786524
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3327
max_us = 4252
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 6

```
ops = 3924032
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 783322
p50_us = 639
p99_us = 1023
p999_us = 1279
p9999_us = 3327
max_us = 4209
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 7

```
ops = 3938887
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786637
p50_us = 639
p99_us = 1055
p999_us = 1151
p9999_us = 3327
max_us = 4873
```

## unpipelined 512-conn (M0 gate mix) m4 rep 7

```
ops = 3916008
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 781982
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 4296
```

## unpipelined 512-conn (M0 gate mix) m4 rep 8

```
ops = 3926083
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783938
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3391
max_us = 4098
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 8

```
ops = 3934942
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785851
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 4243
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 9

```
ops = 3934833
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785852
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4171
```

## unpipelined 512-conn (M0 gate mix) m4 rep 9

```
ops = 3913209
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 781289
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3327
max_us = 4264
```

## unpipelined 512-conn (M0 gate mix) m4 rep 10

```
ops = 3923035
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783492
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 5446
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 10

```
ops = 3924557
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783603
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4267
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 11

```
ops = 3930375
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784805
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3327
max_us = 4248
```

## unpipelined 512-conn (M0 gate mix) m4 rep 11

```
ops = 3933796
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785660
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3455
max_us = 4219
```

## unpipelined 512-conn (M0 gate mix) m4 rep 12

```
ops = 3930863
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 784644
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3327
max_us = 4141
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 12

```
ops = 3939066
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786643
p50_us = 639
p99_us = 1055
p999_us = 1151
p9999_us = 3327
max_us = 4700
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 13

```
ops = 3939447
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 786679
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4415
```

## unpipelined 512-conn (M0 gate mix) m4 rep 13

```
ops = 3937125
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786281
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3391
max_us = 4185
```

## unpipelined 512-conn (M0 gate mix) m4 rep 14

```
ops = 3934643
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785751
p50_us = 639
p99_us = 1055
p999_us = 1151
p9999_us = 3391
max_us = 4555
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 14

```
ops = 3930818
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785001
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3391
max_us = 4247
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 15

```
ops = 3918021
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 782449
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4374
```

## unpipelined 512-conn (M0 gate mix) m4 rep 15

```
ops = 3928308
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784495
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 4237
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 25910355
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2590760
p50_us = 375
p99_us = 671
p999_us = 3583
p9999_us = 18431
max_us = 19096
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 25398213
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2539530
p50_us = 383
p99_us = 671
p999_us = 4351
p9999_us = 16383
max_us = 17453
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 25999389
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2599640
p50_us = 383
p99_us = 623
p999_us = 2687
p9999_us = 17407
max_us = 17641
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 24873431
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2487059
p50_us = 399
p99_us = 735
p999_us = 4735
p9999_us = 15359
max_us = 16650
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 25852557
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2584971
p50_us = 375
p99_us = 655
p999_us = 3391
p9999_us = 18943
max_us = 19413
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24544553
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2454108
p50_us = 407
p99_us = 735
p999_us = 9983
p9999_us = 15359
max_us = 25452
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 25862968
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2586005
p50_us = 383
p99_us = 607
p999_us = 3711
p9999_us = 19967
max_us = 30157
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24744199
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2474114
p50_us = 399
p99_us = 751
p999_us = 4479
p9999_us = 15615
max_us = 17746
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 25079363
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2507626
p50_us = 383
p99_us = 719
p999_us = 2815
p9999_us = 16895
max_us = 18293
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24979892
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2497665
p50_us = 391
p99_us = 687
p999_us = 5119
p9999_us = 17407
max_us = 18221
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25522126
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2551930
p50_us = 383
p99_us = 719
p999_us = 3839
p9999_us = 17919
max_us = 18484
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 24904844
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2490145
p50_us = 383
p99_us = 719
p999_us = 5375
p9999_us = 18431
max_us = 19378
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 6

```
ops = 25639643
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2563628
p50_us = 391
p99_us = 703
p999_us = 3711
p9999_us = 17407
max_us = 18125
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 6

```
ops = 25088668
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2508544
p50_us = 391
p99_us = 703
p999_us = 4031
p9999_us = 14591
max_us = 24737
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 7

```
ops = 25801868
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2579880
p50_us = 383
p99_us = 671
p999_us = 3391
p9999_us = 16895
max_us = 17570
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 7

```
ops = 25251085
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2524788
p50_us = 391
p99_us = 655
p999_us = 4735
p9999_us = 17919
max_us = 18779
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 8

```
ops = 25388820
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2538590
p50_us = 383
p99_us = 735
p999_us = 4031
p9999_us = 17919
max_us = 18584
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 8

```
ops = 25020407
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2501751
p50_us = 391
p99_us = 687
p999_us = 4607
p9999_us = 17407
max_us = 18652
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 9

```
ops = 25443222
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2543991
p50_us = 383
p99_us = 687
p999_us = 2687
p9999_us = 18431
max_us = 19517
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 9

```
ops = 24708875
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2470550
p50_us = 383
p99_us = 735
p999_us = 4991
p9999_us = 17919
max_us = 21241
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 10

```
ops = 25713825
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2571054
p50_us = 383
p99_us = 639
p999_us = 2879
p9999_us = 18943
max_us = 19771
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 10

```
ops = 24839903
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2483695
p50_us = 399
p99_us = 703
p999_us = 4735
p9999_us = 14591
max_us = 17114
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 11

```
ops = 25964194
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2596066
p50_us = 383
p99_us = 623
p999_us = 1087
p9999_us = 14335
max_us = 15307
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 11

```
ops = 25163361
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2516051
p50_us = 391
p99_us = 639
p999_us = 4735
p9999_us = 17919
max_us = 18413
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 12

```
ops = 25654110
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2565145
p50_us = 391
p99_us = 655
p999_us = 2751
p9999_us = 16895
max_us = 18714
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 12

```
ops = 24332701
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2432973
p50_us = 391
p99_us = 767
p999_us = 4863
p9999_us = 17407
max_us = 18160
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 13

```
ops = 25821264
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2581791
p50_us = 383
p99_us = 639
p999_us = 2495
p9999_us = 16127
max_us = 17084
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 13

```
ops = 25246247
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2524307
p50_us = 391
p99_us = 703
p999_us = 4351
p9999_us = 14591
max_us = 16139
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 14

```
ops = 25856611
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2585368
p50_us = 383
p99_us = 607
p999_us = 3839
p9999_us = 18943
max_us = 19878
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 14

```
ops = 25040077
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2503755
p50_us = 383
p99_us = 703
p999_us = 4607
p9999_us = 17407
max_us = 18236
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 15

```
ops = 25835405
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2583251
p50_us = 383
p99_us = 639
p999_us = 2367
p9999_us = 16895
max_us = 17430
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 15

```
ops = 25187937
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2518456
p50_us = 391
p99_us = 655
p999_us = 4607
p9999_us = 16383
max_us = 19561
```
