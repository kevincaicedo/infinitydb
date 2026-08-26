# M4.5 gate-run report

date: 1787458775 (unix) · binary target/release-diag/infinityd (bench-diagnostics) · cells 4 · 3 replicates · S37 ceiling row only · frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off
env-check: OK
tier: reference-box (binding)

notes:
- data root: /home/kcaicedo/bench-data/s37/data-J (ext4)
- durable arms: frames-in-flight auto (fua 3 / flush 1) · barrier-class flush · staging-mib 4 · device-write-mbps probe-file · seal-pace off
- --only-s37: every other row was skipped; their gate keys are absent
- s37 row: 4 cells · 3 replicates (ABBA) · tiered always, MEM-BUDGET 128mb/cell, 1000000 keys × 1 KiB filled per leg, then 100 % SET closed-loop pipeline 1 at 64 and 256 conns for 20 s · arm B = --blind-overwrite-ceiling (unsound ceiling instrument; bench-diagnostics build)

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
| S37 step 1: blind-overwrite ceiling throughput over the shipping path at 256 conns | >= 1.15 x (B ÷ A ops/s on the beyond-RAM tiered always write leg, per-replicate median; B unsound) | 4.58 | PASS (informational) |
| S37 step 1: blind-overwrite ceiling p99 gain at 256 conns | >= 1.2 x (A ÷ B p99, per-replicate median; B unsound) | 1.45 | PASS (informational) |
| S37 step 1: share of arm B's SETs that skipped a cold read (validity — 0 = the instrument never engaged) | >= 0.1 share (blind_overwrites_ceiling ÷ SETs, arm B at 256 conns, per-replicate median) | 1.01 | PASS |

## write amplification by row

Per namespace, worst first — never a node-wide blend (M4-S16).

| row | write amplification |
|---|---|
| cold-overwrite-ceiling | S37 step 1 (plan rule): B ÷ A throughput and A ÷ B p99 on the beyond-RAM tiered always write legs; B is an UNSOUND upper bound (the cold record is orphaned) — "< 15 % throughput and < 20 % p99 ⇒ step 2 Rejected"; `blind_share_arm_b` is the share of B's SETs that skipped a cold read (0 = the instrument never engaged), `cold_resolve_share_arm_a` the share of A's SETs that paid one |

## s37 per-leg samples

```
rep0 A c64  ops/s=12429    p50_us=2967   p99_us=29119   p999_us=34943   sets=248644 cold_resolves=710383 (2.857/set) blind=0 (0.000/set)
rep0 A c256 ops/s=21975    p50_us=8927   p99_us=38527   p999_us=46207   sets=439950 cold_resolves=1254355 (2.851/set) blind=0 (0.000/set)
rep0 B c64  ops/s=38666    p50_us=791    p99_us=23807   p999_us=30655   sets=773372 cold_resolves=1267330 (1.639/set) blind=779403 (1.008/set)
rep0 B c256 ops/s=90994    p50_us=1343   p99_us=27583   p999_us=55295   sets=1825973 cold_resolves=6835654 (3.744/set) blind=1871197 (1.025/set)
rep1 B c64  ops/s=39654    p50_us=799    p99_us=23615   p999_us=28735   sets=793127 cold_resolves=1301746 (1.641/set) blind=795106 (1.002/set)
rep1 B c256 ops/s=100235   p50_us=1339   p99_us=25983   p999_us=31103   sets=2005074 cold_resolves=7610804 (3.796/set) blind=2016716 (1.006/set)
rep1 A c64  ops/s=18565    p50_us=2895   p99_us=9119    p999_us=12191   sets=371366 cold_resolves=1062420 (2.861/set) blind=0 (0.000/set)
rep1 A c256 ops/s=13723    p50_us=11423  p99_us=72447   p999_us=88831   sets=275159 cold_resolves=799840 (2.907/set) blind=0 (0.000/set)
rep2 A c64  ops/s=10666    p50_us=3071   p99_us=33279   p999_us=43519   sets=213355 cold_resolves=655909 (3.074/set) blind=0 (0.000/set)
rep2 A c256 ops/s=22042    p50_us=8607   p99_us=37759   p999_us=43007   sets=441459 cold_resolves=1259787 (2.854/set) blind=0 (0.000/set)
rep2 B c64  ops/s=23092    p50_us=841    p99_us=59391   p999_us=80383   sets=462045 cold_resolves=609484 (1.319/set) blind=436787 (0.945/set)
rep2 B c256 ops/s=100917   p50_us=1323   p99_us=25983   p999_us=32319   sets=2018830 cold_resolves=6653492 (3.296/set) blind=2036163 (1.009/set)
```
