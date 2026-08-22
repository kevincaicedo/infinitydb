# M4.5 gate-run report

date: 1787413891 (unix) · binary target/release/infinityd · cells 4 · 1 replicates · S35 row only · frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s35-gate/data (ext4)
- durable arms: frames-in-flight 3 · barrier-class fua · staging-mib 2 · device-write-mbps probe-file · seal-pace off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 4c/1c per-replicate ratios (median of 1; spread): barrier 1.435–1.435 · client p50 1.504–1.504 — client histogram 256 sub-buckets/octave (≈ 0.4 %, 2 µs at 512–1024 µs); the barrier p50 is the server's 32-sub-bucket histogram (≈ 3 %, 16 µs at that octave)
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 1; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell (interleaved per replicate); max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fua_latency_p50_us (cell median, whole-session histogram); device tail = fua_latency_p99_us (worst cell); 4c/1c ratios are medians of per-replicate ratios — the barrier ratio binds (F2's contention term), the client ratio is informational (ADR-0087 D8, amended 2026-08-22)

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
| S35: N-cell vs 1-cell barrier p50 ratio (S34's F2 contention term) | <= 1.3 x (N-cell barrier p50 / 1-cell barrier p50 at 32 conns, per-replicate median; FLUSH read ~1.8, FUA K = 3 read 1.09–1.15 on 08-22) | 1.43 | FAIL |
| S35: N-cell vs 1-cell always client p50 ratio (informational since 2026-08-22 — carries the pipeline's seal wait at K ≥ 2) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns, per-replicate median on the 0.4 % client histogram; FLUSH read 1.8, FUA K = 1 1.45, K = 3 1.25–1.38 on the 3 % instrument) | 1.50 | FAIL (informational) |
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
rep0 4c c32  ops/s=38944    p50_us=737    mean_us=820    p99_us=1943    max_us=11847    barrier_p50_us=607   barrier_p99_us=1439   p50/barrier=1.21 frames_in_flight_max=3 acks/fsync=2.2 frames=184455 parked=0 write_stall_p99_us=1503 padding_pct=51.6 waits_fill=0
rep0 4c c256 ops/s=160031   p50_us=1159   mean_us=1598   p99_us=17727   max_us=64773    barrier_p50_us=671   barrier_p99_us=2111   frames_in_flight_max=3 acks/fsync=14.9 frames=133128 parked=0 write_stall_p99_us=2111 padding_pct=12.3 waits_fill=0
rep0 4c read c64 P16 ops/s=1587853  p50_us=701    p99_us=1019    p999_us=1099    nils=351
rep0 1c c32  ops/s=40215    p50_us=490    mean_us=790    p99_us=3263    max_us=68724    barrier_p50_us=423   barrier_p99_us=1279   p50/barrier=1.16 frames_in_flight_max=3 acks/fsync=26.4 frames=15551 parked=0 write_stall_p99_us=1343 padding_pct=9.3 waits_fill=0 4c/1c: p50=1.504 barrier=1.435
```
