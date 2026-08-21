# M4.5 gate-run report

date: 1787339480 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 4 · barrier-class fua · staging-mib 4
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 4 · barrier-class fua · staging-mib 4
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35: 2 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
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
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.14 | PASS |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.31 | FAIL |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=29514    p50_us=751    p99_us=16383   max_us=29659    barrier_p50_us=655   barrier_p99_us=15871  p50/barrier=1.15 frames_in_flight_max=4 acks/fsync=2.1 frames=155924 parked=0
rep0 4c c256 ops/s=197880   p50_us=1151   p99_us=2943    max_us=68677    barrier_p50_us=703   barrier_p99_us=2431   frames_in_flight_max=4 acks/fsync=12.2 frames=191634 parked=0
rep0 4c read c64 P16 ops/s=1586892  p50_us=703    p99_us=1007    p999_us=1503    nils=0
rep1 4c c32  ops/s=27571    p50_us=751    p99_us=21503   max_us=34050    barrier_p50_us=671   barrier_p99_us=17407  p50/barrier=1.12 frames_in_flight_max=4 acks/fsync=2.1 frames=149750 parked=0
rep1 4c c256 ops/s=187210   p50_us=1151   p99_us=5631    max_us=46284    barrier_p50_us=703   barrier_p99_us=9215   frames_in_flight_max=4 acks/fsync=12.2 frames=188882 parked=0
rep1 4c read c64 P16 ops/s=1590662  p50_us=671    p99_us=943     p999_us=1471    nils=0
rep2 4c c32  ops/s=9253     p50_us=783    p99_us=120831  max_us=186138   barrier_p50_us=687   barrier_p99_us=1631   p50/barrier=1.14 frames_in_flight_max=4 acks/fsync=2.1 frames=45149 parked=0
rep2 4c c256 ops/s=207565   p50_us=1119   p99_us=2879    max_us=36839    barrier_p50_us=735   barrier_p99_us=2175   frames_in_flight_max=4 acks/fsync=12.1 frames=204265 parked=0
rep2 4c read c64 P16 ops/s=1579264  p50_us=719    p99_us=1007    p999_us=1503    nils=0
rep0 1c c32  ops/s=50908    p50_us=575    p99_us=1343    max_us=26599    barrier_p50_us=559   barrier_p99_us=1279   p50/barrier=1.03 frames_in_flight_max=4 acks/fsync=15.3 frames=37408 parked=0
rep1 1c c32  ops/s=49473    p50_us=575    p99_us=1375    max_us=17695    barrier_p50_us=575   barrier_p99_us=1311   p50/barrier=1.00 frames_in_flight_max=4 acks/fsync=15.5 frames=35781 parked=0
rep2 1c c32  ops/s=49597    p50_us=575    p99_us=1375    max_us=16712    barrier_p50_us=591   barrier_p99_us=1311   p50/barrier=0.97 frames_in_flight_max=4 acks/fsync=15.7 frames=35538 parked=0
```
