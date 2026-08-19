# M4.5 gate-run report

date: 1787170002 (unix) · binary /home/kcaicedo/.cache/inf-campaign/infinityd-s27-pre · cells 4 · S27 row only
env-check: OK
tier: dev (non-binding)

notes:
- dev-tier run: verdicts are non-binding; the S29 AC binds on the reference box
- --only-s27: the S29 scaling row was skipped; its gate keys are absent
- s27: WARNING — parked_total delta is 0: the provoked regime did not engage; the refusal/decay verdicts are vacuous on this device state
- s27 row shape: provoked regime (--log-staging-mib 1), 3 back-to-back 10s 100% SET legs (32 conns × pipeline 4, 1 KiB values) on flat everysec + one informational always leg; parked_total delta 0

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | 0.60 | FAIL (DEV-TIER, non-binding) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | 0.68 | FAIL (DEV-TIER, non-binding) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | 3556.64 | FAIL (DEV-TIER, non-binding) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| durable-write-backpressure | not measured by this row — the S27 row gates the backpressure shape (refusals, decay, max) in a provoked staging regime; write amplification is unchanged by it |

## s27 per-repeat samples

```
rep0 everysec ops/s=527263   p99_us=303     max_us=717244   busy=56519
rep1 everysec ops/s=107487   p99_us=287     max_us=3556635  busy=1281
rep2 everysec ops/s=358404   p99_us=279     max_us=613159   busy=2843
regime: parked_total(delta)=0 write_stall_p99_us(worst cell)=0
always  informational ops/s=2463     p99_us=13055   max_us=26763    busy=0
```
