# M2 gate-run report

date: 1786829033 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- always row: 1633544 gated acks / 16422 fsyncs = ratio 99.5; fsync latency p50/p99/p999 = 2623/3967/5759 us (HDR ~3% quantization); log_writes_per_iter 0.029 (72674 frames / 2472960 iterations); group formation p50/p99 = 101/135 records vs ~256 available in-flight writes/cell = 0.39x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 2373169 ops/s (spread 0.66%) vs everysec 769256 ops/s (spread 157.25%) — signed penalty +67.59%; p999 831 → 17919 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 69631/10223615/10657687 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421973272 B (document 32768 B) vs VmRSS 1406312448 B — 1.1% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 1867775/11022934/11022934 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 40 ckpt cycles / 33 manifests / 57 segments truncated across 3 pressure legs; p99.9 655359 µs under continuous checkpoints vs 319487 µs baseline; peak RSS delta 2.7 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| everysec penalty vs memory mode | < 10 % | 67.59 | FAIL |
| always grouped writes | >= 300000 w/s | 148534.65 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 655359.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 2.75 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.03 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 99.47 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.11 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 32042172
errors = 0
busy_retryable = 0
elapsed_s = 10.003
ops_per_sec = 3203408
p50_us = 319
p99_us = 527
p999_us = 703
p9999_us = 10495
max_us = 11075
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 26386677
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2638343
p50_us = 383
p99_us = 687
p999_us = 847
p9999_us = 10495
max_us = 10935
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 31730263
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3172608
p50_us = 319
p99_us = 575
p999_us = 671
p9999_us = 10239
max_us = 10797
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 4017794
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 802334
p50_us = 623
p99_us = 1007
p999_us = 1439
p9999_us = 3583
max_us = 4983
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 4015737
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 801590
p50_us = 623
p99_us = 1023
p999_us = 1503
p9999_us = 3199
max_us = 3981
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 4020485
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 802634
p50_us = 623
p99_us = 1007
p999_us = 1471
p9999_us = 3199
max_us = 5681
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 26896196
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2689307
p50_us = 367
p99_us = 671
p999_us = 1279
p9999_us = 15103
max_us = 16196
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 27371345
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2736817
p50_us = 359
p99_us = 639
p999_us = 927
p9999_us = 14847
max_us = 15710
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 26923126
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2691971
p50_us = 367
p99_us = 735
p999_us = 10495
p9999_us = 14591
max_us = 15544
```

## always grouped writes (always-grouped)

```
ops = 1486418
errors = 0
busy_retryable = 0
elapsed_s = 10.007
ops_per_sec = 148535
p50_us = 6655
p99_us = 11263
p999_us = 13823
p9999_us = 18431
max_us = 20921
```

## everysec row memory-ns rep 0

```
ops = 23734470
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2373169
p50_us = 423
p99_us = 751
p999_us = 879
p9999_us = 1023
max_us = 1569
```

## everysec row everysec rep 0

```
ops = 4816022
errors = 16782
busy_retryable = 16782
elapsed_s = 12.119
ops_per_sec = 397406
p50_us = 471
p99_us = 895
p999_us = 589823
p9999_us = 3240725
max_us = 3240725
error_sample = -BUSY durable log staging is full, retry
```

## everysec row everysec rep 1

```
ops = 16072357
errors = 4442
busy_retryable = 4442
elapsed_s = 10.001
ops_per_sec = 1607024
p50_us = 479
p99_us = 1055
p999_us = 15359
p9999_us = 35839
max_us = 864652
error_sample = -BUSY durable log staging is full, retry
```

## everysec row memory-ns rep 1

```
ops = 23678431
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2367527
p50_us = 431
p99_us = 719
p999_us = 831
p9999_us = 943
max_us = 1459
```

## everysec row memory-ns rep 2

```
ops = 23836256
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2383302
p50_us = 431
p99_us = 687
p999_us = 799
p9999_us = 911
max_us = 2459
```

## everysec row everysec rep 2

```
ops = 7693688
errors = 2382
busy_retryable = 2382
elapsed_s = 10.001
ops_per_sec = 769256
p50_us = 471
p99_us = 959
p999_us = 17919
p9999_us = 2839272
max_us = 2839272
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 0

```
ops = 5407233
errors = 64629
busy_retryable = 64629
elapsed_s = 11.911
ops_per_sec = 453952
p50_us = 503
p99_us = 1023
p999_us = 491519
p9999_us = 3276799
max_us = 3588596
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 0

```
ops = 9220435
errors = 13643
busy_retryable = 13643
elapsed_s = 10.276
ops_per_sec = 897271
p50_us = 487
p99_us = 1151
p999_us = 61439
p9999_us = 2578894
max_us = 2578894
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 1

```
ops = 2499537
errors = 801
busy_retryable = 801
elapsed_s = 10.613
ops_per_sec = 235521
p50_us = 471
p99_us = 943
p999_us = 1245183
p9999_us = 2637898
max_us = 2637898
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 1

```
ops = 9865827
errors = 20983
busy_retryable = 20983
elapsed_s = 10.345
ops_per_sec = 953701
p50_us = 479
p99_us = 1023
p999_us = 319487
p9999_us = 688127
max_us = 701645
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure baseline rep 2

```
ops = 7259590
errors = 68325
busy_retryable = 68325
elapsed_s = 10.448
ops_per_sec = 694863
p50_us = 463
p99_us = 943
p999_us = 262143
p9999_us = 2091323
max_us = 2091323
error_sample = -BUSY durable log staging is full, retry
```

## ckpt-pressure pressure rep 2

```
ops = 3785770
errors = 1662
busy_retryable = 1662
elapsed_s = 12.009
ops_per_sec = 315236
p50_us = 471
p99_us = 831
p999_us = 655359
p9999_us = 4255224
max_us = 4255224
error_sample = -BUSY durable log staging is full, retry
```
