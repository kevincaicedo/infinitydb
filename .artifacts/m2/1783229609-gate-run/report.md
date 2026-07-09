# M2 gate-run report

date: 1783229609 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- server cells pinned: --pin-start 4 (same cpu set both legs)
- pipelined 1:10 (M0 gate mix): m1 2507163 ops/s (spread 10.62%) vs m2 2496216 ops/s (spread 9.17%) — signed ops delta -0.44% · p999 799 → 783 µs (-2.00%)
- unpipelined 512-conn (M0 gate mix): m1 756635 ops/s (spread 0.97%) vs m2 753216 ops/s (spread 1.10%) — signed ops delta -0.45% · p999 1151 → 1151 µs (+0.00%)
- ttl-heavy 1:1 writes (M1 gate mix): m1 2255524 ops/s (spread 32.80%) vs m2 2235116 ops/s (spread 4.63%) — signed ops delta -0.90% · p999 5759 → 4735 µs (-17.78%)
- always row: 1172514 gated acks / 70217 fsyncs = ratio 16.7; fsync latency p50/p99/p999 = 5247/11519/15615 us (HDR ~3% quantization); log_writes_per_iter 0.016 (70217 frames / 4330496 iterations)
- everysec row: memory-ns 2187779 ops/s (spread 2.44%) vs everysec 1476211 ops/s (spread 57.42%) — signed penalty +32.52%; p999 943 → 27647 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 24575/802815/1052009 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421940504 B vs VmRSS 1405898752 B — 1.1% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-m2-press (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 499711/1520735/1520735 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 176 ckpt cycles / 176 manifests / 220 segments truncated across 3 pressure legs; p99.9 108543 µs under continuous checkpoints vs 44031 µs baseline; peak RSS delta 14.4 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
- S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound
- external gate row `external:recovery_gbps_per_cell` = 0.7 supplied from S13 recovery artifact
- external gate row `external:recovery_10gb_boot_s` = 9.86 supplied from S15 cold-boot artifact
- external gate row `external:dst_sweep_violations` = 0 supplied from S19 sweep manifest
- external gate row `external:crash_matrix_failures` = 0 supplied from S17 matrix run
- external gate row `external:m0m1_regression_pct` = 1.5 supplied from M0/M1 regression gate-runs
- campaign: external artifacts: dst-sweep-10k-s22-20260705 (10k seeds, 0 violations); crash-matrix 256 seeds/combination green (CRASH_MATRIX_SEEDS=256, 2026-07-05); recovery-replay-cold-20260705 (ick-tail cold 0.65 GiB/s = 0.70 GB/s, disposition in README); parallel-boot-cold-20260705 (9.78-9.86 s / 11.01 GiB cold); m0m1 re-pass: .artifacts/m0/1783228176 + .artifacts/m1/1783228421 vs archived M1-era runs, worst binding-row regression +1.5% (pubsub fan-out p99 0.68->0.69 ms); box: user-designated reference HomeLab i7-13700KF, ADATA LEGEND 700 Gen3 DRAM-less NVMe (master-plan Gen4 profile deviation disclosed)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.44 | PASS |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.45 | PASS |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.90 | PASS |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | 0.00 | PASS |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | 32.52 | FAIL |
| always grouped writes | >= 300000 w/s | 106272.58 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | 0.70 | FAIL |
| 10 GB node cold boot | < 15 s | 9.86 | PASS |
| DST durability oracle: 10k seeds | <= 0 violations | 0.00 | PASS |
| Crash matrix green in CI | <= 0 failures | 0.00 | PASS |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 108543.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 14.36 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | 1.50 | PASS |
| One log write per iteration | <= 1 writes/iter | 0.02 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 16.70 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.14 | PASS |

## pipelined 1:10 (M0 gate mix) m1-baseline rep 0

```
ops = 22947904
errors = 0
elapsed_s = 10.001
ops_per_sec = 2294472
p50_us = 447
p99_us = 783
p999_us = 911
p9999_us = 10495
max_us = 11524
```

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 26103607
errors = 0
elapsed_s = 10.001
ops_per_sec = 2610059
p50_us = 391
p99_us = 639
p999_us = 735
p9999_us = 11007
max_us = 11668
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 24757242
errors = 0
elapsed_s = 10.001
ops_per_sec = 2475414
p50_us = 407
p99_us = 767
p999_us = 863
p9999_us = 975
max_us = 3365
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 1

```
ops = 24817555
errors = 0
elapsed_s = 10.001
ops_per_sec = 2481417
p50_us = 407
p99_us = 719
p999_us = 799
p9999_us = 911
max_us = 2262
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 2

```
ops = 25074842
errors = 0
elapsed_s = 10.001
ops_per_sec = 2507163
p50_us = 415
p99_us = 655
p999_us = 735
p9999_us = 831
max_us = 2747
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 24965183
errors = 0
elapsed_s = 10.001
ops_per_sec = 2496216
p50_us = 407
p99_us = 719
p999_us = 783
p9999_us = 895
max_us = 1669
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 25287264
errors = 0
elapsed_s = 10.001
ops_per_sec = 2528444
p50_us = 399
p99_us = 687
p999_us = 751
p9999_us = 863
max_us = 1213
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 3

```
ops = 25609987
errors = 0
elapsed_s = 10.001
ops_per_sec = 2560705
p50_us = 399
p99_us = 623
p999_us = 703
p9999_us = 783
max_us = 1429
```

## pipelined 1:10 (M0 gate mix) m1-baseline rep 4

```
ops = 25384631
errors = 0
elapsed_s = 10.001
ops_per_sec = 2538186
p50_us = 431
p99_us = 655
p999_us = 799
p9999_us = 879
max_us = 1105
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 23813679
errors = 0
elapsed_s = 10.001
ops_per_sec = 2381094
p50_us = 407
p99_us = 799
p999_us = 879
p9999_us = 1007
max_us = 1139
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 0

```
ops = 3814979
errors = 0
elapsed_s = 5.008
ops_per_sec = 761847
p50_us = 655
p99_us = 1183
p999_us = 1311
p9999_us = 3391
max_us = 4219
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3806590
errors = 0
elapsed_s = 5.007
ops_per_sec = 760228
p50_us = 655
p99_us = 1055
p999_us = 1151
p9999_us = 3391
max_us = 5113
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3765417
errors = 0
elapsed_s = 5.008
ops_per_sec = 751920
p50_us = 671
p99_us = 1119
p999_us = 1215
p9999_us = 1407
max_us = 2443
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 1

```
ops = 3777887
errors = 0
elapsed_s = 5.007
ops_per_sec = 754510
p50_us = 671
p99_us = 1087
p999_us = 1151
p9999_us = 1375
max_us = 3990
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 2

```
ops = 3789378
errors = 0
elapsed_s = 5.008
ops_per_sec = 756635
p50_us = 655
p99_us = 1087
p999_us = 1151
p9999_us = 1279
max_us = 3390
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3771310
errors = 0
elapsed_s = 5.007
ops_per_sec = 753216
p50_us = 671
p99_us = 1087
p999_us = 1151
p9999_us = 1375
max_us = 4127
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3770464
errors = 0
elapsed_s = 5.007
ops_per_sec = 753043
p50_us = 671
p99_us = 1087
p999_us = 1151
p9999_us = 1471
max_us = 4295
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 3

```
ops = 3787755
errors = 0
elapsed_s = 5.008
ops_per_sec = 756387
p50_us = 671
p99_us = 1087
p999_us = 1151
p9999_us = 1407
max_us = 4385
```

## unpipelined 512-conn (M0 gate mix) m1-baseline rep 4

```
ops = 3796154
errors = 0
elapsed_s = 5.007
ops_per_sec = 758151
p50_us = 671
p99_us = 1055
p999_us = 1119
p9999_us = 1439
max_us = 4362
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3774751
errors = 0
elapsed_s = 5.008
ops_per_sec = 753818
p50_us = 671
p99_us = 1087
p999_us = 1151
p9999_us = 1439
max_us = 4143
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 0

```
ops = 15476309
errors = 0
elapsed_s = 10.001
ops_per_sec = 1547457
p50_us = 671
p99_us = 1119
p999_us = 5759
p9999_us = 13055
max_us = 13580
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 22353901
errors = 0
elapsed_s = 10.001
ops_per_sec = 2235116
p50_us = 439
p99_us = 783
p999_us = 4991
p9999_us = 16127
max_us = 16860
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 22208103
errors = 0
elapsed_s = 10.001
ops_per_sec = 2220556
p50_us = 447
p99_us = 735
p999_us = 4991
p9999_us = 16895
max_us = 17324
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 1

```
ops = 22327655
errors = 0
elapsed_s = 10.001
ops_per_sec = 2232516
p50_us = 431
p99_us = 767
p999_us = 7167
p9999_us = 17919
max_us = 20724
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 2

```
ops = 22703460
errors = 0
elapsed_s = 10.001
ops_per_sec = 2270071
p50_us = 431
p99_us = 735
p999_us = 5631
p9999_us = 17919
max_us = 18196
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 22516296
errors = 0
elapsed_s = 10.001
ops_per_sec = 2251343
p50_us = 439
p99_us = 719
p999_us = 4735
p9999_us = 17407
max_us = 21243
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 21481548
errors = 0
elapsed_s = 10.001
ops_per_sec = 2147914
p50_us = 455
p99_us = 847
p999_us = 4735
p9999_us = 16895
max_us = 17086
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 3

```
ops = 22874624
errors = 0
elapsed_s = 10.001
ops_per_sec = 2287186
p50_us = 431
p99_us = 687
p999_us = 5119
p9999_us = 17919
max_us = 20914
```

## ttl-heavy 1:1 writes (M1 gate mix) m1-baseline rep 4

```
ops = 22558273
errors = 0
elapsed_s = 10.001
ops_per_sec = 2255524
p50_us = 431
p99_us = 783
p999_us = 5759
p9999_us = 17919
max_us = 18248
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 22373162
errors = 0
elapsed_s = 10.001
ops_per_sec = 2237028
p50_us = 439
p99_us = 751
p999_us = 4479
p9999_us = 17407
max_us = 21309
```

## always grouped writes (always-grouped)

```
ops = 1063573
errors = 0
elapsed_s = 10.008
ops_per_sec = 106273
p50_us = 9215
p99_us = 18943
p999_us = 23039
p9999_us = 26111
max_us = 27751
```

## everysec row memory-ns rep 0

```
ops = 21880361
errors = 0
elapsed_s = 10.001
ops_per_sec = 2187779
p50_us = 455
p99_us = 847
p999_us = 959
p9999_us = 1119
max_us = 2080
```

## everysec row everysec rep 0

```
ops = 13035729
errors = 0
elapsed_s = 10.742
ops_per_sec = 1213555
p50_us = 527
p99_us = 1183
p999_us = 47103
p9999_us = 475135
max_us = 949128
```

## everysec row everysec rep 1

```
ops = 6550082
errors = 9289
elapsed_s = 10.001
ops_per_sec = 654926
p50_us = 527
p99_us = 1055
p999_us = 458751
p9999_us = 753663
max_us = 899132
```

## everysec row memory-ns rep 1

```
ops = 21957157
errors = 0
elapsed_s = 10.001
ops_per_sec = 2195410
p50_us = 455
p99_us = 831
p999_us = 943
p9999_us = 1119
max_us = 1962
```

## everysec row memory-ns rep 2

```
ops = 21684137
errors = 0
elapsed_s = 10.001
ops_per_sec = 2168168
p50_us = 471
p99_us = 847
p999_us = 943
p9999_us = 1119
max_us = 2010
```

## everysec row everysec rep 2

```
ops = 14763977
errors = 0
elapsed_s = 10.001
ops_per_sec = 1476211
p50_us = 527
p99_us = 2015
p999_us = 27647
p9999_us = 59391
max_us = 66434
```

## everysec row everysec rep 3

```
ops = 15020462
errors = 0
elapsed_s = 10.001
ops_per_sec = 1501841
p50_us = 527
p99_us = 2111
p999_us = 25599
p9999_us = 55295
max_us = 57225
```

## everysec row memory-ns rep 3

```
ops = 21464689
errors = 0
elapsed_s = 10.001
ops_per_sec = 2146218
p50_us = 463
p99_us = 927
p999_us = 1023
p9999_us = 1215
max_us = 1752
```

## everysec row memory-ns rep 4

```
ops = 21998685
errors = 0
elapsed_s = 10.001
ops_per_sec = 2199595
p50_us = 455
p99_us = 799
p999_us = 911
p9999_us = 1055
max_us = 1853
```

## everysec row everysec rep 4

```
ops = 15027777
errors = 0
elapsed_s = 10.001
ops_per_sec = 1502563
p50_us = 511
p99_us = 2175
p999_us = 25599
p9999_us = 57343
max_us = 59028
```

## ckpt-pressure baseline rep 0

```
ops = 14736403
errors = 0
elapsed_s = 10.008
ops_per_sec = 1472481
p50_us = 527
p99_us = 1663
p999_us = 45055
p9999_us = 73727
max_us = 77719
```

## ckpt-pressure pressure rep 0

```
ops = 13759395
errors = 0
elapsed_s = 10.038
ops_per_sec = 1370789
p50_us = 543
p99_us = 1855
p999_us = 45055
p9999_us = 104447
max_us = 128589
```

## ckpt-pressure pressure rep 1

```
ops = 3008899
errors = 800
elapsed_s = 10.117
ops_per_sec = 297423
p50_us = 527
p99_us = 1055
p999_us = 884735
p9999_us = 1277951
max_us = 1550952
```

## ckpt-pressure baseline rep 1

```
ops = 14593153
errors = 0
elapsed_s = 10.001
ops_per_sec = 1459135
p50_us = 527
p99_us = 1791
p999_us = 36863
p9999_us = 59391
max_us = 80953
```

## ckpt-pressure baseline rep 2

```
ops = 14765699
errors = 0
elapsed_s = 10.001
ops_per_sec = 1476394
p50_us = 559
p99_us = 1759
p999_us = 44031
p9999_us = 65535
max_us = 91758
```

## ckpt-pressure pressure rep 2

```
ops = 8819290
errors = 14768
elapsed_s = 10.015
ops_per_sec = 880589
p50_us = 543
p99_us = 1567
p999_us = 108543
p9999_us = 786431
max_us = 796796
```
