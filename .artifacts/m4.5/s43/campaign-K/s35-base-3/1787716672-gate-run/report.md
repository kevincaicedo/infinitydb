# M4.5 gate-run report

date: 1787716672 (unix) · binary target/release/infinityd · cells 4 · 1 replicates · S35 row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 · device-probe off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s43/data (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 · device-probe off
- --only-s35: the S29 and S27 rows were skipped; their gate keys are absent
- s35 4c/1c per-replicate ratios (median of 1; spread): barrier 1.955–1.955 · client p50 1.861–1.861 — client histogram 256 sub-buckets/octave (≈ 0.4 %, 2 µs at 512–1024 µs); the barrier p50 is the server's 32-sub-bucket histogram (≈ 3 %, 16 µs at that octave)
- s35: 2 durable leg(s) saw a device barrier p99 > 10 ms (the S34 drive-state bad mode) — a device row, not an engine row; re-run with fstrim + a longer --leg-idle-s before citing
- s35 row: flat always, no fill (the AC leg runs first on a fresh server so its barrier histogram holds only its own frames), 200000-key space × 1 KiB, 10s legs, median of 1; AC leg 32 conns pipeline 1 on 4 cells then on 1 cell (interleaved per replicate); max leg 256 conns; read leg 64 conns × P16 100% GET over the keys the write legs populated (nils disclosed); 40s idle before every durable leg; barrier = fsync_latency_p50_us (cell median, whole-session histogram); device tail = fsync_latency_p99_us (worst cell); 4c/1c ratios are medians of per-replicate ratios — the barrier ratio binds (F2's contention term), the client ratio is informational (ADR-0087 D8, amended 2026-08-22)

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
| S35: N-cell vs 1-cell barrier p50 ratio (device characterization — informational since the ADR-0087 fourth amendment, 2026-08-22) | <= 1.3 x (N-cell barrier p50 / 1-cell barrier p50 at 32 conns, per-replicate median; FLUSH read ~1.8, FUA read 1.35–1.54 at every K on 08-22 — qd4 vs qd1 on the device) | 1.96 | FAIL (informational) |
| S35: N-cell vs 1-cell always client p50 ratio (informational since 2026-08-22 — carries the pipeline's seal wait at K ≥ 2) | <= 1.3 x (N-cell p50 / 1-cell p50 at 32 conns, per-replicate median on the 0.4 % client histogram; FLUSH read 1.8, FUA K = 1 1.45, K = 3 1.25–1.38 on the 3 % instrument) | 1.86 | FAIL (informational) |
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
| S39d: fixed-work first-boot recovery, slowest cell's engine total, arm over baseline | <= 1.05 x (recover_total_us on the slowest cell, arm ÷ baseline, per-replicate median; diagnostic — the per-phase ratios name the term) | — | PENDING (tooling) |
| S39d: absolute first-boot recovery wall on the recycled log (the S18 < 15 s STOP gate re-read) | <= 15 s (process launch to loading:0 on every cell, arm, per-replicate median; the dataset is the row's fixed work, disclosed in the report) | — | PENDING (tooling) |
| S39d: both arms recovered exactly the records written (fixed-work validity) | >= 1 1 = every replicate pair recovered warm + tail records on both arms, 0 = a pair did not (the row is then not fixed-work) | — | PENDING (tooling) |
| S39d: slack-audit phase time over the cell sums, arm over baseline (the recycling term, informational) | <= 1.5 x (Σcells recover_audit_us arm ÷ baseline, per-replicate median; residue is decoded and CRC-validated where zeros are skipped word-wise) | — | PENDING (tooling) |
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
| S39d: replay-phase rate on the slowest cell, arm (C38b's statistic on the loop clock, this row's shape — informational) | >= 1 GB/s per cell (recover_replay_bytes ÷ recover_replay_us on the slowest cell, per-replicate median) | — | PENDING (tooling) |
| S39d: replay-phase rate on the slowest cell, baseline (informational) | >= 1 GB/s per cell (same statistic, baseline arm) | — | PENDING (tooling) |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| frame-pipeline | not measured by this row — the S35 row gates the pipeline's latency shape; the class's padding/zero-fill disclosures are INFO counters (log_padding_bytes, zero_fill_bytes) and S36 owns write_amp_log_ckpt |

## s35 per-leg samples

```
rep0 4c c32  ops/s=4705     p50_us=5119   mean_us=6797   p99_us=28991   max_us=36512    barrier_p50_us=2751  barrier_p99_us=25087  p50/barrier=1.86 frames_in_flight_max=1 acks/fsync=4.3 frames=44465 parked=0 write_stall_p99_us=895 padding_pct=0.0 waits_fill=0 waits_group=0 round_target=15
rep0 4c c256 ops/s=52342    p50_us=5007   mean_us=4887   p99_us=7327    max_us=16847    barrier_p50_us=2687  barrier_p99_us=19967  frames_in_flight_max=1 acks/fsync=35.5 frames=103022 parked=0 write_stall_p99_us=639 padding_pct=0.0 waits_fill=0 waits_group=0 round_target=45
rep0 4c read c64 P16 ops/s=1590512  p50_us=585    p99_us=1131    p999_us=1331    nils=693632
rep0 1c c32  ops/s=11468    p50_us=2751   mean_us=2789   p99_us=4071    max_us=12485    barrier_p50_us=1407  barrier_p99_us=2175   p50/barrier=1.96 frames_in_flight_max=1 acks/fsync=16.0 frames=12460 parked=0 write_stall_p99_us=87 padding_pct=0.0 waits_fill=0 waits_group=0 round_target=3 4c/1c: p50=1.861 barrier=1.955
```
