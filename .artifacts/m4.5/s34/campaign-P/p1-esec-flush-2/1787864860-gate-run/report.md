# M2 gate-run report

date: 1787864860 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2398645 ops/s (spread 1.61%) vs everysec 1404571 ops/s (spread 107.25%) — signed penalty +41.44%; p999 859 → 38783 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 73727/10207645/10207645 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 41.44 | FAIL |
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
ops = 24076451
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2407365
p50_us = 412
p99_us = 717
p999_us = 859
p9999_us = 1001
max_us = 1483
```

## everysec row everysec rep 0

```
ops = 18835184
errors = 0
busy_retryable = 0
elapsed_s = 10.175
ops_per_sec = 1851123
p50_us = 474
p99_us = 977
p999_us = 4543
p9999_us = 217599
max_us = 382686
```

## everysec row everysec rep 1

```
ops = 3925122
errors = 0
busy_retryable = 0
elapsed_s = 11.387
ops_per_sec = 344707
p50_us = 475
p99_us = 831
p999_us = 679935
p9999_us = 3082492
max_us = 3082492
```

## everysec row memory-ns rep 1

```
ops = 23690762
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2368795
p50_us = 429
p99_us = 719
p999_us = 865
p9999_us = 1119
max_us = 1820
```

## everysec row memory-ns rep 2

```
ops = 23989500
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2398645
p50_us = 423
p99_us = 693
p999_us = 827
p9999_us = 957
max_us = 1331
```

## everysec row everysec rep 2

```
ops = 14234699
errors = 0
busy_retryable = 0
elapsed_s = 10.135
ops_per_sec = 1404571
p50_us = 499
p99_us = 1031
p999_us = 38783
p9999_us = 471039
max_us = 669026
```
