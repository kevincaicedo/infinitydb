# M2 gate-run report

date: 1786421205 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 1734228 gated acks / 17226 fsyncs = ratio 100.7; fsync latency p50/p99/p999 = 2495/3967/4991 us (HDR ~3% quantization); log_writes_per_iter 0.041 (105720 frames / 2592768 iterations); group formation p50/p99 = 101/135 records vs ~256 available in-flight writes/cell = 0.39x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 2144843 ops/s (spread 2.14%) vs everysec 1423463 ops/s (spread 19.01%) — signed penalty +33.63%; p999 1151 → 3519 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 58367/2752511/3375010 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421973272 B (document 32768 B) vs VmRSS 1407070208 B — 1.1% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 172031/3647595/3647595 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 64 ckpt cycles / 58 manifests / 105 segments truncated across 3 pressure legs; p99.9 376831 µs under continuous checkpoints vs 258047 µs baseline; peak RSS delta 2.8 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| everysec penalty vs memory mode | < 10 % | 33.63 | FAIL |
| always grouped writes | >= 300000 w/s | 157939.82 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 376831.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 2.77 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.04 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 100.68 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.06 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 28761611
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2875797
p50_us = 343
p99_us = 655
p999_us = 975
p9999_us = 10239
max_us = 11451
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 28983833
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2898047
p50_us = 343
p99_us = 639
p999_us = 831
p9999_us = 1119
max_us = 3638
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 27959872
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2795687
p50_us = 359
p99_us = 703
p999_us = 895
p9999_us = 1119
max_us = 1492
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3519386
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 702806
p50_us = 687
p99_us = 1503
p999_us = 1823
p9999_us = 4031
max_us = 6223
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3480969
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 695237
p50_us = 703
p99_us = 1503
p999_us = 1759
p9999_us = 1983
max_us = 4339
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3450090
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 688998
p50_us = 719
p99_us = 1503
p999_us = 1791
p9999_us = 2047
max_us = 3781
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 25362454
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2535914
p50_us = 383
p99_us = 751
p999_us = 1791
p9999_us = 14079
max_us = 14798
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 24893217
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2489001
p50_us = 391
p99_us = 783
p999_us = 1247
p9999_us = 14591
max_us = 24522
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 24655947
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2465319
p50_us = 391
p99_us = 767
p999_us = 1343
p9999_us = 14591
max_us = 16171
```

## always grouped writes (always-grouped)

```
ops = 1580706
errors = 0
busy_retryable = 0
elapsed_s = 10.008
ops_per_sec = 157940
p50_us = 6271
p99_us = 10495
p999_us = 13055
p9999_us = 16383
max_us = 18491
```

## everysec row memory-ns rep 0

```
ops = 21451062
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2144843
p50_us = 471
p99_us = 895
p999_us = 1183
p9999_us = 1535
max_us = 2175
```

## everysec row everysec rep 0

```
ops = 14455894
errors = 4025
busy_retryable = 4025
elapsed_s = 10.155
ops_per_sec = 1423463
p50_us = 511
p99_us = 991
p999_us = 3071
p9999_us = 425983
max_us = 486898
error_sample = -BUSY durable log staging is full, retry
```

## everysec row everysec rep 1

```
ops = 14063773
errors = 31057
busy_retryable = 31057
elapsed_s = 10.163
ops_per_sec = 1383873
p50_us = 511
p99_us = 1007
p999_us = 14335
p9999_us = 409599
max_us = 523325
error_sample = -BUSY durable log staging is full, retry
```

## everysec row memory-ns rep 1

```
ops = 21324852
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2132230
p50_us = 463
p99_us = 879
p999_us = 1151
p9999_us = 1599
max_us = 2066
```

## everysec row memory-ns rep 2

```
ops = 21783468
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2178097
p50_us = 463
p99_us = 799
p999_us = 1055
p9999_us = 1311
max_us = 2023
```

## everysec row everysec rep 2

```
ops = 16546653
errors = 4346
busy_retryable = 4346
elapsed_s = 10.001
ops_per_sec = 1654452
p50_us = 503
p99_us = 1007
p999_us = 3519
p9999_us = 286719
max_us = 509904
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 0

```
ops = 10052038
errors = 7458
busy_retryable = 7458
elapsed_s = 10.466
ops_per_sec = 960444
p50_us = 511
p99_us = 1023
p999_us = 258047
p9999_us = 652636
max_us = 652636
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 0

```
ops = 7771943
errors = 55042
busy_retryable = 55042
elapsed_s = 10.001
ops_per_sec = 777101
p50_us = 511
p99_us = 1055
p999_us = 352255
p9999_us = 589823
max_us = 641597
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 1

```
ops = 8063432
errors = 19483
busy_retryable = 19483
elapsed_s = 10.101
ops_per_sec = 798260
p50_us = 511
p99_us = 1087
p999_us = 376831
p9999_us = 516095
max_us = 519083
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 1

```
ops = 8316421
errors = 7802
busy_retryable = 7802
elapsed_s = 11.419
ops_per_sec = 728276
p50_us = 511
p99_us = 1055
p999_us = 352255
p9999_us = 1733874
max_us = 1733874
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 2

```
ops = 16024332
errors = 50433
busy_retryable = 50433
elapsed_s = 10.001
ops_per_sec = 1602234
p50_us = 495
p99_us = 975
p999_us = 1631
p9999_us = 81919
max_us = 770310
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 2

```
ops = 7934624
errors = 71714
busy_retryable = 71714
elapsed_s = 10.001
ops_per_sec = 793357
p50_us = 511
p99_us = 1119
p999_us = 385023
p9999_us = 761530
max_us = 761530
error_sample = -BUSY durable log staging is full, retry
```
