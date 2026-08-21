# M4.5 gate-run report

date: 1787336385 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 1 · barrier-class fua · staging-mib 4
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 1 · barrier-class fua · staging-mib 4
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 row: flat always, 200000 keys × 1 KiB fill, 10s legs, median of 3; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell; max leg 256 conns; read leg 64 conns × P16 100% GET; 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram incl. the fill's frames); device tail = fua_latency_p99_us (worst cell)

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.95 | FAIL |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.50 | FAIL |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=27386    p50_us=1151   p99_us=2559    max_us=10452    barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.95 frames_in_flight_max=1
rep0 4c c256 ops/s=134069   p50_us=1375   p99_us=17919   max_us=366424   barrier_p50_us=655   barrier_p99_us=1727   frames_in_flight_max=1
rep0 4c read c64 P16 ops/s=1570153  p50_us=655    p99_us=1087    p999_us=1599    nils=0
rep1 4c c32  ops/s=27426    p50_us=1151   p99_us=2559    max_us=11086    barrier_p50_us=623   barrier_p99_us=1375   p50/barrier=1.85 frames_in_flight_max=1
rep1 4c c256 ops/s=173789   p50_us=1375   p99_us=3007    max_us=45702    barrier_p50_us=671   barrier_p99_us=1631   frames_in_flight_max=1
rep1 4c read c64 P16 ops/s=1598631  p50_us=591    p99_us=1007    p999_us=1407    nils=0
rep2 4c c32  ops/s=27321    p50_us=1151   p99_us=2559    max_us=10461    barrier_p50_us=591   barrier_p99_us=1343   p50/barrier=1.95 frames_in_flight_max=1
rep2 4c c256 ops/s=174023   p50_us=1343   p99_us=3199    max_us=40670    barrier_p50_us=655   barrier_p99_us=1631   frames_in_flight_max=1
rep2 4c read c64 P16 ops/s=1589019  p50_us=655    p99_us=991     p999_us=1375    nils=0
rep0 1c c32  ops/s=38419    p50_us=767    p99_us=1567    max_us=32830    barrier_p50_us=391   barrier_p99_us=1055   p50/barrier=1.96 frames_in_flight_max=1
rep1 1c c32  ops/s=38968    p50_us=767    p99_us=1535    max_us=40021    barrier_p50_us=383   barrier_p99_us=1087   p50/barrier=2.00 frames_in_flight_max=1
rep2 1c c32  ops/s=38696    p50_us=767    p99_us=1535    max_us=42927    barrier_p50_us=391   barrier_p99_us=1087   p50/barrier=1.96 frames_in_flight_max=1
```
