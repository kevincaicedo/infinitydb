# M2-S22 cold-cache recovery replay — the §6 ≥ 1 GB/s/cell gate row

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `2f7b07b` (clean)
Box: HomeLab i7-13700KF, **ADATA LEGEND 700 (consumer NVMe, PCIe Gen3, DRAM-less)** —
the user-designated M2 reference box; the master plan §19 reference profile names
NVMe Gen4 (deviation disclosed).

## Command

```
INF_BENCH_COOL=1 INF_BENCH_DIR=$HOME/.cache/inf-m2-bench INF_BENCH_REPS=5 \
  taskset -c 4 cargo bench -p inf-server --bench recovery_replay
```

Cold cache per rep via sync + fadvise(DONTNEED) over every image file (sudo-free;
method in the bench source).

## Result (5 replicates each, digest-deterministic across all reps — L7)

| row | wall | throughput | spread |
|---|---|---|---|
| tail-only (1024.7 MiB) | 1469.8–1500.4 ms | **0.67–0.68 GiB/s = 0.72–0.73 GB/s** | 2.1% |
| ick-tail (1024.8 MiB)  | 1532.4–1539.1 ms | **0.65 GiB/s = 0.70 GB/s** | 0.4% |
| slack-floor (S14 scan) | 89.5–91.4 ms fixed cost | — | 2.1% |

## Verdict vs the ≥ 1 GB/s/cell gate: FAIL on this device, with disposition

- Warm-cache (CPU path) on the same box: **1.11 GiB/s = 1.20 GB/s** — the replay
  path itself clears the gate (`recovery-replay-dev-20260703`).
- Cold single-stream composes serially: device read (~1.5–1.6 GB/s single-stream on
  this Gen3 drive) → apply (1.20 GB/s), with **no read-ahead/apply overlap** in the
  recovery machine: 1/(1/1.55 + 1/1.20) ≈ 0.68 GB/s — exactly what is measured.
- Two independent paths forward (either closes the gate): (a) Gen4 reference device
  (≥ 5 GB/s single-stream ⇒ composed ≈ 0.97–1.1 GB/s — still marginal), or
  (b) overlap device reads with apply in `Recovery` (double-buffered read-ahead —
  bounds composed throughput by min(read, apply) = 1.2 GB/s on this box).
  (b) is the real fix and is recorded as the post-M2 remediation in the M2 verdict
  ADR; the **user-facing §6 recovery gate (10 GB < 15 s) passes cold on this box**
  (see `parallel-boot-cold-20260705`).
