# M4.5 gate-run report

date: 1787170319 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s27-pre · cells 4 · S27 row only
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
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | 0.86 | FAIL (DEV-TIER, non-binding) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | 0.79 | FAIL (DEV-TIER, non-binding) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | 1562.34 | FAIL (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| durable-write-backpressure | not measured by this row — the S27 row gates the backpressure shape (refusals, decay, max); write amplification is unchanged by it |

## s27 per-repeat samples

```
provoked rep0 everysec ops/s=652998   p99_us=335     max_us=612558   busy=100537
provoked rep1 everysec ops/s=530578   p99_us=351     max_us=829775   busy=45126
provoked rep2 everysec ops/s=329195   p99_us=295     max_us=2475629  busy=64980
provoked regime: parked_total(delta)=0 write_stall_p99_us(worst cell)=0
provoked always informational ops/s=5988     p99_us=13823   max_us=162503   busy=0
d5-shape rep0 everysec ops/s=451986   p99_us=139     max_us=466502   busy=6859
d5-shape rep1 everysec ops/s=263356   p99_us=139     max_us=1562337  busy=2080
d5-shape rep2 everysec ops/s=358371   p99_us=155     max_us=1380233  busy=9846
```
