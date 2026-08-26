# M2 gate-run report

date: 1787718758 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: io-properties.toml copied into the row's data dir
- everysec row: memory-ns 2404045 ops/s (spread 3.88%) vs everysec 1022241 ops/s (spread 81.40%) — signed penalty +57.48%; p999 761 → 135679 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 5119/344063/371212 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 57.48 | FAIL |
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
ops = 24217469
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2421414
p50_us = 419
p99_us = 651
p999_us = 761
p9999_us = 855
max_us = 1105
```

## everysec row everysec rep 0

```
ops = 17341305
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1733924
p50_us = 507
p99_us = 1011
p999_us = 8671
p9999_us = 122879
max_us = 187343
```

## everysec row everysec rep 1

```
ops = 9087031
errors = 0
busy_retryable = 0
elapsed_s = 10.077
ops_per_sec = 901779
p50_us = 468
p99_us = 997
p999_us = 167935
p9999_us = 210431
max_us = 334217
```

## everysec row memory-ns rep 1

```
ops = 24043822
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2404045
p50_us = 421
p99_us = 647
p999_us = 755
p9999_us = 843
max_us = 1035
```

## everysec row memory-ns rep 2

```
ops = 23285714
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2328254
p50_us = 451
p99_us = 757
p999_us = 903
p9999_us = 1059
max_us = 1372
```

## everysec row everysec rep 2

```
ops = 10223562
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1022241
p50_us = 481
p99_us = 1021
p999_us = 135679
p9999_us = 202239
max_us = 202851
```
