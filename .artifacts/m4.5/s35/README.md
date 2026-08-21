# M4.5-S35 — Bounded frame pipeline (ADR-0087) — dev-tier preview artifacts

**Dev tier. Not §19-valid** (no competitors in-run, governor `powersave` as
found — sudo unavailable —, no `gate-run`). Inputs to the reference-box
campaign, never claims.

Box: the ADR-0022 D1 reference box (i7-13700KF, ADATA LEGEND 700 DRAM-less
Gen3, ext4, kernel 7.0.0-30). Cells pinned 0,2,4,6 (`--pin-start 0`),
generator pinned 8,10,12,14. Binary: this branch (S35 tree, `71bb130-dirty`),
the **same binary every arm**; the arm flags are the only difference; every
arm runs `--barrier-class fua`.

Shape: the S34 F2 discriminator — 200k × 1 KB load at 64 conns, then 128k
ops at 32 conns, zipfian, 100 % write, `FSYNC always`, `tri-bench --shape kv`
(`~/bench-harness`). Raw server logs and data dirs: `~/bench-data/s35-ab/`.

| arm | flags | staging resident/cell |
|---|---|---|
| `k1` | `--frames-in-flight 1 --log-staging-mib 4` | 8 MiB (S34's arm — the baseline) |
| `k3s2` | `--frames-in-flight 3 --log-staging-mib 2` | 8 MiB (the L5-neutral reference arm, 4 × 2 MiB) |
| `k4` | `--frames-in-flight 4 --log-staging-mib 4` | 20 MiB (depth trend; +12 MiB/cell attributed, `log_staging_bytes`) |

| file | what |
|---|---|
| `preview.sh` | the driver (arms in order k1, k3s2, k4; 20 s idle before each run; 4 cells then 1 cell, 3 reps) |
| `preview-20260821.txt` | its output — **7 of 18 runs hit the S34 drive-state bad mode** (device write-through p99 20–60 ms; k1 2/6, k3s2 3/6, k4 2/6 — position-independent) |
| `preview-rotated.sh` | rotated order (k4, k1, k3s2), **40 s idle** before each run, 4 cells only |
| `preview-rotated-20260821.txt` | its output — **9 of 9 runs clean**; the table below |
| `durable-sweep-10k-20260821.log` | `just durable-sweep` with K varied by seed: 0 violations, 0 refusals, pipeline coverage per shard |

## Result (rotated pass, 4 cells, 3 replicates each, 40 s spacing)

| arm | ops/s | p50 ms | p99 ms | max ms | write-through p50 µs | p50 ÷ barrier | in-flight max |
|---|---|---|---|---|---|---|---|
| k1 (S34) | 28,156–28,653 | 1.088–1.106 | 2.52–2.53 | 10.7–11.5 | 591 | 1.84–1.87 | 1 |
| k3s2 | **39,477–39,848** | **0.712–0.733** | **1.90–1.91** | 10.1–11.0 | 591–607 | 1.18–1.24 | 3 |
| k4 | **40,496–40,730** | **0.719–0.721** | **1.67–1.80** | 10.0–10.9 | 623–655 | 1.10–1.15 | 4 |

Against ADR-0087 D8 at this shape: `always` p50 falls from ~1.85 barrier
windows to 1.1–1.2 (the AC's ≤ 1.2 × barrier + loop overhead — k4 inside,
k3s2 at the line); throughput +39–42 %; p99 better, not worse; `max`
unchanged — the falsifier ("p99/max worse than K = 1") is silent.

## 1 cell (first pass, good-mode replicates only; 20 s spacing)

| arm | ops/s | p50 ms | p99 ms | 1c/4c p50 ratio (vs the rotated 4c rows) |
|---|---|---|---|---|
| k1 | 37,812–37,887 | 0.748–0.765 | 1.52–1.59 | **1.45** (S34's AC ≤ 1.3 — the "two windows") |
| k3s2 | 46,146–46,919 | 0.581–0.640 | 1.43–1.64 | **1.18** |
| k4 | 44,946–46,509 | 0.582–0.657 | 1.41–1.41 | **1.16** |

The 1-cell rows carry the first pass's drive-state caveat (2 of the 9
1-cell runs were bad-mode and are excluded: k1 r2 p99 59.8 ms, k3s2 r1
p99 27.4 ms; listed in `preview-20260821.txt`). A 1-cell rotated pass with
the 40 s rule is the cheap next step; the reference-box campaign owns the
AC.

## What the bad mode was, again

The S34 finding reproduces with more incidence at K > 1: every arm writes
512–805 MiB of zero-fill per run on top of the previous run's undigested
writes, and at 20 s spacing the DRAM-less drive's tail moves (device
`fua_latency_p99` 20–60 ms, `log_write_stall_p99` the same — the device,
not the engine: counters, waits and in-flight depth are identical between
good and bad replicates of one arm). 40 s of idle removes it in 9/9 runs.
Campaign rule unchanged: fstrim + idle between legs, `zero_fill_bytes`
disclosed per row; segment recycling (S36) removes the second write.

## Instrumentation note

`frame_waits_barrier` / `frame_waits_rotation` in the two preview files
count **LOG steps** held (the binary these runs used); the tree now counts
**episodes** (one per held frame). The LOG-step counts are the loop's
iteration rate during the 64-conn load phase's FLUSH-class window before
the class-upgrade rotation (a due frame held behind an in-flight write
on the un-zeroed segment 0), not a pipeline pathology — the NVMe e2e
(one writer before the upgrade, eight after) asserts
`frame_waits_barrier == 0` for a pure-`always` FUA run.
