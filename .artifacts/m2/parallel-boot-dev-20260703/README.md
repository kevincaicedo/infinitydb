# M2-S15 parallel cold-boot rehearsal — dev tier, 2026-07-03

**Claim rehearsed:** 10 GB node (8 cells) cold-boots to serving < 15 s
(§6 recovery gate; the *binding* row is the reference box at S22, cold
page cache). Dev tier — NOT a product performance claim (L10).

## Method

`cargo bench -p inf-server --bench parallel_boot` (defaults: 8 cells ×
1280 MiB). Per cell: synthetic image in the steady-state `ick-tail`
shape (checkpoint at ~50% + manifest + truncated prefix + tail frames;
512 B values, 64-record frames, 256 MiB segments). Boot = the real
`infinityd` assembly: SO_REUSEPORT listeners, uring drivers, control
thread + recovery board, **loop-resident recovery** (each cell serves
`-LOADING` from its first iteration while MAINTAIN replays in 8 MiB
steps). Timed span: cell-thread spawn → `RecoveryBoard::all_ready`.

## Environment

- Linux dev box (M0 profile), 24 CPUs, governor `performance`,
  30 GiB RAM (~19 GiB available), consumer NVMe (the S07-documented
  device), unpinned cell threads.
- **Cache state disclosed:** rep 0 runs page-cache-warm (images built
  immediately before); reps 1–2 re-read ~11 GiB through a cache that
  cannot hold it all — effectively device-bound sequential reads.

## Results (`run.txt`)

| rep | wall to all-ready | aggregate replay | note |
|-----|-------------------|------------------|------|
| 0 | **3.30 s** | 3.33 GiB/s | cache-assisted |
| 1 | **8.98 s** | 1.23 GiB/s | device-bound |
| 2 | **9.07 s** | 1.21 GiB/s | device-bound |

Replay set = 11.01 GiB of data extents + `.ick` (≈ 2.53 M records/cell
× 8). All reps land under the 15 s gate value *on this box*, including
the device-bound reps where the consumer NVMe's shared sequential-read
ceiling (~1.2 GiB/s across 8 concurrent readers) is the limiter — the
reference Gen4 NVMe has several times that ceiling.

## Honest notes

- Aggregate ÷ 8 (~0.15 GiB/s/cell device-bound) is **not** the per-cell
  replay throughput gate (≥ 1 GB/s/cell) — that is CPU-side replay,
  measured single-cell at S13 (1.11 GiB/s warm). Here 8 cells share one
  device; wall time is the slowest cell. Both gates bind at S22.
- Byte totals in progress reporting are file extents including
  preallocated slack (upper bound; slack is credited when a segment
  completes, and never charged to the test-only throttle).
- `-LOADING` behavior during this window is byte-diffed against Redis
  8.0.5 separately: `loading-redis-capture-20260703/` + the pinned
  `node_e2e` loading tests.
