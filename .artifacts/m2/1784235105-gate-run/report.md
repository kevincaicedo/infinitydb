# M2 gate-run report

date: 1784235105 (unix) · cells: 4 · replicates: 5 · duration: 10s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 1697692 gated acks / 17193 fsyncs = ratio 98.7; fsync latency p50/p99/p999 = 2495/4031/5375 us (HDR ~3% quantization); log_writes_per_iter 0.039 (101377 frames / 2574336 iterations); group formation p50/p99 = 99/131 records vs ~256 available in-flight writes/cell = 0.39x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 2124540 ops/s (spread 3.31%) vs everysec 1912103 ops/s (spread 83.95%) — signed penalty +10.00%; p999 1215 → 2751 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 53247/6291455/8835465 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421973272 B (document 32768 B) vs VmRSS 1406701568 B — 1.1% divergence
- S12 pressure data root: /home/kcaicedo/Documents/Projects/databases/infinitydb/target/bench-scratch (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 319487/3288929/3288929 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 140 ckpt cycles / 136 manifests / 264 segments truncated across 3 pressure legs; p99.9 65535 µs under continuous checkpoints vs 3263 µs baseline; peak RSS delta 3.4 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
- S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | 10.00 | PASS (DEV-TIER, non-binding) |
| always grouped writes | >= 300000 w/s | 155283.52 | FAIL (DEV-TIER, non-binding) |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 65535.00 | FAIL (DEV-TIER, non-binding) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 3.39 | PASS (DEV-TIER, non-binding) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.04 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 98.74 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.09 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 28866150
errors = 0
elapsed_s = 10.001
ops_per_sec = 2886250
p50_us = 343
p99_us = 639
p999_us = 895
p9999_us = 10495
max_us = 11074
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 27510594
errors = 0
elapsed_s = 10.001
ops_per_sec = 2750747
p50_us = 359
p99_us = 703
p999_us = 895
p9999_us = 1247
max_us = 1670
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 28418245
errors = 0
elapsed_s = 10.001
ops_per_sec = 2841426
p50_us = 351
p99_us = 655
p999_us = 879
p9999_us = 1215
max_us = 2216
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 27904301
errors = 0
elapsed_s = 10.001
ops_per_sec = 2790065
p50_us = 367
p99_us = 719
p999_us = 879
p9999_us = 1087
max_us = 1480
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 28645099
errors = 0
elapsed_s = 10.001
ops_per_sec = 2864182
p50_us = 351
p99_us = 639
p999_us = 863
p9999_us = 1215
max_us = 1781
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3553601
errors = 0
elapsed_s = 5.007
ops_per_sec = 709700
p50_us = 703
p99_us = 1503
p999_us = 1823
p9999_us = 3455
max_us = 5052
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3578947
errors = 0
elapsed_s = 5.007
ops_per_sec = 714755
p50_us = 687
p99_us = 1503
p999_us = 1695
p9999_us = 1983
max_us = 4269
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3540626
errors = 0
elapsed_s = 5.007
ops_per_sec = 707128
p50_us = 703
p99_us = 1535
p999_us = 1727
p9999_us = 1951
max_us = 3807
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 3595209
errors = 0
elapsed_s = 5.007
ops_per_sec = 717977
p50_us = 687
p99_us = 1503
p999_us = 1663
p9999_us = 1983
max_us = 4342
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 3518651
errors = 0
elapsed_s = 5.008
ops_per_sec = 702673
p50_us = 703
p99_us = 1503
p999_us = 1759
p9999_us = 2015
max_us = 4242
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 24120119
errors = 0
elapsed_s = 10.001
ops_per_sec = 2411742
p50_us = 391
p99_us = 847
p999_us = 2495
p9999_us = 14335
max_us = 15586
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 24881650
errors = 0
elapsed_s = 10.001
ops_per_sec = 2487867
p50_us = 391
p99_us = 767
p999_us = 1567
p9999_us = 15359
max_us = 15945
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 24825245
errors = 0
elapsed_s = 10.001
ops_per_sec = 2482241
p50_us = 391
p99_us = 751
p999_us = 1727
p9999_us = 15359
max_us = 15932
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 24324282
errors = 0
elapsed_s = 10.001
ops_per_sec = 2432125
p50_us = 399
p99_us = 783
p999_us = 1695
p9999_us = 15359
max_us = 16584
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 25066158
errors = 0
elapsed_s = 10.001
ops_per_sec = 2506324
p50_us = 383
p99_us = 735
p999_us = 1663
p9999_us = 15615
max_us = 15861
```

## always grouped writes (always-grouped)

```
ops = 1554191
errors = 0
elapsed_s = 10.009
ops_per_sec = 155284
p50_us = 6271
p99_us = 10751
p999_us = 12799
p9999_us = 14079
max_us = 15632
```

## everysec row memory-ns rep 0

```
ops = 21426985
errors = 0
elapsed_s = 10.002
ops_per_sec = 2142376
p50_us = 463
p99_us = 879
p999_us = 1215
p9999_us = 1695
max_us = 2420
```

## everysec row everysec rep 0

```
ops = 19123729
errors = 262
elapsed_s = 10.001
ops_per_sec = 1912103
p50_us = 543
p99_us = 1023
p999_us = 1471
p9999_us = 10239
max_us = 17329
```

## everysec row everysec rep 1

```
ops = 19237861
errors = 1399
elapsed_s = 10.002
ops_per_sec = 1923405
p50_us = 511
p99_us = 1007
p999_us = 1631
p9999_us = 9983
max_us = 95938
```

## everysec row memory-ns rep 1

```
ops = 21559031
errors = 0
elapsed_s = 10.001
ops_per_sec = 2155589
p50_us = 463
p99_us = 831
p999_us = 1119
p9999_us = 1599
max_us = 3105
```

## everysec row memory-ns rep 2

```
ops = 21028800
errors = 0
elapsed_s = 10.001
ops_per_sec = 2102606
p50_us = 471
p99_us = 911
p999_us = 1247
p9999_us = 1631
max_us = 2423
```

## everysec row everysec rep 2

```
ops = 16143704
errors = 2841
elapsed_s = 10.001
ops_per_sec = 1614172
p50_us = 511
p99_us = 1007
p999_us = 3007
p9999_us = 360447
max_us = 436923
```

## everysec row everysec rep 3

```
ops = 3720181
errors = 7467
elapsed_s = 11.174
ops_per_sec = 332937
p50_us = 527
p99_us = 1087
p999_us = 671743
p9999_us = 1972886
max_us = 1972886
```

## everysec row memory-ns rep 3

```
ops = 20854527
errors = 0
elapsed_s = 10.001
ops_per_sec = 2085161
p50_us = 471
p99_us = 927
p999_us = 1247
p9999_us = 1663
max_us = 2387
```

## everysec row memory-ns rep 4

```
ops = 21248322
errors = 0
elapsed_s = 10.001
ops_per_sec = 2124540
p50_us = 471
p99_us = 895
p999_us = 1183
p9999_us = 1599
max_us = 2399
```

## everysec row everysec rep 4

```
ops = 19383630
errors = 0
elapsed_s = 10.001
ops_per_sec = 1938138
p50_us = 503
p99_us = 991
p999_us = 2751
p9999_us = 12543
max_us = 25158
```

## ckpt-pressure baseline rep 0

```
ops = 18630527
errors = 0
elapsed_s = 10.001
ops_per_sec = 1862787
p50_us = 527
p99_us = 1119
p999_us = 1791
p9999_us = 14847
max_us = 16902
```

## ckpt-pressure pressure rep 0

```
ops = 6794323
errors = 37380
elapsed_s = 10.001
ops_per_sec = 679331
p50_us = 543
p99_us = 1183
p999_us = 385023
p9999_us = 688127
max_us = 733377
```

## ckpt-pressure pressure rep 1

```
ops = 11550503
errors = 55972
elapsed_s = 10.001
ops_per_sec = 1154908
p50_us = 527
p99_us = 1151
p999_us = 65535
p9999_us = 450559
max_us = 525886
```

## ckpt-pressure baseline rep 1

```
ops = 13758198
errors = 24971
elapsed_s = 10.396
ops_per_sec = 1323429
p50_us = 503
p99_us = 1007
p999_us = 5759
p9999_us = 606207
max_us = 670587
```

## ckpt-pressure baseline rep 2

```
ops = 13711062
errors = 9176
elapsed_s = 10.001
ops_per_sec = 1370917
p50_us = 527
p99_us = 1055
p999_us = 3263
p9999_us = 557055
max_us = 597558
```

## ckpt-pressure pressure rep 2

```
ops = 16713631
errors = 18967
elapsed_s = 10.014
ops_per_sec = 1669093
p50_us = 543
p99_us = 1119
p999_us = 8063
p9999_us = 129023
max_us = 588333
```
