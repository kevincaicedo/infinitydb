# M4.5 gate-run report

date: 1787339985 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 1 · barrier-class fua · staging-mib 4
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 1 · barrier-class fua · staging-mib 4
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 3; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell; max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram); device tail = fua_latency_p99_us (worst cell)

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.89 | FAIL |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.46 | FAIL |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=28232    p50_us=1119   p99_us=2559    max_us=11269    barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.89 frames_in_flight_max=1 acks/fsync=4.3 frames=72471 parked=0
rep0 4c c256 ops/s=174610   p50_us=1343   p99_us=3071    max_us=52317    barrier_p50_us=655   barrier_p99_us=1663   frames_in_flight_max=1 acks/fsync=33.8 frames=62780 parked=0
rep0 4c read c64 P16 ops/s=1593362  p50_us=623    p99_us=1055    p999_us=1535    nils=520
rep1 4c c32  ops/s=28438    p50_us=1119   p99_us=2559    max_us=11173    barrier_p50_us=591   barrier_p99_us=1407   p50/barrier=1.89 frames_in_flight_max=1 acks/fsync=4.3 frames=72925 parked=0
rep1 4c c256 ops/s=134114   p50_us=1407   p99_us=18431   max_us=344224   barrier_p50_us=671   barrier_p99_us=1823   frames_in_flight_max=1 acks/fsync=33.9 frames=52235 parked=0
rep1 4c read c64 P16 ops/s=1585969  p50_us=703    p99_us=1055    p999_us=1471    nils=1677
rep2 4c c32  ops/s=27110    p50_us=1087   p99_us=2559    max_us=108398   barrier_p50_us=591   barrier_p99_us=1407   p50/barrier=1.84 frames_in_flight_max=1 acks/fsync=4.3 frames=70481 parked=0
rep2 4c c256 ops/s=173659   p50_us=1343   p99_us=3135    max_us=44616    barrier_p50_us=655   barrier_p99_us=1663   frames_in_flight_max=1 acks/fsync=33.7 frames=62982 parked=0
rep2 4c read c64 P16 ops/s=1581496  p50_us=623    p99_us=1055    p999_us=1535    nils=436
rep0 1c c32  ops/s=39603    p50_us=767    p99_us=1471    max_us=41071    barrier_p50_us=383   barrier_p99_us=767    p50/barrier=2.00 frames_in_flight_max=1 acks/fsync=16.0 frames=27911 parked=0
rep1 1c c32  ops/s=39414    p50_us=767    p99_us=1503    max_us=33273    barrier_p50_us=383   barrier_p99_us=751    p50/barrier=2.00 frames_in_flight_max=1 acks/fsync=16.2 frames=27560 parked=0
rep2 1c c32  ops/s=39308    p50_us=783    p99_us=1471    max_us=33029    barrier_p50_us=383   barrier_p99_us=751    p50/barrier=2.04 frames_in_flight_max=1 acks/fsync=16.2 frames=27633 parked=0
```
