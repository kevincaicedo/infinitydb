# M4.5 gate-run report

date: 1787427398 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S39b row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s39b/data (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s39b: the S29, S27, S35 and S36 rows were skipped; their gate keys are absent
- s39b row: 4 cells · 3 replicates (ABBA) · write leg 150 s at 32 conns · segment-bytes 268435456 · ckpt floor 268435456 · arm --segment-recycle-slots 1 vs baseline --no-segment-recycle · device stat nvme0n1 · first-generation trigger: every cell truncated ≥ 1 and rotated ≥ 2

| gate | threshold | measured | verdict |
|---|---|---|---|
| S29: tiered always scaling slope (c256/c64) | >= 2 x (ops/s ratio across 4x conns) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 64 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always parity at 256 conns | >= 0.7 x (tiered ops/s / flat ops/s, same node+session) | — | PENDING (tooling) |
| S29: tiered:flat always p99 ratio at 256 conns | <= 4 x (tiered p99 / flat p99 — pre-fix read ~40x) | — | PENDING (tooling) |
| S27: client-visible -BUSY refusals under provoked staging pressure | <= 0.05 % of operations (ADR-0081 D5: pacing, not refusal) | — | PENDING (tooling) |
| S27: last:first throughput across back-to-back write repeats | >= 0.9 x (the finding's signature was 2.4x monotonic decay) | — | PENDING (tooling) |
| S27: worst per-leg max latency at everysec under provoked pressure | <= 50 ms (ADR-0081 D5: max <= 50 ms at everysec) | — | PENDING (tooling) |
| S35: always p50 over the barrier p50 at 32 conns | <= 1.3 x (client p50 / barrier p50; plan AC 1.2 + 0.1-window loop overhead — K = 1 reads ~1.85) | — | PENDING (tooling) |
| S35: N-cell vs 1-cell barrier p50 ratio (device characterization — informational since the ADR-0087 fourth amendment, 2026-08-22) | <= 1.3 x (N-cell barrier p50 / 1-cell barrier p50 at 32 conns, per-replicate median; FLUSH read ~1.8, FUA read 1.35–1.54 at every K on 08-22 — qd4 vs qd1 on the device) | — | PENDING (tooling) |
| S35: N-cell vs 1-cell always client p50 ratio (informational since 2026-08-22 — carries the pipeline's seal wait at K ≥ 2) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns, per-replicate median on the 0.4 % client histogram; FLUSH read 1.8, FUA K = 1 1.45, K = 3 1.25–1.38 on the 3 % instrument) | — | PENDING (tooling) |
| S36: server CPU across the pure-write everysec leg | >= 300 % of one core (400 = four cells flat out; pre-S36 read 123–185) | — | PENDING (tooling) |
| S36: everysec pure-write throughput vs the same-session tmpfs control | >= 0.85 x (device arm ops/s / tmpfs control ops/s, same binary, same session) | — | PENDING (tooling) |
| S36: checkpoint + MANIFEST bytes over log frame bytes (the derived trigger's 1/α bound) | <= 500 milli-x (ADR-0088 D4: interval = α × last checkpoint ⇒ checkpoint ≤ log / α; UNDEFINED until a checkpoint publishes) | — | PENDING (tooling) |
| S36: log + checkpoint write amplification (informational — the padding term is shape-dependent) | <= 1600 milli-x ((log frames + checkpoint + MANIFEST bytes) / encoded record bytes; log_padding_pct disclosed beside it) | — | PENDING (tooling) |
| S36: everysec max latency at the comparator-matched offered rate (S27 D5) | <= 50 ms (ADR-0081 D5 bar at an offered rate; latency from the intended send instant) | — | PENDING (tooling) |
| S39b: warmed zero-fill bytes per log frame byte under recycling (arm) | <= 0.1 x (Δzero_fill_bytes / Δlog_frame_bytes after the first generation, per-replicate median; baseline ≈ 1.0; > 0.3 = falsifier (a)) | 0.38 | FAIL |
| S39b: rotations not served by the pool over the warmed window, worst cell | <= 2 segments (Δrotations − Δrecycled, worst cell, per-replicate median; ADR-0090 D6: recycled ≥ rotations − 2) | 3.00 | FAIL |
| S39b: padding share moved by recycling (the S39c control) | <= 3 percentage points (|arm − baseline| warmed log_padding_pct, per-replicate median; recycling must not touch the 2.2-record frame's block) | 0.04 | PASS |
| S39b: always c32 p50 under recycling over baseline | <= 1.05 x (arm / baseline, per-replicate median; falsifier (b): the rename or the residue read costs loop latency) | 0.97 | PASS |
| S39b: always c32 p99 under recycling over baseline | <= 1.1 x (arm / baseline, per-replicate median) | 0.52 | PASS |
| S39b: read leg under recycling over baseline (the E4.7 ±2 % rule) | >= 0.98 x (arm / baseline GET ops/s at 64 conns × pipeline 16, per-replicate median) | 0.98 | PASS |
| S39b: crash-restart recovery time on the recycled log over baseline | <= 1.05 x (arm / baseline seconds to loading:0 after SIGKILL, per-replicate median; falsifier (c): the residue scan is not O(frame) | 1.25 | FAIL |
| S39b: rotations per cell the row actually performed (validity) | >= 8 rotations (min over cells and arms, per-replicate min; ADR-0090 D6 asks ≥ 8) | 9.00 | PASS |
| S39b: accounted host bytes per log frame byte under recycling (informational) | <= 2 x (Δ(log + zero-fill + ckpt + MANIFEST) / Δlog_frame_bytes, warmed; baseline ≈ 2.2 with the zero-fill term) | 1.56 | PASS (informational) |
| S39b: block-device sectors written per log frame byte under recycling (informational) | <= 2 x (Δsectors_written × 512 / Δlog_frame_bytes, warmed; /sys/block/<dev>/stat — journal + metadata included, NAND amplification not) | 1.57 | PASS (informational) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| segment-recycling | the warmed figures are deltas between the first-generation snapshot and the leg's end (ADR-0090 A4); `host_bytes_per_log_byte` is accounted host bytes (log + zero-fill + checkpoint + MANIFEST) over log frame bytes, `device_bytes_per_log_byte` the block device's sectors-written × 512 over the same — journal and metadata included, NAND amplification not (A3) |

## s39b per-leg samples

```
rep0 base c32 ops=32226 p50_us=731 p99_us=5023 max_us=150778 barrier_p50_us=607 barrier_p99_us=1887 parked=0 read_ops=1617180 rotations=42 recycled=0 misses=0 fallbacks=0 truncated=38 rotations_min_cell=10 trigger_at_s=15.0 warmed=true firstgen[zero_fill=2254700544 frame_bytes=1124339712 share=2.005] warmed[zero_fill=9682026496 frame_bytes=9571192832 share=1.012 padding_pct=51.7 host_per_log=2.199 device_per_log=2.203 ckpt=1789239296 deficit=9] recovery_s=3.508 recover_residue_slacks=0 recover_residue_stops=0
rep0 arm c32 ops=31474 p50_us=711 p99_us=2591 max_us=194998 barrier_p50_us=607 barrier_p99_us=1567 parked=0 read_ops=1587228 rotations=40 recycled=24 misses=20 fallbacks=0 truncated=36 rotations_min_cell=10 trigger_at_s=26.1 warmed=true firstgen[zero_fill=2291138560 frame_bytes=1137516544 share=2.014] warmed[zero_fill=3024093184 frame_bytes=9275035648 share=0.326 padding_pct=51.7 host_per_log=1.508 device_per_log=1.512 ckpt=1683877888 deficit=2] recovery_s=4.080 recover_residue_slacks=5 recover_residue_stops=4
rep1 arm c32 ops=29171 p50_us=711 p99_us=11871 max_us=255325 barrier_p50_us=591 barrier_p99_us=2175 parked=0 read_ops=1617168 rotations=38 recycled=20 misses=22 fallbacks=0 truncated=34 rotations_min_cell=9 trigger_at_s=20.0 warmed=true firstgen[zero_fill=2293235712 frame_bytes=1139240960 share=2.013] warmed[zero_fill=3560964096 frame_bytes=8560406528 share=0.416 padding_pct=51.8 host_per_log=1.600 device_per_log=1.605 ckpt=1578594304 deficit=3] recovery_s=4.704 recover_residue_slacks=7 recover_residue_stops=6
rep1 base c32 ops=29424 p50_us=731 p99_us=14943 max_us=314304 barrier_p50_us=607 barrier_p99_us=5375 parked=0 read_ops=1610991 rotations=38 recycled=0 misses=0 fallbacks=0 truncated=35 rotations_min_cell=9 trigger_at_s=22.0 warmed=true firstgen[zero_fill=2435579904 frame_bytes=1208553472 share=2.015] warmed[zero_fill=8614313984 frame_bytes=8567218176 share=1.005 padding_pct=51.8 host_per_log=2.196 device_per_log=2.201 ckpt=1631170560 deficit=8] recovery_s=3.772 recover_residue_slacks=0 recover_residue_stops=0
rep2 base c32 ops=30932 p50_us=727 p99_us=6751 max_us=270632 barrier_p50_us=591 barrier_p99_us=1951 parked=0 read_ops=1588667 rotations=40 recycled=0 misses=0 fallbacks=0 truncated=36 rotations_min_cell=10 trigger_at_s=16.0 warmed=true firstgen[zero_fill=2273050624 frame_bytes=1126576128 share=2.018] warmed[zero_fill=9538109440 frame_bytes=9126633472 share=1.045 padding_pct=51.7 host_per_log=2.230 device_per_log=2.235 ckpt=1683894272 deficit=8] recovery_s=3.744 recover_residue_slacks=0 recover_residue_stops=0
rep2 arm c32 ops=34074 p50_us=713 p99_us=2263 max_us=80301 barrier_p50_us=591 barrier_p99_us=1567 parked=0 read_ops=1551377 rotations=44 recycled=25 misses=23 fallbacks=0 truncated=40 rotations_min_cell=11 trigger_at_s=16.0 warmed=true firstgen[zero_fill=2351693824 frame_bytes=1166839808 share=2.015] warmed[zero_fill=3822321664 frame_bytes=10125680640 share=0.377 padding_pct=51.7 host_per_log=1.565 device_per_log=1.569 ckpt=1894596608 deficit=3] recovery_s=5.278 recover_residue_slacks=8 recover_residue_stops=7
```
