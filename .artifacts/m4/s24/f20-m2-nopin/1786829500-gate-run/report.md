# M2 gate-run report

date: 1786829500 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 1626428 gated acks / 16477 fsyncs = ratio 98.7; fsync latency p50/p99/p999 = 2623/3967/5759 us (HDR ~3% quantization); log_writes_per_iter 0.038 (92929 frames / 2419712 iterations); group formation p50/p99 = 99/131 records vs ~256 available in-flight writes/cell = 0.39x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 2089018 ops/s (spread 4.39%) vs everysec 1916791 ops/s (spread 79.32%) — signed penalty +8.24%; p999 1279 → 1823 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 57343/7995391/9697298 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421973272 B (document 32768 B) vs VmRSS 1407119360 B — 1.1% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 200703/3742065/3742065 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 128 ckpt cycles / 128 manifests / 287 segments truncated across 3 pressure legs; p99.9 368639 µs under continuous checkpoints vs 450559 µs baseline; peak RSS delta 3.4 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| everysec penalty vs memory mode | < 10 % | 8.24 | PASS |
| always grouped writes | >= 300000 w/s | 147730.92 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 368639.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 3.40 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.04 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 98.71 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.06 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 28718974
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2871504
p50_us = 343
p99_us = 655
p999_us = 927
p9999_us = 11007
max_us = 12550
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 29201799
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2919794
p50_us = 343
p99_us = 639
p999_us = 879
p9999_us = 10495
max_us = 10815
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 29362456
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2935921
p50_us = 335
p99_us = 639
p999_us = 879
p9999_us = 10751
max_us = 11505
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3755548
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 750040
p50_us = 655
p99_us = 1471
p999_us = 1791
p9999_us = 4351
max_us = 6692
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3783680
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 755676
p50_us = 655
p99_us = 1471
p999_us = 1695
p9999_us = 3391
max_us = 4483
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3780500
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 754874
p50_us = 655
p99_us = 1471
p999_us = 1663
p9999_us = 3327
max_us = 4333
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 24911853
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2490908
p50_us = 391
p99_us = 735
p999_us = 3263
p9999_us = 16895
max_us = 17703
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 25622533
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2561963
p50_us = 383
p99_us = 735
p999_us = 2623
p9999_us = 15871
max_us = 16749
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 25380737
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2537785
p50_us = 383
p99_us = 735
p999_us = 2015
p9999_us = 15103
max_us = 15777
```

## always grouped writes (always-grouped)

```
ops = 1478718
errors = 0
busy_retryable = 0
elapsed_s = 10.010
ops_per_sec = 147731
p50_us = 6655
p99_us = 11263
p999_us = 14079
p9999_us = 17919
max_us = 19283
```

## everysec row memory-ns rep 0

```
ops = 21293821
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2129112
p50_us = 471
p99_us = 879
p999_us = 1215
p9999_us = 1663
max_us = 2199
```

## everysec row everysec rep 0

```
ops = 19190995
errors = 570
busy_retryable = 570
elapsed_s = 10.012
ops_per_sec = 1916791
p50_us = 503
p99_us = 975
p999_us = 1823
p9999_us = 24063
max_us = 155013
error_sample = -BUSY durable log staging is full, retry
```

## everysec row everysec rep 1

```
ops = 5137493
errors = 9847
busy_retryable = 9847
elapsed_s = 11.687
ops_per_sec = 439595
p50_us = 503
p99_us = 1119
p999_us = 540671
p9999_us = 1900543
max_us = 2194187
error_sample = -BUSY durable log staging is full, retry
```

## everysec row memory-ns rep 1

```
ops = 20376719
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2037422
p50_us = 487
p99_us = 975
p999_us = 1279
p9999_us = 1759
max_us = 4068
```

## everysec row memory-ns rep 2

```
ops = 20892410
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2089018
p50_us = 471
p99_us = 959
p999_us = 1279
p9999_us = 1695
max_us = 2483
```

## everysec row everysec rep 2

```
ops = 19601393
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1959908
p50_us = 503
p99_us = 975
p999_us = 1535
p9999_us = 11263
max_us = 13992
```

## ckpt-pressure baseline rep 0

```
ops = 6201038
errors = 89801
busy_retryable = 89801
elapsed_s = 11.941
ops_per_sec = 519323
p50_us = 495
p99_us = 1055
p999_us = 450559
p9999_us = 1703935
max_us = 1997495
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 0

```
ops = 6864988
errors = 44563
busy_retryable = 44563
elapsed_s = 10.001
ops_per_sec = 686414
p50_us = 511
p99_us = 1119
p999_us = 376831
p9999_us = 622591
max_us = 637493
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 1

```
ops = 7805265
errors = 96538
busy_retryable = 96538
elapsed_s = 10.001
ops_per_sec = 780412
p50_us = 511
p99_us = 1055
p999_us = 368639
p9999_us = 557055
max_us = 588471
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 1

```
ops = 9358490
errors = 152353
busy_retryable = 152353
elapsed_s = 10.001
ops_per_sec = 935741
p50_us = 503
p99_us = 1055
p999_us = 335871
p9999_us = 524287
max_us = 543002
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 2

```
ops = 8456947
errors = 88835
busy_retryable = 88835
elapsed_s = 10.001
ops_per_sec = 845584
p50_us = 495
p99_us = 975
p999_us = 450559
p9999_us = 728945
max_us = 728945
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 2

```
ops = 18930287
errors = 2331
busy_retryable = 2331
elapsed_s = 10.001
ops_per_sec = 1892817
p50_us = 511
p99_us = 1007
p999_us = 4863
p9999_us = 24063
max_us = 33343
error_sample = -BUSY durable log staging is full, retry
```
