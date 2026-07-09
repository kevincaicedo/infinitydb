# M2-S19 DST durability sweep — 10,000 seeds (gate artifact, dev tier)

- **Result: 10,000 seeds, 0 durability-oracle violations** (the §6
  `dst_sweep` gate value). 133 boots (1.33%) legally refused with the
  named ADR-0018 taxonomy fail-stop (a validating frame beyond lost
  un-fsynced bytes — reorder physics); every refusal passed the §8.2
  survival audit (manifest → ick → tail prefix): nothing acked was lost.
- Command: `inf-sim --scenario m2-durable --sweep 10000 --seed 0xD5EE0000 --shard I/8 --out .`
  (8 shard processes; per-seed results in results-shard-*.txt; any seed
  replays byte-identically via `inf-sim --scenario m2-durable --seed N`).
- Scenario per seed: 2 cells over one SimDisk, real DDL (always+everysec
  namespaces), 8 writers (3 always / 3 everysec / 2 memory), 140 ops each,
  16 KiB segments + 24 KiB ckpt interval (rotations, checkpoint cycles,
  MANIFEST swaps, truncation in-run), seeded power cut at any pipeline
  stage, every 8th seed double-cut (a second cut lands mid-recovery).
- Oracle: always-acked survives any cut; everysec loses at most 1s +
  100ms scheduler slop of simulated time; un-acked writes admissible
  either way (log-prefix semantics). Canary: Plant::FsyncLies caught at
  every probed seed (test pins <= 1000).
- Environment: Linux 7.0.0-27-generic, commit 4e9bb6b + working set,
  release build, 2026-07-03T20:15:48Z. Dev tier — S22 re-runs the sweep for
  the release evidence set.
