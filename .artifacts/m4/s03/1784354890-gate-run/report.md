# M4 gate-run report

date: 1784354890 (unix) · cells: 4 · duration: 2s · replicates: 1 · degenerate-case A/B (M4-S03; hard sub-gate, re-run at week-4 risk gate + S24)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- env-check FAILED and was overridden (--unsafe-env): not citation-grade
- dev-tier run: reference-box gates report measured values, non-binding verdicts — the degenerate-case verdict binds on the reference box (week-4 risk gate + S24)
- p99.9 deltas are quantized by LogHistogram (32 sub-buckets/octave ≈ 3%): 0.0% = same bucket; any non-zero delta spans ≥ 1 bucket
- m4 binary target/release/infinityd: hash64:780f8d57dbc8ab4a (10538800 bytes)
- m3 baseline /tmp/claude-1000/-home-kcaicedo-Documents-Projects-databases/146c45b6-8d4c-463b-a1e1-345fa4ab8b4c/scratchpad/m3-baseline/target/release/infinityd: hash64:60afaf32c23bce09 (10479640 bytes) — pin this fingerprint across the week-4 and S24 re-runs; the commit it was built from is recorded in the ledger row (C15 lesson)
- pipelined 1:10 (M0 gate mix): m3 4367210 ops/s (spread 0.00%) vs m4 4376431 ops/s (spread 0.00%) — signed ops delta +0.21% · p999 655 → 1823 µs (+178.32%) · peak-RSS 154746880 → 154292224 B (-0.29%)
- unpipelined 512-conn (M0 gate mix): m3 1049217 ops/s (spread 0.00%) vs m4 1124059 ops/s (spread 0.00%) — signed ops delta +7.13% · p999 1663 → 1983 µs (+19.24%) · peak-RSS 110587904 → 110686208 B (+0.09%)
- ttl-heavy 1:1 writes (M1 gate mix): m3 3551986 ops/s (spread 0.00%) vs m4 3768838 ops/s (spread 0.00%) — signed ops delta +6.11% · p999 2431 → 1951 µs (-19.74%) · peak-RSS 251858944 → 254799872 B (+1.17%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | 178.32 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | 19.24 | FAIL (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | 0.00 | PASS (DEV-TIER, non-binding) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | 1.17 | FAIL (DEV-TIER, non-binding) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | 0.00 | PASS |
| Tiering code-path counters identically zero | <= 0 counter sum | 0.00 | PASS |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | 0.00 | PASS |

## pipelined 1:10 (M0 gate mix) m3-baseline rep 0

```
ops = 8738314
errors = 0
elapsed_s = 2.001
ops_per_sec = 4367210
p50_us = 223
p99_us = 431
p999_us = 655
p9999_us = 4607
max_us = 4869
```

## pipelined 1:10 (M0 gate mix) m4 rep 0

```
ops = 8756314
errors = 0
elapsed_s = 2.001
ops_per_sec = 4376431
p50_us = 223
p99_us = 487
p999_us = 1823
p9999_us = 4863
max_us = 5169
```

## unpipelined 512-conn (M0 gate mix) m3-baseline rep 0

```
ops = 2104883
errors = 0
elapsed_s = 2.006
ops_per_sec = 1049217
p50_us = 479
p99_us = 863
p999_us = 1663
p9999_us = 3135
max_us = 3798
```

## unpipelined 512-conn (M0 gate mix) m4 rep 0

```
ops = 2254033
errors = 0
elapsed_s = 2.005
ops_per_sec = 1124059
p50_us = 439
p99_us = 847
p999_us = 1983
p9999_us = 2879
max_us = 3783
```

## ttl-heavy 1:1 writes (M1 gate mix) m3-baseline rep 0

```
ops = 7106989
errors = 0
elapsed_s = 2.001
ops_per_sec = 3551986
p50_us = 279
p99_us = 575
p999_us = 2431
p9999_us = 10239
max_us = 10455
```

## ttl-heavy 1:1 writes (M1 gate mix) m4 rep 0

```
ops = 7540801
errors = 0
elapsed_s = 2.001
ops_per_sec = 3768838
p50_us = 263
p99_us = 495
p999_us = 1951
p9999_us = 9983
max_us = 10305
```
