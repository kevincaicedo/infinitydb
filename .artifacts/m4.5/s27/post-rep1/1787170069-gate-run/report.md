# M4.5 gate-run report

date: 1787170069 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s27-post · cells 4 · S27 row only
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- --only-s27: the S29 scaling row was skipped; its gate keys are absent
- s27 row shape: provoked regime (--log-staging-mib 1), 3 back-to-back 10s 100% SET legs (32 conns × pipeline 4, 1 KiB values) on flat everysec + one informational always leg; parked_total delta 2383

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | 0.00 | PASS (DEV-TIER, non-binding) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | 0.68 | FAIL (DEV-TIER, non-binding) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | 1062.60 | FAIL (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| durable-write-backpressure | not measured by this row — the S27 row gates the backpressure shape (refusals, decay, max) in a provoked staging regime; write amplification is unchanged by it |

## s27 per-repeat samples

```
rep0 everysec ops/s=568054   p99_us=311     max_us=490621   busy=0
rep1 everysec ops/s=322623   p99_us=367     max_us=1062604  busy=0
rep2 everysec ops/s=386895   p99_us=319     max_us=881264   busy=0
regime: parked_total(delta)=2383 write_stall_p99_us(worst cell)=223
always  informational ops/s=15834    p99_us=13055   max_us=22120    busy=0
```
