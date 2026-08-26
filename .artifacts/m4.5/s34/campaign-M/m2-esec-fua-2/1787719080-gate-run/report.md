# M2 gate-run report

date: 1787719080 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: io-properties.toml copied into the row's data dir
- everysec row: memory-ns 2380125 ops/s (spread 3.19%) vs everysec 993327 ops/s (spread 110.12%) — signed penalty +58.27%; p999 813 → 145919 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 5503/671743/1084876 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 58.27 | FAIL |
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
ops = 23994089
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2399141
p50_us = 425
p99_us = 659
p999_us = 771
p9999_us = 861
max_us = 1101
```

## everysec row everysec rep 0

```
ops = 13819484
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1381763
p50_us = 493
p99_us = 983
p999_us = 98559
p9999_us = 165887
max_us = 263234
```

## everysec row everysec rep 1

```
ops = 2879929
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 287955
p50_us = 432
p99_us = 69375
p999_us = 526335
p9999_us = 1422384
max_us = 1422384
```

## everysec row memory-ns rep 1

```
ops = 23803964
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2380125
p50_us = 425
p99_us = 695
p999_us = 813
p9999_us = 909
max_us = 1117
```

## everysec row memory-ns rep 2

```
ops = 23233691
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2323101
p50_us = 420
p99_us = 785
p999_us = 915
p9999_us = 1021
max_us = 1370
```

## everysec row everysec rep 2

```
ops = 9934528
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 993327
p50_us = 476
p99_us = 953
p999_us = 145919
p9999_us = 218111
max_us = 284743
```
