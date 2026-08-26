# M2 gate-run report

date: 1787718973 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2345296 ops/s (spread 1.25%) vs everysec 1104592 ops/s (spread 39.03%) — signed penalty +52.90%; p999 893 → 55807 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 73727/5111807/5469731 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 52.90 | FAIL |
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
ops = 23456285
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2345296
p50_us = 418
p99_us = 793
p999_us = 899
p9999_us = 1043
max_us = 1340
```

## everysec row everysec rep 0

```
ops = 14703247
errors = 0
busy_retryable = 0
elapsed_s = 10.652
ops_per_sec = 1380379
p50_us = 481
p99_us = 947
p999_us = 16575
p9999_us = 608255
max_us = 752581
```

## everysec row everysec rep 1

```
ops = 9776021
errors = 0
busy_retryable = 0
elapsed_s = 10.298
ops_per_sec = 949308
p50_us = 497
p99_us = 1005
p999_us = 55807
p9999_us = 982676
max_us = 982676
```

## everysec row memory-ns rep 1

```
ops = 23411612
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2340850
p50_us = 420
p99_us = 763
p999_us = 893
p9999_us = 1015
max_us = 1525
```

## everysec row memory-ns rep 2

```
ops = 23705833
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2370265
p50_us = 420
p99_us = 745
p999_us = 861
p9999_us = 985
max_us = 1269
```

## everysec row everysec rep 2

```
ops = 11047323
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1104592
p50_us = 484
p99_us = 909
p999_us = 77311
p9999_us = 583679
max_us = 599140
```
