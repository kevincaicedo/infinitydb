# M4 gate-run report

date: 1786842878 (unix) · cells: 4 · duration: 10s · replicates: 6 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3000421 ops/s (spread 18.03%) vs m4 2997170 ops/s (spread 8.65%) — signed ops delta -0.11% · p999 655 → 671 µs (+2.44%) · peak-RSS 187428864 → 187973632 B (+0.29%)
- unpipelined 512-conn (M0 gate mix): m3 784330 ops/s (spread 0.75%) vs m4 783564 ops/s (spread 0.30%) — signed ops delta -0.10% · p999 1279 → 1215 µs (-5.00%) · peak-RSS 119066624 → 119570432 B (+0.42%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2553113 ops/s (spread 5.26%) vs m4 2533006 ops/s (spread 4.07%) — signed ops delta -0.79% · p999 4351 → 5119 µs (+17.65%) · peak-RSS 239407104 → 239415296 B (+0.00%)
- mixed_attribution_divergence_pct = 6.5 supplied externally (--mixed-attribution-pct; see --campaign-note)
- cache_isolation_p99_delta_pct = 82.9 supplied externally (--cache-isolation-pct; see --campaign-note)
- recovery:tiered_gbps_per_cell = 0.266 supplied externally (--recovery-gbps-per-cell; see --campaign-note)
- recovery:tiered_10gb_boot_s = 5.906 supplied externally (--recovery-boot-s; see --campaign-note)
- dst:never_none_violations = 0 supplied externally (--dst-violations; see --campaign-note)
- crash:matrix_failures = 0 supplied externally (--crash-failures; see --campaign-note)
- m3:regression_worst_pct = -0.7 supplied externally (--m3-regression-pct; see --campaign-note)
- endurance:rss_slope_pct_per_24h = 0.234 supplied externally (--endurance-rss-slope-pct; see --campaign-note)
- endurance:crashes = 0 supplied externally (--endurance-crashes; see --campaign-note)
- ycsb:hot_set_p50_delta_pct = -12 supplied externally (--hot-set-p50-pct; see --campaign-note)
- ycsb:hot_set_p99_delta_pct = 328.21 supplied externally (--hot-set-p99-pct; see --campaign-note)
- ycsb:hot_set_p999_delta_pct = 40.29 supplied externally (--hot-set-p999-pct; see --campaign-note)
- ycsb:cold_read_p99_ms = 3.65 supplied externally (--cold-read-p99-ms; see --campaign-note)
- campaign: S24 campaign 2026-08-15, server infinityd-6bd25b1 (hash dda9e8ba7a5813dc): hot-set + cold-read + WA from .artifacts/m4/s24/ycsb (pipeline 1 gate leg) and ycsb-loaded (pipeline 8); mixed/isolation from .artifacts/m4/s20/1786835097-mixed-audit; recovery from .artifacts/m4/s24/recovery (recovery-analysis.md); DST from .artifacts/m4/s24/dst (m4-recovery + m4-tiered, 10k seeds each); crash matrix cargo test -p crash-matrix 25 rows green; m3 regression from S24 phase 2 (2026-08-11); endurance from .artifacts/v0.4.0/soak-unified-20260814-0351. Foreground-protection carrier deliberately omitted: not re-run this campaign, so the row stays PENDING rather than citing a July artifact

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.11 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 2.44 | FAIL |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.10 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.79 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 17.65 | FAIL |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.42 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | 6.50 | PASS |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | 82.90 | FAIL |
| Hot set at memory speed: memory-hit p50 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | -12.00 | PASS |
| Hot set at memory speed: memory-hit p99 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split | 328.21 | FAIL |
| Hot set at memory speed: memory-hit p99.9 delta | <= 10 pct vs RAM-resident reference leg, memory-hit split (LogHistogram ~3% buckets) | 40.29 | FAIL |
| Cold reads: p99 < 1.5 ms on NVMe under loaded zipfian rows | < 1.5 ms, cold-read split histogram, worst loaded row | 3.65 | FAIL |
| Memory honesty: RSS slope over the 24 h endurance run | < 0.5 pct per 24 h (storm-resistant first/last-5% medians) | 0.23 | PASS |
| Endurance: zero crashes over the full 24 h run | <= 0 crashes | 0.00 | PASS |
| M3 regression: worst M3 gate delta on memory-mode namespaces | <= 5 pct vs M3 baseline artifact, worst gate | -0.70 | PASS |
| Recovery with tiering on: replay throughput per cell | >= 1 GB/s/cell | 0.27 | FAIL |
| Recovery with tiering on: 10 GB boot | < 15 s | 5.91 | PASS |
| Never-none invariant: zero violations in the 10k-seed DST sweep | <= 0 violations | 0.00 | PASS |
| Crash + ENOSPC matrices: all fault points green | <= 0 failing rows | 0.00 | PASS |
| Foreground protection: p99.9 during demotion + compaction storms | < 2 ms | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 30181274
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3017795
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 10951
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 30007529
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3000421
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 10880
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 29932475
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2992910
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10495
max_us = 11245
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 27589400
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2758648
p50_us = 367
p99_us = 639
p999_us = 719
p9999_us = 10495
max_us = 11742
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 30060636
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3005717
p50_us = 335
p99_us = 575
p999_us = 655
p9999_us = 10751
max_us = 11090
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 30305492
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3030178
p50_us = 335
p99_us = 503
p999_us = 591
p9999_us = 10495
max_us = 11143
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 30120560
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3011726
p50_us = 335
p99_us = 559
p999_us = 623
p9999_us = 10495
max_us = 11725
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 29932492
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2992906
p50_us = 335
p99_us = 607
p999_us = 687
p9999_us = 10495
max_us = 11217
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 29708931
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2970514
p50_us = 343
p99_us = 591
p999_us = 671
p9999_us = 10495
max_us = 12149
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 29966961
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2996345
p50_us = 343
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 10975
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 24896270
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2489305
p50_us = 407
p99_us = 687
p999_us = 767
p9999_us = 10495
max_us = 10977
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 29975002
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2997170
p50_us = 335
p99_us = 559
p999_us = 639
p9999_us = 10495
max_us = 11464
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 3914193
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 781713
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3263
max_us = 4931
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 3930211
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784957
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3391
max_us = 4298
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 3924911
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783804
p50_us = 639
p99_us = 1023
p999_us = 1279
p9999_us = 3519
max_us = 4454
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 3925879
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783991
p50_us = 639
p99_us = 1055
p999_us = 1247
p9999_us = 3391
max_us = 4437
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 3923563
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783564
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3455
max_us = 4086
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 3903205
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 779454
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3391
max_us = 4568
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 3929216
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 784330
p50_us = 639
p99_us = 1023
p999_us = 1183
p9999_us = 3391
max_us = 4199
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 3926284
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 784102
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3391
max_us = 4221
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 3921854
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783265
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3519
max_us = 4482
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 3922584
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 783415
p50_us = 639
p99_us = 1055
p999_us = 1279
p9999_us = 3391
max_us = 4248
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 3932988
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 785322
p50_us = 639
p99_us = 1055
p999_us = 1183
p9999_us = 3327
max_us = 4224
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 3922246
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 783237
p50_us = 639
p99_us = 1055
p999_us = 1215
p9999_us = 3327
max_us = 4244
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 25895786
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2589284
p50_us = 375
p99_us = 655
p999_us = 5119
p9999_us = 20991
max_us = 21739
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 24694727
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2469134
p50_us = 391
p99_us = 719
p999_us = 5631
p9999_us = 18943
max_us = 21299
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 25693380
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2569079
p50_us = 383
p99_us = 703
p999_us = 3647
p9999_us = 17407
max_us = 18024
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 24961266
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2495863
p50_us = 391
p99_us = 687
p999_us = 4479
p9999_us = 16895
max_us = 17628
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 25333446
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2533006
p50_us = 391
p99_us = 719
p999_us = 5119
p9999_us = 20991
max_us = 21672
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 24990690
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2498755
p50_us = 391
p99_us = 703
p999_us = 4351
p9999_us = 16383
max_us = 18756
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 25653867
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2565097
p50_us = 383
p99_us = 655
p999_us = 2943
p9999_us = 18943
max_us = 19400
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 25239116
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2523605
p50_us = 391
p99_us = 655
p999_us = 4223
p9999_us = 14591
max_us = 15442
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 25516892
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2551399
p50_us = 383
p99_us = 655
p999_us = 5503
p9999_us = 19455
max_us = 20453
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 24351724
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2434866
p50_us = 423
p99_us = 735
p999_us = 11263
p9999_us = 15871
max_us = 21105
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 25534243
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2553113
p50_us = 391
p99_us = 655
p999_us = 4351
p9999_us = 19455
max_us = 20442
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 24863718
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2486083
p50_us = 391
p99_us = 735
p999_us = 5375
p9999_us = 18431
max_us = 20320
```
