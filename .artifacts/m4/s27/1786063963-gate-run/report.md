# M4 gate-run report

date: 1786063963 (unix) · cells: 4 · duration: 10s · replicates: 8 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/0ff70128-f0f2-4226-9508-f52e3e4de141/scratchpad/bins/infinityd-s27: hash64:76e226a50381d437 (12688280 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/0ff70128-f0f2-4226-9508-f52e3e4de141/scratchpad/s27-baseline/target/release/infinityd: hash64:458d8ce79c0cac03 (12620880 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 2778834 ops/s (spread 4.62%) vs m4 2839428 ops/s (spread 3.83%) — signed ops delta +2.18% · p999 975 → 943 µs (-3.28%) · peak-RSS 186671104 → 187072512 B (+0.22%)
- unpipelined 512-conn (M0 gate mix): m3 689210 ops/s (spread 4.79%) vs m4 687420 ops/s (spread 2.56%) — signed ops delta -0.26% · p999 1919 → 1919 µs (+0.00%) · peak-RSS 116363264 → 116379648 B (+0.01%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2463696 ops/s (spread 6.40%) vs m4 2429109 ops/s (spread 2.48%) — signed ops delta -1.40% · p999 3519 → 2879 µs (-18.19%) · peak-RSS 237649920 → 236404736 B (-0.52%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.26 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 1.40 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.22 | PASS (DEV-TIER, non-binding) |
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
ops = 28430907
errors = 0
elapsed_s = 10.001
ops_per_sec = 2842745
p50_us = 351
p99_us = 671
p999_us = 911
p9999_us = 10751
max_us = 11042
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 28302520
errors = 0
elapsed_s = 10.001
ops_per_sec = 2829916
p50_us = 343
p99_us = 655
p999_us = 911
p9999_us = 10751
max_us = 11989
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 27791956
errors = 0
elapsed_s = 10.001
ops_per_sec = 2778834
p50_us = 351
p99_us = 687
p999_us = 975
p9999_us = 11007
max_us = 11821
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 27702233
errors = 0
elapsed_s = 10.001
ops_per_sec = 2769888
p50_us = 351
p99_us = 687
p999_us = 943
p9999_us = 11007
max_us = 11302
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 28531969
errors = 0
elapsed_s = 10.001
ops_per_sec = 2852856
p50_us = 343
p99_us = 671
p999_us = 911
p9999_us = 10751
max_us = 11133
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 27647483
errors = 0
elapsed_s = 10.001
ops_per_sec = 2764389
p50_us = 359
p99_us = 671
p999_us = 975
p9999_us = 10751
max_us = 11491
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 28889881
errors = 0
elapsed_s = 10.001
ops_per_sec = 2888605
p50_us = 343
p99_us = 639
p999_us = 863
p9999_us = 10495
max_us = 12503
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 28438848
errors = 0
elapsed_s = 10.001
ops_per_sec = 2843570
p50_us = 359
p99_us = 687
p999_us = 943
p9999_us = 10495
max_us = 20441
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 27444576
errors = 0
elapsed_s = 10.001
ops_per_sec = 2744162
p50_us = 367
p99_us = 687
p999_us = 943
p9999_us = 10751
max_us = 11330
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 27607982
errors = 0
elapsed_s = 10.001
ops_per_sec = 2760485
p50_us = 375
p99_us = 751
p999_us = 959
p9999_us = 10751
max_us = 11068
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 27645991
errors = 0
elapsed_s = 10.001
ops_per_sec = 2764266
p50_us = 351
p99_us = 687
p999_us = 975
p9999_us = 10751
max_us = 12168
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 28398109
errors = 0
elapsed_s = 10.001
ops_per_sec = 2839428
p50_us = 351
p99_us = 655
p999_us = 943
p9999_us = 10751
max_us = 11108
```

## pipelined 1:10 (M0 gate mix) m4 rep 6

```
ops = 27743024
errors = 0
elapsed_s = 10.001
ops_per_sec = 2773969
p50_us = 351
p99_us = 671
p999_us = 943
p9999_us = 11263
max_us = 12259
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 6

```
ops = 28568416
errors = 0
elapsed_s = 10.001
ops_per_sec = 2856510
p50_us = 343
p99_us = 655
p999_us = 959
p9999_us = 10751
max_us = 11757
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 7

```
ops = 27606490
errors = 0
elapsed_s = 10.001
ops_per_sec = 2760332
p50_us = 359
p99_us = 687
p999_us = 975
p9999_us = 11519
max_us = 13972
```

## pipelined 1:10 (M0 gate mix) m4 rep 7

```
ops = 28049545
errors = 0
elapsed_s = 10.001
ops_per_sec = 2804597
p50_us = 359
p99_us = 655
p999_us = 879
p9999_us = 10495
max_us = 10979
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3377844
errors = 0
elapsed_s = 5.007
ops_per_sec = 674617
p50_us = 719
p99_us = 1535
p999_us = 1951
p9999_us = 5119
max_us = 7138
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3494778
errors = 0
elapsed_s = 5.009
ops_per_sec = 697708
p50_us = 703
p99_us = 1503
p999_us = 1855
p9999_us = 3583
max_us = 4627
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3453057
errors = 0
elapsed_s = 5.009
ops_per_sec = 689390
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 3967
max_us = 5611
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3457257
errors = 0
elapsed_s = 5.008
ops_per_sec = 690377
p50_us = 703
p99_us = 1535
p999_us = 1887
p9999_us = 3519
max_us = 4529
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3411862
errors = 0
elapsed_s = 5.007
ops_per_sec = 681397
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 3839
max_us = 5443
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3363815
errors = 0
elapsed_s = 5.008
ops_per_sec = 671749
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 3583
max_us = 4354
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3450922
errors = 0
elapsed_s = 5.007
ops_per_sec = 689210
p50_us = 703
p99_us = 1503
p999_us = 1919
p9999_us = 3647
max_us = 4562
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3412014
errors = 0
elapsed_s = 5.008
ops_per_sec = 681340
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 4607
max_us = 6164
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3380267
errors = 0
elapsed_s = 5.007
ops_per_sec = 675093
p50_us = 719
p99_us = 1535
p999_us = 1919
p9999_us = 4351
max_us = 6554
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3500897
errors = 0
elapsed_s = 5.007
ops_per_sec = 699155
p50_us = 703
p99_us = 1503
p999_us = 1887
p9999_us = 3455
max_us = 4648
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3336067
errors = 0
elapsed_s = 5.008
ops_per_sec = 666126
p50_us = 735
p99_us = 1535
p999_us = 1887
p9999_us = 3711
max_us = 4601
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3451744
errors = 0
elapsed_s = 5.008
ops_per_sec = 689283
p50_us = 703
p99_us = 1535
p999_us = 1887
p9999_us = 3519
max_us = 4434
```

## unpipelined 512-conn (M0 gate mix) m4 rep 6

```
ops = 3442076
errors = 0
elapsed_s = 5.007
ops_per_sec = 687420
p50_us = 703
p99_us = 1535
p999_us = 1919
p9999_us = 3711
max_us = 7087
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 6

```
ops = 3418505
errors = 0
elapsed_s = 5.008
ops_per_sec = 682598
p50_us = 719
p99_us = 1503
p999_us = 1887
p9999_us = 3583
max_us = 4327
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 7

```
ops = 3335934
errors = 0
elapsed_s = 5.007
ops_per_sec = 666196
p50_us = 735
p99_us = 1567
p999_us = 2047
p9999_us = 4863
max_us = 6913
```

## unpipelined 512-conn (M0 gate mix) m4 rep 7

```
ops = 3466138
errors = 0
elapsed_s = 5.007
ops_per_sec = 692206
p50_us = 703
p99_us = 1535
p999_us = 1887
p9999_us = 3775
max_us = 4750
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 24574289
errors = 0
elapsed_s = 10.001
ops_per_sec = 2457158
p50_us = 391
p99_us = 783
p999_us = 3775
p9999_us = 17919
max_us = 20778
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 24639793
errors = 0
elapsed_s = 10.001
ops_per_sec = 2463696
p50_us = 391
p99_us = 751
p999_us = 1695
p9999_us = 13311
max_us = 13867
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 24604046
errors = 0
elapsed_s = 10.001
ops_per_sec = 2460096
p50_us = 391
p99_us = 783
p999_us = 2047
p9999_us = 14591
max_us = 15997
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 24167930
errors = 0
elapsed_s = 10.001
ops_per_sec = 2416467
p50_us = 399
p99_us = 831
p999_us = 2559
p9999_us = 14591
max_us = 15608
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 23973165
errors = 0
elapsed_s = 10.001
ops_per_sec = 2397041
p50_us = 399
p99_us = 783
p999_us = 4991
p9999_us = 17407
max_us = 18218
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24823566
errors = 0
elapsed_s = 10.001
ops_per_sec = 2482079
p50_us = 391
p99_us = 735
p999_us = 3519
p9999_us = 17407
max_us = 18749
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 24230532
errors = 0
elapsed_s = 10.001
ops_per_sec = 2422705
p50_us = 391
p99_us = 783
p999_us = 5503
p9999_us = 19455
max_us = 20312
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24293814
errors = 0
elapsed_s = 10.001
ops_per_sec = 2429109
p50_us = 407
p99_us = 783
p999_us = 2111
p9999_us = 14079
max_us = 15100
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 23972347
errors = 0
elapsed_s = 10.001
ops_per_sec = 2396953
p50_us = 399
p99_us = 815
p999_us = 2879
p9999_us = 15359
max_us = 16307
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24961270
errors = 0
elapsed_s = 10.001
ops_per_sec = 2495814
p50_us = 391
p99_us = 751
p999_us = 1823
p9999_us = 13823
max_us = 17157
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25022037
errors = 0
elapsed_s = 10.001
ops_per_sec = 2501895
p50_us = 383
p99_us = 751
p999_us = 3839
p9999_us = 16895
max_us = 19788
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 24086234
errors = 0
elapsed_s = 10.001
ops_per_sec = 2408294
p50_us = 399
p99_us = 799
p999_us = 1951
p9999_us = 13823
max_us = 23119
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 6

```
ops = 24508898
errors = 0
elapsed_s = 10.001
ops_per_sec = 2450574
p50_us = 399
p99_us = 783
p999_us = 3967
p9999_us = 16895
max_us = 17887
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 6

```
ops = 24523861
errors = 0
elapsed_s = 10.001
ops_per_sec = 2452078
p50_us = 391
p99_us = 767
p999_us = 2303
p9999_us = 15103
max_us = 26545
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 7

```
ops = 23445918
errors = 0
elapsed_s = 10.001
ops_per_sec = 2344321
p50_us = 415
p99_us = 831
p999_us = 4607
p9999_us = 16895
max_us = 17632
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 7

```
ops = 24421660
errors = 0
elapsed_s = 10.001
ops_per_sec = 2441853
p50_us = 399
p99_us = 767
p999_us = 1855
p9999_us = 13567
max_us = 14125
```
