# M2 gate-run report

date: 1787864428 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2357181 ops/s (spread 9.69%) vs everysec 1914698 ops/s (spread 56.12%) — signed penalty +18.77%; p999 881 → 12927 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 60415/4718591/5526116 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 18.77 | FAIL |
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
ops = 24074639
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2407187
p50_us = 422
p99_us = 697
p999_us = 829
p9999_us = 973
max_us = 1419
```

## everysec row everysec rep 0

```
ops = 19149631
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1914698
p50_us = 523
p99_us = 1099
p999_us = 3839
p9999_us = 21055
max_us = 27910
```

## everysec row everysec rep 1

```
ops = 8619846
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 861871
p50_us = 473
p99_us = 897
p999_us = 295935
p9999_us = 953568
max_us = 953568
```

## everysec row memory-ns rep 1

```
ops = 21790422
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2178747
p50_us = 461
p99_us = 835
p999_us = 961
p9999_us = 1115
max_us = 1537
```

## everysec row memory-ns rep 2

```
ops = 23575033
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2357181
p50_us = 423
p99_us = 741
p999_us = 881
p9999_us = 1011
max_us = 1347
```

## everysec row everysec rep 2

```
ops = 19601428
errors = 0
busy_retryable = 0
elapsed_s = 10.123
ops_per_sec = 1936320
p50_us = 477
p99_us = 1047
p999_us = 12927
p9999_us = 142335
max_us = 161386
```
