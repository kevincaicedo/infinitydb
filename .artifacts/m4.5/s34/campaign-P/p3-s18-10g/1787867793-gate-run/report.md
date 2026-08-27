# M4.5 gate-run report

date: 1787867793 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S39d row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s34/data-P (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class fua · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off
- --only-s39d: the S29, S27, S35, S36 and S39b rows were skipped; their gate keys are absent
- s39d --s39d-baseline flush-class: the baseline log is packed (no 4 KiB frame alignment), so `frame_bytes_x` reads above 1.0 by the arm's padding share by design — a format difference, not a fixed-work violation; the replay-phase keys compare the same records from both formats
- s39d row: 4 cells · 3 replicates (ABBA) · fixed work: 10000000 warm + 200000 tail records × 1 KiB at 32 conns pipeline 16 · segment-bytes 268435456 · ckpt floor 268435456 · boundary = INF.CKPT WAIT after the warm fill, truncation settled 2 s · SIGKILL after the tail's last ack · 40 s idle · cold boot (every file of the image sync + fadvise DONTNEED via `inf cache-evict` after the idle, both arms — the power-loss shape) · one boot · arm (server default) vs baseline --barrier-class flush · device stat nvme0n1
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
| S39d: fixed-work first-boot recovery, slowest cell's engine total, arm over baseline | <= 1.05 x (recover_total_us on the slowest cell, arm ÷ baseline, per-replicate median; diagnostic — the per-phase ratios name the term) | 1.09 | FAIL (informational) |
| S39d: absolute first-boot recovery wall on the recycled log (the S18 < 15 s STOP gate re-read) | <= 15 s (process launch to loading:0 on every cell, arm, per-replicate median; the dataset is the row's fixed work, disclosed in the report) | 8.38 | PASS |
| S39d: both arms recovered exactly the records written (fixed-work validity) | >= 1 1 = every replicate pair recovered warm + tail records on both arms, 0 = a pair did not (the row is then not fixed-work) | 1.00 | PASS |
| S39d: slack-audit phase time over the cell sums, arm over baseline (the recycling term, informational) | <= 1.5 x (Σcells recover_audit_us arm ÷ baseline, per-replicate median; residue is decoded and CRC-validated where zeros are skipped word-wise) | 17.24 | FAIL (informational) |
| S40: everysec max latency at the memtier shape, worst leg (S27 D5) | <= 50 ms (client max from the intended send instant, 32 conns pipeline 1 at 100 k offered over 1 M keys, worst of --replicates legs) | — | PENDING (tooling) |
| S40: achieved over offered rate, worst leg (validity — a saturated leg's max is a saturation number) | >= 0.9 x (achieved ops/s ÷ offered, min over legs) | — | PENDING (tooling) |
| S40: seconds whose per-second maximum exceeded 50 ms, summed over legs (informational) | <= 0 seconds (per-second client maxima; an isolated event reads 1, a regime reads many) | — | PENDING (tooling) |
| S37 step 1: blind-overwrite ceiling throughput over the shipping path at 256 conns | >= 1.15 x (B ÷ A ops/s on the beyond-RAM tiered always write leg, per-replicate median; B unsound) | — | PENDING (tooling) |
| S37 step 1: blind-overwrite ceiling p99 gain at 256 conns | >= 1.2 x (A ÷ B p99, per-replicate median; B unsound) | — | PENDING (tooling) |
| S37 step 1: share of arm B's SETs that skipped a cold read (validity — 0 = the instrument never engaged) | >= 0.1 share (blind_overwrites_ceiling ÷ SETs, arm B at 256 conns, per-replicate median) | — | PENDING (tooling) |
| S37 step 2 discriminator: widest COLD-READ-QD arm throughput over the baseline cap at 256 conns | >= 1 x (arm ÷ baseline ops/s on the beyond-RAM tiered always write leg, per-replicate median; the ledger reads it against the ceiling's gap) | — | PENDING (tooling) |
| S37 step 2 discriminator: widest COLD-READ-QD arm p99 over the baseline cap at 256 conns (the tail the wider cap charges) | <= 1.1 x (arm ÷ baseline p99, per-replicate median) | — | PENDING (tooling) |
| S37 step 2 discriminator: the baseline's device QD p99 at issue (≈ the cap = the cap bound; validity of the discriminator) | >= 1 device reads in flight at the p99 issue (INFO cold_read_qd_p99, worst cell, whole-session histogram — carries the c64 leg's samples) | — | PENDING (tooling) |
| S42: first boot over second boot of a fresh data directory (the in-boot probe's cost) | <= 15 s (spawn to first accepted connection, first − second, per-replicate median) | — | PENDING (tooling) |
| S42: every first boot reported io_properties_source=probed-at-boot on every cell (validity) | >= 1 1 = every replicate, 0 = a replicate did not | — | PENDING (tooling) |
| S42: every second boot reported io_properties_source=file with the identity verified (validity) | >= 1 1 = every replicate, 0 = a replicate did not | — | PENDING (tooling) |
| S42: the file the first boot wrote is schema 3 (identity-bound) | >= 3 io_properties_schema (INFO) | — | PENDING (tooling) |
| S42: always p50 over the barrier p50 at 32 conns on the directory the first boot probed (S35's bar on the stock boot) | <= 1.3 x (client p50 / barrier p50 of the class the probe chose; S35's bar) | — | PENDING (tooling) |
| S42: always c32 ops/s on the stock boot (informational — read against the probed arm's replicate spread) | >= 0 ops/s (32 conns closed loop, per-replicate median) | — | PENDING (tooling) |
| S39d: replay-phase rate on the slowest cell, arm (C38b's statistic on the loop clock, this row's shape — informational) | >= 1 GB/s per cell (recover_replay_bytes ÷ recover_replay_us on the slowest cell, per-replicate median) | 0.35 | FAIL (informational) |
| S39d: replay-phase rate on the slowest cell, baseline (informational) | >= 1 GB/s per cell (same statistic, baseline arm) | 0.37 | FAIL (informational) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| recovery-attribution | fixed-work recovery (ADR-0090 A10): `recovery_total_x` is the slowest cell's engine total (first step to completion on the loop clock) arm ÷ baseline; `recovery_wall_x` the harness wall from process launch to loading:0; `phase_*_x` the per-phase work ratio over the cell sums; `records_recovered_match` = 1 when both arms recovered exactly the records written; `frame_bytes_x` discloses the encoded-bytes parity the group formation allows |

## s39d per-leg samples

```
rep0 base warm_ops=47785 tail_ops=48418 warmed=true warm[rotations=36 recycled=0 truncated=36 rotations_min_cell=9 waits_expired=0] end[rotations=36 recycled=0 truncated=36 frame_bytes=10691616804 zero_fill=0] cold_boot=18files/12684863904B boot_wall_s=7.762 proc_read_bytes=11565846528 records_recovered=10200000 slowest_cell[total_ms=7730.6 start_ms=16.6 ckpt_ms=6930.7 replay_ms=731.9 audit_ms=51.4 finish_ms=0.0 dominating=ckpt] sum_cells[total_ms=30877.4 ckpt_ms=27640.7 replay_ms=2959.1 audit_ms=211.2 finish_ms=0.1 ckpt_bytes=10537377792 replay_bytes=1028421272 replay_frames=57062 audit_bytes=1119062376 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep0 arm warm_ops=80102 tail_ops=141275 warmed=true warm[rotations=49 recycled=4 truncated=49 rotations_min_cell=12 waits_expired=35] end[rotations=49 recycled=4 truncated=49 frame_bytes=12012441600 zero_fill=12664176640] cold_boot=22files/13758605728B boot_wall_s=8.343 proc_read_bytes=12684902400 records_recovered=10200000 slowest_cell[total_ms=8303.2 start_ms=16.6 ckpt_ms=6835.9 replay_ms=705.6 audit_ms=744.9 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=33174.7 ckpt_ms=27321.6 replay_ms=2778.6 audit_ms=3008.2 finish_ms=0.5 ckpt_bytes=10537377792 replay_bytes=985620480 replay_frames=66964 audit_bytes=1161863168 audit_valid_frames=0 audit_foreign_frames=0 stale_files=4 residue_slacks=0 residue_stops=0]
rep1 arm warm_ops=67098 tail_ops=18081 warmed=true warm[rotations=52 recycled=4 truncated=52 rotations_min_cell=13 waits_expired=36] end[rotations=52 recycled=4 truncated=52 frame_bytes=12005625856 zero_fill=13327663104] cold_boot=22files/13758605728B boot_wall_s=8.384 proc_read_bytes=12664455168 records_recovered=10200000 slowest_cell[total_ms=8351.9 start_ms=16.5 ckpt_ms=6875.9 replay_ms=542.6 audit_ms=916.7 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=33358.1 ckpt_ms=27506.2 replay_ms=2161.4 audit_ms=3625.3 finish_ms=0.6 ckpt_bytes=10537377792 replay_bytes=761200640 replay_frames=55796 audit_bytes=1386283008 audit_valid_frames=0 audit_foreign_frames=0 stale_files=4 residue_slacks=0 residue_stops=0]
rep1 base warm_ops=51242 tail_ops=37510 warmed=true warm[rotations=36 recycled=0 truncated=36 rotations_min_cell=9 waits_expired=0] end[rotations=36 recycled=0 truncated=36 frame_bytes=10690689460 zero_fill=0] cold_boot=18files/12684863904B boot_wall_s=7.691 proc_read_bytes=11564986368 records_recovered=10200000 slowest_cell[total_ms=7657.9 start_ms=16.8 ckpt_ms=6889.1 replay_ms=699.3 audit_ms=52.6 finish_ms=0.0 dominating=ckpt] sum_cells[total_ms=30622.6 ckpt_ms=27520.0 replay_ms=2825.9 audit_ms=210.3 finish_ms=0.1 ckpt_bytes=10537377792 replay_bytes=1027557540 replay_frames=52884 audit_bytes=1119926108 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep2 base warm_ops=48792 tail_ops=39908 warmed=true warm[rotations=36 recycled=0 truncated=36 rotations_min_cell=9 waits_expired=0] end[rotations=36 recycled=0 truncated=36 frame_bytes=10690590372 zero_fill=0] cold_boot=18files/12684863904B boot_wall_s=7.617 proc_read_bytes=11564941312 records_recovered=10200000 slowest_cell[total_ms=7584.8 start_ms=16.7 ckpt_ms=6821.8 replay_ms=695.6 audit_ms=50.6 finish_ms=0.1 dominating=ckpt] sum_cells[total_ms=30293.1 ckpt_ms=27187.5 replay_ms=2830.4 audit_ms=208.0 finish_ms=0.2 ckpt_bytes=10537377792 replay_bytes=1027512398 replay_frames=55210 audit_bytes=1119971250 audit_valid_frames=0 audit_foreign_frames=0 stale_files=0 residue_slacks=0 residue_stops=0]
rep2 arm warm_ops=75486 tail_ops=245194 warmed=true warm[rotations=51 recycled=4 truncated=51 rotations_min_cell=12 waits_expired=40] end[rotations=52 recycled=5 truncated=51 frame_bytes=12012367872 zero_fill=13433831424] cold_boot=22files/13758605728B boot_wall_s=9.017 proc_read_bytes=12953337856 records_recovered=10200000 slowest_cell[total_ms=8984.6 start_ms=17.2 ckpt_ms=6860.6 replay_ms=891.9 audit_ms=1214.8 finish_ms=0.2 dominating=ckpt] sum_cells[total_ms=34223.4 ckpt_ms=27415.3 replay_ms=2790.4 audit_ms=3950.1 finish_ms=0.6 ckpt_bytes=10537377792 replay_bytes=945938432 replay_frames=66014 audit_bytes=1469980672 audit_valid_frames=0 audit_foreign_frames=13405 stale_files=3 residue_slacks=1 residue_stops=1]
```
