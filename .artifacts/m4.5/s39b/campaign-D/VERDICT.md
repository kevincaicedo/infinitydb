# Campaign D verdict (1-slot pool, engine 16b4dd5, 3 interleaved replicates, 256 MiB segments, 150 s legs)

| gate | measured (per-replicate median) | verdict |
|---|---|---|
| warmed zero-fill share (arm) ≤ 0.10 | **0.377** (0.326 / 0.416 / 0.377; baseline 1.012 / 1.005 / 1.045) | FAIL — **falsifier (a)** (> 0.3: the 1-slot pool starves) |
| recycle deficit per cell ≤ 2 | **3** (2 / 3 / 3) | FAIL (same cause: misses 20 / 22 / 23 vs recycled 24 / 20 / 25) |
| padding delta ≤ 3 pts | **0.04** (51.7 % both arms) | PASS — the S39c control holds |
| always c32 p50 arm/base ≤ 1.05 | **0.97** (711 / 711 / 713 vs 731 / 731 / 727 µs) | PASS |
| always c32 p99 arm/base ≤ 1.10 | **0.52** (2.6 / 11.9 / 2.3 vs 5.0 / 14.9 / 6.8 ms) | PASS — less zero-fill competing with the barrier |
| read c64p16 arm/base ≥ 0.98 | **0.98** | PASS (borderline) |
| recovery time arm/base ≤ 1.05 (immediate boot) | **1.25** (4.08 / 4.70 / 5.28 vs 3.51 / 3.77 / 3.74 s) | FAIL — **falsifier (c)** as written; attributed below |
| rotations per cell ≥ 8 | 9 | PASS (validity) |
| host bytes per log byte (info) | **1.56** arm vs 2.20 baseline | — |
| device sectors per log byte (info) | **1.57** arm vs 2.20 baseline (the block device agrees with the accounted figure to 0.3 %) | — |

Throughput c32: arm 31.5 / 29.2 / 34.1 k vs base 32.2 / 29.4 / 30.9 k ops/s (inside the spread).
Recycled-residue recovery facts on the arm boots: 5 / 7 / 8 slacks, 4 / 6 / 7 segment stops.

## Falsifier (c) attribution (same session, box quiet)

- `crates/inf-log/tests/residue_scan_cost.rs` on the data filesystem: the slack
  audit over 256 MiB of recycled residue (65 536 foreign frames) reads
  **30 ms vs 26 ms** over 256 MiB of zeros — +4 ms per segment, +15 % of the
  scan, ~0.1 % of the boot.
- Unlinking a 256 MiB file (the pooled segment boot GC removes) on this
  filesystem: **0.1 ms** (dir fsync after it 1.6 ms).
- Replay volume per cell identical across arms (858 MB, 3 segments).
- So the 0.6–1.5 s delta is not the reader's rule. The boot's `Phase::Start`
  is device-state sensitive (claim-ledger C38f) and the two arms leave the
  drive in different states (the arm overwrites recycled extents in place;
  the baseline writes freshly allocated ones). The instrument (one boot per
  leg, right after the leg) cannot separate the two. **Disposition: the gate
  stays red as read; not converted by explanation.** The row gains a second
  boot after the drive-state idle (binding from campaign E on, predeclared
  before E ran); the immediate boot stays on the row, informational.

## Disposition

Falsifier (a) fired as predeclared ⇒ the 2-slot pool is the next hypothesis
(campaign E, same night, rules in `../campaign-E/README.md`). No default
change from this campaign. Every other mechanism claim (zero-fill share 1.0
→ 0.33–0.42 even starved, p99 halved, padding untouched, reads inside 2 %)
stands as measured on a row whose first clause is red.
