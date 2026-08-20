# M4.5 gate-run report

date: 1787186136 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s31base · cells 4 · 1 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 1 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 10259 @64 → 29560 @256; flat 13182 @64 → 52295 @256
- --only-s29: the S27 backpressure row was skipped; its gate keys are absent

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.88 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.78 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.57 | FAIL (DEV-TIER, non-binding) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | 2.00 | PASS (DEV-TIER, non-binding) |
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
rep0 s29tiered c64  ops/s=10259    p50_us=6399   p99_us=9983    p999_us=13311   busy=0 acks/fsync=8.88 flush_rounds=0
rep0 s29tiered c256 ops/s=29560    p50_us=8703   p99_us=15103   p999_us=20479   busy=0 acks/fsync=33.94 flush_rounds=0
rep0 s29flat   c64  ops/s=13182    p50_us=4991   p99_us=7807    p999_us=49151   busy=0 acks/fsync=9.01 flush_rounds=0
rep0 s29flat   c256 ops/s=52295    p50_us=5119   p99_us=7551    p999_us=10751   busy=0 acks/fsync=35.85 flush_rounds=0
```
