# M4.5 gate-run report

date: 1787195039 (unix) · binary target/release/infinityd · cells 4 · 3 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 3 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 10007 @64 → 29174 @256; flat 8311 @64 → 36326 @256
- --only-s29: the S27 backpressure row was skipped; its gate keys are absent

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.92 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 1.20 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.80 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | 0.63 | PASS (DEV-TIER, non-binding) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| tiered-always-scaling | not measured by this row — the S29 row gates the concurrency slope; write amplification for tiered namespaces is owned by the M4 S16 rows |

## per-leg samples

```
rep0 s29tiered c64  ops/s=9325     p50_us=6527   p99_us=31231   p999_us=40959   busy=0 acks/fsync=8.86 flush_rounds=40 cold_reads=35000 promotions=0
rep0 s29tiered c256 ops/s=30458    p50_us=8703   p99_us=13311   p999_us=17407   busy=0 acks/fsync=34.68 flush_rounds=113 cold_reads=107493 promotions=0
rep0 s29flat   c64  ops/s=13253    p50_us=4991   p99_us=7551    p999_us=8959    busy=0 acks/fsync=8.96 flush_rounds=0 cold_reads=0 promotions=0
rep0 s29flat   c256 ops/s=29157    p50_us=5375   p99_us=29695   p999_us=34815   busy=0 acks/fsync=35.58 flush_rounds=0 cold_reads=0 promotions=0
rep1 s29tiered c64  ops/s=10088    p50_us=6399   p99_us=9727    p999_us=11775   busy=0 acks/fsync=8.94 flush_rounds=44 cold_reads=40269 promotions=0
rep1 s29tiered c256 ops/s=27984    p50_us=8703   p99_us=26111   p999_us=38911   busy=0 acks/fsync=34.63 flush_rounds=108 cold_reads=100329 promotions=0
rep1 s29flat   c64  ops/s=8311     p50_us=5119   p99_us=29695   p999_us=33791   busy=0 acks/fsync=8.96 flush_rounds=0 cold_reads=0 promotions=0
rep1 s29flat   c256 ops/s=41343    p50_us=5119   p99_us=26111   p999_us=385023  busy=0 acks/fsync=35.98 flush_rounds=0 cold_reads=0 promotions=0
rep2 s29tiered c64  ops/s=10007    p50_us=6527   p99_us=9983    p999_us=11775   busy=0 acks/fsync=8.93 flush_rounds=44 cold_reads=40328 promotions=0
rep2 s29tiered c256 ops/s=29174    p50_us=8703   p99_us=16895   p999_us=37887   busy=0 acks/fsync=34.67 flush_rounds=112 cold_reads=104553 promotions=0
rep2 s29flat   c64  ops/s=8076     p50_us=5119   p99_us=32255   p999_us=46079   busy=0 acks/fsync=8.95 flush_rounds=0 cold_reads=0 promotions=0
rep2 s29flat   c256 ops/s=36326    p50_us=5119   p99_us=26623   p999_us=770047  busy=0 acks/fsync=36.05 flush_rounds=0 cold_reads=0 promotions=0
```
