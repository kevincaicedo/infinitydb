# M4 gate-run report

date: 1786845205 (unix) · cells: 4 · duration: 10s · replicates: 16 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 2991381 ops/s (spread 3.74%) vs m4 2998486 ops/s (spread 4.36%) — signed ops delta +0.24% · p999 655 → 655 µs (+0.00%) · peak-RSS 187904000 → 187977728 B (+0.04%)
- unpipelined 512-conn (M0 gate mix): m3 784221 ops/s (spread 0.64%) vs m4 784228 ops/s (spread 1.14%) — signed ops delta +0.00% · p999 1183 → 1215 µs (+2.70%) · peak-RSS 119554048 → 119595008 B (+0.03%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2522770 ops/s (spread 5.66%) vs m4 2534857 ops/s (spread 6.58%) — signed ops delta +0.48% · p999 4351 → 4351 µs (+0.00%) · peak-RSS 239292416 → 239955968 B (+0.28%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.70 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.28 | PASS |
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
ops = 30381046
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3037662
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10495
max_us = 10862
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 29934962
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2993143
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 10819
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 29886059
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2988202
p50_us = 351
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 11197
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 30106236
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3010184
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 11364
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 30104225
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3010062
p50_us = 343
p99_us = 527
p999_us = 607
p9999_us = 10495
max_us = 10826
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 29984239
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2998081
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 13425
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 29747528
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2974388
p50_us = 335
p99_us = 575
p999_us = 671
p9999_us = 10495
max_us = 10904
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 29810453
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2980702
p50_us = 343
p99_us = 559
p999_us = 639
p9999_us = 10751
max_us = 11192
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 30040286
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3003661
p50_us = 335
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 10872
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 29917496
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2991381
p50_us = 343
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 20812
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 29787483
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2978402
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11521
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 30168737
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3016518
p50_us = 351
p99_us = 559
p999_us = 687
p9999_us = 10495
max_us = 10767
```

## pipelined 1:10 (M0 gate mix) m4 rep 6

```
ops = 29071657
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2906840
p50_us = 343
p99_us = 671
p999_us = 751
p9999_us = 10495
max_us = 11072
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 6

```
ops = 29471179
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2946728
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11133
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 7

```
ops = 30299485
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3029593
p50_us = 335
p99_us = 495
p999_us = 575
p9999_us = 10495
max_us = 11110
```

## pipelined 1:10 (M0 gate mix) m4 rep 7

```
ops = 29823532
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2981994
p50_us = 359
p99_us = 575
p999_us = 687
p9999_us = 10495
max_us = 11042
```

## pipelined 1:10 (M0 gate mix) m4 rep 8

```
ops = 29960730
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2995718
p50_us = 343
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 10984
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 8

```
ops = 29839285
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2983567
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 11195
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 9

```
ops = 29565181
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2956190
p50_us = 367
p99_us = 559
p999_us = 687
p9999_us = 10495
max_us = 10966
```

## pipelined 1:10 (M0 gate mix) m4 rep 9

```
ops = 29527537
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2952430
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11026
```

## pipelined 1:10 (M0 gate mix) m4 rep 10

```
ops = 29919642
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2991603
p50_us = 343
p99_us = 591
p999_us = 687
p9999_us = 10751
max_us = 11055
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 10

```
ops = 29181435
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2917785
p50_us = 343
p99_us = 639
p999_us = 719
p9999_us = 10495
max_us = 11070
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 11

```
ops = 30293990
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3029018
p50_us = 335
p99_us = 511
p999_us = 591
p9999_us = 10495
max_us = 10976
```

## pipelined 1:10 (M0 gate mix) m4 rep 11

```
ops = 30037723
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3003408
p50_us = 343
p99_us = 527
p999_us = 607
p9999_us = 10495
max_us = 11057
```

## pipelined 1:10 (M0 gate mix) m4 rep 12

```
ops = 29949747
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2994536
p50_us = 335
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 10997
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 12

```
ops = 30089493
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3008576
p50_us = 343
p99_us = 527
p999_us = 607
p9999_us = 10495
max_us = 10933
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 13

```
ops = 30091146
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3008763
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11093
```

## pipelined 1:10 (M0 gate mix) m4 rep 13

```
ops = 30237051
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3023289
p50_us = 335
p99_us = 503
p999_us = 591
p9999_us = 10495
max_us = 10991
```

## pipelined 1:10 (M0 gate mix) m4 rep 14

```
ops = 29988414
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2998486
p50_us = 343
p99_us = 591
p999_us = 671
p9999_us = 10495
max_us = 13144
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 14

```
ops = 30026442
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3002244
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11062
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 15

```
ops = 29815243
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2981167
p50_us = 343
p99_us = 607
p999_us = 703
p9999_us = 10495
max_us = 11311
```

## pipelined 1:10 (M0 gate mix) m4 rep 15

```
ops = 29884615
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2988086
p50_us = 335
p99_us = 591
p999_us = 655
p9999_us = 10495
max_us = 11096
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3895481
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 777961
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3263
max_us = 4069
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3937105
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 786179
p50_us = 639
p99_us = 1023
p999_us = 1151
p9999_us = 3327
max_us = 4406
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3927850
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 784106
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 5664
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3926638
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784180
p50_us = 639
p99_us = 1055
p999_us = 1311
p9999_us = 3391
max_us = 4376
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3927002
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784125
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3455
max_us = 4674
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3936432
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786187
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4754
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3926755
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784199
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4539
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3930840
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784927
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4056
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3925453
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783840
p50_us = 639
p99_us = 1023
p999_us = 1279
p9999_us = 3391
max_us = 4288
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3914116
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 781506
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4179
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3927270
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784221
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4141
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3920738
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783025
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3327
max_us = 4109
```

## unpipelined 512-conn (M0 gate mix) m4 rep 6

```
ops = 3924050
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783671
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3327
max_us = 4201
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 6

```
ops = 3931435
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 785055
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4285
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 7

```
ops = 3928112
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 784275
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 6162
```

## unpipelined 512-conn (M0 gate mix) m4 rep 7

```
ops = 3927116
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784228
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 6015
```

## unpipelined 512-conn (M0 gate mix) m4 rep 8

```
ops = 3940284
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 786833
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4217
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 8

```
ops = 3925627
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784012
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 4192
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 9

```
ops = 3922929
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783415
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 4183
```

## unpipelined 512-conn (M0 gate mix) m4 rep 9

```
ops = 3940355
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786918
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3455
max_us = 4452
```

## unpipelined 512-conn (M0 gate mix) m4 rep 10

```
ops = 3935415
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 785877
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 4686
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 10

```
ops = 3938352
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 786546
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4227
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 11

```
ops = 3933589
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 785578
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3263
max_us = 4087
```

## unpipelined 512-conn (M0 gate mix) m4 rep 11

```
ops = 3924614
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783742
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4167
```

## unpipelined 512-conn (M0 gate mix) m4 rep 12

```
ops = 3933468
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 785474
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3455
max_us = 6248
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 12

```
ops = 3928312
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784530
p50_us = 639
p99_us = 1023
p999_us = 1247
p9999_us = 3327
max_us = 4124
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 13

```
ops = 3920578
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783010
p50_us = 639
p99_us = 1087
p999_us = 1279
p9999_us = 3455
max_us = 4523
```

## unpipelined 512-conn (M0 gate mix) m4 rep 13

```
ops = 3924259
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783687
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4246
```

## unpipelined 512-conn (M0 gate mix) m4 rep 14

```
ops = 3927321
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 784282
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4232
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 14

```
ops = 3918180
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 782380
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3327
max_us = 5672
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 15

```
ops = 3923257
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 783307
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4271
```

## unpipelined 512-conn (M0 gate mix) m4 rep 15

```
ops = 3929629
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784773
p50_us = 639
p99_us = 1023
p999_us = 1215
p9999_us = 3327
max_us = 4176
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 25713330
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2571021
p50_us = 383
p99_us = 671
p999_us = 3583
p9999_us = 18431
max_us = 19230
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 25289520
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2528607
p50_us = 391
p99_us = 655
p999_us = 4351
p9999_us = 14335
max_us = 17051
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 25285321
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2528240
p50_us = 383
p99_us = 719
p999_us = 3775
p9999_us = 17919
max_us = 18564
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 25351982
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2534857
p50_us = 391
p99_us = 623
p999_us = 4607
p9999_us = 17407
max_us = 18502
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 25595260
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2559200
p50_us = 383
p99_us = 655
p999_us = 6143
p9999_us = 20991
max_us = 21488
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24968571
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2496546
p50_us = 391
p99_us = 655
p999_us = 6015
p9999_us = 19455
max_us = 20602
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 25752399
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2574907
p50_us = 383
p99_us = 671
p999_us = 1823
p9999_us = 15359
max_us = 17511
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 25068122
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2506468
p50_us = 391
p99_us = 671
p999_us = 4607
p9999_us = 14591
max_us = 15702
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 25721934
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2571890
p50_us = 383
p99_us = 639
p999_us = 3007
p9999_us = 17407
max_us = 17718
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 25145826
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2514273
p50_us = 391
p99_us = 639
p999_us = 4607
p9999_us = 17407
max_us = 25442
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25171177
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2516811
p50_us = 399
p99_us = 671
p999_us = 3263
p9999_us = 16895
max_us = 17346
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 25030546
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2502737
p50_us = 391
p99_us = 687
p999_us = 4351
p9999_us = 15359
max_us = 15973
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 6

```
ops = 25889434
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2588623
p50_us = 383
p99_us = 607
p999_us = 3455
p9999_us = 17407
max_us = 17893
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 6

```
ops = 25027340
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2502427
p50_us = 391
p99_us = 703
p999_us = 4735
p9999_us = 16383
max_us = 17448
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 7

```
ops = 25661090
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2565785
p50_us = 391
p99_us = 655
p999_us = 1567
p9999_us = 14591
max_us = 15625
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 7

```
ops = 25132599
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2512959
p50_us = 391
p99_us = 687
p999_us = 4351
p9999_us = 14335
max_us = 15081
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 8

```
ops = 25863830
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2585994
p50_us = 383
p99_us = 607
p999_us = 3391
p9999_us = 17407
max_us = 18350
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 8

```
ops = 24323798
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2432076
p50_us = 415
p99_us = 751
p999_us = 10239
p9999_us = 15615
max_us = 25103
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 9

```
ops = 25621988
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2561873
p50_us = 383
p99_us = 687
p999_us = 3327
p9999_us = 18943
max_us = 19560
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 9

```
ops = 24964581
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2496142
p50_us = 391
p99_us = 735
p999_us = 4351
p9999_us = 14591
max_us = 15147
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 10

```
ops = 25191107
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2518792
p50_us = 391
p99_us = 687
p999_us = 10751
p9999_us = 14591
max_us = 15203
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 10

```
ops = 24922984
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2491986
p50_us = 391
p99_us = 719
p999_us = 4351
p9999_us = 15615
max_us = 16793
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 11

```
ops = 25155262
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2515178
p50_us = 407
p99_us = 703
p999_us = 11263
p9999_us = 16383
max_us = 19815
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 11

```
ops = 24220508
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2421757
p50_us = 415
p99_us = 719
p999_us = 4223
p9999_us = 14591
max_us = 16968
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 12

```
ops = 25725344
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2572152
p50_us = 383
p99_us = 623
p999_us = 4479
p9999_us = 18431
max_us = 19559
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 12

```
ops = 24476796
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2447392
p50_us = 407
p99_us = 703
p999_us = 4607
p9999_us = 16895
max_us = 17644
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 13

```
ops = 25740517
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2573728
p50_us = 383
p99_us = 639
p999_us = 3647
p9999_us = 17407
max_us = 30019
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 13

```
ops = 24974811
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2497175
p50_us = 391
p99_us = 671
p999_us = 4351
p9999_us = 17407
max_us = 17901
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 14

```
ops = 25823192
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2581941
p50_us = 383
p99_us = 671
p999_us = 2175
p9999_us = 15615
max_us = 16591
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 14

```
ops = 25231255
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2522770
p50_us = 391
p99_us = 607
p999_us = 5119
p9999_us = 17407
max_us = 18346
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 15

```
ops = 25738006
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2573473
p50_us = 391
p99_us = 623
p999_us = 2815
p9999_us = 16895
max_us = 18177
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 15

```
ops = 24602309
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2459901
p50_us = 407
p99_us = 751
p999_us = 9983
p9999_us = 15359
max_us = 26480
```
