# M2-S13 recovery-replay rehearsal — dev tier (2026-07-03)

## Environment

| | |
|---|---|
| Date | 2026-07-03 |
| Host | HomeLab dev box (i7-13700KF, 30 GiB), **dev tier — not the reference box** |
| Kernel | 7.0.0-27-generic |
| Governor / EPP | `performance` / `performance` (verified via sysfs before the run) |
| Pinning | `taskset -c 4` (single P-core; recovery is single-threaded per cell — L1) |
| Tree | `8af916e` + the in-progress S13/S14 working set (this bench and the code it measures land in the same commit; disclosed dirty) |
| Command | `INF_BENCH_REPS=5 taskset -c 4 cargo bench -p inf-server --bench recovery_replay` |
| Page cache | **warm** — the build phase writes the image immediately before timing. This row isolates the CPU-side replay path; the cold-read leg rides the device (S04 dev artifact: 3.62 GiB/s CRC-validating sequential read, warm; reference-box cold row binds at S22) |

## What is measured

`open_cell_log` end to end — the real boot path: MANIFEST read → `.ick`
footer peek + presize → `.ick` streamed load through
`Keyspace::apply_record` → tail replay via `SegmentReader` → S14 slack
scans → rotor reopen. Synthetic image: 512 B values, 12 B unique keys,
64-record frames, ~2.03 M records ≈ 1 GiB.

## Results (5 replicates, medians)

| Row | Shape | Median | Throughput | Gate view (decimal GB) |
|---|---|---|---|---|
| `tail-only` | full-log replay, no ckpt, 256 MiB segments | 1041 ms | **0.96 GiB/s** | **1.03 GB/s** |
| `ick-tail` | ckpt at 50% + manifest + truncated prefix (steady state), 64 MiB segments | 898 ms | **1.11 GiB/s** | **1.20 GB/s** |
| `slack-floor` | 4 MiB data in full-size sparse segments — S14 per-boot scan cost isolated | 59 ms | ~9 GiB/s over ~512 MiB of sparse zeros | fixed per-boot cost |

State digest identical across all replicates per row (the L7 determinism
assert, enforced in-harness).

## The investigation (hypothesis → measure → fix)

First run of the same rows measured **0.84 GiB/s** (tail-only),
**0.96 GiB/s** (ick-tail), and a **~200 ms** slack-floor row. Two
mechanisms found and fixed:

1. **Doubling-rehash storm during bulk apply.** `initial_keys` defaults
   to 0, so a 2 M-entry replay pays ~15 stop-and-copy rehashes. A presize
   probe (`INF_BENCH_PRESIZE`, bench-local) measured the ceiling:
   tail-only 0.84 → 1.00 GiB/s, ick-tail 0.96 → 1.19 GiB/s. Fix shipped
   as designed by ADR-0016 (the `.ick` footer's per-ns entry counts are
   exactly this hint): `read_ick_counts` footer peek →
   `Keyspace::reserve_ns` → `CellStore::reserve_keys`, wired into
   recovery before the streamed load. The ick-tail row above shows the
   shipped path (1.11 GiB/s); tail-only has no `.ick` to presize from and
   keeps the storm — acceptable because a no-checkpoint log is bounded by
   the checkpoint interval (small) in steady state.
2. **Byte-wise zero scanning in the S14 slack scan.** The every-boot
   tail-region audit scanned preallocated zeros one byte at a time
   (~2.5 GiB/s): 200 ms fixed cost per boot at 256 MiB segments. Fixed
   with a 16-byte-word zero-run skip: 59 ms median (~9 GiB/s, memory-
   bandwidth class; first replicate ~90 ms — cold-start variance,
   disclosed).

## Disposition

**Evidence-pending (reference box).** Dev-tier rehearsal only: the
≥ 1 GB/s/cell gate value binds on the reference NVMe with a cold-cache
row at S22 (M2 §6). What this row establishes now: the CPU-side replay
path sustains the gate value with headroom in the steady-state (ick+tail)
shape, the presize path works end to end, the S14 audit's fixed cost is
~60 ms/boot at 256 MiB segments, and recovery is digest-deterministic
across replicates.

Raw output: `bench-output.txt` (this directory).
