# M4.5 gate-run report

date: 1787416126 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 1 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 1 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 4c/1c per-replicate ratios (median of 3; spread): barrier 1.543–1.543 · client p50 1.437–1.499 — client histogram 256 sub-buckets/octave (≈ 0.4 %, 2 µs at 512–1024 µs); the barrier p50 is the server's 32-sub-bucket histogram (≈ 3 %, 16 µs at that octave)
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
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | 1.86 | FAIL |
| S35: N-cell vs 1-cell barrier p50 ratio (S34's F2 contention term) | <= 1.3 x (N-cell barrier p50 / 1-cell barrier p50 at 32 conns, per-replicate median; FLUSH read ~1.8, FUA K = 3 read 1.09–1.15 on 08-22) | 1.54 | FAIL |
| S35: N-cell vs 1-cell always client p50 ratio (informational since 2026-08-22 — carries the pipeline's seal wait at K ≥ 2) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns, per-replicate median on the 0.4 % client histogram; FLUSH read 1.8, FUA K = 1 1.45, K = 3 1.25–1.38 on the 3 % instrument) | 1.47 | FAIL (informational) |
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
rep0 4c c32  ops/s=27816    p50_us=1091   mean_us=1149   p99_us=2663    max_us=11440    barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.85 frames_in_flight_max=1 acks/fsync=4.3 frames=68572 parked=0 write_stall_p99_us=1631 padding_pct=34.9 waits_fill=3188
rep0 4c c256 ops/s=169743   p50_us=1335   mean_us=1506   p99_us=3599    max_us=30386    barrier_p50_us=655   barrier_p99_us=1759   frames_in_flight_max=1 acks/fsync=33.8 frames=61488 parked=0 write_stall_p99_us=1823 padding_pct=5.0 waits_fill=0
rep0 4c read c64 P16 ops/s=1618145  p50_us=723    p99_us=989     p999_us=1087    nils=707
rep0 1c c32  ops/s=40977    p50_us=759    mean_us=780    p99_us=1419    max_us=10238    barrier_p50_us=383   barrier_p99_us=751    p50/barrier=1.98 frames_in_flight_max=1 acks/fsync=17.2 frames=26887 parked=0 write_stall_p99_us=1151 padding_pct=9.2 waits_fill=11 4c/1c: p50=1.437 barrier=1.543
rep1 4c c32  ops/s=27686    p50_us=1103   mean_us=1155   p99_us=2607    max_us=11669    barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.87 frames_in_flight_max=1 acks/fsync=4.3 frames=68157 parked=0 write_stall_p99_us=1567 padding_pct=34.8 waits_fill=3062
rep1 4c c256 ops/s=150514   p50_us=1347   mean_us=1699   p99_us=12511   max_us=86750    barrier_p50_us=655   barrier_p99_us=1759   frames_in_flight_max=1 acks/fsync=33.8 frames=55532 parked=0 write_stall_p99_us=1855 padding_pct=5.0 waits_fill=0
rep1 4c read c64 P16 ops/s=1608444  p50_us=687    p99_us=979     p999_us=1091    nils=1363
rep1 1c c32  ops/s=40924    p50_us=749    mean_us=781    p99_us=1427    max_us=12025    barrier_p50_us=383   barrier_p99_us=751    p50/barrier=1.96 frames_in_flight_max=1 acks/fsync=16.6 frames=27872 parked=0 write_stall_p99_us=751 padding_pct=9.0 waits_fill=11 4c/1c: p50=1.473 barrier=1.543
rep2 4c c32  ops/s=27717    p50_us=1099   mean_us=1153   p99_us=2727    max_us=11002    barrier_p50_us=591   barrier_p99_us=1375   p50/barrier=1.86 frames_in_flight_max=1 acks/fsync=4.3 frames=68154 parked=0 write_stall_p99_us=1599 padding_pct=34.8 waits_fill=2953
rep2 4c c256 ops/s=144397   p50_us=1319   mean_us=1771   p99_us=16223   max_us=164915   barrier_p50_us=655   barrier_p99_us=1823   frames_in_flight_max=1 acks/fsync=33.9 frames=53825 parked=0 write_stall_p99_us=1919 padding_pct=5.0 waits_fill=0
rep2 4c read c64 P16 ops/s=1626289  p50_us=641    p99_us=867     p999_us=979     nils=1624
rep2 1c c32  ops/s=41052    p50_us=733    mean_us=779    p99_us=1419    max_us=11782    barrier_p50_us=383   barrier_p99_us=767    p50/barrier=1.91 frames_in_flight_max=1 acks/fsync=16.4 frames=27159 parked=0 write_stall_p99_us=1183 padding_pct=9.3 waits_fill=17 4c/1c: p50=1.499 barrier=1.543
```
