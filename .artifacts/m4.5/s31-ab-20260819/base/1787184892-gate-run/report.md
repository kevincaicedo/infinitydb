# M4.5 gate-run report

date: 1787184892 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s31base · cells 4 · 3 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 3 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 9052 @64 → 18571 @256; flat 10804 @64 → 41493 @256
- --only-s29: the S27 backpressure row was skipped; its gate keys are absent

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.05 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.84 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.45 | FAIL (DEV-TIER, non-binding) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | 1.30 | PASS (DEV-TIER, non-binding) |
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
rep0 s29tiered c64  ops/s=10243    p50_us=6399   p99_us=9983    p999_us=13823   busy=0 acks/fsync=8.92 flush_rounds=0
rep0 s29tiered c256 ops/s=29928    p50_us=8703   p99_us=14591   p999_us=19455   busy=0 acks/fsync=33.99 flush_rounds=0
rep0 s29flat   c64  ops/s=9090     p50_us=5119   p99_us=28671   p999_us=30719   busy=0 acks/fsync=8.98 flush_rounds=0
rep0 s29flat   c256 ops/s=18910    p50_us=5119   p99_us=106495  p999_us=950271  busy=0 acks/fsync=35.95 flush_rounds=0
rep1 s29tiered c64  ops/s=8023     p50_us=6655   p99_us=29183   p999_us=32255   busy=0 acks/fsync=8.88 flush_rounds=0
rep1 s29tiered c256 ops/s=18571    p50_us=9215   p99_us=35839   p999_us=40959   busy=0 acks/fsync=34.28 flush_rounds=0
rep1 s29flat   c64  ops/s=12673    p50_us=4991   p99_us=16895   p999_us=25087   busy=0 acks/fsync=9.01 flush_rounds=0
rep1 s29flat   c256 ops/s=41493    p50_us=5247   p99_us=27647   p999_us=69631   busy=0 acks/fsync=35.82 flush_rounds=0
rep2 s29tiered c64  ops/s=9052     p50_us=6527   p99_us=26623   p999_us=75775   busy=0 acks/fsync=8.89 flush_rounds=0
rep2 s29tiered c256 ops/s=17542    p50_us=9215   p99_us=39935   p999_us=46079   busy=0 acks/fsync=34.27 flush_rounds=0
rep2 s29flat   c64  ops/s=10804    p50_us=4991   p99_us=28159   p999_us=31743   busy=0 acks/fsync=9.00 flush_rounds=0
rep2 s29flat   c256 ops/s=51732    p50_us=4991   p99_us=7295    p999_us=69631   busy=0 acks/fsync=35.98 flush_rounds=0
```
