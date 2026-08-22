# M4.5 gate-run report

date: 1787369466 (unix) · binary target/release/infinityd · cells 4 · 5 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35: 2 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 5; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell (interleaved per replicate); max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram); device tail = fua_latency_p99_us (worst cell)

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
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.34 | FAIL |
| S36: server CPU across the pure-write everysec leg | >= 300 % of one core (400 = four cells flat out; pre-S36 read 123–185) | — | PENDING (tooling) |
| S36: everysec pure-write throughput vs the same-session tmpfs control | >= 0.85 x (device arm ops/s / tmpfs control ops/s, same binary, same session) | — | PENDING (tooling) |
| S36: checkpoint + MANIFEST bytes over log frame bytes (the derived trigger's 1/α bound) | <= 500 milli-x (ADR-0088 D4: interval = α × last checkpoint ⇒ checkpoint ≤ log / α; UNDEFINED until a checkpoint publishes) | — | PENDING (tooling) |
| S36: log + checkpoint write amplification (informational — the padding term is shape-dependent) | <= 1600 milli-x ((log frames + checkpoint + MANIFEST bytes) / encoded record bytes; log_padding_pct disclosed beside it) | — | PENDING (tooling) |
| S36: everysec max latency at the comparator-matched offered rate (S27 D5) | <= 50 ms (ADR-0081 D5 bar at an offered rate; latency from the intended send instant) | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=38982    p50_us=735    p99_us=1951    max_us=12663    barrier_p50_us=623   barrier_p99_us=1471   p50/barrier=1.18 frames_in_flight_max=3 acks/fsync=2.3 frames=180968 parked=0 write_stall_p99_us=1567
rep0 4c c256 ops/s=90034    p50_us=1215   p99_us=24063   max_us=62182    barrier_p50_us=655   barrier_p99_us=15103  frames_in_flight_max=3 acks/fsync=14.9 frames=85918 parked=0 write_stall_p99_us=15103
rep0 4c read c64 P16 ops/s=1612433  p50_us=639    p99_us=879     p999_us=1023    nils=8964
rep0 1c c32  ops/s=53432    p50_us=543    p99_us=1311    max_us=10262    barrier_p50_us=559   barrier_p99_us=1279   p50/barrier=0.97 frames_in_flight_max=3 acks/fsync=20.2 frames=28596 parked=0 write_stall_p99_us=1311
rep1 4c c32  ops/s=36149    p50_us=751    p99_us=2111    max_us=37326    barrier_p50_us=607   barrier_p99_us=1471   p50/barrier=1.24 frames_in_flight_max=3 acks/fsync=2.2 frames=172132 parked=0 write_stall_p99_us=1631
rep1 4c c256 ops/s=120661   p50_us=1215   p99_us=25087   max_us=80458    barrier_p50_us=655   barrier_p99_us=6527   frames_in_flight_max=3 acks/fsync=14.9 frames=108022 parked=0 write_stall_p99_us=8447
rep1 4c read c64 P16 ops/s=1560399  p50_us=671    p99_us=1151    p999_us=1247    nils=3158
rep1 1c c32  ops/s=48852    p50_us=559    p99_us=1631    max_us=31393    barrier_p50_us=527   barrier_p99_us=1311   p50/barrier=1.06 frames_in_flight_max=3 acks/fsync=19.4 frames=25788 parked=0 write_stall_p99_us=1407
rep2 4c c32  ops/s=38704    p50_us=751    p99_us=1951    max_us=11467    barrier_p50_us=639   barrier_p99_us=1471   p50/barrier=1.18 frames_in_flight_max=3 acks/fsync=2.2 frames=183087 parked=0 write_stall_p99_us=1535
rep2 4c c256 ops/s=166822   p50_us=1183   p99_us=13055   max_us=86683    barrier_p50_us=671   barrier_p99_us=2175   frames_in_flight_max=3 acks/fsync=14.8 frames=131090 parked=0 write_stall_p99_us=2239
rep2 4c read c64 P16 ops/s=1609261  p50_us=623    p99_us=927     p999_us=1007    nils=368
rep2 1c c32  ops/s=40584    p50_us=543    p99_us=9727    max_us=28945    barrier_p50_us=527   barrier_p99_us=1887   p50/barrier=1.03 frames_in_flight_max=3 acks/fsync=21.0 frames=19779 parked=0 write_stall_p99_us=15359
rep3 4c c32  ops/s=38922    p50_us=751    p99_us=1951    max_us=15517    barrier_p50_us=623   barrier_p99_us=1407   p50/barrier=1.21 frames_in_flight_max=3 acks/fsync=2.2 frames=186001 parked=0 write_stall_p99_us=1503
rep3 4c c256 ops/s=190113   p50_us=1183   p99_us=3455    max_us=29662    barrier_p50_us=671   barrier_p99_us=2047   frames_in_flight_max=3 acks/fsync=14.9 frames=154207 parked=0 write_stall_p99_us=2047
rep3 4c read c64 P16 ops/s=1593891  p50_us=735    p99_us=1007    p999_us=1247    nils=0
rep3 1c c32  ops/s=7536     p50_us=735    p99_us=62463   max_us=179554   barrier_p50_us=655   barrier_p99_us=62463  p50/barrier=1.12 frames_in_flight_max=3 acks/fsync=13.0 frames=6406 parked=0 write_stall_p99_us=61439
rep4 4c c32  ops/s=39111    p50_us=751    p99_us=1951    max_us=23129    barrier_p50_us=607   barrier_p99_us=1439   p50/barrier=1.24 frames_in_flight_max=3 acks/fsync=2.2 frames=187225 parked=0 write_stall_p99_us=1535
rep4 4c c256 ops/s=186242   p50_us=1183   p99_us=4223    max_us=30361    barrier_p50_us=671   barrier_p99_us=2015   frames_in_flight_max=3 acks/fsync=14.9 frames=150648 parked=0 write_stall_p99_us=2015
rep4 4c read c64 P16 ops/s=1595090  p50_us=671    p99_us=991     p999_us=1087    nils=0
rep4 1c c32  ops/s=53429    p50_us=575    p99_us=1311    max_us=9642     barrier_p50_us=559   barrier_p99_us=1279   p50/barrier=1.03 frames_in_flight_max=3 acks/fsync=18.6 frames=31436 parked=0 write_stall_p99_us=1311
```
