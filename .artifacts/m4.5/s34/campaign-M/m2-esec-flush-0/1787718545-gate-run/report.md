# M2 gate-run report

date: 1787718545 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2354012 ops/s (spread 1.09%) vs everysec 1536190 ops/s (spread 38.07%) — signed penalty +34.74%; p999 901 → 16703 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 55295/3276799/3760158 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 34.74 | FAIL |
| always grouped writes | >= 300000 w/s | — | PENDING (tooling) |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | — | PENDING (tooling) |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | — | PENDING (tooling) |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | — | PENDING (tooling) |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | — | PENDING (tooling) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | — | PENDING (tooling) |

## everysec row memory-ns rep 0

```
ops = 23543132
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2353984
p50_us = 410
p99_us = 807
p999_us = 931
p9999_us = 1055
max_us = 1514
```

## everysec row everysec rep 0

```
ops = 15364129
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1536190
p50_us = 478
p99_us = 993
p999_us = 17407
p9999_us = 454655
max_us = 454713
```

## everysec row everysec rep 1

```
ops = 13820634
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 1381843
p50_us = 480
p99_us = 941
p999_us = 16703
p9999_us = 343039
max_us = 682991
```

## everysec row memory-ns rep 1

```
ops = 23800145
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2379653
p50_us = 425
p99_us = 675
p999_us = 795
p9999_us = 895
max_us = 1243
```

## everysec row memory-ns rep 2

```
ops = 23543367
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2354012
p50_us = 430
p99_us = 795
p999_us = 901
p9999_us = 1059
max_us = 1397
```

## everysec row everysec rep 2

```
ops = 19682140
errors = 0
busy_retryable = 0
elapsed_s = 10.008
ops_per_sec = 1966659
p50_us = 472
p99_us = 1067
p999_us = 10175
p9999_us = 24255
max_us = 27067
```
