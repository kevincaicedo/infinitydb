# M4-S11 flush bandwidth — ≥ 0.8× device sequential-write gate (§4.1)

date: 2026-07-18 · box: HomeLab reference box (i7-13700KF, ADATA LEGEND
700 Gen3 DRAM-less NVMe — §19 device deviation disclosed, ADR-0022 D4
class) · kernel 7.0.0-28 · governor/EPP performance, turbo off ·
`taskset -c 4` · ext4, INF_BENCH_DIR=$HOME/.cache/inf-bench-tier ·
mode Direct (ADR-0054 default) · fdatasync every 64 MiB both legs ·
git 147c33a + S11 tree (bench binary from the S11 tree)

Command: `INF_BENCH_DIR=$HOME/.cache/inf-bench-tier taskset -c 4 cargo
bench -p inf-log --bench tier_flush`

## Run 2 (batched writer — TIER_BATCH_FRAMES=256, 1 MiB device writes; 512 MiB/leg, ABBA ×3)

    pipeline rep 0: 134 MiB/s data (1 files sealed)
  rep 0: raw 137 MiB/s | pipeline 134 MiB/s | ratio 0.981
    pipeline rep 1: 542 MiB/s data (1 files sealed)
  rep 1: raw 1181 MiB/s | pipeline 542 MiB/s | ratio 0.459
    pipeline rep 2: 1201 MiB/s data (1 files sealed)
  rep 2: raw 1189 MiB/s | pipeline 1201 MiB/s | ratio 1.011
median pipeline/raw ratio: 0.981 (gate: >= 0.8x) → PASS

Reading: reps 0 and 2 pair legs under comparable device states (both
exhausted / both fresh) and read 0.98–1.01×; rep 1's legs straddle a
sustained-write collapse transition (the device's known DRAM-less
behavior) — the ratio's denominator and numerator saw different devices.
Median PASSES; the pipeline is device-bound (1201 MiB/s where raw reads
1189), not structure-bound.

## Run 1 (frame-granular writer — the losing A/B, recorded per L4, not merged)

1 GiB/leg, ABBA ×3, same box/mode/cadence, pre-batching TierWriter
(one 4 KiB pwrite per frame):

  rep 0: raw 1165 MiB/s | pipeline 264 MiB/s | ratio 0.227
  rep 1: raw  466 MiB/s | pipeline 261 MiB/s | ratio 0.560
  rep 2: raw  142 MiB/s | pipeline 264 MiB/s | ratio 1.855
  median 0.560 → FAIL (pipeline pinned at ~262 MiB/s = syscall/QD1-bound)

Disposition: the flat ~262 MiB/s across wildly varying device states is
the signature of a structure-bound pipeline — one syscall per 4 KiB
frame. Fixed by batching full frames into one aligned 1 MiB device write
(`TIER_BATCH_FRAMES`); the batched shape is what shipped.

Dev-tier caveat: single-box, device deviation disclosed; the S24
campaign re-reads this row with the campaign validity bundle.
