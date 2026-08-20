# M4.5 gate-run report

date: 1787186014 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s31driver · cells 4 · 3 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 3 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 10302 @64 → 30184 @256; flat 13037 @64 → 52167 @256
- --only-s29: the S27 backpressure row was skipped; its gate keys are absent

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.93 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.79 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.58 | FAIL (DEV-TIER, non-binding) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | 1.79 | PASS (DEV-TIER, non-binding) |
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
rep0 s29tiered c64  ops/s=10366    p50_us=6271   p99_us=9471    p999_us=11263   busy=0 acks/fsync=8.92 flush_rounds=46
rep0 s29tiered c256 ops/s=30184    p50_us=8703   p99_us=13311   p999_us=17407   busy=0 acks/fsync=34.52 flush_rounds=115
rep0 s29flat   c64  ops/s=13037    p50_us=4991   p99_us=7679    p999_us=51199   busy=0 acks/fsync=8.98 flush_rounds=0
rep0 s29flat   c256 ops/s=52167    p50_us=5119   p99_us=7295    p999_us=10239   busy=0 acks/fsync=35.93 flush_rounds=0
rep1 s29tiered c64  ops/s=10270    p50_us=6399   p99_us=9727    p999_us=11263   busy=0 acks/fsync=8.95 flush_rounds=44
rep1 s29tiered c256 ops/s=24022    p50_us=8703   p99_us=36863   p999_us=69631   busy=0 acks/fsync=34.36 flush_rounds=96
rep1 s29flat   c64  ops/s=8188     p50_us=5119   p99_us=31743   p999_us=41983   busy=0 acks/fsync=9.00 flush_rounds=0
rep1 s29flat   c256 ops/s=6908     p50_us=5119   p99_us=1540095 p999_us=2686975 busy=0 acks/fsync=36.42 flush_rounds=0
rep2 s29tiered c64  ops/s=10302    p50_us=6271   p99_us=9727    p999_us=11775   busy=0 acks/fsync=8.91 flush_rounds=45
rep2 s29tiered c256 ops/s=30360    p50_us=8703   p99_us=13311   p999_us=16383   busy=0 acks/fsync=34.84 flush_rounds=115
rep2 s29flat   c64  ops/s=13059    p50_us=4991   p99_us=7679    p999_us=59391   busy=0 acks/fsync=8.99 flush_rounds=0
rep2 s29flat   c256 ops/s=52648    p50_us=5119   p99_us=7423    p999_us=9471    busy=0 acks/fsync=35.94 flush_rounds=0
```
