# S39b campaign G — ADR-0090 D9: the bounded pool wait, A/B against the one-slot immediate arm

Rules written before the run (ADR-0090 A9, 2026-08-22).

## What is compared

Both arms run one-slot recycling. The only difference is the prealloc
policy when the pool is empty at rotation:

- **baseline (A):** `--segment-recycle-slots 1 --recycle-wait off` — the
  immediate fresh prealloc, campaign F's arm byte-for-byte.
- **arm (B):** `--segment-recycle-slots 1 --recycle-wait quarter` — the
  MAINTAIN prealloc re-checks the pool each slice until the active segment
  is a quarter full, then falls back to a fresh segment paced from its own
  origin.

Recycling-off is **not** re-run: its effect is measured (campaign F: host
bytes/log byte 2.20 → 1.42, p99 0.20×, padding untouched, every correctness
row green).

Row: `gate-run m4.5 --only-s39b --s39b-baseline wait-off --recycle-wait
quarter`, the campaign-F shape — reference box, 4 cells pinned from core 0,
`--barrier-class fua`, K auto (3 × 4 MiB), fill on, 256 MiB segments at the
256 MiB checkpoint floor, 150 s write legs at 32 conns closed-loop `always`,
40 s drive-state idle before every leg and before the single first-boot
recovery, three interleaved replicates (ABBA), `/sys/block/nvme0n1/stat`
sampled. Bench loadgen pinned to cores 8,10,12,14.

## Hypothesis (ADR-0090 D9 / A9)

- warmed `Δzero_fill / Δlog_frame_bytes` on B **≤ 0.10** (A read
  0.216–0.234 in campaign F);
- waits end predominantly **satisfied** (`s39b:wait_satisfied_share_arm
  ≥ 0.5`);
- host bytes/log byte on B ≈ 1.30 (A ≈ 1.42; recycling-off 2.20);
- `rotations_unzeroed = 0`, `inline_preallocs = 0`, `prealloc_failures = 0`
  on B in every replicate;
- p50 ≤ 1.05×, p99 ≤ 1.1×, reads ≥ 0.98×, padding within 3 pt of A;
- accounted host bytes agree with device sectors as in F (0.3 %).

## Decision rule (no third branch)

- any correctness row red (the same-commit sweeps in `../d9/sweeps/`), or
  `rotations_unzeroed` > 0 on B → **D9 Rejected** (`--recycle-wait` default
  returns to `off`); the one-slot default stands on campaign F's evidence;
- share ≤ 0.10 → **D9 Accepted**, one-slot default confirmed;
- 0.10 < share but satisfied waits dominate and no latency/read/padding
  control fails → **D9 Revised** (number `Evidence-pending`, mechanism
  kept), one-slot default confirmed;
- share inside A's spread (no measurable improvement) → **D9 Rejected**
  (default `off`), one-slot default confirmed on the same grounds.

Recovery (one first boot per fresh crashed image) stays on the row as a
control and is **not** a decision input: its attribution is S39d's
(ADR-0090 A10). The `s39b_recovery_time_x` gate may read red here without
moving the decision — it compares B to A, neither of which is recycling-off.

## Disclosures

- No `fstrim` (no sudo in the agent shell); the drive carries the day's
  earlier campaigns. Same for both arms by interleaving.
- The `/sys/block` counter is host sectors written, not NAND wear.
- The S39b gates `s39b_warmed_zero_fill_share` (≤ 0.10) and
  `s39b_recycle_deficit_per_cell` are evaluated on B as written; the
  `s39b_*_x` ratio gates compare B against A, not against recycling-off.
