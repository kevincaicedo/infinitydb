# M4.5-S39b — segment recycling (ADR-0090 as amended 2026-08-22): correctness rows

Engine: the S39b commit on `feat/m4.5-indexes-query` (see `sweeps/header.txt`
for the exact hash the sweeps ran on; the tree was dirty at sweep time — the
engine code was the commit's, the docs were still being written; the
reference-box campaign runs on the committed tree).

## What landed (ADR-0090 D1–D5 as amended by the review of the Proposed text)

- `inf-log::reader`: `ReadError::ForeignSegment` — a fully decoded frame at
  its stored offset stamped for another segment id (A1). Terminal for the
  reader like every misplaced frame; only recovery classifies it.
- `inf-log::tail`: `RegionEvidence::{foreign_frames, max_foreign_epoch,
  is_recycled_residue}`; foreign frames skipped by their padded extent only
  after the CRC passed.
- `inf-server::recover`: foreign stop = classified data end
  (`segment_residue_stops`), audit `recycled_residue_slacks` (never a hole,
  never torn; an empty recycled next is a legal tail), the resume epoch folds
  `max_foreign_epoch`. **Correctness-only fix found on the way:** the existing
  stale-trailing path read the resume offset from the segment it then removed
  (the live segment would have been reopened at offset 0) — pinned by
  `recover_stamp.rs::stale_trailing_segment_beyond_the_last_data_resumes_at_
  that_data_end`.
- `inf-log::segment`: `SealedMeta { id, prezeroed }`, the bounded pool
  (`SegmentConfig::recycle_slots`, default 1), `forget_sealed → SealedDisposal
  ::{Recycled, Unlink}`, rename-based prealloc with `fully_allocated()` re-read,
  the rename's dir entry on the same `PreallocBarrier` (A2), counters.
- `inf-server::durable`: recycle-or-unlink truncation; `accounted_host_write_
  bytes` + `write_amp_milli_accounted_host` (A3); `segments_recycled`,
  `recycle_misses`, `recycle_fallbacks`, `recycle_pool_bytes`,
  `segment_rotations`, `segment_preallocs`; INFO + boot line carry the
  recovery facts (`recover_segment_residue_stops`, `recover_recycled_residue_
  slacks`).
- `infinityd --segment-recycle-slots N` / `--no-segment-recycle`.
- `inf-sim --scenario m2-recycle` + the recycle oracle; `m2-durable` runs the
  default pool on 3 of 4 `Direct` seeds; a refused boot on the recycling
  scenario is a finding.
- Fence pins: `commit.rs::recycled_segment_frames_are_fenced_behind_the_rename_
  barrier` (FUA frame of the renamed segment completes first → no ack until
  the dir barrier), `segment_recycle.rs` (barrier returned on every rename-
  based prealloc, pool bound, sparse/rename fallbacks, residue reads foreign).
- Crash rows (`recover_recycled.rs`, 8 rows): residue behind this life's data;
  empty recycled next (legal tail, write-through from frame 0); sealed
  recycled segment mid-log; torn this-life write over residue; twice-recycled
  file; **refusal row** (same-segment frame behind a hole attesting coverage
  → refused through the new rule); the unattested sibling (truncated as
  today); determinism.
- Fuzz: `segment_read` gains the foreign shape (reader and scanner fuzzed from
  the same bytes; named seed `corpora/segment_read/foreign-segment-20260822`);
  300 s `frame_decode` (35.0 M runs) + 300 s + 120 s `segment_read`
  (360 k + 154 k runs), no findings.

## Sweeps (`sweeps/`, all 8 shards, every seed run)

| row | seeds | violations | refused | recycled | residue slacks proven at reboot |
|---|---|---|---|---|---|
| `m2-recycle` | 10 000 | 0 | 0 | 1 330 729 | 25 310 |
| `m2-durable` | 10 000 | 0 | 0 | 316 963 | 9 050 |
| `m2-mode-transition` | 4 000 | 0 | 0 | 49 451 | 1 403 |
| `m2-reorder-window` | 2 000 | 0 | 0 | 40 | 40 |
| `m2-device-budget` | 1 000 | 0 | 0 | 0 (pool off by design) | 0 |

(`just recycle-sweep` / `durable-sweep` / `transition-sweep` / `reorder-sweep`
/ `budget-sweep`; the manifests carry the per-shard figures.)

## The planted-bug canary (`canary/`)

`RUSTFLAGS="--cfg inf_canary_foreign_segment"` compiles the reader and the
scanner blind to the segment id (a rustc cfg on a scratch target dir — never a
Cargo feature, never reachable from a shipped binary). `m2-recycle` over the
first 64 seeds: **50 of 64 seeds refuse boot** on the ADR-0031 D3 seq-
continuity fail-stop ("frame seq 74 follows seq 57 within epoch 1") — the
refusal failure mode ADR-0090 named — against **0 of 64** with the rule in.
`canary/manifest-canary-64.txt`, `canary/results-canary-64.txt`.

## Default

`recycle_slots = 1` ships **on** with every row above green in the same commit
(ADR-0090 A5's condition). The reference-box measurement row (ADR-0090 D6 as
amended) is `inf-bench gate-run m4.5 --only-s39b`; its artifacts land under
`campaign/` when run.
