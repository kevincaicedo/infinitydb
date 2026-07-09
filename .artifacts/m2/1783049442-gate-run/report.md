# M2 gate-run report

date: 1783049442 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- S12 pressure data root: /home/kcaicedo/.cache/inf-m2-press (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 197 ckpt cycles / 197 manifests / 252 segments truncated across 3 pressure legs; p99.9 51199 µs under continuous checkpoints vs 43007 µs baseline; peak RSS delta 17.5 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| always grouped writes | >= 300000 w/s | — | PENDING (tooling) |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 51199.00 | FAIL (DEV-TIER, non-binding) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 17.47 | PASS (DEV-TIER, non-binding) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | — | PENDING (tooling) |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | — | PENDING (tooling) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 27421021
errors = 0
elapsed_s = 10.001
ops_per_sec = 2741778
p50_us = 367
p99_us = 607
p999_us = 703
p9999_us = 10495
max_us = 20801
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 26306960
errors = 0
elapsed_s = 10.001
ops_per_sec = 2630383
p50_us = 391
p99_us = 623
p999_us = 703
p9999_us = 815
max_us = 2391
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 25254508
errors = 0
elapsed_s = 10.001
ops_per_sec = 2525156
p50_us = 407
p99_us = 703
p999_us = 799
p9999_us = 911
max_us = 2374
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3843634
errors = 0
elapsed_s = 5.008
ops_per_sec = 767461
p50_us = 655
p99_us = 1055
p999_us = 1279
p9999_us = 3327
max_us = 4065
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3827619
errors = 0
elapsed_s = 5.007
ops_per_sec = 764450
p50_us = 655
p99_us = 1087
p999_us = 1183
p9999_us = 1503
max_us = 3928
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3827587
errors = 0
elapsed_s = 5.007
ops_per_sec = 764419
p50_us = 655
p99_us = 1087
p999_us = 1151
p9999_us = 1535
max_us = 4341
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 23488722
errors = 0
elapsed_s = 10.001
ops_per_sec = 2348534
p50_us = 423
p99_us = 783
p999_us = 2015
p9999_us = 12799
max_us = 13147
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 23288024
errors = 0
elapsed_s = 10.001
ops_per_sec = 2328545
p50_us = 423
p99_us = 767
p999_us = 975
p9999_us = 13311
max_us = 13704
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 23062360
errors = 0
elapsed_s = 10.001
ops_per_sec = 2305998
p50_us = 423
p99_us = 767
p999_us = 1007
p9999_us = 13055
max_us = 13554
```

## ckpt-pressure baseline rep 0

```
ops = 14894778
errors = 0
elapsed_s = 10.001
ops_per_sec = 1489302
p50_us = 527
p99_us = 1407
p999_us = 43007
p9999_us = 67583
max_us = 85544
```

## ckpt-pressure pressure rep 0

```
ops = 6787022
errors = 2634
elapsed_s = 10.009
ops_per_sec = 678105
p50_us = 543
p99_us = 1631
p999_us = 286719
p9999_us = 1053216
max_us = 1053216
```

## ckpt-pressure pressure rep 1

```
ops = 10963561
errors = 15643
elapsed_s = 10.016
ops_per_sec = 1094659
p50_us = 543
p99_us = 2111
p999_us = 51199
p9999_us = 622591
max_us = 931547
```

## ckpt-pressure baseline rep 1

```
ops = 14419826
errors = 0
elapsed_s = 10.001
ops_per_sec = 1441823
p50_us = 543
p99_us = 2015
p999_us = 28159
p9999_us = 48127
max_us = 69410
```

## ckpt-pressure baseline rep 2

```
ops = 14012659
errors = 51786
elapsed_s = 10.001
ops_per_sec = 1401101
p50_us = 511
p99_us = 1567
p999_us = 45055
p9999_us = 204799
max_us = 376484
```

## ckpt-pressure pressure rep 2

```
ops = 13682421
errors = 0
elapsed_s = 10.001
ops_per_sec = 1368084
p50_us = 543
p99_us = 1535
p999_us = 46079
p9999_us = 90111
max_us = 95742
```
