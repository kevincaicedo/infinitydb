# M4 gate-run report

date: 1784427397 (unix) · cells: 4 · duration: 10s · replicates: 9 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/4647ffef-5bfd-4608-a978-9262faea01f7/scratchpad/bin/infinityd: hash64:1240b1a264960461 (10593368 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/146c45b6-8d4c-463b-a1e1-345fa4ab8b4c/scratchpad/m3-baseline/target/release/infinityd: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m3 2847038 ops/s (spread 5.21%) vs m4 2867758 ops/s (spread 2.64%) — signed ops delta +0.73% · p999 687 → 703 µs (+2.33%) · peak-RSS 188870656 → 189075456 B (+0.11%)
- unpipelined 512-conn (M0 gate mix): m3 720358 ops/s (spread 21.62%) vs m4 727605 ops/s (spread 2.50%) — signed ops delta +1.01% · p999 1279 → 1311 µs (+2.50%) · peak-RSS 120066048 → 120606720 B (+0.45%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2421688 ops/s (spread 58.60%) vs m4 2408902 ops/s (spread 6.05%) — signed ops delta -0.53% · p999 4607 → 4479 µs (-2.78%) · peak-RSS 237359104 → 237948928 B (+0.25%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 2.33 | FAIL |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 2.50 | FAIL |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.53 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.45 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 28014227
errors = 0
elapsed_s = 10.001
ops_per_sec = 2801013
p50_us = 351
p99_us = 671
p999_us = 783
p9999_us = 10751
max_us = 11643
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 29168026
errors = 0
elapsed_s = 10.001
ops_per_sec = 2916454
p50_us = 343
p99_us = 607
p999_us = 703
p9999_us = 11007
max_us = 21707
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 28624091
errors = 0
elapsed_s = 10.002
ops_per_sec = 2861952
p50_us = 351
p99_us = 623
p999_us = 703
p9999_us = 831
max_us = 2801
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 28871944
errors = 0
elapsed_s = 10.001
ops_per_sec = 2886845
p50_us = 351
p99_us = 591
p999_us = 655
p9999_us = 767
max_us = 1839
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 28837101
errors = 0
elapsed_s = 10.001
ops_per_sec = 2883325
p50_us = 351
p99_us = 559
p999_us = 655
p9999_us = 783
max_us = 4091
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 28410370
errors = 0
elapsed_s = 10.001
ops_per_sec = 2840679
p50_us = 351
p99_us = 639
p999_us = 735
p9999_us = 879
max_us = 3346
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 28873335
errors = 0
elapsed_s = 10.001
ops_per_sec = 2886933
p50_us = 359
p99_us = 575
p999_us = 655
p9999_us = 751
max_us = 3271
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 28681150
errors = 0
elapsed_s = 10.001
ops_per_sec = 2867728
p50_us = 359
p99_us = 575
p999_us = 655
p9999_us = 799
max_us = 4334
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 28553543
errors = 0
elapsed_s = 10.001
ops_per_sec = 2854978
p50_us = 359
p99_us = 607
p999_us = 687
p9999_us = 831
max_us = 3890
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 28692443
errors = 0
elapsed_s = 10.001
ops_per_sec = 2868873
p50_us = 351
p99_us = 623
p999_us = 703
p9999_us = 831
max_us = 3721
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 28721784
errors = 0
elapsed_s = 10.001
ops_per_sec = 2871864
p50_us = 359
p99_us = 575
p999_us = 687
p9999_us = 895
max_us = 4480
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 28397950
errors = 0
elapsed_s = 10.001
ops_per_sec = 2839470
p50_us = 359
p99_us = 591
p999_us = 687
p9999_us = 831
max_us = 3977
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 6

```
ops = 27389998
errors = 0
elapsed_s = 10.001
ops_per_sec = 2738623
p50_us = 391
p99_us = 655
p999_us = 751
p9999_us = 1151
max_us = 4926
```

## pipelined 1:10 (M0 gate mix) m4 rep 6

```
ops = 28537149
errors = 0
elapsed_s = 10.001
ops_per_sec = 2853324
p50_us = 383
p99_us = 591
p999_us = 703
p9999_us = 847
max_us = 4962
```

## pipelined 1:10 (M0 gate mix) m4 rep 7

```
ops = 28613676
errors = 0
elapsed_s = 10.001
ops_per_sec = 2860972
p50_us = 359
p99_us = 591
p999_us = 655
p9999_us = 751
max_us = 2612
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 7

```
ops = 28474010
errors = 0
elapsed_s = 10.001
ops_per_sec = 2847038
p50_us = 359
p99_us = 575
p999_us = 671
p9999_us = 847
max_us = 3098
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 8

```
ops = 27809449
errors = 0
elapsed_s = 10.001
ops_per_sec = 2780643
p50_us = 367
p99_us = 639
p999_us = 751
p9999_us = 927
max_us = 3933
```

## pipelined 1:10 (M0 gate mix) m4 rep 8

```
ops = 28680591
errors = 0
elapsed_s = 10.001
ops_per_sec = 2867758
p50_us = 359
p99_us = 591
p999_us = 655
p9999_us = 751
max_us = 3804
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3599944
errors = 0
elapsed_s = 5.008
ops_per_sec = 718868
p50_us = 703
p99_us = 1151
p999_us = 1567
p9999_us = 3519
max_us = 5087
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3610851
errors = 0
elapsed_s = 5.008
ops_per_sec = 720967
p50_us = 687
p99_us = 1151
p999_us = 1439
p9999_us = 3647
max_us = 4702
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3588655
errors = 0
elapsed_s = 5.008
ops_per_sec = 716618
p50_us = 703
p99_us = 1151
p999_us = 1279
p9999_us = 1695
max_us = 4089
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3594069
errors = 0
elapsed_s = 5.009
ops_per_sec = 717571
p50_us = 703
p99_us = 1119
p999_us = 1279
p9999_us = 1727
max_us = 4413
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3588824
errors = 0
elapsed_s = 5.008
ops_per_sec = 716676
p50_us = 703
p99_us = 1151
p999_us = 1279
p9999_us = 1695
max_us = 4405
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3574664
errors = 0
elapsed_s = 5.007
ops_per_sec = 713920
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 1759
max_us = 4193
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3597143
errors = 0
elapsed_s = 5.008
ops_per_sec = 718339
p50_us = 703
p99_us = 1119
p999_us = 1279
p9999_us = 2431
max_us = 6246
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3608488
errors = 0
elapsed_s = 5.009
ops_per_sec = 720358
p50_us = 703
p99_us = 1119
p999_us = 1279
p9999_us = 1631
max_us = 4224
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 2882852
errors = 0
elapsed_s = 5.008
ops_per_sec = 575604
p50_us = 799
p99_us = 3135
p999_us = 10751
p9999_us = 14591
max_us = 25308
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3657565
errors = 0
elapsed_s = 5.008
ops_per_sec = 730324
p50_us = 687
p99_us = 1151
p999_us = 1343
p9999_us = 1791
max_us = 4186
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3646166
errors = 0
elapsed_s = 5.009
ops_per_sec = 727883
p50_us = 703
p99_us = 1151
p999_us = 1311
p9999_us = 2367
max_us = 6275
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3648729
errors = 0
elapsed_s = 5.007
ops_per_sec = 728654
p50_us = 687
p99_us = 1151
p999_us = 1311
p9999_us = 1759
max_us = 4157
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 6

```
ops = 3655995
errors = 0
elapsed_s = 5.009
ops_per_sec = 729917
p50_us = 703
p99_us = 1119
p999_us = 1279
p9999_us = 1631
max_us = 3398
```

## unpipelined 512-conn (M0 gate mix) m4 rep 6

```
ops = 3643728
errors = 0
elapsed_s = 5.008
ops_per_sec = 727605
p50_us = 703
p99_us = 1151
p999_us = 1343
p9999_us = 2047
max_us = 6937
```

## unpipelined 512-conn (M0 gate mix) m4 rep 7

```
ops = 3664989
errors = 0
elapsed_s = 5.007
ops_per_sec = 731914
p50_us = 687
p99_us = 1151
p999_us = 1279
p9999_us = 1631
max_us = 4279
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 7

```
ops = 3662408
errors = 0
elapsed_s = 5.008
ops_per_sec = 731352
p50_us = 703
p99_us = 1119
p999_us = 1247
p9999_us = 1631
max_us = 4920
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 8

```
ops = 3615399
errors = 0
elapsed_s = 5.007
ops_per_sec = 722060
p50_us = 703
p99_us = 1183
p999_us = 1375
p9999_us = 2687
max_us = 9000
```

## unpipelined 512-conn (M0 gate mix) m4 rep 8

```
ops = 3666730
errors = 0
elapsed_s = 5.008
ops_per_sec = 732141
p50_us = 687
p99_us = 1151
p999_us = 1279
p9999_us = 1631
max_us = 4241
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 10255468
errors = 0
elapsed_s = 10.001
ops_per_sec = 1025399
p50_us = 879
p99_us = 2111
p999_us = 6399
p9999_us = 10239
max_us = 10662
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 23146147
errors = 0
elapsed_s = 10.001
ops_per_sec = 2314352
p50_us = 415
p99_us = 799
p999_us = 5887
p9999_us = 17407
max_us = 17783
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 24603922
errors = 0
elapsed_s = 10.001
ops_per_sec = 2460108
p50_us = 399
p99_us = 687
p999_us = 4351
p9999_us = 18431
max_us = 21208
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 23868792
errors = 0
elapsed_s = 10.001
ops_per_sec = 2386608
p50_us = 407
p99_us = 719
p999_us = 6655
p9999_us = 17919
max_us = 19345
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24220191
errors = 0
elapsed_s = 10.001
ops_per_sec = 2421688
p50_us = 399
p99_us = 703
p999_us = 4031
p9999_us = 18431
max_us = 35445
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 24375918
errors = 0
elapsed_s = 10.001
ops_per_sec = 2437301
p50_us = 399
p99_us = 671
p999_us = 4351
p9999_us = 18943
max_us = 21969
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 24092047
errors = 0
elapsed_s = 10.001
ops_per_sec = 2408902
p50_us = 407
p99_us = 751
p999_us = 4479
p9999_us = 18943
max_us = 22334
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 23834682
errors = 0
elapsed_s = 10.001
ops_per_sec = 2383159
p50_us = 415
p99_us = 687
p999_us = 4991
p9999_us = 17919
max_us = 22303
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24339788
errors = 0
elapsed_s = 10.001
ops_per_sec = 2433655
p50_us = 399
p99_us = 687
p999_us = 3711
p9999_us = 18431
max_us = 31832
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 24228766
errors = 0
elapsed_s = 10.001
ops_per_sec = 2422559
p50_us = 407
p99_us = 687
p999_us = 4479
p9999_us = 18431
max_us = 22629
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 24430040
errors = 0
elapsed_s = 10.001
ops_per_sec = 2442673
p50_us = 399
p99_us = 687
p999_us = 4479
p9999_us = 18943
max_us = 38994
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 24297876
errors = 0
elapsed_s = 10.001
ops_per_sec = 2429516
p50_us = 399
p99_us = 719
p999_us = 4607
p9999_us = 18943
max_us = 22707
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 6

```
ops = 24448808
errors = 0
elapsed_s = 10.001
ops_per_sec = 2444592
p50_us = 399
p99_us = 687
p999_us = 4351
p9999_us = 18431
max_us = 20015
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 6

```
ops = 23394171
errors = 0
elapsed_s = 10.001
ops_per_sec = 2339152
p50_us = 423
p99_us = 751
p999_us = 4991
p9999_us = 17919
max_us = 26290
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 7

```
ops = 23922469
errors = 0
elapsed_s = 10.001
ops_per_sec = 2391908
p50_us = 407
p99_us = 767
p999_us = 12543
p9999_us = 18431
max_us = 27013
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 7

```
ops = 23861156
errors = 0
elapsed_s = 10.001
ops_per_sec = 2385826
p50_us = 407
p99_us = 735
p999_us = 4863
p9999_us = 18943
max_us = 23803
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 8

```
ops = 24222653
errors = 0
elapsed_s = 10.001
ops_per_sec = 2421972
p50_us = 399
p99_us = 719
p999_us = 4031
p9999_us = 18431
max_us = 24661
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 8

```
ops = 23998767
errors = 0
elapsed_s = 10.001
ops_per_sec = 2399595
p50_us = 407
p99_us = 735
p999_us = 4607
p9999_us = 18943
max_us = 22219
```
