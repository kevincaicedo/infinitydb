# M4.5 gate-run report

date: 1787159548 (unix) · binary target/release/infinityd · cells 4 · 3 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 3 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 10076 @64 → 29719 @256; flat 12986 @64 → 51743 @256

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.95 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.78 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.57 | FAIL (DEV-TIER, non-binding) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | 2.03 | PASS (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| tiered-always-scaling | not measured by this row — the S29 row gates the concurrency slope; write amplification for tiered namespaces is owned by the M4 S16 rows |

## per-leg samples

```
rep0 s29tiered c64  ops/s=10044    p50_us=6527   p99_us=10239   busy=0 acks/fsync=8.91
rep0 s29tiered c256 ops/s=29719    p50_us=8703   p99_us=15103   busy=0 acks/fsync=34.04
rep0 s29flat   c64  ops/s=13286    p50_us=4991   p99_us=7551    busy=0 acks/fsync=8.99
rep0 s29flat   c256 ops/s=52375    p50_us=5119   p99_us=7423    busy=0 acks/fsync=35.83
rep1 s29tiered c64  ops/s=10076    p50_us=6399   p99_us=10239   busy=0 acks/fsync=8.89
rep1 s29tiered c256 ops/s=29688    p50_us=8703   p99_us=15103   busy=0 acks/fsync=34.06
rep1 s29flat   c64  ops/s=8249     p50_us=5119   p99_us=32767   busy=0 acks/fsync=9.01
rep1 s29flat   c256 ops/s=34076    p50_us=5119   p99_us=27647   busy=0 acks/fsync=36.19
rep2 s29tiered c64  ops/s=10101    p50_us=6399   p99_us=10239   busy=0 acks/fsync=8.92
rep2 s29tiered c256 ops/s=29875    p50_us=8703   p99_us=14847   busy=0 acks/fsync=34.07
rep2 s29flat   c64  ops/s=12986    p50_us=4991   p99_us=7679    busy=0 acks/fsync=8.96
rep2 s29flat   c256 ops/s=51743    p50_us=5119   p99_us=7295    busy=0 acks/fsync=35.82
```
