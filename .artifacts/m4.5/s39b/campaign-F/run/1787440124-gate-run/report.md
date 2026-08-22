# M4.5 gate-run report

date: 1787440124 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S39b row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s39b/data-F (ext4)
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
| S39b: warmed zero-fill bytes per log frame byte under recycling (arm) | <= 0.1 x (Δzero_fill_bytes / Δlog_frame_bytes after the first generation, per-replicate median; baseline ≈ 1.0; > 0.3 = falsifier (a)) | 0.22 | FAIL |
| S39b: rotations not served by the pool over the warmed window, worst cell | <= 2 segments (Δrotations − Δrecycled, worst cell, per-replicate median; ADR-0090 D6: recycled ≥ rotations − 2) | 2.00 | PASS |
| S39b: padding share moved by recycling (the S39c control) | <= 3 percentage points (|arm − baseline| warmed log_padding_pct, per-replicate median; recycling must not touch the 2.2-record frame's block) | 0.05 | PASS |
| S39b: always c32 p50 under recycling over baseline | <= 1.05 x (arm / baseline, per-replicate median; falsifier (b): the rename or the residue read costs loop latency) | 0.97 | PASS |
| S39b: always c32 p99 under recycling over baseline | <= 1.1 x (arm / baseline, per-replicate median) | 0.20 | PASS |
| S39b: read leg under recycling over baseline (the E4.7 ±2 % rule) | >= 0.98 x (arm / baseline GET ops/s at 64 conns × pipeline 16, per-replicate median) | 0.98 | FAIL |
| S39b: first crash-restart recovery boot on the recycled log over baseline | <= 1.05 x (arm / baseline seconds from first process launch to loading:0 after SIGKILL and --leg-idle-s, per-replicate median; falsifier (c): the residue scan is not O(frame)) | 1.29 | FAIL |
| S39b: rotations per cell the row actually performed (validity) | >= 8 rotations (min over cells and arms, per-replicate min; ADR-0090 D6 asks ≥ 8) | 9.00 | PASS |
| S39b: accounted host bytes per log frame byte under recycling (informational) | <= 2 x (Δ(log + zero-fill + ckpt + MANIFEST) / Δlog_frame_bytes, warmed; baseline ≈ 2.2 with the zero-fill term) | 1.42 | PASS (informational) |
| S39b: block-device sectors written per log frame byte under recycling (informational) | <= 2 x (Δsectors_written × 512 / Δlog_frame_bytes, warmed; /sys/block/<dev>/stat — journal + metadata included, NAND amplification not) | 1.42 | PASS (informational) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| segment-recycling | the warmed figures are deltas between the first-generation snapshot and the leg's end (ADR-0090 A4); `host_bytes_per_log_byte` is accounted host bytes (log + zero-fill + checkpoint + MANIFEST) over log frame bytes, `device_bytes_per_log_byte` the block device's sectors-written × 512 over the same — journal and metadata included, NAND amplification not (A3) |

## s39b per-leg samples

```
rep0 base c32 ops=27728 p50_us=727 p99_us=16511 max_us=333597 barrier_p50_us=591 barrier_p99_us=14847 parked=0 read_ops=1623408 rotations=36 recycled=0 misses=0 fallbacks=0 pool_full=0 truncated=32 rotations_min_cell=9 trigger_at_s=15.0 warmed=true firstgen[zero_fill=2412249088 frame_bytes=1184145408 share=2.037] warmed[zero_fill=8255700992 frame_bytes=8091717632 share=1.020 padding_pct=51.9 host_per_log=2.202 device_per_log=2.207 ckpt=1473224704 deficit=7] recovery_first_boot_s=3.925 recover_residue_slacks=0 recover_residue_stops=0
rep0 arm c32 ops=31584 p50_us=717 p99_us=2599 max_us=144600 barrier_p50_us=591 barrier_p99_us=1791 parked=0 read_ops=1566680 rotations=43 recycled=30 misses=17 fallbacks=0 pool_full=4 truncated=39 rotations_min_cell=10 trigger_at_s=16.0 warmed=true firstgen[zero_fill=2559311872 frame_bytes=1257385984 share=2.035] warmed[zero_fill=2004090880 frame_bytes=9263808512 share=0.216 padding_pct=51.7 host_per_log=1.415 device_per_log=1.420 ckpt=1841893376 deficit=2] recovery_first_boot_s=3.535 recover_residue_slacks=8 recover_residue_stops=8
rep1 arm c32 ops=34456 p50_us=711 p99_us=2079 max_us=75407 barrier_p50_us=607 barrier_p99_us=1503 parked=0 read_ops=1590185 rotations=45 recycled=31 misses=18 fallbacks=0 pool_full=2 truncated=41 rotations_min_cell=11 trigger_at_s=15.0 warmed=true firstgen[zero_fill=2414346240 frame_bytes=1185542144 share=2.036] warmed[zero_fill=2417491968 frame_bytes=10317185024 share=0.234 padding_pct=51.8 host_per_log=1.423 device_per_log=1.428 ckpt=1947324416 deficit=2] recovery_first_boot_s=5.225 recover_residue_slacks=9 recover_residue_stops=9
rep1 base c32 ops=29340 p50_us=731 p99_us=6575 max_us=367336 barrier_p50_us=607 barrier_p99_us=1887 parked=0 read_ops=1594155 rotations=39 recycled=0 misses=0 fallbacks=0 pool_full=0 truncated=35 rotations_min_cell=9 trigger_at_s=26.1 warmed=true firstgen[zero_fill=2529689600 frame_bytes=1241583616 share=2.037] warmed[zero_fill=8556380160 frame_bytes=8491298816 share=1.008 padding_pct=51.8 host_per_log=2.200 device_per_log=2.205 ckpt=1631313920 deficit=8] recovery_first_boot_s=3.601 recover_residue_slacks=0 recover_residue_stops=0
rep2 base c32 ops=29296 p50_us=731 p99_us=10943 max_us=254956 barrier_p50_us=607 barrier_p99_us=2047 parked=0 read_ops=1609839 rotations=39 recycled=0 misses=0 fallbacks=0 pool_full=0 truncated=34 rotations_min_cell=9 trigger_at_s=15.0 warmed=true firstgen[zero_fill=2445803520 frame_bytes=1200857088 share=2.037] warmed[zero_fill=8656257024 frame_bytes=8611807232 share=1.005 padding_pct=51.9 host_per_log=2.188 device_per_log=2.199 ckpt=1578561536 deficit=8] recovery_first_boot_s=3.834 recover_residue_slacks=0 recover_residue_stops=0
rep2 arm c32 ops=32409 p50_us=709 p99_us=2223 max_us=267668 barrier_p50_us=591 barrier_p99_us=1567 parked=0 read_ops=1576101 rotations=42 recycled=29 misses=17 fallbacks=0 pool_full=1 truncated=38 rotations_min_cell=10 trigger_at_s=15.1 warmed=true firstgen[zero_fill=2420375552 frame_bytes=1188167680 share=2.037] warmed[zero_fill=2143027200 frame_bytes=9653215232 share=0.222 padding_pct=51.9 host_per_log=1.407 device_per_log=1.412 ckpt=1789263872 deficit=2] recovery_first_boot_s=4.957 recover_residue_slacks=7 recover_residue_stops=7
```
