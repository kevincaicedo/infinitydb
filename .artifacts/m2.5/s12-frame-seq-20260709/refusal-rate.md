# M2.5-S12 — taxonomy-refusal availability, before/after frame format v2

- **Date:** 2026-07-09 · **Box:** HomeLab dev box (i7-13700KF, kernel
  7.0.0-27) — CPU-only deterministic sim, tier-independent (L7: the sweep
  replays byte-identically anywhere).
- **Instrument:** `inf-sim --scenario m2-durable --sweep 10000 --seed
  0xD5EE0000 --shard i/8` (the `just durable-sweep` shape; identical seed
  base both arms — a paired A/B over the same 10,000 power-cut universes).
- **Tree:** baseline = pre-S12 working tree (S14 stall device active);
  post = frame format v2 (ADR-0031) with the stamp taxonomy.

| Arm | Seeds | Oracle violations | Taxonomy refusals | Rate |
|-----|-------|-------------------|-------------------|------|
| Baseline (frame v1 policy) | 10,000 | 0 | **231** | **2.31%** |
| Post-S12, first cut (`sweep-post/`) | 10,000 | 0 | 1 | 0.01% |
| Post-S12, final (`sweep-post2/`) | 10,000 | 0 | **0** | **0.00%** |

- The plan's carried figure (1.33%) predates M2.5-S14: the sim device-stall
  model widened the cut-timing distribution and more than doubled the
  reorder-hole shapes on this seed base (231 vs ~133). The gate value
  (< 0.1% at 10k seeds) is met with margin either way.
- The single first-cut refusal (seed `0xd5ee1c21`) was a **policy bug found
  by the sweep**, not physics: the seq-contiguity rule spanned a segment
  rotation, misclassifying a torn rotation-tail frame (seq 250 lost at
  seg-000000's end; seq 251 survived at seg-000001 offset 0) as
  malformation. Fixed: continuity resets at segment boundaries (a torn
  rotation tail is indistinguishable from prealloc slack); the
  cross-segment **attestation** check (`covered_lsn` reaching past an
  earlier segment's surviving data end → refuse) covers the lying-disk
  variant of the same shape — a lie frame v1 could not even see.
- Refusals that remain by design (each pinned by a deliberate test, not
  observed in the sweep because honest physics cannot produce them):
  attested coverage past the data end (`recover_torn::attesting_survivor…`),
  v1 frames beyond a gap (`…v1_survivor…`), sealed-slack validating frames
  (`…corruption_in_a_sealed_segment…`), prefix stamp malformations
  (`recover_stamp.rs`), and the begin-LSN guard (unchanged).
- Refused boots keep the survival audit (unchanged); the `FsyncLies`
  canary is caught by the ack-stream oracle regardless of tail
  classification (`planted_lying_fsync_is_caught_within_1000_seeds`,
  green post-change).
- Crash matrix: green at `CRASH_MATRIX_SEEDS=256` post-change.
- Fuzz smoke post-change: `frame_decode` 104,234,089 runs / 301 s, 0
  findings (both formats explored); `segment_read` smoke in
  `fuzz-smoke.txt`. The ≥ 24 h cumulative post-change requirement accrues
  on the S02 nightly runner (S15).
- Companion sweeps: `sweep-combined/` (4k m2-combined seeds, S14 scenario,
  post-change).
