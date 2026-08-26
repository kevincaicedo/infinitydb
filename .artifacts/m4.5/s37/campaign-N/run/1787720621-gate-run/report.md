# M4.5 gate-run report

date: 1787720621 (unix) · binary target/release/infinityd · cells 4 · 3 replicates · S37 cold-read-qd discriminator row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s37/data-N (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off · flush-group-window-us 0 (off) · device-probe off
- --only-s37: every other row was skipped; their gate keys are absent
- s37 row (step 2 discriminator): 4 cells · 3 replicates (arm order rotated per replicate) · tiered always, MEM-BUDGET 128mb/cell, 1000000 keys × 1 KiB filled per leg, then 100 % SET closed-loop pipeline 1 at 64 and 256 conns for 20 s · arms = COLD-READ-QD qd64, qd128, qd256 on the shipping binary (baseline first; ADR-0055 D2 cap, pool = qd × 16 KiB per cell)
- s37 c64 qd128 vs qd64: ops 1.801 × · p50 0.950 × · p99 0.263 × · cold_read_qd_p99 7 (base 8) · cold_read_p99_us 1951 · queue_full 0 · pool_dry 0 (medians of 3 pairs)
- s37 c64 qd256 vs qd64: ops 1.794 × · p50 0.969 × · p99 0.441 × · cold_read_qd_p99 7 (base 8) · cold_read_p99_us 21503 · queue_full 0 · pool_dry 0 (medians of 3 pairs)
- s37 c256 qd128 vs qd64: ops 0.977 × · p50 0.979 × · p99 0.991 × · cold_read_qd_p99 8 (base 8) · cold_read_p99_us 2015 · queue_full 0 · pool_dry 0 (medians of 3 pairs)
- s37 c256 qd256 vs qd64: ops 1.010 × · p50 0.983 × · p99 1.020 × · cold_read_qd_p99 8 (base 8) · cold_read_p99_us 2431 · queue_full 0 · pool_dry 0 (medians of 3 pairs)

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
| S37 step 2 discriminator: widest COLD-READ-QD arm throughput over the baseline cap at 256 conns | >= 1 x (arm ÷ baseline ops/s on the beyond-RAM tiered always write leg, per-replicate median; the ledger reads it against the ceiling's gap) | 1.01 | PASS (informational) |
| S37 step 2 discriminator: widest COLD-READ-QD arm p99 over the baseline cap at 256 conns (the tail the wider cap charges) | <= 1.1 x (arm ÷ baseline p99, per-replicate median) | 1.02 | PASS (informational) |
| S37 step 2 discriminator: the baseline's device QD p99 at issue (≈ the cap = the cap bound; validity of the discriminator) | >= 1 device reads in flight at the p99 issue (INFO cold_read_qd_p99, worst cell, whole-session histogram — carries the c64 leg's samples) | 8.00 | PASS (informational) |
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
| cold-read-qd-discriminator | S37 step 2 discriminator (plan S37, 2026-08-23): each wider COLD-READ-QD arm over the baseline cap on the beyond-RAM tiered always write legs — `qd_wide_ops_x` against the ceiling's gap decides queueing (shaper) vs the read (shadow slots); `qd_base_cold_qd_p99` ≈ the cap means the cap bound; a p99 ratio above 1.1 is a tail cost the wider cap charges; not a write-amplification row |

## s37 per-leg samples

```
rep0 qd64  c64  ops/s=15352    p50_us=2927   p99_us=26303   p999_us=31359   sets=307098 cold_resolves=299108 (0.974/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=15615 cold_queue_full=0 cold_pool_dry=0
rep0 qd64  c256 ops/s=10560    p50_us=10847  p99_us=146943  p999_us=238079  sets=211444 cold_resolves=195011 (0.922/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=22527 cold_queue_full=0 cold_pool_dry=0
rep0 qd128 c64  ops/s=13048    p50_us=2967   p99_us=31423   p999_us=102143  sets=261103 cold_resolves=257885 (0.988/set) blind=0 (0.000/set) cold_qd_p99=7 cold_read_p99_us=18431 cold_queue_full=0 cold_pool_dry=0
rep0 qd128 c256 ops/s=18209    p50_us=9887   p99_us=63615   p999_us=149503  sets=364583 cold_resolves=338665 (0.929/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=15871 cold_queue_full=0 cold_pool_dry=0
rep0 qd256 c64  ops/s=10633    p50_us=3031   p99_us=33919   p999_us=39295   sets=212695 cold_resolves=216503 (1.018/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=23551 cold_queue_full=0 cold_pool_dry=0
rep0 qd256 c256 ops/s=22022    p50_us=9183   p99_us=38655   p999_us=46207   sets=440960 cold_resolves=420515 (0.954/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=10495 cold_queue_full=0 cold_pool_dry=0
rep1 qd128 c64  ops/s=19122    p50_us=2887   p99_us=8863    p999_us=11967   sets=382510 cold_resolves=363979 (0.952/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=1951 cold_queue_full=0 cold_pool_dry=0
rep1 qd128 c256 ops/s=21113    p50_us=8703   p99_us=42623   p999_us=53759   sets=422792 cold_resolves=435361 (1.030/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=2015 cold_queue_full=0 cold_pool_dry=0
rep1 qd256 c64  ops/s=19045    p50_us=2911   p99_us=8639    p999_us=11487   sets=381020 cold_resolves=363860 (0.955/set) blind=0 (0.000/set) cold_qd_p99=7 cold_read_p99_us=1983 cold_queue_full=0 cold_pool_dry=0
rep1 qd256 c256 ops/s=18731    p50_us=9439   p99_us=74239   p999_us=160255  sets=375471 cold_resolves=384037 (1.023/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=2111 cold_queue_full=0 cold_pool_dry=0
rep1 qd64  c64  ops/s=10617    p50_us=3039   p99_us=33663   p999_us=46847   sets=212375 cold_resolves=217561 (1.024/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=23551 cold_queue_full=0 cold_pool_dry=0
rep1 qd64  c256 ops/s=21760    p50_us=8831   p99_us=43007   p999_us=52223   sets=435734 cold_resolves=413770 (0.950/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=12799 cold_queue_full=0 cold_pool_dry=0
rep2 qd256 c64  ops/s=13454    p50_us=3023   p99_us=30143   p999_us=36991   sets=269136 cold_resolves=254226 (0.945/set) blind=0 (0.000/set) cold_qd_p99=7 cold_read_p99_us=21503 cold_queue_full=0 cold_pool_dry=0
rep2 qd256 c256 ops/s=21816    p50_us=9151   p99_us=38783   p999_us=46719   sets=436867 cold_resolves=417188 (0.955/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=2431 cold_queue_full=0 cold_pool_dry=0
rep2 qd64  c64  ops/s=7305     p50_us=3119   p99_us=68351   p999_us=136191  sets=146132 cold_resolves=147307 (1.008/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=58367 cold_queue_full=0 cold_pool_dry=0
rep2 qd64  c256 ops/s=21595    p50_us=9311   p99_us=38015   p999_us=46079   sets=432434 cold_resolves=411089 (0.951/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=14591 cold_queue_full=0 cold_pool_dry=0
rep2 qd128 c64  ops/s=19041    p50_us=2911   p99_us=8799    p999_us=12127   sets=380927 cold_resolves=363906 (0.955/set) blind=0 (0.000/set) cold_qd_p99=7 cold_read_p99_us=1823 cold_queue_full=0 cold_pool_dry=0
rep2 qd128 c256 ops/s=21096    p50_us=9119   p99_us=40319   p999_us=48895   sets=422546 cold_resolves=435741 (1.031/set) blind=0 (0.000/set) cold_qd_p99=8 cold_read_p99_us=1855 cold_queue_full=0 cold_pool_dry=0
```
