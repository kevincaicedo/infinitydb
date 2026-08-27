# M2 gate-run report

date: 1787864534 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: io-properties.toml copied into the row's data dir
- everysec row: memory-ns 2356726 ops/s (spread 2.35%) vs everysec 1046163 ops/s (spread 71.05%) — signed penalty +55.61%; p999 843 → 136703 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 5119/368639/518260 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 55.61 | FAIL |
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
ops = 23351591
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2334881
p50_us = 454
p99_us = 731
p999_us = 877
p9999_us = 1039
max_us = 3194
```

## everysec row everysec rep 0

```
ops = 16437833
errors = 0
busy_retryable = 0
elapsed_s = 10.058
ops_per_sec = 1634252
p50_us = 491
p99_us = 841
p999_us = 45311
p9999_us = 182783
max_us = 244287
```

## everysec row everysec rep 1

```
ops = 8910221
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 890903
p50_us = 470
p99_us = 1287
p999_us = 153087
p9999_us = 214527
max_us = 214965
```

## everysec row memory-ns rep 1

```
ops = 23905238
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2390255
p50_us = 428
p99_us = 701
p999_us = 843
p9999_us = 1005
max_us = 1454
```

## everysec row memory-ns rep 2

```
ops = 23569887
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2356726
p50_us = 442
p99_us = 707
p999_us = 837
p9999_us = 983
max_us = 1361
```

## everysec row everysec rep 2

```
ops = 10589201
errors = 0
busy_retryable = 0
elapsed_s = 10.122
ops_per_sec = 1046163
p50_us = 477
p99_us = 953
p999_us = 136703
p9999_us = 217599
max_us = 218841
```
