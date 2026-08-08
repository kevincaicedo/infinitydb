# M2 gate-run report

date: 1783049677 (unix) · cells: 4 · replicates: 1 · duration: 10s
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: reference-box gates report measured values, non-binding verdicts
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- S12 pressure data root: /tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 324 ckpt cycles / 324 manifests / 476 segments truncated across 3 pressure legs; p99.9 1119 µs under continuous checkpoints vs 975 µs baseline; peak RSS delta 4.3 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
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
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 1119.00 | PASS (DEV-TIER, non-binding) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 4.28 | PASS (DEV-TIER, non-binding) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | — | PENDING (tooling) |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | — | PENDING (tooling) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 27341854
errors = 0
elapsed_s = 10.001
ops_per_sec = 2733793
p50_us = 367
p99_us = 591
p999_us = 687
p9999_us = 10495
max_us = 11148
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3865496
errors = 0
elapsed_s = 5.008
ops_per_sec = 771901
p50_us = 655
p99_us = 1087
p999_us = 1215
p9999_us = 3391
max_us = 4104
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 23778965
errors = 0
elapsed_s = 10.001
ops_per_sec = 2377589
p50_us = 415
p99_us = 719
p999_us = 1855
p9999_us = 12799
max_us = 14293
```

## ckpt-pressure baseline rep 0

```
ops = 19599724
errors = 0
elapsed_s = 10.001
ops_per_sec = 1959730
p50_us = 511
p99_us = 959
p999_us = 1087
p9999_us = 1279
max_us = 3117
```

## ckpt-pressure pressure rep 0

```
ops = 18596553
errors = 0
elapsed_s = 10.001
ops_per_sec = 1859380
p50_us = 543
p99_us = 991
p999_us = 1183
p9999_us = 1439
max_us = 5169
```

## ckpt-pressure pressure rep 1

```
ops = 18943615
errors = 0
elapsed_s = 10.001
ops_per_sec = 1894136
p50_us = 527
p99_us = 959
p999_us = 1119
p9999_us = 1279
max_us = 2325
```

## ckpt-pressure baseline rep 1

```
ops = 20125163
errors = 0
elapsed_s = 10.001
ops_per_sec = 2012262
p50_us = 503
p99_us = 831
p999_us = 975
p9999_us = 1119
max_us = 2373
```

## ckpt-pressure baseline rep 2

```
ops = 20139218
errors = 0
elapsed_s = 10.001
ops_per_sec = 2013636
p50_us = 503
p99_us = 799
p999_us = 927
p9999_us = 1055
max_us = 3735
```

## ckpt-pressure pressure rep 2

```
ops = 18924085
errors = 0
elapsed_s = 10.002
ops_per_sec = 1892112
p50_us = 543
p99_us = 927
p999_us = 1087
p9999_us = 1247
max_us = 1975
```
