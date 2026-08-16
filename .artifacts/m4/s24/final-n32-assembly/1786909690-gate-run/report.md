# M4 gate-run report

date: 1786909690 (unix) · cells: 4 · duration: 10s · replicates: 32 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: OK
tier: reference-box (binding)

notes:
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1: hash64:1010d6649adf52a6 (12723224 bytes)
- m3 baseline /home/kcaicedo/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- server cells pinned: --pin-start 4 (same cpu set both legs)
- slot crossover active (week-4 instrument fix): servers respawn per replicate and the binary↔slot assignment alternates; legs run in spawn order so slot + load-order bias cancels in the leg medians over an even replicate count
- pipelined 1:10 (M0 gate mix): m3 3147453 ops/s (spread 4.72%) vs m4 3162276 ops/s (spread 7.51%) — signed ops delta +0.47% · p999 799 → 799 µs (+0.00%) · peak-RSS 188129280 → 188637184 B (+0.27%)
- unpipelined 512-conn (M0 gate mix): m3 807288 ops/s (spread 0.91%) vs m4 806772 ops/s (spread 1.21%) — signed ops delta -0.06% · p999 1823 → 1791 µs (-1.76%) · peak-RSS 119926784 → 120328192 B (+0.33%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 2691103 ops/s (spread 7.10%) vs m4 2688844 ops/s (spread 7.57%) — signed ops delta -0.08% · p999 4095 → 4351 µs (+6.25%) · peak-RSS 243089408 → 243896320 B (+0.33%)
- write amplification 1.890× supplied externally (milli 1890) — the gate row binds this value; the memory-mode rows above report `n/a` because no tiered namespace exists on a node this harness can build yet (S19/S22 own the tiered rows)
- mixed_attribution_divergence_pct = 6.5 supplied externally (--mixed-attribution-pct; see --campaign-note)
- cache_isolation_p99_delta_pct = 82.9 supplied externally (--cache-isolation-pct; see --campaign-note)
- recovery:tiered_gbps_per_cell = 0.266 supplied externally (--recovery-gbps-per-cell; see --campaign-note)
- recovery:tiered_10gb_boot_s = 5.906 supplied externally (--recovery-boot-s; see --campaign-note)
- dst:never_none_violations = 0 supplied externally (--dst-violations; see --campaign-note)
- crash:matrix_failures = 0 supplied externally (--crash-failures; see --campaign-note)
- m3:regression_worst_pct = -0.7 supplied externally (--m3-regression-pct; see --campaign-note)
- storm:foreground_p999_ms = 1.721 supplied externally (--foreground-p999-ms; see --campaign-note)
- endurance:rss_slope_pct_per_24h = 0.234 supplied externally (--endurance-rss-slope-pct; see --campaign-note)
- endurance:crashes = 0 supplied externally (--endurance-crashes; see --campaign-note)
- ycsb:hot_set_p50_delta_pct = -12 supplied externally (--hot-set-p50-pct; see --campaign-note)
- ycsb:hot_set_p99_delta_pct = 328.21 supplied externally (--hot-set-p99-pct; see --campaign-note)
- ycsb:hot_set_p999_delta_pct = 40.29 supplied externally (--hot-set-p999-pct; see --campaign-note)
- ycsb:cold_read_p99_ms = 3.65 supplied externally (--cold-read-p99-ms; see --campaign-note)
- campaign: S24 final assembly 2026-08-16, n=32 (supersedes the n=6 .artifacts/m4/s24/final table and the artifact-less n=32 pair salvaged in .artifacts/m4/s24/degenerate-n32). Server infinityd-6bd25b1 vs M3 baseline infinityd-m3-a1ebcb9, both staged outside the checkout. Box state: a leftover recovery-cold infinityd (pid 104598, 2.3 GB RSS, four cell threads on cpu 5 burning ~4% continuously) was found contending with the pinned cell set 4-7; it was SIGSTOPped and verified at zero ticks before this leg started, and an earlier attempt of this same leg was discarded because it had run alongside it. Carriers: hot-set + cold-read from .artifacts/m4/s24/ycsb (pipeline 1 gate leg) and ycsb-loaded (pipeline 8); write amplification 1.890x from cargo test -p inf-store --test tiered_write_amp; mixed/isolation from .artifacts/m4/s20/1786835097-mixed-audit; recovery from .artifacts/m4/s24/recovery (recovery-analysis.md); DST from .artifacts/m4/s24/dst (m4-recovery + m4-tiered, 10k seeds each); crash matrix cargo test -p crash-matrix 25 rows green; m3 regression from S24 phase 2 (2026-08-11); endurance from .artifacts/v0.4.0/soak-unified-20260814-0351; foreground protection 1.721 ms = worst p99.9 of 6 device-loaded storm replicates in .artifacts/m4/s24/storms (demotion 1.702-1.715, compaction 1.699-1.721), which closes the last PENDING row

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS |
| Degenerate A/B: pipelined p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at 32 sub-buckets/octave = ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | 0.00 | PASS |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.06 | PASS |
| Degenerate A/B: unpipelined p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | 0.00 | PASS |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.08 | PASS |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 0 % vs M3 baseline — SAME HISTOGRAM BUCKET OR BETTER (ADR-0070 D4b, 2026-08-16). LogHistogram quantises at ~3%/bucket, so the only readable states are 0.00 (same bucket) and >= 1 bucket; the former 1% threshold was unreadable and a same-binary A/A control failed it | 6.25 | FAIL |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 0.33 | PASS |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | 1.89 | PASS |
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
| Foreground protection: p99.9 during demotion + compaction storms | < 2 ms | 1.72 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| pipelined 1:10 (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| unpipelined 512-conn (M0 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ttl-heavy 1:1 writes (M1 gate mix) | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 30750796
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3074741
p50_us = 319
p99_us = 623
p999_us = 847
p9999_us = 10495
max_us = 11084
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 31027791
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3102446
p50_us = 319
p99_us = 623
p999_us = 799
p9999_us = 10239
max_us = 10661
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 1

```
ops = 30936646
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3093306
p50_us = 319
p99_us = 591
p999_us = 767
p9999_us = 10239
max_us = 11004
```

## pipelined 1:10 (M0 gate mix) m4 rep 1

```
ops = 31307600
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3130435
p50_us = 311
p99_us = 591
p999_us = 847
p9999_us = 10239
max_us = 10754
```

## pipelined 1:10 (M0 gate mix) m4 rep 2

```
ops = 31492207
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3148807
p50_us = 319
p99_us = 591
p999_us = 799
p9999_us = 10239
max_us = 10723
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 2

```
ops = 30810454
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3080673
p50_us = 327
p99_us = 607
p999_us = 863
p9999_us = 10239
max_us = 12714
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 3

```
ops = 32132373
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3212827
p50_us = 311
p99_us = 495
p999_us = 719
p9999_us = 10239
max_us = 11177
```

## pipelined 1:10 (M0 gate mix) m4 rep 3

```
ops = 30890777
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3088650
p50_us = 327
p99_us = 623
p999_us = 847
p9999_us = 10495
max_us = 11480
```

## pipelined 1:10 (M0 gate mix) m4 rep 4

```
ops = 30514328
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3051103
p50_us = 327
p99_us = 655
p999_us = 831
p9999_us = 10751
max_us = 12889
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 4

```
ops = 31546675
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3154205
p50_us = 319
p99_us = 575
p999_us = 767
p9999_us = 10239
max_us = 10970
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 5

```
ops = 31348766
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3134476
p50_us = 319
p99_us = 559
p999_us = 847
p9999_us = 10495
max_us = 12896
```

## pipelined 1:10 (M0 gate mix) m4 rep 5

```
ops = 32076790
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3207327
p50_us = 311
p99_us = 495
p999_us = 783
p9999_us = 10239
max_us = 10718
```

## pipelined 1:10 (M0 gate mix) m4 rep 6

```
ops = 32181112
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3217686
p50_us = 311
p99_us = 487
p999_us = 719
p9999_us = 10239
max_us = 10920
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 6

```
ops = 31985047
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3198119
p50_us = 319
p99_us = 503
p999_us = 799
p9999_us = 9983
max_us = 11429
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 7

```
ops = 31493723
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3148957
p50_us = 319
p99_us = 543
p999_us = 815
p9999_us = 10239
max_us = 11155
```

## pipelined 1:10 (M0 gate mix) m4 rep 7

```
ops = 32010278
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3200655
p50_us = 319
p99_us = 503
p999_us = 735
p9999_us = 10239
max_us = 10809
```

## pipelined 1:10 (M0 gate mix) m4 rep 8

```
ops = 30842548
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3083813
p50_us = 327
p99_us = 591
p999_us = 815
p9999_us = 10495
max_us = 11420
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 8

```
ops = 31496652
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3149307
p50_us = 319
p99_us = 543
p999_us = 831
p9999_us = 10495
max_us = 10763
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 9

```
ops = 31532113
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3152869
p50_us = 319
p99_us = 559
p999_us = 751
p9999_us = 10239
max_us = 10829
```

## pipelined 1:10 (M0 gate mix) m4 rep 9

```
ops = 31626702
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3162276
p50_us = 319
p99_us = 543
p999_us = 799
p9999_us = 10239
max_us = 11039
```

## pipelined 1:10 (M0 gate mix) m4 rep 10

```
ops = 31252353
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3124815
p50_us = 319
p99_us = 607
p999_us = 911
p9999_us = 10239
max_us = 11017
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 10

```
ops = 31166752
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3116239
p50_us = 319
p99_us = 575
p999_us = 751
p9999_us = 10495
max_us = 11094
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 11

```
ops = 31201218
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 3119652
p50_us = 319
p99_us = 591
p999_us = 815
p9999_us = 10239
max_us = 11038
```

## pipelined 1:10 (M0 gate mix) m4 rep 11

```
ops = 31489586
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3148539
p50_us = 311
p99_us = 591
p999_us = 863
p9999_us = 10751
max_us = 12917
```

## pipelined 1:10 (M0 gate mix) m4 rep 12

```
ops = 31344795
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3134118
p50_us = 319
p99_us = 607
p999_us = 831
p9999_us = 10495
max_us = 12864
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 12

```
ops = 31509602
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3150557
p50_us = 319
p99_us = 543
p999_us = 799
p9999_us = 10239
max_us = 10930
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 13

```
ops = 30705059
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3070117
p50_us = 327
p99_us = 607
p999_us = 847
p9999_us = 10495
max_us = 11075
```

## pipelined 1:10 (M0 gate mix) m4 rep 13

```
ops = 31438737
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3143461
p50_us = 327
p99_us = 543
p999_us = 767
p9999_us = 10495
max_us = 11146
```

## pipelined 1:10 (M0 gate mix) m4 rep 14

```
ops = 31779669
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3177557
p50_us = 319
p99_us = 559
p999_us = 799
p9999_us = 10239
max_us = 10969
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 14

```
ops = 31810139
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3180656
p50_us = 319
p99_us = 527
p999_us = 815
p9999_us = 10239
max_us = 11504
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 15

```
ops = 31175203
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3117066
p50_us = 327
p99_us = 623
p999_us = 863
p9999_us = 10495
max_us = 11248
```

## pipelined 1:10 (M0 gate mix) m4 rep 15

```
ops = 31129869
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3112584
p50_us = 327
p99_us = 559
p999_us = 751
p9999_us = 10495
max_us = 11252
```

## pipelined 1:10 (M0 gate mix) m4 rep 16

```
ops = 31676054
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3167241
p50_us = 319
p99_us = 559
p999_us = 783
p9999_us = 5887
max_us = 11834
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 16

```
ops = 31610799
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3160661
p50_us = 311
p99_us = 559
p999_us = 783
p9999_us = 10495
max_us = 11010
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 17

```
ops = 31282255
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3127815
p50_us = 311
p99_us = 591
p999_us = 783
p9999_us = 10495
max_us = 11950
```

## pipelined 1:10 (M0 gate mix) m4 rep 17

```
ops = 31867657
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3186355
p50_us = 343
p99_us = 591
p999_us = 767
p9999_us = 10239
max_us = 11252
```

## pipelined 1:10 (M0 gate mix) m4 rep 18

```
ops = 31795856
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3179197
p50_us = 311
p99_us = 559
p999_us = 719
p9999_us = 10495
max_us = 10983
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 18

```
ops = 31478127
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3147453
p50_us = 319
p99_us = 575
p999_us = 751
p9999_us = 10239
max_us = 11478
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 19

```
ops = 30923019
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3091872
p50_us = 319
p99_us = 591
p999_us = 895
p9999_us = 10239
max_us = 11419
```

## pipelined 1:10 (M0 gate mix) m4 rep 19

```
ops = 31758286
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3175420
p50_us = 319
p99_us = 527
p999_us = 799
p9999_us = 10751
max_us = 12693
```

## pipelined 1:10 (M0 gate mix) m4 rep 20

```
ops = 31668201
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3166380
p50_us = 319
p99_us = 527
p999_us = 799
p9999_us = 10495
max_us = 12943
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 20

```
ops = 31731091
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3172651
p50_us = 319
p99_us = 527
p999_us = 799
p9999_us = 10495
max_us = 10816
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 21

```
ops = 30755131
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3075177
p50_us = 335
p99_us = 575
p999_us = 783
p9999_us = 10495
max_us = 11037
```

## pipelined 1:10 (M0 gate mix) m4 rep 21

```
ops = 32073868
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3206956
p50_us = 311
p99_us = 495
p999_us = 735
p9999_us = 10239
max_us = 10861
```

## pipelined 1:10 (M0 gate mix) m4 rep 22

```
ops = 31681451
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3167718
p50_us = 319
p99_us = 559
p999_us = 799
p9999_us = 10239
max_us = 11773
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 22

```
ops = 31223740
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3121999
p50_us = 319
p99_us = 591
p999_us = 799
p9999_us = 10495
max_us = 11606
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 23

```
ops = 31501271
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3149725
p50_us = 319
p99_us = 575
p999_us = 767
p9999_us = 10239
max_us = 11860
```

## pipelined 1:10 (M0 gate mix) m4 rep 23

```
ops = 32116429
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3211239
p50_us = 311
p99_us = 495
p999_us = 767
p9999_us = 10239
max_us = 11100
```

## pipelined 1:10 (M0 gate mix) m4 rep 24

```
ops = 31606802
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3160288
p50_us = 343
p99_us = 607
p999_us = 799
p9999_us = 10239
max_us = 11633
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 24

```
ops = 32049834
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3204583
p50_us = 319
p99_us = 495
p999_us = 735
p9999_us = 10239
max_us = 10582
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 25

```
ops = 32033067
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3202952
p50_us = 319
p99_us = 527
p999_us = 783
p9999_us = 10239
max_us = 10988
```

## pipelined 1:10 (M0 gate mix) m4 rep 25

```
ops = 31914655
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3191071
p50_us = 319
p99_us = 511
p999_us = 687
p9999_us = 10239
max_us = 10771
```

## pipelined 1:10 (M0 gate mix) m4 rep 26

```
ops = 31695664
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3169204
p50_us = 311
p99_us = 575
p999_us = 799
p9999_us = 10239
max_us = 11687
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 26

```
ops = 31285082
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3128087
p50_us = 327
p99_us = 591
p999_us = 799
p9999_us = 10239
max_us = 11136
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 27

```
ops = 31251387
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3124789
p50_us = 319
p99_us = 591
p999_us = 767
p9999_us = 10239
max_us = 10787
```

## pipelined 1:10 (M0 gate mix) m4 rep 27

```
ops = 31592900
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3158852
p50_us = 319
p99_us = 559
p999_us = 879
p9999_us = 10239
max_us = 10969
```

## pipelined 1:10 (M0 gate mix) m4 rep 28

```
ops = 30542200
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3053794
p50_us = 327
p99_us = 607
p999_us = 879
p9999_us = 10239
max_us = 13677
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 28

```
ops = 31965447
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3196177
p50_us = 319
p99_us = 503
p999_us = 767
p9999_us = 10239
max_us = 10657
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 29

```
ops = 30644958
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3064119
p50_us = 359
p99_us = 623
p999_us = 895
p9999_us = 10239
max_us = 11151
```

## pipelined 1:10 (M0 gate mix) m4 rep 29

```
ops = 31492515
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3148867
p50_us = 335
p99_us = 591
p999_us = 799
p9999_us = 10495
max_us = 11400
```

## pipelined 1:10 (M0 gate mix) m4 rep 30

```
ops = 31767696
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3176427
p50_us = 319
p99_us = 527
p999_us = 735
p9999_us = 10495
max_us = 11153
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 30

```
ops = 31866746
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3186270
p50_us = 319
p99_us = 527
p999_us = 847
p9999_us = 10239
max_us = 10712
```

## pipelined 1:10 (M0 gate mix) m3-baseline rep 31

```
ops = 31363730
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 3135961
p50_us = 319
p99_us = 559
p999_us = 783
p9999_us = 5631
max_us = 11856
```

## pipelined 1:10 (M0 gate mix) m4 rep 31

```
ops = 29804339
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2980058
p50_us = 335
p99_us = 671
p999_us = 895
p9999_us = 10239
max_us = 11462
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 4025946
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 804023
p50_us = 623
p99_us = 1023
p999_us = 1791
p9999_us = 3583
max_us = 8558
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 4019608
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 802581
p50_us = 623
p99_us = 1055
p999_us = 1791
p9999_us = 3327
max_us = 4858
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 1

```
ops = 4035098
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 805618
p50_us = 607
p99_us = 1055
p999_us = 1759
p9999_us = 3455
max_us = 4549
```

## unpipelined 512-conn (M0 gate mix) m4 rep 1

```
ops = 4006446
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 799981
p50_us = 623
p99_us = 1119
p999_us = 1695
p9999_us = 3327
max_us = 4493
```

## unpipelined 512-conn (M0 gate mix) m4 rep 2

```
ops = 4027349
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 804222
p50_us = 623
p99_us = 1023
p999_us = 1791
p9999_us = 3391
max_us = 4582
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 2

```
ops = 4025619
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 803615
p50_us = 623
p99_us = 1023
p999_us = 1759
p9999_us = 3391
max_us = 4260
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 3

```
ops = 4029638
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 804485
p50_us = 623
p99_us = 1023
p999_us = 1887
p9999_us = 3583
max_us = 5503
```

## unpipelined 512-conn (M0 gate mix) m4 rep 3

```
ops = 4015058
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 801482
p50_us = 623
p99_us = 1055
p999_us = 1791
p9999_us = 3583
max_us = 4972
```

## unpipelined 512-conn (M0 gate mix) m4 rep 4

```
ops = 4019274
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 802321
p50_us = 623
p99_us = 1023
p999_us = 1823
p9999_us = 3391
max_us = 4279
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 4

```
ops = 4021980
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 803202
p50_us = 623
p99_us = 1023
p999_us = 1823
p9999_us = 3455
max_us = 4313
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 5

```
ops = 4020555
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 802694
p50_us = 623
p99_us = 1023
p999_us = 1599
p9999_us = 3391
max_us = 5261
```

## unpipelined 512-conn (M0 gate mix) m4 rep 5

```
ops = 4010622
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 800631
p50_us = 623
p99_us = 1055
p999_us = 1823
p9999_us = 3583
max_us = 7688
```

## unpipelined 512-conn (M0 gate mix) m4 rep 6

```
ops = 4022234
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 802898
p50_us = 623
p99_us = 1023
p999_us = 1759
p9999_us = 3327
max_us = 4268
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 6

```
ops = 4029621
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 804394
p50_us = 623
p99_us = 1023
p999_us = 1759
p9999_us = 3519
max_us = 4684
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 7

```
ops = 4028758
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 804236
p50_us = 623
p99_us = 1023
p999_us = 1887
p9999_us = 3519
max_us = 6144
```

## unpipelined 512-conn (M0 gate mix) m4 rep 7

```
ops = 4039875
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 806778
p50_us = 607
p99_us = 1007
p999_us = 1887
p9999_us = 3263
max_us = 4070
```

## unpipelined 512-conn (M0 gate mix) m4 rep 8

```
ops = 4041788
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 807164
p50_us = 607
p99_us = 1007
p999_us = 1791
p9999_us = 3583
max_us = 5590
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 8

```
ops = 4044399
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807453
p50_us = 607
p99_us = 1007
p999_us = 1919
p9999_us = 4223
max_us = 5709
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 9

```
ops = 4042608
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 807288
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3519
max_us = 5847
```

## unpipelined 512-conn (M0 gate mix) m4 rep 9

```
ops = 4054366
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 809301
p50_us = 607
p99_us = 1007
p999_us = 1695
p9999_us = 3263
max_us = 4252
```

## unpipelined 512-conn (M0 gate mix) m4 rep 10

```
ops = 4046294
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807773
p50_us = 607
p99_us = 1007
p999_us = 1919
p9999_us = 3519
max_us = 4247
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 10

```
ops = 4049467
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 808574
p50_us = 607
p99_us = 1007
p999_us = 1951
p9999_us = 3327
max_us = 4654
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 11

```
ops = 4055500
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 809527
p50_us = 607
p99_us = 1007
p999_us = 1791
p9999_us = 3455
max_us = 4865
```

## unpipelined 512-conn (M0 gate mix) m4 rep 11

```
ops = 4038573
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806224
p50_us = 607
p99_us = 1007
p999_us = 1823
p9999_us = 3775
max_us = 5281
```

## unpipelined 512-conn (M0 gate mix) m4 rep 12

```
ops = 4054560
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 809297
p50_us = 607
p99_us = 1007
p999_us = 1759
p9999_us = 3327
max_us = 4402
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 12

```
ops = 4054290
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 809369
p50_us = 607
p99_us = 1007
p999_us = 1823
p9999_us = 3391
max_us = 4343
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 13

```
ops = 4038597
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 806187
p50_us = 607
p99_us = 1023
p999_us = 1823
p9999_us = 3327
max_us = 4187
```

## unpipelined 512-conn (M0 gate mix) m4 rep 13

```
ops = 4052811
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 809366
p50_us = 607
p99_us = 1007
p999_us = 1791
p9999_us = 3583
max_us = 5536
```

## unpipelined 512-conn (M0 gate mix) m4 rep 14

```
ops = 4041489
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 807097
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3327
max_us = 4260
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 14

```
ops = 4040364
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806592
p50_us = 623
p99_us = 1007
p999_us = 1727
p9999_us = 3327
max_us = 4426
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 15

```
ops = 4049615
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 808347
p50_us = 607
p99_us = 1007
p999_us = 1695
p9999_us = 3455
max_us = 6001
```

## unpipelined 512-conn (M0 gate mix) m4 rep 15

```
ops = 4038399
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806206
p50_us = 607
p99_us = 1023
p999_us = 1919
p9999_us = 3455
max_us = 4111
```

## unpipelined 512-conn (M0 gate mix) m4 rep 16

```
ops = 4041918
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 807133
p50_us = 623
p99_us = 1007
p999_us = 1791
p9999_us = 3391
max_us = 4445
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 16

```
ops = 4051862
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 808921
p50_us = 607
p99_us = 1007
p999_us = 1823
p9999_us = 3455
max_us = 4217
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 17

```
ops = 4049086
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 808415
p50_us = 607
p99_us = 1007
p999_us = 1759
p9999_us = 3327
max_us = 4220
```

## unpipelined 512-conn (M0 gate mix) m4 rep 17

```
ops = 4036170
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 806012
p50_us = 623
p99_us = 1007
p999_us = 1695
p9999_us = 3327
max_us = 4045
```

## unpipelined 512-conn (M0 gate mix) m4 rep 18

```
ops = 4040660
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806657
p50_us = 607
p99_us = 1007
p999_us = 1727
p9999_us = 4223
max_us = 9766
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 18

```
ops = 4047438
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807996
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3327
max_us = 4729
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 19

```
ops = 4047674
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 807983
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3327
max_us = 4615
```

## unpipelined 512-conn (M0 gate mix) m4 rep 19

```
ops = 4056405
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 809732
p50_us = 607
p99_us = 1007
p999_us = 1727
p9999_us = 3199
max_us = 4358
```

## unpipelined 512-conn (M0 gate mix) m4 rep 20

```
ops = 4039960
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 806772
p50_us = 607
p99_us = 1007
p999_us = 1919
p9999_us = 3263
max_us = 4537
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 20

```
ops = 4044104
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807291
p50_us = 607
p99_us = 1023
p999_us = 1855
p9999_us = 3263
max_us = 4323
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 21

```
ops = 4043966
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 807530
p50_us = 607
p99_us = 1007
p999_us = 1727
p9999_us = 3391
max_us = 4318
```

## unpipelined 512-conn (M0 gate mix) m4 rep 21

```
ops = 4038690
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 806552
p50_us = 607
p99_us = 1007
p999_us = 1759
p9999_us = 3391
max_us = 5067
```

## unpipelined 512-conn (M0 gate mix) m4 rep 22

```
ops = 4045326
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807562
p50_us = 607
p99_us = 1007
p999_us = 1823
p9999_us = 3327
max_us = 4043
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 22

```
ops = 4041325
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806740
p50_us = 607
p99_us = 1023
p999_us = 1919
p9999_us = 3647
max_us = 6996
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 23

```
ops = 4040004
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806500
p50_us = 607
p99_us = 1007
p999_us = 1887
p9999_us = 3519
max_us = 4294
```

## unpipelined 512-conn (M0 gate mix) m4 rep 23

```
ops = 4039304
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 806548
p50_us = 623
p99_us = 1007
p999_us = 1823
p9999_us = 3391
max_us = 4164
```

## unpipelined 512-conn (M0 gate mix) m4 rep 24

```
ops = 4042832
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807110
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3391
max_us = 5539
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 24

```
ops = 4050283
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 808558
p50_us = 607
p99_us = 1007
p999_us = 1759
p9999_us = 3391
max_us = 4470
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 25

```
ops = 4042397
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 807156
p50_us = 607
p99_us = 1007
p999_us = 1919
p9999_us = 3519
max_us = 6489
```

## unpipelined 512-conn (M0 gate mix) m4 rep 25

```
ops = 4054134
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 809510
p50_us = 607
p99_us = 1007
p999_us = 1631
p9999_us = 3327
max_us = 5139
```

## unpipelined 512-conn (M0 gate mix) m4 rep 26

```
ops = 4036945
errors = 0
busy_retryable = 0
elapsed_s = 5.010
ops_per_sec = 805812
p50_us = 607
p99_us = 1007
p999_us = 1727
p9999_us = 3647
max_us = 5572
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 26

```
ops = 4042313
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 807291
p50_us = 623
p99_us = 1007
p999_us = 1823
p9999_us = 3391
max_us = 4457
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 27

```
ops = 4041430
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 807062
p50_us = 623
p99_us = 1007
p999_us = 1727
p9999_us = 3327
max_us = 4428
```

## unpipelined 512-conn (M0 gate mix) m4 rep 27

```
ops = 4023003
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 803135
p50_us = 623
p99_us = 1023
p999_us = 1855
p9999_us = 3455
max_us = 4262
```

## unpipelined 512-conn (M0 gate mix) m4 rep 28

```
ops = 4035355
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 805701
p50_us = 623
p99_us = 1007
p999_us = 1855
p9999_us = 3327
max_us = 4606
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 28

```
ops = 4039356
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 806460
p50_us = 607
p99_us = 1007
p999_us = 1791
p9999_us = 3455
max_us = 4449
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 29

```
ops = 4044168
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807347
p50_us = 607
p99_us = 1007
p999_us = 1791
p9999_us = 3327
max_us = 6178
```

## unpipelined 512-conn (M0 gate mix) m4 rep 29

```
ops = 4044448
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807439
p50_us = 607
p99_us = 1007
p999_us = 1727
p9999_us = 3391
max_us = 4506
```

## unpipelined 512-conn (M0 gate mix) m4 rep 30

```
ops = 4049372
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 808658
p50_us = 607
p99_us = 1007
p999_us = 1855
p9999_us = 3455
max_us = 5937
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 30

```
ops = 4055928
errors = 0
busy_retryable = 0
elapsed_s = 5.008
ops_per_sec = 809935
p50_us = 607
p99_us = 1007
p999_us = 1663
p9999_us = 3327
max_us = 4006
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 31

```
ops = 4040677
errors = 0
busy_retryable = 0
elapsed_s = 5.007
ops_per_sec = 806952
p50_us = 607
p99_us = 1007
p999_us = 1887
p9999_us = 3327
max_us = 5128
```

## unpipelined 512-conn (M0 gate mix) m4 rep 31

```
ops = 4043881
errors = 0
busy_retryable = 0
elapsed_s = 5.009
ops_per_sec = 807265
p50_us = 623
p99_us = 1007
p999_us = 1727
p9999_us = 3263
max_us = 4235
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 27379130
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2737608
p50_us = 359
p99_us = 623
p999_us = 2943
p9999_us = 18431
max_us = 19349
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 26659916
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2665676
p50_us = 367
p99_us = 639
p999_us = 4479
p9999_us = 16127
max_us = 17168
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 1

```
ops = 27300428
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2729684
p50_us = 359
p99_us = 655
p999_us = 1887
p9999_us = 16127
max_us = 16662
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 1

```
ops = 26042502
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2603933
p50_us = 383
p99_us = 719
p999_us = 4223
p9999_us = 16383
max_us = 19008
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 2

```
ops = 27667072
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2766319
p50_us = 351
p99_us = 559
p999_us = 2431
p9999_us = 18943
max_us = 20476
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 2

```
ops = 26335318
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2633178
p50_us = 367
p99_us = 687
p999_us = 4223
p9999_us = 17919
max_us = 18636
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 3

```
ops = 27241834
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2723821
p50_us = 359
p99_us = 639
p999_us = 2495
p9999_us = 19455
max_us = 20376
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 3

```
ops = 26139832
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2613612
p50_us = 375
p99_us = 735
p999_us = 4863
p9999_us = 23039
max_us = 24482
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 4

```
ops = 26985555
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2698177
p50_us = 359
p99_us = 687
p999_us = 2687
p9999_us = 19455
max_us = 20429
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 4

```
ops = 26852305
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2684951
p50_us = 359
p99_us = 607
p999_us = 3967
p9999_us = 17407
max_us = 19089
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 5

```
ops = 27122663
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2711874
p50_us = 359
p99_us = 607
p999_us = 4031
p9999_us = 23551
max_us = 23991
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 5

```
ops = 25975092
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2597157
p50_us = 367
p99_us = 735
p999_us = 4607
p9999_us = 19967
max_us = 22249
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 6

```
ops = 27256088
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2725267
p50_us = 367
p99_us = 687
p999_us = 3199
p9999_us = 18943
max_us = 21202
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 6

```
ops = 26346443
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2634311
p50_us = 359
p99_us = 687
p999_us = 4223
p9999_us = 17919
max_us = 19891
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 7

```
ops = 27046767
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2704383
p50_us = 359
p99_us = 671
p999_us = 2751
p9999_us = 16895
max_us = 17621
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 7

```
ops = 26910745
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2690740
p50_us = 359
p99_us = 607
p999_us = 3903
p9999_us = 15871
max_us = 16718
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 8

```
ops = 27295695
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2729230
p50_us = 359
p99_us = 591
p999_us = 4031
p9999_us = 22015
max_us = 23075
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 8

```
ops = 26938343
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2693532
p50_us = 359
p99_us = 591
p999_us = 4095
p9999_us = 17919
max_us = 18450
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 9

```
ops = 27098276
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2709463
p50_us = 359
p99_us = 671
p999_us = 2303
p9999_us = 16383
max_us = 16760
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 9

```
ops = 26851516
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2684819
p50_us = 367
p99_us = 623
p999_us = 4223
p9999_us = 15615
max_us = 16330
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 10

```
ops = 27412783
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2740922
p50_us = 351
p99_us = 575
p999_us = 4991
p9999_us = 22527
max_us = 23705
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 10

```
ops = 26793625
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2679068
p50_us = 359
p99_us = 607
p999_us = 4863
p9999_us = 19967
max_us = 20855
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 11

```
ops = 27264634
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2726148
p50_us = 359
p99_us = 639
p999_us = 3839
p9999_us = 19967
max_us = 20671
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 11

```
ops = 27050256
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2704686
p50_us = 359
p99_us = 575
p999_us = 4031
p9999_us = 16895
max_us = 18252
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 12

```
ops = 26812407
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2680892
p50_us = 375
p99_us = 639
p999_us = 2431
p9999_us = 18431
max_us = 20373
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 12

```
ops = 26756570
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2675294
p50_us = 359
p99_us = 607
p999_us = 5375
p9999_us = 20479
max_us = 21223
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 13

```
ops = 27474338
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2747049
p50_us = 359
p99_us = 575
p999_us = 3455
p9999_us = 20479
max_us = 21101
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 13

```
ops = 26871532
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2686810
p50_us = 359
p99_us = 607
p999_us = 4351
p9999_us = 16895
max_us = 20107
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 14

```
ops = 27080632
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2707741
p50_us = 359
p99_us = 607
p999_us = 11007
p9999_us = 15871
max_us = 17412
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 14

```
ops = 26166734
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2616356
p50_us = 367
p99_us = 687
p999_us = 5119
p9999_us = 22015
max_us = 22757
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 15

```
ops = 27689804
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2768612
p50_us = 359
p99_us = 575
p999_us = 2175
p9999_us = 17919
max_us = 18476
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 15

```
ops = 26833053
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2682943
p50_us = 359
p99_us = 591
p999_us = 4479
p9999_us = 18431
max_us = 19285
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 16

```
ops = 25639365
errors = 0
busy_retryable = 0
elapsed_s = 10.005
ops_per_sec = 2562642
p50_us = 399
p99_us = 751
p999_us = 10751
p9999_us = 21503
max_us = 22710
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 16

```
ops = 26500419
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2649681
p50_us = 367
p99_us = 639
p999_us = 4031
p9999_us = 16895
max_us = 17507
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 17

```
ops = 26309645
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2630674
p50_us = 383
p99_us = 735
p999_us = 2943
p9999_us = 17919
max_us = 18292
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 17

```
ops = 26416267
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2641235
p50_us = 367
p99_us = 655
p999_us = 4223
p9999_us = 16383
max_us = 17478
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 18

```
ops = 27272610
errors = 0
busy_retryable = 0
elapsed_s = 10.002
ops_per_sec = 2726669
p50_us = 359
p99_us = 607
p999_us = 5503
p9999_us = 23039
max_us = 23443
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 18

```
ops = 26459826
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2645628
p50_us = 359
p99_us = 687
p999_us = 5119
p9999_us = 19967
max_us = 22530
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 19

```
ops = 26905431
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2690197
p50_us = 367
p99_us = 703
p999_us = 4735
p9999_us = 20991
max_us = 21725
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 19

```
ops = 26080945
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2607812
p50_us = 383
p99_us = 719
p999_us = 11263
p9999_us = 14847
max_us = 24356
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 20

```
ops = 26891969
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2688844
p50_us = 367
p99_us = 687
p999_us = 2623
p9999_us = 17919
max_us = 18890
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 20

```
ops = 26511012
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2650757
p50_us = 367
p99_us = 639
p999_us = 4479
p9999_us = 18943
max_us = 20006
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 21

```
ops = 27307352
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2730353
p50_us = 359
p99_us = 655
p999_us = 3199
p9999_us = 17919
max_us = 19630
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 21

```
ops = 26156480
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2615316
p50_us = 375
p99_us = 671
p999_us = 4479
p9999_us = 17919
max_us = 21306
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 22

```
ops = 27481198
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2747743
p50_us = 359
p99_us = 575
p999_us = 4351
p9999_us = 20991
max_us = 21593
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 22

```
ops = 26619454
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2661605
p50_us = 367
p99_us = 623
p999_us = 4479
p9999_us = 16127
max_us = 18238
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 23

```
ops = 27521644
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2751795
p50_us = 359
p99_us = 591
p999_us = 2015
p9999_us = 16895
max_us = 17007
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 23

```
ops = 25970550
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2596706
p50_us = 391
p99_us = 751
p999_us = 4863
p9999_us = 16895
max_us = 28174
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 24

```
ops = 27306832
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2730329
p50_us = 359
p99_us = 623
p999_us = 3007
p9999_us = 20991
max_us = 21261
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 24

```
ops = 26914436
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2691103
p50_us = 359
p99_us = 591
p999_us = 4095
p9999_us = 17407
max_us = 20331
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 25

```
ops = 27228892
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2722530
p50_us = 359
p99_us = 639
p999_us = 3519
p9999_us = 18943
max_us = 19739
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 25

```
ops = 26244800
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2624135
p50_us = 359
p99_us = 703
p999_us = 4223
p9999_us = 15359
max_us = 16215
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 26

```
ops = 27086211
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2708321
p50_us = 359
p99_us = 623
p999_us = 3007
p9999_us = 21503
max_us = 21909
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 26

```
ops = 26349182
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2634567
p50_us = 367
p99_us = 703
p999_us = 4351
p9999_us = 17919
max_us = 19204
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 27

```
ops = 27096388
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2709277
p50_us = 367
p99_us = 607
p999_us = 4351
p9999_us = 20991
max_us = 22723
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 27

```
ops = 26608933
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2660557
p50_us = 367
p99_us = 639
p999_us = 4607
p9999_us = 18431
max_us = 19447
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 28

```
ops = 27491627
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2748831
p50_us = 359
p99_us = 607
p999_us = 2687
p9999_us = 16895
max_us = 17888
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 28

```
ops = 25780174
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2577636
p50_us = 391
p99_us = 735
p999_us = 6783
p9999_us = 19967
max_us = 31511
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 29

```
ops = 26929831
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2692628
p50_us = 367
p99_us = 687
p999_us = 2687
p9999_us = 18943
max_us = 20182
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 29

```
ops = 26711695
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2670809
p50_us = 359
p99_us = 639
p999_us = 4479
p9999_us = 17919
max_us = 20584
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 30

```
ops = 27263698
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2726036
p50_us = 359
p99_us = 607
p999_us = 4351
p9999_us = 21503
max_us = 22281
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 30

```
ops = 25816897
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2581342
p50_us = 367
p99_us = 735
p999_us = 4479
p9999_us = 15615
max_us = 16541
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 31

```
ops = 27161047
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2715740
p50_us = 359
p99_us = 639
p999_us = 3967
p9999_us = 18431
max_us = 19496
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 31

```
ops = 26163323
errors = 0
busy_retryable = 0
elapsed_s = 10.001
ops_per_sec = 2615968
p50_us = 375
p99_us = 671
p999_us = 4479
p9999_us = 16895
max_us = 18291
```
