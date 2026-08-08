# M4 gate-run report

date: 1784355216 (unix) · cells: 4 · duration: 8s · replicates: 3 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:780f8d57dbc8ab4a (10538800 bytes)
- m3 baseline target/release/infinityd: hash64:780f8d57dbc8ab4a (10538800 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- pipelined 1:10 (M0 gate mix): m3 4231867 ops/s (spread 1.94%) vs m4 4346223 ops/s (spread 3.24%) — signed ops delta +2.70% · p999 1375 → 1119 µs (-18.62%) · peak-RSS 189730816 → 189857792 B (+0.07%)
- unpipelined 512-conn (M0 gate mix): m3 1037047 ops/s (spread 5.52%) vs m4 1039889 ops/s (spread 4.30%) — signed ops delta +0.27% · p999 2943 → 2815 µs (-4.35%) · peak-RSS 133935104 → 134578176 B (+0.48%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 3440275 ops/s (spread 7.66%) vs m4 3529871 ops/s (spread 4.01%) — signed ops delta +2.60% · p999 1695 → 2367 µs (+39.65%) · peak-RSS 264380416 → 267862016 B (+1.32%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 39.65 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 1.32 | FAIL (DEV-TIER, non-binding) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 33858806
errors = 0
elapsed_s = 8.001
ops_per_sec = 4231867
p50_us = 231
p99_us = 463
p999_us = 1663
p9999_us = 8703
max_us = 10233
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 35000023
errors = 0
elapsed_s = 8.001
ops_per_sec = 4374478
p50_us = 227
p99_us = 383
p999_us = 1055
p9999_us = 8447
max_us = 10378
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 33872436
errors = 0
elapsed_s = 8.001
ops_per_sec = 4233604
p50_us = 235
p99_us = 431
p999_us = 1119
p9999_us = 2047
max_us = 3329
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 34351197
errors = 0
elapsed_s = 8.001
ops_per_sec = 4293501
p50_us = 231
p99_us = 423
p999_us = 1087
p9999_us = 2175
max_us = 4088
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 33696148
errors = 0
elapsed_s = 8.001
ops_per_sec = 4211606
p50_us = 235
p99_us = 439
p999_us = 1375
p9999_us = 2111
max_us = 4207
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 34773824
errors = 0
elapsed_s = 8.001
ops_per_sec = 4346223
p50_us = 227
p99_us = 423
p999_us = 1183
p9999_us = 2111
max_us = 4198
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 5190544
errors = 0
elapsed_s = 5.005
ops_per_sec = 1037047
p50_us = 479
p99_us = 975
p999_us = 2943
p9999_us = 5119
max_us = 7333
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 5090357
errors = 0
elapsed_s = 5.005
ops_per_sec = 1016978
p50_us = 487
p99_us = 975
p999_us = 2879
p9999_us = 4095
max_us = 5469
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 5314302
errors = 0
elapsed_s = 5.006
ops_per_sec = 1061652
p50_us = 471
p99_us = 847
p999_us = 2815
p9999_us = 5247
max_us = 6125
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 5120812
errors = 0
elapsed_s = 5.005
ops_per_sec = 1023081
p50_us = 487
p99_us = 911
p999_us = 3135
p9999_us = 5247
max_us = 7626
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 5407088
errors = 0
elapsed_s = 5.005
ops_per_sec = 1080315
p50_us = 471
p99_us = 815
p999_us = 2431
p9999_us = 4735
max_us = 6727
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 5205373
errors = 0
elapsed_s = 5.006
ops_per_sec = 1039889
p50_us = 479
p99_us = 991
p999_us = 2623
p9999_us = 4735
max_us = 7848
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 28111546
errors = 0
elapsed_s = 8.001
ops_per_sec = 3513583
p50_us = 279
p99_us = 559
p999_us = 1695
p9999_us = 19967
max_us = 22758
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 28242033
errors = 0
elapsed_s = 8.001
ops_per_sec = 3529871
p50_us = 271
p99_us = 543
p999_us = 2495
p9999_us = 18943
max_us = 21582
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 27455020
errors = 0
elapsed_s = 8.001
ops_per_sec = 3431531
p50_us = 279
p99_us = 559
p999_us = 2367
p9999_us = 22015
max_us = 34048
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 26003460
errors = 0
elapsed_s = 8.001
ops_per_sec = 3250086
p50_us = 295
p99_us = 623
p999_us = 2751
p9999_us = 22015
max_us = 34571
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 27525208
errors = 0
elapsed_s = 8.001
ops_per_sec = 3440275
p50_us = 279
p99_us = 543
p999_us = 1087
p9999_us = 22015
max_us = 31241
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 28589171
errors = 0
elapsed_s = 8.001
ops_per_sec = 3573247
p50_us = 271
p99_us = 503
p999_us = 2111
p9999_us = 21503
max_us = 23066
```
