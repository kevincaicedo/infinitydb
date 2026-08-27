# M2 gate-run report

date: 1787864750 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2375757 ops/s (spread 1.59%) vs everysec 1020307 ops/s (spread 86.96%) — signed penalty +57.05%; p999 861 → 172543 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 75775/4587519/5376282 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 57.05 | FAIL |
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
ops = 23697073
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2369365
p50_us = 407
p99_us = 783
p999_us = 949
p9999_us = 1099
max_us = 1620
```

## everysec row everysec rep 0

```
ops = 18760426
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1875796
p50_us = 482
p99_us = 1007
p999_us = 13823
p9999_us = 122879
max_us = 322827
```

## everysec row everysec rep 1

```
ops = 9886297
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 988518
p50_us = 474
p99_us = 901
p999_us = 236031
p9999_us = 840768
max_us = 840768
```

## everysec row memory-ns rep 1

```
ops = 23760347
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2375757
p50_us = 419
p99_us = 723
p999_us = 861
p9999_us = 1043
max_us = 1582
```

## everysec row memory-ns rep 2

```
ops = 24075553
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2407215
p50_us = 422
p99_us = 673
p999_us = 809
p9999_us = 995
max_us = 1429
```

## everysec row everysec rep 2

```
ops = 10204490
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1020307
p50_us = 494
p99_us = 1017
p999_us = 172543
p9999_us = 636891
max_us = 636891
```
