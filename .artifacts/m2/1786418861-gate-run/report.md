# M2 gate-run report

date: 1786418861 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- pressure-data-root resolves under /tmp (tmpfs-likely): the everysec row writes ~13 GB with truncation disabled — a 16 GB tmpfs exhausts mid-row and the engine fail-stops per §8.4. Pass --pressure-data-root on a real filesystem.
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 11486662 gated acks / 412159 fsyncs = ratio 27.9; fsync latency p50/p99/p999 = 52/495/559 us (HDR ~3% quantization); log_writes_per_iter 0.161 (562678 frames / 3497984 iterations); group formation p50/p99 = 24/155 records vs ~256 available in-flight writes/cell = 0.09x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 1440470 ops/s (spread 2.73%) vs everysec 1346452 ops/s (spread 5.76%) — signed penalty +6.53%; p999 1503 → 1599 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 343/943/1104 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421973272 B (document 32768 B) vs VmRSS 1407127552 B — 1.1% divergence
- S12 pressure data root: /tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 503/1122/1122 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 324 ckpt cycles / 324 manifests / 324 segments truncated across 3 pressure legs; p99.9 1663 µs under continuous checkpoints vs 1567 µs baseline; peak RSS delta 3.9 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| everysec penalty vs memory mode | < 10 % | 6.53 | PASS |
| always grouped writes | >= 300000 w/s | 1042512.26 | PASS |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 1663.00 | PASS |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 3.86 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.16 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 27.87 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.06 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 21200824
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2119805
p50_us = 471
p99_us = 847
p999_us = 1007
p9999_us = 6783
max_us = 10485
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 20846984
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2084397
p50_us = 479
p99_us = 863
p999_us = 991
p9999_us = 1375
max_us = 3830
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 20119676
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2011711
p50_us = 495
p99_us = 911
p999_us = 1087
p9999_us = 11007
max_us = 16268
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 2499177
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 499019
p50_us = 1055
p99_us = 1663
p999_us = 2751
p9999_us = 5759
max_us = 7687
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 2495410
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 498321
p50_us = 1087
p99_us = 1631
p999_us = 1983
p9999_us = 3071
max_us = 5010
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 2468497
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 492941
p50_us = 1055
p99_us = 1727
p999_us = 2015
p9999_us = 2623
max_us = 4803
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 17621140
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1761865
p50_us = 559
p99_us = 1087
p999_us = 6655
p9999_us = 16127
max_us = 16766
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 17723070
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1772087
p50_us = 559
p99_us = 1007
p999_us = 9727
p9999_us = 16383
max_us = 17230
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 17344661
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1734205
p50_us = 575
p99_us = 1055
p999_us = 10239
p9999_us = 16383
max_us = 17115
```

## always grouped writes (always-grouped)

```
ops = 10426807
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1042512
p50_us = 927
p99_us = 1823
p999_us = 1983
p9999_us = 2175
max_us = 4085
```

## everysec row memory-ns rep 0

```
ops = 14588485
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1458649
p50_us = 687
p99_us = 1311
p999_us = 1567
p9999_us = 1855
max_us = 3294
```

## everysec row everysec rep 0

```
ops = 13651365
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1364885
p50_us = 735
p99_us = 1407
p999_us = 1599
p9999_us = 1855
max_us = 3472
```

## everysec row everysec rep 1

```
ops = 13466562
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1346452
p50_us = 751
p99_us = 1311
p999_us = 1503
p9999_us = 1759
max_us = 3627
```

## everysec row memory-ns rep 1

```
ops = 14407052
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1440470
p50_us = 703
p99_us = 1247
p999_us = 1407
p9999_us = 1631
max_us = 2749
```

## everysec row memory-ns rep 2

```
ops = 14195697
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1419387
p50_us = 719
p99_us = 1311
p999_us = 1503
p9999_us = 1727
max_us = 2227
```

## everysec row everysec rep 2

```
ops = 12875691
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1287330
p50_us = 799
p99_us = 1407
p999_us = 1663
p9999_us = 1887
max_us = 2524
```

## ckpt-pressure baseline rep 0

```
ops = 13580608
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1357858
p50_us = 735
p99_us = 1311
p999_us = 1503
p9999_us = 1727
max_us = 2546
```

## ckpt-pressure pressure rep 0

```
ops = 12373753
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1237175
p50_us = 815
p99_us = 1631
p999_us = 1855
p9999_us = 2175
max_us = 3708
```

## ckpt-pressure pressure rep 1

```
ops = 12702816
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1270078
p50_us = 799
p99_us = 1439
p999_us = 1663
p9999_us = 1887
max_us = 3571
```

## ckpt-pressure baseline rep 1

```
ops = 13111711
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1310954
p50_us = 783
p99_us = 1375
p999_us = 1695
p9999_us = 1887
max_us = 2519
```

## ckpt-pressure baseline rep 2

```
ops = 13475727
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1347383
p50_us = 751
p99_us = 1375
p999_us = 1567
p9999_us = 1791
max_us = 2772
```

## ckpt-pressure pressure rep 2

```
ops = 12780473
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1277858
p50_us = 783
p99_us = 1439
p999_us = 1631
p9999_us = 1887
max_us = 2644
```
