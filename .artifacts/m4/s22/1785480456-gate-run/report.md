# M4 gate-run report

date: 1785480456 (unix) · cells: 4 · conns: 8 · pipeline: 8 · duration: 4s · dataset: 2× budget · ycsb rows: 10 (M4-S22)
env-check: FAILED (overridden — NOT citation-grade)
tier: dev (non-binding)

notes:
- harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)
- E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the keyspace is unordered, no ordered range-scan exists (documented deviation)
- value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's 10×100 B fields); D's `latest` uses a per-connection frontier estimate
- dataset: 32768 keys × 1024 B = 32 MiB = 2× the 16 MiB memory budget · seed 0x1d0c2026 · θ 0.99
- mode: HARNESS-VALIDATION (named-absent tiered rows) — measured fact: `-ERR tiered namespaces are not command-addressable yet (M4 command wiring)`; rows run against a memory-mode namespace to validate the generator, loader, and report machinery; no tiered gate row is produced
- zipf self-check: top-1% share measured 57.66% vs analytic 56.70% (θ=0.99, n=32768, 2000000 draws)
- seed verification: 100000 ops regenerated identically (checksum 0x99492abc0bdf27ad)
- loader: 32768 keys in 0.0s (1228992 sets/s), DBSIZE == keys asserted
- saturation (ycsb-a-zipfian): generator unsaturated at 8 conns (+50% conns moved ops/s +2.7%)

| gate | threshold | measured | verdict |
|---|---|---|---|
| Degenerate A/B: pipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: pipelined p99.9 regression | <= 1 % vs M3 baseline (LogHistogram ~3% buckets: nonzero spans >= 1 bucket) | — | PENDING (tooling) |
| Degenerate A/B: unpipelined ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: unpipelined p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy ops regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: ttl-heavy p99.9 regression | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Degenerate A/B: peak-RSS regression (worst row) | <= 1 % vs M3 baseline | — | PENDING (tooling) |
| Memory-mode node constructs zero tiered tables | <= 0 tables | — | PENDING (tooling) |
| Tiering code-path counters identically zero | <= 0 counter sum | — | PENDING (tooling) |
| Write amplification, worst tiered namespace | < 3 x user bytes (wal + flush) | — | PENDING (tooling) |
| Memory-only rows append zero log records (M2 posture carried) | <= 0 records | — | PENDING (tooling) |
| Mixed-node attribution divergence (M4-S20) | <= 10 pct, worst continuous sample | — | PENDING (tooling) |
| Cache-namespace p99 isolation under the mixed node (M4-S20) | <= 10 pct vs same-campaign solo baseline | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| ycsb-a-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-a-uniform | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-b-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-b-uniform | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-c-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-c-uniform | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-d-latest | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-e-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-f-zipfian | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |
| ycsb-f-uniform | n/a (no tiered namespace on the node — memory-mode row) · blob: n/a (no blob activity) |

## loader fill

```
ops = 32768
errors = 0
elapsed_s = 0.027
ops_per_sec = 1228992
p50_us = 79
p99_us = 219
p999_us = 671
p9999_us = 692
max_us = 692
```

## ycsb-a-zipfian

```
workload = a (zipfian)
ops = 7557544
errors = 0
nils = 0
ops_per_sec = 1889301
combined_client p50_us = 32 · p99_us = 53 · p999_us = 69 · max_us = 776
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 57.64%
stream_checksum = 0xfe4f2debcb10224e
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.5
```

## ycsb-a-uniform

```
workload = a (uniform)
ops = 6916896
errors = 0
nils = 0
ops_per_sec = 1729168
combined_client p50_us = 35 · p99_us = 63 · p999_us = 127 · max_us = 1755
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x85758a4a59494c65
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.7
```

## ycsb-b-zipfian

```
workload = b (zipfian)
ops = 6496371
errors = 0
nils = 0
ops_per_sec = 1624033
combined_client p50_us = 35 · p99_us = 71 · p999_us = 139 · max_us = 2069
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 57.63%
stream_checksum = 0x709bc48b0bf4bc11
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-b-uniform

```
workload = b (uniform)
ops = 6121323
errors = 0
nils = 0
ops_per_sec = 1530270
combined_client p50_us = 38 · p99_us = 75 · p999_us = 147 · max_us = 689
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x9fff8216b78556be
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.5
```

## ycsb-c-zipfian

```
workload = c (zipfian)
ops = 6887646
errors = 0
nils = 0
ops_per_sec = 1721858
combined_client p50_us = 37 · p99_us = 62 · p999_us = 147 · max_us = 1693
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 57.64%
stream_checksum = 0xb32a8fd71b645a3f
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.5
```

## ycsb-c-uniform

```
workload = c (uniform)
ops = 5447830
errors = 0
nils = 0
ops_per_sec = 1361895
combined_client p50_us = 46 · p99_us = 83 · p999_us = 171 · max_us = 1984
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x94148c3dcafd5718
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-d-latest

```
workload = d (latest)
ops = 6252032
errors = 0
nils = 2750288
ops_per_sec = 1562949
combined_client p50_us = 38 · p99_us = 73 · p999_us = 151 · max_us = 2910
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 57.66%
stream_checksum = 0x308f6ef7d32f804e
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-e-zipfian

```
workload = e (zipfian)
ops = 1493363
errors = 0
nils = 0
ops_per_sec = 373326
combined_client p50_us = 20 · p99_us = 39 · p999_us = 65 · max_us = 644
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0x8b1f167aab5d8804
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-f-zipfian

```
workload = f (zipfian)
ops = 4760070
errors = 0
nils = 0
ops_per_sec = 1189971
combined_client p50_us = 48 · p99_us = 105 · p999_us = 215 · max_us = 2926
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 57.64%
stream_checksum = 0x05746e69d1c49e44
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```

## ycsb-f-uniform

```
workload = f (uniform)
ops = 4381910
errors = 0
nils = 0
ops_per_sec = 1095437
combined_client p50_us = 51 · p99_us = 115 · p999_us = 215 · max_us = 1298
(combined = context only; the split section below is the honest read)
hot_share_top1pct = 0.00%
stream_checksum = 0xcda5eaa8af2b7fa0
memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the ADR-0062 D8 refusal; M4-S26 emits the split service histograms (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names
tripwire sqes/submit = 2.6
```
