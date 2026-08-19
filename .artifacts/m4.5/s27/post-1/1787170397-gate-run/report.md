# M4.5 gate-run report

date: 1787170397 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s27-post · cells 4 · S27 row only
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- --only-s27: the S29 scaling row was skipped; its gate keys are absent
- s27 row: refusal gate spans both leg-sets (3 provoked --log-staging-mib 1 pipeline-4 repeats + always leg, then 3 default-staging pipeline-1 repeats — the ADR-0081 D5 shape); decay and max gate the D5 leg-set only (10s legs, 32 conns, 1 KiB values, flat everysec)

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | 0.00 | PASS (DEV-TIER, non-binding) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | 1.00 | PASS (DEV-TIER, non-binding) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | 697.66 | FAIL (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| durable-write-backpressure | not measured by this row — the S27 row gates the backpressure shape (refusals, decay, max); write amplification is unchanged by it |

## s27 per-repeat samples

```
provoked rep0 everysec ops/s=500085   p99_us=311     max_us=866483   busy=0
provoked rep1 everysec ops/s=377355   p99_us=311     max_us=641877   busy=0
provoked rep2 everysec ops/s=483360   p99_us=319     max_us=586130   busy=0
provoked regime: parked_total(delta)=2894 write_stall_p99_us(worst cell)=279
provoked always informational ops/s=0        p99_us=0       max_us=0        busy=0
d5-shape rep0 everysec ops/s=469422   p99_us=139     max_us=400427   busy=0
d5-shape rep1 everysec ops/s=313641   p99_us=135     max_us=697661   busy=0
d5-shape rep2 everysec ops/s=467093   p99_us=147     max_us=45787    busy=0
```
