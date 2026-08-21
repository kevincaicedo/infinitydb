# M4.5 gate-run report

date: 1787338976 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 2
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 2
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
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.21 | PASS |
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.28 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=39462    p50_us=735    p99_us=1919    max_us=10859    barrier_p50_us=607   barrier_p99_us=1439   p50/barrier=1.21 frames_in_flight_max=3 acks/fsync=2.2 frames=197027 parked=0
rep0 4c c256 ops/s=198372   p50_us=1151   p99_us=2943    max_us=49149    barrier_p50_us=655   barrier_p99_us=1887   frames_in_flight_max=3 acks/fsync=14.8 frames=160607 parked=0
rep0 4c read c64 P16 ops/s=1577606  p50_us=671    p99_us=1119    p999_us=1567    nils=0
rep1 4c c32  ops/s=39449    p50_us=735    p99_us=1951    max_us=11129    barrier_p50_us=623   barrier_p99_us=1439   p50/barrier=1.18 frames_in_flight_max=3 acks/fsync=2.2 frames=193917 parked=0
rep1 4c c256 ops/s=158372   p50_us=1151   p99_us=3455    max_us=77466    barrier_p50_us=655   barrier_p99_us=1951   frames_in_flight_max=3 acks/fsync=14.8 frames=111226 parked=0
rep1 4c read c64 P16 ops/s=1601442  p50_us=623    p99_us=911     p999_us=1503    nils=1931
rep2 4c c32  ops/s=39197    p50_us=735    p99_us=1951    max_us=11326    barrier_p50_us=607   barrier_p99_us=1535   p50/barrier=1.21 frames_in_flight_max=3 acks/fsync=2.2 frames=193105 parked=0
rep2 4c c256 ops/s=196968   p50_us=1183   p99_us=2943    max_us=44573    barrier_p50_us=671   barrier_p99_us=1951   frames_in_flight_max=3 acks/fsync=14.7 frames=155710 parked=0
rep2 4c read c64 P16 ops/s=1597744  p50_us=639    p99_us=911     p999_us=1727    nils=0
rep0 1c c32  ops/s=50580    p50_us=575    p99_us=1343    max_us=17453    barrier_p50_us=559   barrier_p99_us=1279   p50/barrier=1.03 frames_in_flight_max=3 acks/fsync=17.9 frames=31803 parked=0
rep1 1c c32  ops/s=49937    p50_us=575    p99_us=1375    max_us=16734    barrier_p50_us=575   barrier_p99_us=1311   p50/barrier=1.00 frames_in_flight_max=3 acks/fsync=15.9 frames=34852 parked=0
rep2 1c c32  ops/s=49274    p50_us=575    p99_us=1375    max_us=22764    barrier_p50_us=575   barrier_p99_us=1279   p50/barrier=1.00 frames_in_flight_max=3 acks/fsync=16.7 frames=33170 parked=0
```
