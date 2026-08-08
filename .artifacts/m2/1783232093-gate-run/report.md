# M2 gate-run report

date: 1783232093 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m1 2469399 ops/s (spread 47.84%) vs m2 2467775 ops/s (spread 7.68%) — signed ops delta -0.07% · p999 767 → 799 µs (+4.17%)
- unpipelined 512-conn (M0 gate mix): m1 747066 ops/s (spread 1.03%) vs m2 737944 ops/s (spread 1.81%) — signed ops delta -1.22% · p999 1215 → 1279 µs (+5.27%)
- ttl-heavy 1:1 writes (M1 gate mix): m1 2122920 ops/s (spread 39.26%) vs m2 2148347 ops/s (spread 5.97%) — signed ops delta +1.20% · p999 11007 → 4607 µs (-58.14%)
- always row: 1709887 gated acks / 17095 fsyncs = ratio 100.0; fsync latency p50/p99/p999 = 2495/4031/5631 us (HDR ~3% quantization); log_writes_per_iter 0.029 (74045 frames / 2529280 iterations)
- everysec row: memory-ns 2187014 ops/s (spread 8.26%) vs everysec 1464893 ops/s (spread 13.23%) — signed penalty +33.02%; p999 911 → 25599 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 24575/180223/883859 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421940504 B vs VmRSS 1407528960 B — 1.0% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-m2-press (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 45055/1152428/1152428 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 249 ckpt cycles / 249 manifests / 320 segments truncated across 3 pressure legs; p99.9 49151 µs under continuous checkpoints vs 47103 µs baseline; peak RSS delta 8.4 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
- S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound
- external gate row `external:recovery_gbps_per_cell` = 0.7 supplied from S13 recovery artifact
- external gate row `external:recovery_10gb_boot_s` = 9.86 supplied from S15 cold-boot artifact
- external gate row `external:dst_sweep_violations` = 0 supplied from S19 sweep manifest
- external gate row `external:crash_matrix_failures` = 0 supplied from S17 matrix run
- external gate row `external:m0m1_regression_pct` = 1.5 supplied from M0/M1 regression gate-runs
- campaign: external artifacts: dst-sweep-10k-s22-20260705 (10k seeds, 0 violations, regenerated post group-commit fix); crash-matrix 256 seeds/combination green (2026-07-05, post-fix); recovery-replay-cold-20260705 (ick-tail cold 0.65 GiB/s = 0.70 GB/s, disposition in README); parallel-boot-cold-20260705 (9.78-9.86 s / 11.01 GiB cold); m0m1 re-pass: .artifacts/m0/1783228176 + .artifacts/m1/1783228421 vs archived M1-era runs, worst binding-row regression +1.5%; box: user-designated reference HomeLab i7-13700KF, ADATA LEGEND 700 Gen3 DRAM-less NVMe (master-plan Gen4 profile deviation disclosed)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.07 | PASS |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 4.17 | FAIL |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 1.22 | FAIL |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 5.27 | FAIL |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | 33.02 | FAIL |
| always grouped writes | >= 300000 w/s | 155768.25 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | 0.70 | FAIL |
| 10 GB node cold boot | < 15 s | 9.86 | PASS |
| DST durability oracle: 10k seeds | <= 0 violations | 0.00 | PASS |
| Crash matrix green in CI | <= 0 failures | 0.00 | PASS |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 49151.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 8.41 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | 1.50 | PASS |
| One log write per iteration | <= 1 writes/iter | 0.03 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 100.02 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.02 | PASS |

## pipelined 1:10 (M0 gate mix) m1-baseline rep 0

```
ops = 13441845
errors = 0
elapsed_s = 10.001
ops_per_sec = 1344005
p50_us = 783
p99_us = 1279
p999_us = 3199
p9999_us = 5887
max_us = 10366
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 26003978
errors = 0
elapsed_s = 10.001
ops_per_sec = 2600086
p50_us = 391
p99_us = 655
p999_us = 751
p9999_us = 10751
max_us = 11869
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 24941521
errors = 0
elapsed_s = 10.001
ops_per_sec = 2493831
p50_us = 399
p99_us = 719
p999_us = 799
p9999_us = 927
max_us = 3009
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 24591985
errors = 0
elapsed_s = 10.001
ops_per_sec = 2458882
p50_us = 415
p99_us = 703
p999_us = 863
p9999_us = 10495
max_us = 11132
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 25177101
errors = 0
elapsed_s = 10.001
ops_per_sec = 2517414
p50_us = 407
p99_us = 655
p999_us = 735
p9999_us = 831
max_us = 2600
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 24181761
errors = 0
elapsed_s = 10.001
ops_per_sec = 2417907
p50_us = 423
p99_us = 719
p999_us = 815
p9999_us = 1007
max_us = 6945
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 24681030
errors = 0
elapsed_s = 10.001
ops_per_sec = 2467775
p50_us = 415
p99_us = 687
p999_us = 783
p9999_us = 927
max_us = 3139
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25256765
errors = 0
elapsed_s = 10.001
ops_per_sec = 2525394
p50_us = 407
p99_us = 623
p999_us = 703
p9999_us = 799
max_us = 2514
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 24696940
errors = 0
elapsed_s = 10.001
ops_per_sec = 2469399
p50_us = 415
p99_us = 687
p999_us = 767
p9999_us = 863
max_us = 3067
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 24108701
errors = 0
elapsed_s = 10.001
ops_per_sec = 2410588
p50_us = 439
p99_us = 767
p999_us = 863
p9999_us = 975
max_us = 4051
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3740856
errors = 0
elapsed_s = 5.007
ops_per_sec = 747066
p50_us = 671
p99_us = 1119
p999_us = 1311
p9999_us = 3391
max_us = 4393
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3730322
errors = 0
elapsed_s = 5.008
ops_per_sec = 744942
p50_us = 671
p99_us = 1119
p999_us = 1311
p9999_us = 3391
max_us = 4058
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3664664
errors = 0
elapsed_s = 5.009
ops_per_sec = 731557
p50_us = 703
p99_us = 1119
p999_us = 1279
p9999_us = 1663
max_us = 4357
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3740864
errors = 0
elapsed_s = 5.008
ops_per_sec = 747043
p50_us = 671
p99_us = 1119
p999_us = 1215
p9999_us = 1439
max_us = 3975
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3720160
errors = 0
elapsed_s = 5.007
ops_per_sec = 742950
p50_us = 671
p99_us = 1151
p999_us = 1311
p9999_us = 1663
max_us = 4429
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3684634
errors = 0
elapsed_s = 5.009
ops_per_sec = 735646
p50_us = 687
p99_us = 1087
p999_us = 1279
p9999_us = 1759
max_us = 4787
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3695305
errors = 0
elapsed_s = 5.008
ops_per_sec = 737944
p50_us = 687
p99_us = 1151
p999_us = 1247
p9999_us = 1727
max_us = 4462
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3752887
errors = 0
elapsed_s = 5.008
ops_per_sec = 749438
p50_us = 687
p99_us = 1087
p999_us = 1183
p9999_us = 1407
max_us = 4096
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3758606
errors = 0
elapsed_s = 5.007
ops_per_sec = 750662
p50_us = 671
p99_us = 1119
p999_us = 1215
p9999_us = 1375
max_us = 4424
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3726633
errors = 0
elapsed_s = 5.007
ops_per_sec = 744282
p50_us = 687
p99_us = 1087
p999_us = 1215
p9999_us = 1567
max_us = 4440
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 14119755
errors = 0
elapsed_s = 10.001
ops_per_sec = 1411808
p50_us = 815
p99_us = 1279
p999_us = 6655
p9999_us = 7423
max_us = 10493
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22434450
errors = 0
elapsed_s = 10.001
ops_per_sec = 2243180
p50_us = 439
p99_us = 751
p999_us = 4607
p9999_us = 14847
max_us = 16084
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 21486026
errors = 0
elapsed_s = 10.001
ops_per_sec = 2148347
p50_us = 463
p99_us = 863
p999_us = 9727
p9999_us = 15103
max_us = 22191
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 22454584
errors = 0
elapsed_s = 10.001
ops_per_sec = 2245198
p50_us = 431
p99_us = 703
p999_us = 11007
p9999_us = 14591
max_us = 17442
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 22399540
errors = 0
elapsed_s = 10.001
ops_per_sec = 2239655
p50_us = 431
p99_us = 703
p999_us = 11775
p9999_us = 13823
max_us = 14188
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 21151600
errors = 0
elapsed_s = 10.001
ops_per_sec = 2114869
p50_us = 463
p99_us = 879
p999_us = 4607
p9999_us = 17407
max_us = 19440
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 21423584
errors = 0
elapsed_s = 10.001
ops_per_sec = 2142042
p50_us = 463
p99_us = 815
p999_us = 3775
p9999_us = 15103
max_us = 18531
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 20966064
errors = 0
elapsed_s = 10.001
ops_per_sec = 2096330
p50_us = 455
p99_us = 895
p999_us = 11007
p9999_us = 16383
max_us = 21612
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 21231493
errors = 0
elapsed_s = 10.001
ops_per_sec = 2122920
p50_us = 439
p99_us = 879
p999_us = 11519
p9999_us = 14079
max_us = 14774
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22171343
errors = 0
elapsed_s = 10.001
ops_per_sec = 2216893
p50_us = 447
p99_us = 735
p999_us = 4607
p9999_us = 15871
max_us = 18570
```

## always grouped writes (always-grouped)

```
ops = 1559002
errors = 0
elapsed_s = 10.008
ops_per_sec = 155768
p50_us = 6271
p99_us = 10751
p999_us = 12799
p9999_us = 14335
max_us = 17638
```

## everysec row memory-ns rep 0

```
ops = 20257021
errors = 0
elapsed_s = 10.001
ops_per_sec = 2025456
p50_us = 503
p99_us = 911
p999_us = 1055
p9999_us = 1215
max_us = 2087
```

## everysec row everysec rep 0

```
ops = 14826681
errors = 0
elapsed_s = 10.001
ops_per_sec = 1482481
p50_us = 527
p99_us = 1151
p999_us = 31743
p9999_us = 67583
max_us = 84558
```

## everysec row everysec rep 1

```
ops = 12888835
errors = 0
elapsed_s = 10.001
ops_per_sec = 1288701
p50_us = 527
p99_us = 2111
p999_us = 30207
p9999_us = 176127
max_us = 779214
```

## everysec row memory-ns rep 1

```
ops = 22008133
errors = 0
elapsed_s = 10.001
ops_per_sec = 2200537
p50_us = 471
p99_us = 735
p999_us = 863
p9999_us = 959
max_us = 2230
```

## everysec row memory-ns rep 2

```
ops = 21873015
errors = 0
elapsed_s = 10.001
ops_per_sec = 2187014
p50_us = 455
p99_us = 831
p999_us = 943
p9999_us = 1087
max_us = 2129
```

## everysec row everysec rep 2

```
ops = 14780450
errors = 0
elapsed_s = 10.001
ops_per_sec = 1477857
p50_us = 543
p99_us = 2239
p999_us = 25599
p9999_us = 35839
max_us = 39773
```

## everysec row everysec rep 3

```
ops = 14667481
errors = 0
elapsed_s = 10.013
ops_per_sec = 1464893
p50_us = 527
p99_us = 2303
p999_us = 24575
p9999_us = 44031
max_us = 46649
```

## everysec row memory-ns rep 3

```
ops = 22063192
errors = 0
elapsed_s = 10.001
ops_per_sec = 2206016
p50_us = 471
p99_us = 735
p999_us = 847
p9999_us = 959
max_us = 3726
```

## everysec row memory-ns rep 4

```
ops = 21811923
errors = 0
elapsed_s = 10.001
ops_per_sec = 2180871
p50_us = 463
p99_us = 799
p999_us = 911
p9999_us = 1055
max_us = 2229
```

## everysec row everysec rep 4

```
ops = 13940529
errors = 0
elapsed_s = 10.001
ops_per_sec = 1393877
p50_us = 527
p99_us = 2431
p999_us = 25599
p9999_us = 90111
max_us = 570796
```

## ckpt-pressure baseline rep 0

```
ops = 14707075
errors = 0
elapsed_s = 10.015
ops_per_sec = 1468571
p50_us = 503
p99_us = 1055
p999_us = 47103
p9999_us = 71679
max_us = 95590
```

## ckpt-pressure pressure rep 0

```
ops = 12303208
errors = 0
elapsed_s = 10.026
ops_per_sec = 1227120
p50_us = 559
p99_us = 1567
p999_us = 53247
p9999_us = 221183
max_us = 1052270
```

## ckpt-pressure pressure rep 1

```
ops = 13240341
errors = 0
elapsed_s = 10.001
ops_per_sec = 1323862
p50_us = 543
p99_us = 1951
p999_us = 49151
p9999_us = 104447
max_us = 306507
```

## ckpt-pressure baseline rep 1

```
ops = 14756651
errors = 0
elapsed_s = 10.013
ops_per_sec = 1473756
p50_us = 511
p99_us = 1695
p999_us = 45055
p9999_us = 53247
max_us = 66538
```

## ckpt-pressure baseline rep 2

```
ops = 12702793
errors = 250
elapsed_s = 10.019
ops_per_sec = 1267887
p50_us = 527
p99_us = 1119
p999_us = 65535
p9999_us = 262143
max_us = 714258
```

## ckpt-pressure pressure rep 2

```
ops = 13287109
errors = 1086
elapsed_s = 10.014
ops_per_sec = 1326801
p50_us = 559
p99_us = 2431
p999_us = 48127
p9999_us = 83967
max_us = 90686
```
