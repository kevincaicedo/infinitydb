# M2 gate-run report

date: 1787864641 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: io-properties.toml copied into the row's data dir
- everysec row: memory-ns 2367179 ops/s (spread 1.76%) vs everysec 952945 ops/s (spread 82.16%) — signed penalty +59.74%; p999 897 → 141311 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 5119/327679/369871 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 59.74 | FAIL |
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
ops = 23801400
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2379807
p50_us = 413
p99_us = 757
p999_us = 897
p9999_us = 1035
max_us = 1476
```

## everysec row everysec rep 0

```
ops = 17171647
errors = 0
busy_retryable = 0
elapsed_s = 10.178
ops_per_sec = 1687161
p50_us = 494
p99_us = 853
p999_us = 20287
p9999_us = 201727
max_us = 225950
```

## everysec row everysec rep 1

```
ops = 9043490
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 904208
p50_us = 474
p99_us = 1111
p999_us = 141311
p9999_us = 239615
max_us = 239908
```

## everysec row memory-ns rep 1

```
ops = 23383405
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2338065
p50_us = 432
p99_us = 769
p999_us = 879
p9999_us = 1023
max_us = 1332
```

## everysec row memory-ns rep 2

```
ops = 23674332
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2367179
p50_us = 418
p99_us = 775
p999_us = 901
p9999_us = 1031
max_us = 1319
```

## everysec row everysec rep 2

```
ops = 9530807
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 952945
p50_us = 477
p99_us = 989
p999_us = 160255
p9999_us = 251903
max_us = 276254
```
