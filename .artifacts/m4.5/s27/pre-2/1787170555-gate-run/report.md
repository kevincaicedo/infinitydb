# M4.5 gate-run report

date: 1787170555 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s27-pre · cells 4 · S27 row only
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- --only-s27: the S29 scaling row was skipped; its gate keys are absent
- s27: WARNING — parked_total delta is 0 in the provoked set: either pressure never engaged (regime vacuous) or the server predates the counter (pre-fix A/B arm)
- s27 row: refusal gate spans both leg-sets (3 provoked --log-staging-mib 1 pipeline-4 repeats + always leg, then 3 default-staging pipeline-1 repeats — the ADR-0081 D5 shape); decay and max gate the D5 leg-set only (10s legs, 32 conns, 1 KiB values, flat everysec)

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | 0.47 | FAIL (DEV-TIER, non-binding) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | 1.33 | PASS (DEV-TIER, non-binding) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | 2830.53 | FAIL (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| durable-write-backpressure | not measured by this row — the S27 row gates the backpressure shape (refusals, decay, max); write amplification is unchanged by it |

## s27 per-repeat samples

```
provoked rep0 everysec ops/s=428830   p99_us=303     max_us=628373   busy=4305
provoked rep1 everysec ops/s=637963   p99_us=351     max_us=615241   busy=42743
provoked rep2 everysec ops/s=449374   p99_us=375     max_us=1022229  busy=27617
provoked regime: parked_total(delta)=0 write_stall_p99_us(worst cell)=0
provoked always informational ops/s=15794    p99_us=12799   max_us=20550    busy=0
d5-shape rep0 everysec ops/s=228506   p99_us=111     max_us=2830535  busy=1663
d5-shape rep1 everysec ops/s=302790   p99_us=147     max_us=2196489  busy=1914
d5-shape rep2 everysec ops/s=303624   p99_us=135     max_us=618010   busy=35127
```
