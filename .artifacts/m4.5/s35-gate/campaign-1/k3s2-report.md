# M4.5 gate-run report

date: 1787336924 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 2
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 2
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35: 1 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
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
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.19 | PASS |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.25 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=40392    p50_us=703    p99_us=1919    max_us=10639    barrier_p50_us=591   barrier_p99_us=17407  p50/barrier=1.19 frames_in_flight_max=3
rep0 4c c256 ops/s=199587   p50_us=1183   p99_us=3135    max_us=34684    barrier_p50_us=655   barrier_p99_us=5887   frames_in_flight_max=3
rep0 4c read c64 P16 ops/s=1597632  p50_us=543    p99_us=1151    p999_us=1535    nils=0
rep1 4c c32  ops/s=39961    p50_us=735    p99_us=1951    max_us=11329    barrier_p50_us=607   barrier_p99_us=1535   p50/barrier=1.21 frames_in_flight_max=3
rep1 4c c256 ops/s=133082   p50_us=1183   p99_us=25087   max_us=102713   barrier_p50_us=655   barrier_p99_us=2303   frames_in_flight_max=3
rep1 4c read c64 P16 ops/s=1568347  p50_us=575    p99_us=1183    p999_us=1503    nils=0
rep2 4c c32  ops/s=40161    p50_us=719    p99_us=1919    max_us=10530    barrier_p50_us=607   barrier_p99_us=1503   p50/barrier=1.18 frames_in_flight_max=3
rep2 4c c256 ops/s=143276   p50_us=1183   p99_us=25087   max_us=61487    barrier_p50_us=655   barrier_p99_us=2111   frames_in_flight_max=3
rep2 4c read c64 P16 ops/s=1589873  p50_us=623    p99_us=975     p999_us=1471    nils=0
rep0 1c c32  ops/s=48290    p50_us=591    p99_us=1439    max_us=18091    barrier_p50_us=575   barrier_p99_us=1343   p50/barrier=1.03 frames_in_flight_max=3
rep1 1c c32  ops/s=48580    p50_us=575    p99_us=1407    max_us=32174    barrier_p50_us=591   barrier_p99_us=1343   p50/barrier=0.97 frames_in_flight_max=3
rep2 1c c32  ops/s=49331    p50_us=575    p99_us=1407    max_us=30068    barrier_p50_us=559   barrier_p99_us=1343   p50/barrier=1.03 frames_in_flight_max=3
```
