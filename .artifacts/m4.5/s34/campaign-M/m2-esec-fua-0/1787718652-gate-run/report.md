# M2 gate-run report

date: 1787718652 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: io-properties.toml copied into the row's data dir
- everysec row: memory-ns 2404384 ops/s (spread 1.04%) vs everysec 1221714 ops/s (spread 43.86%) — signed penalty +49.19%; p999 785 → 135679 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 4991/360447/425516 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 49.19 | FAIL |
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
ops = 24046587
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2404384
p50_us = 426
p99_us = 671
p999_us = 785
p9999_us = 883
max_us = 1087
```

## everysec row everysec rep 0

```
ops = 13374289
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1337247
p50_us = 484
p99_us = 917
p999_us = 107263
p9999_us = 225791
max_us = 322186
```

## everysec row everysec rep 1

```
ops = 8014873
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 801376
p50_us = 451
p99_us = 8447
p999_us = 184831
p9999_us = 218111
max_us = 300063
```

## everysec row memory-ns rep 1

```
ops = 23815639
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2381284
p50_us = 413
p99_us = 747
p999_us = 875
p9999_us = 985
max_us = 1255
```

## everysec row memory-ns rep 2

```
ops = 24066106
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2406316
p50_us = 423
p99_us = 657
p999_us = 767
p9999_us = 861
max_us = 1139
```

## everysec row everysec rep 2

```
ops = 12218787
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1221714
p50_us = 503
p99_us = 1059
p999_us = 135679
p9999_us = 202239
max_us = 350309
```
