# M2 gate-run report

date: 1783699515 (unix) · cells: 4 · replicates: 3 · duration: 10s
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- M2 leg of the zero-cost rows runs without --data-dir (the memory-only assembly — the S09 posture: no durable plane constructed); the zero-record assert on a durable-enabled node is the node_e2e mixed-class test
- --baseline-bin not given: zero-cost delta rows report PENDING (build the pre-M2 commit's infinityd and pass its path)
- always row: 1684160 gated acks / 17110 fsyncs = ratio 98.4; fsync latency p50/p99/p999 = 2495/4031/5887 us (HDR ~3% quantization); log_writes_per_iter 0.039 (98168 frames / 2531328 iterations); group formation p50/p99 = 99/135 records vs ~256 available in-flight writes/cell = 0.39x (M2.5-S07 gate: >= 0.8x)
- everysec row: memory-ns 2069352 ops/s (spread 0.45%) vs everysec 1860958 ops/s (spread 1.03%) — signed penalty +10.07%; p999 1279 → 2111 µs (§18 flat-tails supporting); fsync latency p50/p99/p999 = 46079/98034/102970 us; both namespaces named (both ride the pump — the row isolates durability cost)
- attribution (durable fill leg, log domains included): sum(domains) 1421940504 B vs VmRSS 1407279104 B — 1.0% divergence
- S12 pressure data root: /home/kcaicedo/.cache/inf-tmp (default is the system temp dir — often tmpfs; point --pressure-data-root at a real filesystem for device-exercising rows)
- S12 pressure fsync latency (worst leg): p50/p99/p999 = 60415/1400004/1400004 us (HDR ~3% quantization)
- S12 pressure: durable everysec 1:1 mix, 200000 keys × 512 B, 240 ckpt cycles / 240 manifests / 416 segments truncated across 3 pressure legs; p99.9 8447 µs under continuous checkpoints vs 36863 µs baseline; peak RSS delta 3.5 MiB (ckpt buffer gauge peaked at 1024 KiB — the L5 domain); truncation ran in-row (reclamation live under load)
- S12 disclosures: foreground latency is client-observed (loop-histogram artifact rides S22); fsync latency histograms export with S21 — fsyncs_completed counters are in the raw INFO; everysec acks on apply, so the p99.9 bar is loop-bound, not fsync-bound

| gate | threshold | measured | verdict |
|---|---|---|---|
| Zero-cost A/B: pipelined ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: pipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined 512-conn ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: unpipelined p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy write-mix ops regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Zero-cost A/B: ttl-heavy p99.9 regression | <= 1 % worse than M1 build (improvements clamp to 0, signed value in notes) | — | PENDING (tooling) |
| Memory-only rows append zero log records | <= 0 records | 0.00 | PASS |
| everysec penalty vs memory mode | < 10 % | 10.07 | FAIL |
| always grouped writes | >= 300000 w/s | 153911.08 | FAIL |
| Replay throughput per cell | >= 1 GB/s/cell | — | PENDING (tooling) |
| 10 GB node cold boot | < 15 s | — | PENDING (tooling) |
| DST durability oracle: 10k seeds | <= 0 violations | — | PENDING (tooling) |
| Crash matrix green in CI | <= 0 failures | — | PENDING (tooling) |
| Checkpoint under full load: foreground p99.9 (anti-BGREWRITEAOF) | < 2000 us | 8447.00 | FAIL |
| RSS under continuous checkpoints vs no-checkpoint control (anti-2x) | <= 64 MiB peak-VmRSS delta (ckpt buffer domain is ~0.5 MiB/cell; a fork/COW would be dataset-sized) | 3.51 | PASS |
| M0/M1 gates re-pass | <= 5 % vs M1 artifact | — | PENDING (tooling) |
| One log write per iteration | <= 1 writes/iter | 0.04 | PASS |
| acks/fsync grouping ratio above floor | >= 2 acks per fsync | 98.43 | PASS (informational) |
| sum(domains) vs RSS divergence (with log domains) | <= 10 % | 1.04 | PASS |

## pipelined 1:10 (M0 gate mix) m2 rep 0

```
ops = 27652693
errors = 0
elapsed_s = 10.001
ops_per_sec = 2764920
p50_us = 351
p99_us = 671
p999_us = 975
p9999_us = 11263
max_us = 12052
```

## pipelined 1:10 (M0 gate mix) m2 rep 1

```
ops = 25882913
errors = 0
elapsed_s = 10.001
ops_per_sec = 2587991
p50_us = 399
p99_us = 767
p999_us = 959
p9999_us = 1503
max_us = 2800
```

## pipelined 1:10 (M0 gate mix) m2 rep 2

```
ops = 27630841
errors = 0
elapsed_s = 10.001
ops_per_sec = 2762710
p50_us = 359
p99_us = 687
p999_us = 943
p9999_us = 1439
max_us = 4249
```

## unpipelined 512-conn (M0 gate mix) m2 rep 0

```
ops = 3454350
errors = 0
elapsed_s = 5.007
ops_per_sec = 689876
p50_us = 703
p99_us = 1503
p999_us = 1823
p9999_us = 3519
max_us = 5127
```

## unpipelined 512-conn (M0 gate mix) m2 rep 1

```
ops = 3349237
errors = 0
elapsed_s = 5.007
ops_per_sec = 668872
p50_us = 735
p99_us = 1503
p999_us = 1791
p9999_us = 2431
max_us = 4544
```

## unpipelined 512-conn (M0 gate mix) m2 rep 2

```
ops = 3539078
errors = 0
elapsed_s = 5.008
ops_per_sec = 706698
p50_us = 703
p99_us = 1471
p999_us = 1631
p9999_us = 1951
max_us = 4014
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 0

```
ops = 24550480
errors = 0
elapsed_s = 10.001
ops_per_sec = 2454747
p50_us = 391
p99_us = 751
p999_us = 2015
p9999_us = 14847
max_us = 15433
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 1

```
ops = 24764858
errors = 0
elapsed_s = 10.001
ops_per_sec = 2476210
p50_us = 391
p99_us = 735
p999_us = 1503
p9999_us = 15615
max_us = 16609
```

## ttl-heavy 1:1 writes (M1 gate mix) m2 rep 2

```
ops = 24040794
errors = 0
elapsed_s = 10.001
ops_per_sec = 2403799
p50_us = 407
p99_us = 815
p999_us = 1407
p9999_us = 15359
max_us = 16023
```

## always grouped writes (always-grouped)

```
ops = 1540247
errors = 0
elapsed_s = 10.007
ops_per_sec = 153911
p50_us = 6271
p99_us = 11007
p999_us = 13055
p9999_us = 15571
max_us = 15571
```

## everysec row memory-ns rep 0

```
ops = 20693921
errors = 0
elapsed_s = 10.001
ops_per_sec = 2069092
p50_us = 479
p99_us = 927
p999_us = 1343
p9999_us = 1919
max_us = 3240
```

## everysec row everysec rep 0

```
ops = 18440754
errors = 0
elapsed_s = 10.001
ops_per_sec = 1843840
p50_us = 527
p99_us = 1119
p999_us = 3135
p9999_us = 16127
max_us = 18724
```

## everysec row everysec rep 1

```
ops = 18613568
errors = 0
elapsed_s = 10.002
ops_per_sec = 1860958
p50_us = 527
p99_us = 1023
p999_us = 2111
p9999_us = 13055
max_us = 17215
```

## everysec row memory-ns rep 1

```
ops = 20696023
errors = 0
elapsed_s = 10.001
ops_per_sec = 2069352
p50_us = 487
p99_us = 927
p999_us = 1279
p9999_us = 1759
max_us = 2983
```

## everysec row memory-ns rep 2

```
ops = 20786993
errors = 0
elapsed_s = 10.001
ops_per_sec = 2078453
p50_us = 487
p99_us = 927
p999_us = 1215
p9999_us = 1599
max_us = 3855
```

## everysec row everysec rep 2

```
ops = 18632840
errors = 0
elapsed_s = 10.001
ops_per_sec = 1863069
p50_us = 527
p99_us = 1055
p999_us = 1919
p9999_us = 23551
max_us = 24375
```

## ckpt-pressure baseline rep 0

```
ops = 18776406
errors = 301
elapsed_s = 10.001
ops_per_sec = 1877405
p50_us = 527
p99_us = 1055
p999_us = 2015
p9999_us = 13311
max_us = 15113
```

## ckpt-pressure pressure rep 0

```
ops = 14421564
errors = 3616
elapsed_s = 10.001
ops_per_sec = 1441973
p50_us = 559
p99_us = 1183
p999_us = 26623
p9999_us = 311295
max_us = 352480
```

## ckpt-pressure pressure rep 1

```
ops = 17225716
errors = 516
elapsed_s = 10.001
ops_per_sec = 1722339
p50_us = 543
p99_us = 1183
p999_us = 8447
p9999_us = 30207
max_us = 32094
```

## ckpt-pressure baseline rep 1

```
ops = 13067847
errors = 26984
elapsed_s = 10.109
ops_per_sec = 1292742
p50_us = 527
p99_us = 1119
p999_us = 36863
p9999_us = 352255
max_us = 405767
```

## ckpt-pressure baseline rep 2

```
ops = 3675695
errors = 3149
elapsed_s = 11.315
ops_per_sec = 324852
p50_us = 543
p99_us = 1119
p999_us = 425983
p9999_us = 4136080
max_us = 4136080
```

## ckpt-pressure pressure rep 2

```
ops = 17485931
errors = 1595
elapsed_s = 10.001
ops_per_sec = 1748348
p50_us = 559
p99_us = 1151
p999_us = 6271
p9999_us = 27135
max_us = 31679
```
