# M2 gate-run report

date: 1783232863 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m1 2536316 ops/s (spread 4.54%) vs m2 2520436 ops/s (spread 7.61%) — signed ops delta -0.63% · p999 783 → 751 µs (-4.09%)
- unpipelined 512-conn (M0 gate mix): m1 744236 ops/s (spread 1.41%) vs m2 751279 ops/s (spread 1.25%) — signed ops delta +0.95% · p999 1215 → 1215 µs (+0.00%)
- ttl-heavy 1:1 writes (M1 gate mix): m1 2211134 ops/s (spread 23.39%) vs m2 2232635 ops/s (spread 2.54%) — signed ops delta +0.97% · p999 4607 → 3967 µs (-13.89%)
- SPAWN RETRY (always row): node stayed in -LOADING for 30s — the boot-wedge watch item fired; node respawned fresh (disclosed, counted; server stderr in the campaign capture dir when INF_GATERUN_STDERR_DIR is set)
- always row: 1403724 gated acks / 14034 fsyncs = ratio 100.0; fsync latency p50/p99/p999 = 3071/4607/6399 us (HDR ~3% quantization); log_writes_per_iter 0.028 (58764 frames / 2080768 iterations)
- everysec row: memory-ns 2188702 ops/s (spread 1.82%) vs everysec 1457525 ops/s (spread 12.84%) — signed penalty +33.41%; p999 927 → 26623 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 25087/311295/590867 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421940504 B vs VmRSS 1405370368 B — 1.2% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-m2-press (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 442367/1591870/1591870 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 114 ckpt cycles / 112 manifests / 133 segments truncated across 3 pressure legs; p99.9 688127 µs under continuous checkpoints vs 241663 µs baseline; peak RSS delta 14.1 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
- S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound
- external gate row `external:recovery_gbps_per_cell` = 0.7 supplied from S13 recovery artifact
- external gate row `external:recovery_10gb_boot_s` = 9.86 supplied from S15 cold-boot artifact
- external gate row `external:dst_sweep_violations` = 0 supplied from S19 sweep manifest
- external gate row `external:crash_matrix_failures` = 0 supplied from S17 matrix run
- external gate row `external:m0m1_regression_pct` = 1.5 supplied from M0/M1 regression gate-runs
- campaign: external artifacts: dst-sweep-10k-s22-20260705 (10k seeds, 0 violations, regenerated post group-commit fix); crash-matrix 256 seeds/combination green (2026-07-05, post-fix); recovery-replay-cold-20260705 (ick-tail cold 0.65 GiB/s = 0.70 GB/s, disposition in README); parallel-boot-cold-20260705 (9.78-9.86 s / 11.01 GiB cold); m0m1 re-pass: .artifacts/m0/1783228176 + .artifacts/m1/1783228421, worst binding-row regression +1.5%; box: user-designated reference HomeLab i7-13700KF, ADATA LEGEND 700 Gen3 DRAM-less NVMe (master-plan Gen4 profile deviation disclosed); prior same-binary campaign runs 1783229609/1783232093 retained for cross-run drift comparison

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.63 | PASS |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | 33.41 | FAIL |
| always grouped writes | >= 300000 w/s | 127959.29 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | 0.70 | FAIL |
| 10 GB node cold boot | < 15 s | 9.86 | PASS |
| DST durability oracle: 10k seeds | <= 0 violations | 0.00 | PASS |
| Crash matrix green in CI | <= 0 failures | 0.00 | PASS |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 688127.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 14.13 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | 1.50 | PASS |
| One log write per iteration | <= 1 writes/iter | 0.03 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 100.02 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.18 | PASS |

## pipelined 1:10 (M0 gate mix) m1-baseline rep 0

```
ops = 26419823
errors = 0
elapsed_s = 10.001
ops_per_sec = 2641703
p50_us = 383
p99_us = 687
p999_us = 783
p9999_us = 10495
max_us = 11002
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 26275278
errors = 0
elapsed_s = 10.001
ops_per_sec = 2627234
p50_us = 391
p99_us = 623
p999_us = 719
p9999_us = 10495
max_us = 11412
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 24357749
errors = 0
elapsed_s = 10.001
ops_per_sec = 2435470
p50_us = 423
p99_us = 767
p999_us = 863
p9999_us = 959
max_us = 2515
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 25268631
errors = 0
elapsed_s = 10.001
ops_per_sec = 2526576
p50_us = 407
p99_us = 751
p999_us = 847
p9999_us = 943
max_us = 4760
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 25428493
errors = 0
elapsed_s = 10.001
ops_per_sec = 2542546
p50_us = 415
p99_us = 703
p999_us = 831
p9999_us = 911
max_us = 3947
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 25325065
errors = 0
elapsed_s = 10.001
ops_per_sec = 2532163
p50_us = 407
p99_us = 639
p999_us = 735
p9999_us = 831
max_us = 1732
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 25207207
errors = 0
elapsed_s = 10.001
ops_per_sec = 2520436
p50_us = 399
p99_us = 687
p999_us = 751
p9999_us = 863
max_us = 2700
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25366358
errors = 0
elapsed_s = 10.001
ops_per_sec = 2536316
p50_us = 399
p99_us = 687
p999_us = 767
p9999_us = 927
max_us = 4167
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 25291785
errors = 0
elapsed_s = 10.001
ops_per_sec = 2528902
p50_us = 399
p99_us = 687
p999_us = 767
p9999_us = 895
max_us = 3546
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 25050438
errors = 0
elapsed_s = 10.001
ops_per_sec = 2504769
p50_us = 415
p99_us = 719
p999_us = 831
p9999_us = 943
max_us = 3408
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3726922
errors = 0
elapsed_s = 5.008
ops_per_sec = 744236
p50_us = 687
p99_us = 1151
p999_us = 1471
p9999_us = 3583
max_us = 6503
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3718176
errors = 0
elapsed_s = 5.008
ops_per_sec = 742501
p50_us = 671
p99_us = 1151
p999_us = 1407
p9999_us = 3455
max_us = 4422
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3761939
errors = 0
elapsed_s = 5.007
ops_per_sec = 751279
p50_us = 671
p99_us = 1087
p999_us = 1247
p9999_us = 1599
max_us = 4441
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3761334
errors = 0
elapsed_s = 5.007
ops_per_sec = 751163
p50_us = 671
p99_us = 1087
p999_us = 1183
p9999_us = 1471
max_us = 3827
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3763621
errors = 0
elapsed_s = 5.007
ops_per_sec = 751598
p50_us = 671
p99_us = 1087
p999_us = 1183
p9999_us = 1535
max_us = 4658
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3763631
errors = 0
elapsed_s = 5.009
ops_per_sec = 751444
p50_us = 671
p99_us = 1087
p999_us = 1215
p9999_us = 1727
max_us = 4217
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3765015
errors = 0
elapsed_s = 5.007
ops_per_sec = 751906
p50_us = 671
p99_us = 1087
p999_us = 1183
p9999_us = 1503
max_us = 3983
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3723871
errors = 0
elapsed_s = 5.007
ops_per_sec = 743707
p50_us = 687
p99_us = 1119
p999_us = 1215
p9999_us = 1631
max_us = 3849
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3711387
errors = 0
elapsed_s = 5.008
ops_per_sec = 741115
p50_us = 687
p99_us = 1119
p999_us = 1247
p9999_us = 1599
max_us = 4402
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3731058
errors = 0
elapsed_s = 5.007
ops_per_sec = 745095
p50_us = 687
p99_us = 1087
p999_us = 1215
p9999_us = 1407
max_us = 4101
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 17505213
errors = 0
elapsed_s = 10.001
ops_per_sec = 1750318
p50_us = 639
p99_us = 1007
p999_us = 8447
p9999_us = 9983
max_us = 13239
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22424081
errors = 0
elapsed_s = 10.001
ops_per_sec = 2242159
p50_us = 439
p99_us = 799
p999_us = 4479
p9999_us = 13055
max_us = 13786
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 22252657
errors = 0
elapsed_s = 10.001
ops_per_sec = 2224971
p50_us = 431
p99_us = 815
p999_us = 1279
p9999_us = 13311
max_us = 13795
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 22114161
errors = 0
elapsed_s = 10.001
ops_per_sec = 2211134
p50_us = 439
p99_us = 799
p999_us = 5503
p9999_us = 16383
max_us = 23158
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 22676787
errors = 0
elapsed_s = 10.001
ops_per_sec = 2267414
p50_us = 439
p99_us = 735
p999_us = 2111
p9999_us = 14335
max_us = 19881
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 21885925
errors = 0
elapsed_s = 10.001
ops_per_sec = 2188346
p50_us = 447
p99_us = 799
p999_us = 4031
p9999_us = 16383
max_us = 18011
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 22452950
errors = 0
elapsed_s = 10.001
ops_per_sec = 2245036
p50_us = 447
p99_us = 671
p999_us = 1343
p9999_us = 13567
max_us = 14002
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 21724322
errors = 0
elapsed_s = 10.001
ops_per_sec = 2172183
p50_us = 447
p99_us = 847
p999_us = 4607
p9999_us = 15871
max_us = 18364
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 22584308
errors = 0
elapsed_s = 10.001
ops_per_sec = 2258140
p50_us = 431
p99_us = 799
p999_us = 2239
p9999_us = 14079
max_us = 14443
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22329395
errors = 0
elapsed_s = 10.001
ops_per_sec = 2232635
p50_us = 439
p99_us = 751
p999_us = 3967
p9999_us = 16383
max_us = 18384
```

## always grouped writes (always-grouped)

```
ops = 1280659
errors = 0
elapsed_s = 10.008
ops_per_sec = 127959
p50_us = 7679
p99_us = 12799
p999_us = 15103
p9999_us = 16383
max_us = 17702
```

## everysec row memory-ns rep 0

```
ops = 21846399
errors = 0
elapsed_s = 10.001
ops_per_sec = 2184357
p50_us = 487
p99_us = 783
p999_us = 927
p9999_us = 1087
max_us = 2613
```

## everysec row everysec rep 0

```
ops = 14756955
errors = 0
elapsed_s = 10.001
ops_per_sec = 1475509
p50_us = 527
p99_us = 1631
p999_us = 46079
p9999_us = 67583
max_us = 89305
```

## everysec row everysec rep 1

```
ops = 13713989
errors = 397
elapsed_s = 10.119
ops_per_sec = 1355286
p50_us = 543
p99_us = 2303
p999_us = 26111
p9999_us = 172031
max_us = 237571
```

## everysec row memory-ns rep 1

```
ops = 22196069
errors = 0
elapsed_s = 10.001
ops_per_sec = 2219322
p50_us = 463
p99_us = 719
p999_us = 831
p9999_us = 927
max_us = 1658
```

## everysec row memory-ns rep 2

```
ops = 21889661
errors = 0
elapsed_s = 10.001
ops_per_sec = 2188702
p50_us = 487
p99_us = 735
p999_us = 927
p9999_us = 1087
max_us = 2235
```

## everysec row everysec rep 2

```
ops = 12884722
errors = 5429
elapsed_s = 10.001
ops_per_sec = 1288321
p50_us = 527
p99_us = 2239
p999_us = 32767
p9999_us = 237567
max_us = 457542
```

## everysec row everysec rep 3

```
ops = 14744715
errors = 0
elapsed_s = 10.001
ops_per_sec = 1474298
p50_us = 511
p99_us = 2239
p999_us = 26111
p9999_us = 35839
max_us = 41001
```

## everysec row memory-ns rep 3

```
ops = 21798620
errors = 0
elapsed_s = 10.001
ops_per_sec = 2179567
p50_us = 463
p99_us = 815
p999_us = 943
p9999_us = 1087
max_us = 1799
```

## everysec row memory-ns rep 4

```
ops = 21976945
errors = 0
elapsed_s = 10.001
ops_per_sec = 2197426
p50_us = 447
p99_us = 831
p999_us = 943
p9999_us = 1087
max_us = 3168
```

## everysec row everysec rep 4

```
ops = 14577071
errors = 568
elapsed_s = 10.001
ops_per_sec = 1457525
p50_us = 527
p99_us = 2303
p999_us = 26623
p9999_us = 44031
max_us = 223094
```

## ckpt-pressure baseline rep 0

```
ops = 8342141
errors = 276
elapsed_s = 10.530
ops_per_sec = 792220
p50_us = 511
p99_us = 991
p999_us = 241663
p9999_us = 670500
max_us = 670500
```

## ckpt-pressure pressure rep 0

```
ops = 4081175
errors = 12816
elapsed_s = 10.386
ops_per_sec = 392934
p50_us = 543
p99_us = 1631
p999_us = 688127
p9999_us = 1146879
max_us = 1254792
```

## ckpt-pressure pressure rep 1

```
ops = 3810870
errors = 5656
elapsed_s = 10.392
ops_per_sec = 366714
p50_us = 543
p99_us = 1279
p999_us = 786431
p9999_us = 996053
max_us = 996053
```

## ckpt-pressure baseline rep 1

```
ops = 14453058
errors = 0
elapsed_s = 10.001
ops_per_sec = 1445117
p50_us = 527
p99_us = 1343
p999_us = 45055
p9999_us = 159743
max_us = 538923
```

## ckpt-pressure baseline rep 2

```
ops = 8126299
errors = 2083
elapsed_s = 10.001
ops_per_sec = 812524
p50_us = 511
p99_us = 975
p999_us = 286719
p9999_us = 868351
max_us = 1332557
```

## ckpt-pressure pressure rep 2

```
ops = 9711269
errors = 315
elapsed_s = 10.001
ops_per_sec = 970994
p50_us = 543
p99_us = 1599
p999_us = 65535
p9999_us = 868351
max_us = 1314989
```
