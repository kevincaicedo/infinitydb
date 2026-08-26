# M4.5 gate-run report

date: 1787456182 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S39d row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s39d/data-H (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s39d: the S29, S27, S35, S36 and S39b rows were skipped; their gate keys are absent
- s39d row: 4 cells · 3 replicates (ABBA) · fixed work: 3000000 warm + 200000 tail records × 1 KiB at 32 conns pipeline 16 · segment-bytes 268435456 · ckpt floor 268435456 · boundary = INF.CKPT WAIT after the warm fill, truncation settled 2 s · SIGKILL after the tail's last ack · 40 s idle · one boot · arm (server default) vs baseline --no-segment-recycle · device stat nvme0n1
- s39d dominating phase on the slowest cell — arm: ckpt×3; baseline: ckpt×3 (per replicate)

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
| S39b: warmed zero-fill bytes per log frame byte under recycling (arm) | <= 0.1 x (Δzero_fill_bytes / Δlog_frame_bytes after the first generation, per-replicate median; baseline ≈ 1.0; > 0.3 = falsifier (a)) | — | PENDING (tooling) |
| S39b: rotations not served by the pool over the warmed window, worst cell | <= 2 segments (Δrotations − Δrecycled, worst cell, per-replicate median; ADR-0090 D6: recycled ≥ rotations − 2) | — | PENDING (tooling) |
| S39b: padding share moved by recycling (the S39c control) | <= 3 percentage points (|arm − baseline| warmed log_padding_pct, per-replicate median; recycling must not touch the 2.2-record frame's block) | — | PENDING (tooling) |
| S39b: always c32 p50 under recycling over baseline | <= 1.05 x (arm / baseline, per-replicate median; falsifier (b): the rename or the residue read costs loop latency) | — | PENDING (tooling) |
| S39b: always c32 p99 under recycling over baseline | <= 1.1 x (arm / baseline, per-replicate median) | — | PENDING (tooling) |
| S39b: read leg under recycling over baseline (the E4.7 ±2 % rule) | >= 0.98 x (arm / baseline GET ops/s at 64 conns × pipeline 16, per-replicate median) | — | PENDING (tooling) |
| S39b: first crash-restart recovery boot on the recycled log over baseline | <= 1.05 x (arm / baseline seconds from first process launch to loading:0 after SIGKILL and --leg-idle-s, per-replicate median; falsifier (c): the residue scan is not O(frame)) | — | PENDING (tooling) |
| S39b: rotations per cell the row actually performed (validity) | >= 8 rotations (min over cells and arms, per-replicate min; ADR-0090 D6 asks ≥ 8) | — | PENDING (tooling) |
| S39b/D9: pool waits fed before the bound over waits ended (arm) | >= 0.5 share (Δrecycle_waits_satisfied / Δ(satisfied + expired), warmed; ADR-0090 A9: waits end predominantly satisfied; absent when no wait ended) | — | PENDING (tooling) |
| S39b/D9: rotations onto an un-zeroed segment on the arm, worst replicate | <= 0 rotations (cumulative at leg end, summed over cells, max over replicates; ADR-0090 D9 falsifier: the deferred fill missed the rotation) | — | PENDING (tooling) |
| S39b/D9: rotations that found no next segment on the arm, worst replicate | <= 0 preallocs (cumulative at leg end, summed over cells, max over replicates; a blocking prealloc on the loop — the wait must never cause one) | — | PENDING (tooling) |
| S39b/D9: preallocs refused for space on the arm, worst replicate | <= 0 failures (cumulative at leg end, summed over cells, max over replicates; ENOSPC is discovered at the bound with 3/4 of the segment as headroom) | — | PENDING (tooling) |
| S39b: accounted host bytes per log frame byte under recycling (informational) | <= 2 x (Δ(log + zero-fill + ckpt + MANIFEST) / Δlog_frame_bytes, warmed; baseline ≈ 2.2 with the zero-fill term) | — | PENDING (tooling) |
| S39b: block-device sectors written per log frame byte under recycling (informational) | <= 2 x (Δsectors_written × 512 / Δlog_frame_bytes, warmed; /sys/block/<dev>/stat — journal + metadata included, NAND amplification not) | — | PENDING (tooling) |
| S39d: fixed-work first-boot recovery, slowest cell's engine total, arm over baseline | <= 1.05 x (recover_total_us on the slowest cell, arm ÷ baseline, per-replicate median; diagnostic — the per-phase ratios name the term) | 1.01 | PASS (informational) |
| S39d: absolute first-boot recovery wall on the recycled log (the S18 < 15 s STOP gate re-read) | <= 15 s (process launch to loading:0 on every cell, arm, per-replicate median; the dataset is the row's fixed work, disclosed in the report) | 3.64 | PASS |
| S39d: both arms recovered exactly the records written (fixed-work validity) | >= 1 1 = every replicate pair recovered warm + tail records on both arms, 0 = a pair did not (the row is then not fixed-work) | 1.00 | PASS |
| S39d: slack-audit phase time over the cell sums, arm over baseline (the recycling term, informational) | <= 1.5 x (Σcells recover_audit_us arm ÷ baseline, per-replicate median; residue is decoded and CRC-validated where zeros are skipped word-wise) | 1.04 | PASS (informational) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| recovery-attribution | fixed-work recovery (ADR-0090 A10): `recovery_total_x` is the slowest cell's engine total (first step to completion on the loop clock) arm ÷ baseline; `recovery_wall_x` the harness wall from process launch to loading:0; `phase_*_x` the per-phase work ratio over the cell sums; `records_recovered_match` = 1 when both arms recovered exactly the records written; `frame_bytes_x` discloses the encoded-bytes parity the group formation allows |

## s39d per-leg samples

```
rep0 base warm_ops=134231 tail_ops=209457 warmed=true warm[rotations=16 recycled=0 truncated=16 rotations_min_cell=4 waits_expired=0] end[rotations=16 recycled=0 truncated=16 frame_bytes=3765198848 zero_fill=5244452864] boot_wall_s=3.517 proc_read_bytes=5183074304 records_recovered=3200000 slowest_cell[total_ms=3489.6 start_ms=3.9 ckpt_ms=2075.9 replay_ms=401.4 audit_ms=1008.3 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=13819.9 ckpt_ms=8245.2 replay_ms=1592.4 audit_ms=3966.5 finish_ms=0.3 ckpt_bytes=3160240128 replay_bytes=441008128 replay_frames=27682 audit_bytes=1706475520 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep0 arm warm_ops=133460 tail_ops=283257 warmed=true warm[rotations=16 recycled=4 truncated=16 rotations_min_cell=4 waits_expired=8] end[rotations=16 recycled=4 truncated=16 frame_bytes=3766304768 zero_fill=4294967296] boot_wall_s=3.919 proc_read_bytes=5307330560 records_recovered=3200000 slowest_cell[total_ms=3884.6 start_ms=3.9 ckpt_ms=2126.0 replay_ms=413.4 audit_ms=1283.3 finish_ms=58.0 dominating=ckpt] sum_cells[total_ms=15009.2 ckpt_ms=8519.0 replay_ms=1637.5 audit_ms=4777.8 finish_ms=59.2 ckpt_bytes=3160240128 replay_bytes=442593280 replay_frames=28586 audit_bytes=1704890368 audit_valid_frames=0 audit_foreign_frames=51974 stale_files=4 residue_slacks=4 residue_stops=4]
rep1 arm warm_ops=84572 tail_ops=291513 warmed=true warm[rotations=16 recycled=4 truncated=16 rotations_min_cell=4 waits_expired=8] end[rotations=16 recycled=4 truncated=16 frame_bytes=3769974784 zero_fill=4294967296] boot_wall_s=3.642 proc_read_bytes=5307330560 records_recovered=3200000 slowest_cell[total_ms=3591.5 start_ms=3.9 ckpt_ms=2072.0 replay_ms=453.2 audit_ms=1062.3 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=14344.2 ckpt_ms=8263.3 replay_ms=1840.1 audit_ms=4225.5 finish_ms=0.5 ckpt_bytes=3160240128 replay_bytes=474947584 replay_frames=30088 audit_bytes=1672536064 audit_valid_frames=0 audit_foreign_frames=52107 stale_files=4 residue_slacks=4 residue_stops=4]
rep1 base warm_ops=97051 tail_ops=206942 warmed=true warm[rotations=16 recycled=0 truncated=16 rotations_min_cell=4 waits_expired=0] end[rotations=16 recycled=0 truncated=16 frame_bytes=3765702656 zero_fill=5319163904] boot_wall_s=3.573 proc_read_bytes=5257785344 records_recovered=3200000 slowest_cell[total_ms=3543.1 start_ms=3.7 ckpt_ms=2056.2 replay_ms=439.7 audit_ms=1043.4 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=14047.5 ckpt_ms=8226.8 replay_ms=1724.6 audit_ms=4080.2 finish_ms=0.2 ckpt_bytes=3160240128 replay_bytes=478363648 replay_frames=29394 audit_bytes=1669120000 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep2 base warm_ops=133404 tail_ops=35342 warmed=true warm[rotations=16 recycled=0 truncated=16 rotations_min_cell=4 waits_expired=0] end[rotations=16 recycled=0 truncated=16 frame_bytes=3768512512 zero_fill=5270405120] boot_wall_s=4.057 proc_read_bytes=5209026560 records_recovered=3200000 slowest_cell[total_ms=3906.0 start_ms=4.2 ckpt_ms=2087.5 replay_ms=429.6 audit_ms=1384.7 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=14612.7 ckpt_ms=8364.3 replay_ms=1674.5 audit_ms=4556.4 finish_ms=0.4 ckpt_bytes=3160240128 replay_bytes=453943296 replay_frames=29286 audit_bytes=1693540352 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep2 arm warm_ops=129966 tail_ops=286810 warmed=true warm[rotations=16 recycled=4 truncated=16 rotations_min_cell=4 waits_expired=8] end[rotations=16 recycled=4 truncated=16 frame_bytes=3767037952 zero_fill=4294967296] boot_wall_s=3.576 proc_read_bytes=5307330560 records_recovered=3200000 slowest_cell[total_ms=3547.1 start_ms=4.1 ckpt_ms=2120.4 replay_ms=330.0 audit_ms=1092.4 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=14137.9 ckpt_ms=8472.9 replay_ms=1331.3 audit_ms=4317.3 finish_ms=0.5 ckpt_bytes=3160240128 replay_bytes=446435328 replay_frames=28657 audit_bytes=1701048320 audit_valid_frames=0 audit_foreign_frames=51913 stale_files=4 residue_slacks=4 residue_stops=4]
```
