# M2 gate-run report

date: 1787718865 (unix) · cells: 4 · duration: 10s · ONLY-EVERYSEC (A/B leg; frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off)
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-M (ext4)
- p99.9 deltas are quantized by the client histogram (256 sub-buckets/octave ≈ 0.4% since 2026-08-22; 32 ≈ 3% before): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- everysec row: memory-ns 2400229 ops/s (spread 2.76%) vs everysec 1209251 ops/s (spread 26.93%) — signed penalty +49.62%; p999 797 → 62847 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 67583/3932159/4062535 us; both namespaces named (both ride the pump — the row isolates durability cost)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | — | PENDING (tooling) |
| everysec penalty vs memory mode | < 10 % | 49.62 | FAIL |
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
ops = 24074979
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2407223
p50_us = 419
p99_us = 675
p999_us = 797
p9999_us = 895
max_us = 1333
```

## everysec row everysec rep 0

```
ops = 12560109
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1255859
p50_us = 475
p99_us = 935
p999_us = 23679
p9999_us = 593919
max_us = 653486
```

## everysec row everysec rep 1

```
ops = 9611221
errors = 0
busy_retryable = 0
elapsed_s = 10.333
ops_per_sec = 930186
p50_us = 482
p99_us = 921
p999_us = 286719
p9999_us = 679563
max_us = 679563
```

## everysec row memory-ns rep 1

```
ops = 24005111
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2400229
p50_us = 423
p99_us = 651
p999_us = 751
p9999_us = 847
max_us = 1191
```

## everysec row memory-ns rep 2

```
ops = 23413074
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2341043
p50_us = 434
p99_us = 741
p999_us = 867
p9999_us = 967
max_us = 1243
```

## everysec row everysec rep 2

```
ops = 12094189
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 1209251
p50_us = 482
p99_us = 935
p999_us = 62847
p9999_us = 538623
max_us = 746410
```
