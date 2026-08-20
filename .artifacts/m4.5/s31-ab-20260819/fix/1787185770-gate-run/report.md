# M4.5 gate-run report

date: 1787185770 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s31fix · cells 4 · 3 replicates
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- row shape: 200000 keys × 1 KiB per namespace, tiered MEM-BUDGET 128mb/cell (demoter active), 100% SET closed-loop (pipeline 1), conns 64 vs 256, 10s legs, median of 3 replicates, fresh server + data-dir per replicate
- data-root must not be tmpfs — the row's fsyncs must hit a real device or the concurrency slope measures the page cache
- medians (ops/s): tiered 10273 @64 → 30337 @256; flat 13327 @64 → 51752 @256
- --only-s29: the S27 backpressure row was skipped; its gate keys are absent

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | 2.95 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.77 | PASS (DEV-TIER, non-binding) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | 0.59 | FAIL (DEV-TIER, non-binding) |
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
rep0 s29tiered c64  ops/s=10285    p50_us=6399   p99_us=9471    p999_us=11263   busy=0 acks/fsync=8.93 flush_rounds=44
rep0 s29tiered c256 ops/s=30540    p50_us=8703   p99_us=13055   p999_us=16127   busy=0 acks/fsync=35.04 flush_rounds=118
rep0 s29flat   c64  ops/s=13087    p50_us=4991   p99_us=7551    p999_us=59391   busy=0 acks/fsync=8.99 flush_rounds=0
rep0 s29flat   c256 ops/s=51752    p50_us=5119   p99_us=7423    p999_us=10239   busy=0 acks/fsync=35.71 flush_rounds=0
rep1 s29tiered c64  ops/s=6800     p50_us=6527   p99_us=31743   p999_us=34815   busy=0 acks/fsync=8.95 flush_rounds=32
rep1 s29tiered c256 ops/s=30337    p50_us=8703   p99_us=13311   p999_us=16895   busy=0 acks/fsync=34.55 flush_rounds=112
rep1 s29flat   c64  ops/s=13515    p50_us=4991   p99_us=7423    p999_us=8959    busy=0 acks/fsync=8.98 flush_rounds=0
rep1 s29flat   c256 ops/s=52535    p50_us=5119   p99_us=7295    p999_us=10495   busy=0 acks/fsync=36.07 flush_rounds=0
rep2 s29tiered c64  ops/s=10273    p50_us=6271   p99_us=9727    p999_us=11519   busy=0 acks/fsync=8.97 flush_rounds=45
rep2 s29tiered c256 ops/s=28328    p50_us=8703   p99_us=14079   p999_us=114687  busy=0 acks/fsync=34.49 flush_rounds=110
rep2 s29flat   c64  ops/s=13327    p50_us=4991   p99_us=7679    p999_us=8959    busy=0 acks/fsync=8.99 flush_rounds=0
rep2 s29flat   c256 ops/s=34142    p50_us=5119   p99_us=34815   p999_us=57343   busy=0 acks/fsync=35.90 flush_rounds=0
```
