# M4.5 gate-run report

date: 1787361601 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35: 1 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
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
| S35: N-cell vs 1-cell always p50 ratio (S34's F2 AC) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns; FLUSH read 1.8, FUA K = 1 1.45) | 1.38 | FAIL |
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
rep0 4c c32  ops/s=29901    p50_us=767    p99_us=8703    max_us=108363   barrier_p50_us=655   barrier_p99_us=2175   p50/barrier=1.17 frames_in_flight_max=3 acks/fsync=2.2 frames=141349 parked=0 write_stall_p99_us=6399
rep0 4c c256 ops/s=201157   p50_us=1183   p99_us=2943    max_us=14789    barrier_p50_us=687   barrier_p99_us=2239   frames_in_flight_max=3 acks/fsync=14.9 frames=163005 parked=0 write_stall_p99_us=2303
rep0 4c read c64 P16 ops/s=1590250  p50_us=639    p99_us=1055    p999_us=1119    nils=0
rep1 4c c32  ops/s=39297    p50_us=735    p99_us=1951    max_us=14386    barrier_p50_us=607   barrier_p99_us=1439   p50/barrier=1.21 frames_in_flight_max=3 acks/fsync=2.2 frames=187085 parked=0 write_stall_p99_us=1503
rep1 4c c256 ops/s=200523   p50_us=1183   p99_us=2879    max_us=11946    barrier_p50_us=655   barrier_p99_us=1983   frames_in_flight_max=3 acks/fsync=14.9 frames=159846 parked=0 write_stall_p99_us=1983
rep1 4c read c64 P16 ops/s=1575509  p50_us=639    p99_us=1087    p999_us=1151    nils=0
rep2 4c c32  ops/s=39120    p50_us=751    p99_us=1951    max_us=11648    barrier_p50_us=607   barrier_p99_us=1471   p50/barrier=1.24 frames_in_flight_max=3 acks/fsync=2.2 frames=185675 parked=0 write_stall_p99_us=1535
rep2 4c c256 ops/s=103095   p50_us=1183   p99_us=25599   max_us=35362    barrier_p50_us=655   barrier_p99_us=16383  frames_in_flight_max=3 acks/fsync=14.8 frames=93795 parked=0 write_stall_p99_us=16127
rep2 4c read c64 P16 ops/s=1600329  p50_us=671    p99_us=975     p999_us=1055    nils=5457
rep0 1c c32  ops/s=56223    p50_us=543    p99_us=1279    max_us=10082    barrier_p50_us=431   barrier_p99_us=1279   p50/barrier=1.26 frames_in_flight_max=3 acks/fsync=23.7 frames=26078 parked=0 write_stall_p99_us=1279
rep1 1c c32  ops/s=55356    p50_us=543    p99_us=1279    max_us=11707    barrier_p50_us=471   barrier_p99_us=1279   p50/barrier=1.15 frames_in_flight_max=3 acks/fsync=22.6 frames=25549 parked=0 write_stall_p99_us=1279
rep2 1c c32  ops/s=48984    p50_us=543    p99_us=3135    max_us=71077    barrier_p50_us=471   barrier_p99_us=1279   p50/barrier=1.15 frames_in_flight_max=3 acks/fsync=22.1 frames=22366 parked=0 write_stall_p99_us=1279
```
