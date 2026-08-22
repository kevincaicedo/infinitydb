# M4.5 gate-run report

date: 1787416631 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 1 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 1 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 4c/1c per-replicate ratios (median of 3; spread): barrier 1.543–1.543 · client p50 1.487–1.495 — client histogram 256 sub-buckets/octave (≈ 0.4 %, 2 µs at 512–1024 µs); the barrier p50 is the server's 32-sub-bucket histogram (≈ 3 %, 16 µs at that octave)
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 3; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell (interleaved per replicate); max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram); device tail = fua_latency_p99_us (worst cell); 4c/1c ratios are medians of per-replicate ratios — the barrier ratio binds (F2's contention term), the client ratio is informational (ADR-0087 D8, amended 2026-08-22)

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.85 | FAIL |
| S35: N-cell vs 1-cell barrier p50 ratio (S34's F2 contention term) | <= 1.3 x (N-cell barrier p50 / 1-cell barrier p50 at 32 conns, per-replicate median; FLUSH read ~1.8, FUA K = 3 read 1.09–1.15 on 08-22) | 1.54 | FAIL |
| S35: N-cell vs 1-cell always client p50 ratio (informational since 2026-08-22 — carries the pipeline's seal wait at K ≥ 2) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns, per-replicate median on the 0.4 % client histogram; FLUSH read 1.8, FUA K = 1 1.45, K = 3 1.25–1.38 on the 3 % instrument) | 1.49 | FAIL (informational) |
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
rep0 4c c32  ops/s=28340    p50_us=1091   mean_us=1128   p99_us=2543    max_us=10795    barrier_p50_us=591   barrier_p99_us=1343   p50/barrier=1.85 frames_in_flight_max=1 acks/fsync=4.3 frames=72721 parked=0 write_stall_p99_us=1535 padding_pct=35.7 waits_fill=0
rep0 4c c256 ops/s=144143   p50_us=1323   mean_us=1774   p99_us=16767   max_us=77938    barrier_p50_us=655   barrier_p99_us=1855   frames_in_flight_max=1 acks/fsync=33.8 frames=54038 parked=0 write_stall_p99_us=1919 padding_pct=5.0 waits_fill=0
rep0 4c read c64 P16 ops/s=1616806  p50_us=641    p99_us=923     p999_us=999     nils=1611
rep0 1c c32  ops/s=41097    p50_us=733    mean_us=778    p99_us=1427    max_us=11988    barrier_p50_us=383   barrier_p99_us=767    p50/barrier=1.91 frames_in_flight_max=1 acks/fsync=16.4 frames=27189 parked=0 write_stall_p99_us=1151 padding_pct=9.0 waits_fill=0 4c/1c: p50=1.488 barrier=1.543
rep1 4c c32  ops/s=26963    p50_us=1075   mean_us=1186   p99_us=2543    max_us=111342   barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.82 frames_in_flight_max=1 acks/fsync=4.3 frames=69514 parked=0 write_stall_p99_us=1567 padding_pct=35.7 waits_fill=0
rep1 4c c256 ops/s=146430   p50_us=1327   mean_us=1747   p99_us=14943   max_us=103119   barrier_p50_us=655   barrier_p99_us=1855   frames_in_flight_max=1 acks/fsync=33.8 frames=54492 parked=0 write_stall_p99_us=1919 padding_pct=5.0 waits_fill=0
rep1 4c read c64 P16 ops/s=1555780  p50_us=709    p99_us=967     p999_us=1043    nils=1494
rep1 1c c32  ops/s=41929    p50_us=719    mean_us=762    p99_us=1419    max_us=9934     barrier_p50_us=383   barrier_p99_us=767    p50/barrier=1.88 frames_in_flight_max=1 acks/fsync=17.5 frames=26771 parked=0 write_stall_p99_us=1151 padding_pct=9.0 waits_fill=0 4c/1c: p50=1.495 barrier=1.543
rep2 4c c32  ops/s=28327    p50_us=1099   mean_us=1128   p99_us=2543    max_us=15522    barrier_p50_us=591   barrier_p99_us=1343   p50/barrier=1.86 frames_in_flight_max=1 acks/fsync=4.3 frames=72926 parked=0 write_stall_p99_us=1535 padding_pct=35.8 waits_fill=0
rep2 4c c256 ops/s=143959   p50_us=1319   mean_us=1776   p99_us=15391   max_us=74813    barrier_p50_us=655   barrier_p99_us=1887   frames_in_flight_max=1 acks/fsync=33.9 frames=53857 parked=0 write_stall_p99_us=1919 padding_pct=5.0 waits_fill=0
rep2 4c read c64 P16 ops/s=1588766  p50_us=667    p99_us=1031    p999_us=1099    nils=1602
rep2 1c c32  ops/s=40662    p50_us=739    mean_us=786    p99_us=1423    max_us=11514    barrier_p50_us=383   barrier_p99_us=751    p50/barrier=1.93 frames_in_flight_max=1 acks/fsync=16.6 frames=27241 parked=0 write_stall_p99_us=1151 padding_pct=9.0 waits_fill=0 4c/1c: p50=1.487 barrier=1.543
```
