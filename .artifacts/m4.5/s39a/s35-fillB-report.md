# M4.5 gate-run report

date: 1787373515 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35: 2 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 3; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell (interleaved per replicate); max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram); device tail = fua_latency_p99_us (worst cell)

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
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.67 | FAIL |
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
rep0 4c c32  ops/s=27286    p50_us=1119   p99_us=2751    max_us=11382    barrier_p50_us=591   barrier_p99_us=1407   p50/barrier=1.89 frames_in_flight_max=2 acks/fsync=4.3 frames=66907 parked=0 write_stall_p99_us=1695 padding_pct=34.7 waits_fill=63510
rep0 4c c256 ops/s=120757   p50_us=1151   p99_us=25087   max_us=58874    barrier_p50_us=671   barrier_p99_us=17919  frames_in_flight_max=3 acks/fsync=18.1 frames=93427 parked=0 write_stall_p99_us=17407 padding_pct=10.7 waits_fill=55254
rep0 4c read c64 P16 ops/s=1605496  p50_us=655    p99_us=959     p999_us=1023    nils=2943
rep0 1c c32  ops/s=44658    p50_us=671    p99_us=1407    max_us=10326    barrier_p50_us=559   barrier_p99_us=1279   p50/barrier=1.20 frames_in_flight_max=3 acks/fsync=17.4 frames=29219 parked=0 write_stall_p99_us=1279 padding_pct=11.7 waits_fill=6376
rep1 4c c32  ops/s=26343    p50_us=1087   p99_us=2687    max_us=110674   barrier_p50_us=591   barrier_p99_us=1439   p50/barrier=1.84 frames_in_flight_max=3 acks/fsync=4.3 frames=65133 parked=0 write_stall_p99_us=1631 padding_pct=34.8 waits_fill=61715
rep1 4c c256 ops/s=175029   p50_us=1119   p99_us=14335   max_us=52857    barrier_p50_us=671   barrier_p99_us=2559   frames_in_flight_max=3 acks/fsync=18.2 frames=122342 parked=0 write_stall_p99_us=2559 padding_pct=10.6 waits_fill=71407
rep1 4c read c64 P16 ops/s=1601051  p50_us=703    p99_us=927     p999_us=1087    nils=249
rep1 1c c32  ops/s=44105    p50_us=687    p99_us=1439    max_us=12237    barrier_p50_us=575   barrier_p99_us=1279   p50/barrier=1.19 frames_in_flight_max=3 acks/fsync=17.2 frames=28282 parked=0 write_stall_p99_us=1311 padding_pct=11.6 waits_fill=6020
rep2 4c c32  ops/s=20425    p50_us=1119   p99_us=7167    max_us=143908   barrier_p50_us=591   barrier_p99_us=1407   p50/barrier=1.89 frames_in_flight_max=2 acks/fsync=4.3 frames=48716 parked=0 write_stall_p99_us=1855 padding_pct=35.0 waits_fill=45785
rep2 4c c256 ops/s=212770   p50_us=1119   p99_us=2879    max_us=15962    barrier_p50_us=687   barrier_p99_us=2111   frames_in_flight_max=3 acks/fsync=18.2 frames=140916 parked=0 write_stall_p99_us=2175 padding_pct=10.7 waits_fill=82982
rep2 4c read c64 P16 ops/s=1604331  p50_us=671    p99_us=991     p999_us=1087    nils=0
rep2 1c c32  ops/s=32710    p50_us=655    p99_us=16383   max_us=28412    barrier_p50_us=575   barrier_p99_us=15871  p50/barrier=1.14 frames_in_flight_max=3 acks/fsync=17.6 frames=22029 parked=0 write_stall_p99_us=15615 padding_pct=11.6 waits_fill=2985
```
