# M2 gate-run report

date: 1783111464 (unix) · cells: 4 · replicates: 5 · duration: 5s
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 14173434 gated acks / 597815 fsyncs = ratio 23.7; fsync latency p50/p99/p999 = 93/351/639 us (HDR ~3% quantization)
- S12 pressure data root: /tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 57 ckpt cycles / 57 manifests / 110 segments truncated across 1 pressure legs; p99.9 2431 µs under continuous checkpoints vs 1631 µs baseline; peak RSS delta 3.9 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| everysec penalty vs memory mode | < 10 % | — | PENDING (tooling) |
| always grouped writes | >= 300000 w/s | 2341529.10 | PASS (DEV-TIER, non-binding) |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 2431.00 | FAIL (DEV-TIER, non-binding) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 3.93 | PASS (DEV-TIER, non-binding) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | — | PENDING (tooling) |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 23.71 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 19024400
errors = 0
elapsed_s = 5.001
ops_per_sec = 3804096
p50_us = 255
p99_us = 527
p999_us = 655
p9999_us = 4031
max_us = 4250
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 18437544
errors = 0
elapsed_s = 5.001
ops_per_sec = 3686889
p50_us = 271
p99_us = 527
p999_us = 1375
p9999_us = 8447
max_us = 8837
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 18395653
errors = 0
elapsed_s = 5.001
ops_per_sec = 3678424
p50_us = 271
p99_us = 495
p999_us = 1247
p9999_us = 2239
max_us = 3094
```

## pipelined 1:10 (M0 gate mix) m2 rep 3

```
ops = 17575228
errors = 0
elapsed_s = 5.001
ops_per_sec = 3514502
p50_us = 295
p99_us = 591
p999_us = 1183
p9999_us = 2303
max_us = 3475
```

## pipelined 1:10 (M0 gate mix) m2 rep 4

```
ops = 18303002
errors = 0
elapsed_s = 5.001
ops_per_sec = 3659847
p50_us = 271
p99_us = 511
p999_us = 1247
p9999_us = 2303
max_us = 4258
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 5513357
errors = 0
elapsed_s = 5.006
ops_per_sec = 1101420
p50_us = 471
p99_us = 847
p999_us = 2751
p9999_us = 5503
max_us = 7933
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 5321300
errors = 0
elapsed_s = 5.005
ops_per_sec = 1063213
p50_us = 487
p99_us = 879
p999_us = 2495
p9999_us = 5119
max_us = 8371
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 5445281
errors = 0
elapsed_s = 5.006
ops_per_sec = 1087732
p50_us = 479
p99_us = 831
p999_us = 1631
p9999_us = 3071
max_us = 4818
```

## unpipelined 512-conn (M0 gate mix) m2 rep 3

```
ops = 5359645
errors = 0
elapsed_s = 5.006
ops_per_sec = 1070650
p50_us = 471
p99_us = 847
p999_us = 1695
p9999_us = 3135
max_us = 4608
```

## unpipelined 512-conn (M0 gate mix) m2 rep 4

```
ops = 5436649
errors = 0
elapsed_s = 5.006
ops_per_sec = 1086093
p50_us = 487
p99_us = 863
p999_us = 2303
p9999_us = 4735
max_us = 8273
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 15972270
errors = 0
elapsed_s = 5.001
ops_per_sec = 3193947
p50_us = 303
p99_us = 639
p999_us = 2175
p9999_us = 13311
max_us = 19854
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 15070096
errors = 0
elapsed_s = 5.001
ops_per_sec = 3013420
p50_us = 311
p99_us = 687
p999_us = 14079
p9999_us = 16895
max_us = 30536
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 15831178
errors = 0
elapsed_s = 5.001
ops_per_sec = 3165770
p50_us = 303
p99_us = 639
p999_us = 2431
p9999_us = 16383
max_us = 17212
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 3

```
ops = 15301238
errors = 0
elapsed_s = 5.001
ops_per_sec = 3059641
p50_us = 311
p99_us = 655
p999_us = 2175
p9999_us = 17407
max_us = 18377
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 4

```
ops = 15343449
errors = 0
elapsed_s = 5.001
ops_per_sec = 3068036
p50_us = 311
p99_us = 623
p999_us = 2015
p9999_us = 19455
max_us = 37605
```

## always grouped writes (always-grouped)

```
ops = 11709667
errors = 0
elapsed_s = 5.001
ops_per_sec = 2341529
p50_us = 415
p99_us = 959
p999_us = 2239
p9999_us = 3647
max_us = 6361
```

## ckpt-pressure baseline rep 0

```
ops = 13991833
errors = 0
elapsed_s = 5.001
ops_per_sec = 2797821
p50_us = 351
p99_us = 703
p999_us = 1631
p9999_us = 3199
max_us = 5535
```

## ckpt-pressure pressure rep 0

```
ops = 12797800
errors = 0
elapsed_s = 5.001
ops_per_sec = 2559053
p50_us = 383
p99_us = 815
p999_us = 2431
p9999_us = 3711
max_us = 5767
```
