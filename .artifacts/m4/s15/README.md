# M4-S15 — Copy-forward compaction + head advancement: evidence bundle (2026-07-29)

Dev-tier per the box profile (`box-state.txt`: governor powersave / EPP
performance disclosed; S15 work dirty in tree, disclosed — no device or
comparative numbers claimed here). Story: dead-ratio-triggered
copy-forward relocation, two-phase retirement gated on a covering
checkpoint swap, unlink at the plane layer, cold-floor advancement
(ADR-0059).

## AC 1 — endurance slice: disk oscillates, floor advances, statvfs reclaims

`crates/inf-store/tests/tiered_compaction.rs::endurance_slice_disk_oscillates_and_statvfs_reclaims`
(StdSegmentFs on a real tempdir): 10 cycles × 1500 keys of sustained
overwrites; per cycle the reclaim pipeline runs (compact → publish →
retire → unlink). Asserts: on-disk bytes stay under the oscillation
bound from cycle 3 on, every unlinked path is ENOENT, `cold_floor()`
advanced, final usage below peak, and `statvfs` available bytes came
back (16 MiB shared-box slack). Run log:
`tiered-compaction-release-20260729.txt` — 8/8 PASS.

## AC 2 — crash between copy-forward-empty and covering checkpoint

`tests/crash-matrix/tests/recovery_v2.rs` (+ 3 rows in
`tests/crash-matrix/m4.toml`):

- `s15_covering_swap_abort_serves_from_prior_unit` — `manifest_rename_fail`
  at the covering swap: retirement aborts, nothing unlinks, the prior
  unit stays authoritative and every manifested range it names is
  readable (the premature-unlink catcher).
- `s15_covering_swap_dir_fsync_resolves_and_boot_gc_reclaims` —
  `dir_fsync_fail` after rename: crash resolves the new unit; the
  retired-but-not-unlinked file is reclaimed by boot GC (ADR-0057 D6-1).
- `s15_unlink_failure_is_nonfatal_and_redriven` — `tier_unlink_fail`:
  reclaim deferred, no crash, redriven later.

Run log: `crash-matrix-recovery-v2-20260729.txt` — 9/9 PASS.

## AC 3 — foreground p99.9 under full slice budget

`crates/inf-store/benches/compaction_storm.rs` (MemFs substrate,
disclosed — isolates engine-side cost from device jitter): 400k-op
storm, 85%-of-writes-hit-hottest-20% skew, COMPACT_SLICE = 1 MiB fed
every maintain round, publish/retire/unlink every 64 rounds. 5
replicates (`compaction-storm-20260729/replicate-{1..5}.txt`):

| replicate | p50 | p99 | p99.9 | max |
|---|---|---|---|---|
| 1 | 42 ns | 1449 ns | 2991 ns | 862 µs |
| 2 | 43 ns | 1518 ns | 2976 ns | 839 µs |
| 3 | 43 ns | 1496 ns | 2966 ns | 877 µs |
| 4 | 42 ns | 1328 ns | 2917 ns | 849 µs |
| 5 | 42 ns | 1424 ns | 3068 ns | 851 µs |

p99.9 ≈ 3.0 µs ≪ 2 ms AC; max ≈ 0.9 ms also under it. Each replicate
relocated >0 records, retired and unlinked files, committed bytes ≤
budget + one page, 0 stalls.

## AC 4 — DST relocation oracle + zero dangling refs, 10k seeds

`bins/inf-sim` `m4-recovery` grew the compaction leg: pre-walk
copy-forward bursts (1-in-5 pressure-armed), a mid-walk pause assertion
(no relocation while a walk is pinned), retire → covering swap →
commit → unlink (1-in-3 left to boot GC), relocation-origin markers on
displacement (ADR-0059 D9), and a post-retirement in-life audit. The
per-life oracle checks no loss/dup across compaction × checkpoint ×
crash and zero checkpoint refs into retired/unlinked files.

Sweep: `recovery-compaction-sweep-20260729/` — **10,000 seeds, 0
violations** (base 0x515C0DE), 3,340,660 refs, 13,814,951 images,
667,320 relocations, 1,231 files retired, 817 unlinked in-life, 414
reclaimed by boot GC, 7,554 cut-before-publish lives, 10,121 flush-lag
lives. Single-seed determinism verified (`just sim-smoke`, seed
0xC0FFEE hash-identical across two runs).

## Finding (designed around in this story): D9 stale-twin replay hazard

An unlogged relocation A→B breaks `ColdDisplace` exact-replay: replay
resurrects the checkpoint ref at A, the displace marker targeting B
misses it, and the image *re-inserts* — an immortal stale twin serving
old bytes. Fix (ADR-0059 D9): compaction pauses while a walk is
pinned; a bounded relocation-origin map (cap 3, scan defers at cap)
stages one extra `ColdDisplace` per origin at the next displacement;
replay's displace register widened to a bounded list (≤ 4, ADR-0057 D4
amended). Pinned by
`relocation_origin_markers_replay_exactly` and the counter-test
`relocation_origin_markers_are_load_bearing` (suppressing markers
provably leaves the stale twin behind).

## Validation run (all green, sequential per box discipline)

`just check` · `cargo deny check` · `cargo test -p inf-store --release`
(23 suites) · `cargo test -p inf-log -p crash-matrix` ·
`cargo test -p inf-runtime --features uring` · `just sim-smoke` ·
`just durable-sweep` (10k seeds, 0 violations) · m4-recovery 10k-seed
sweep under `ulimit -v 8388608`. Not run: Miri/Loom (no unsafe or
concurrency-topology change — compaction is single-owner cell state);
no new fuzz target (no decoder changed; relocation copies verbatim
bytes already covered by the tier-frame fuzz surface).
