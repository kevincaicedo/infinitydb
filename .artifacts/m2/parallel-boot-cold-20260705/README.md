# M2-S22 cold-cache 10 GB parallel boot — the §6 "10 GB < 15 s" gate row: PASS

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `2f7b07b` (clean)
Box: HomeLab i7-13700KF, ADATA LEGEND 700 (consumer NVMe, PCIe Gen3, DRAM-less) —
the user-designated M2 reference box (Gen4 deviation disclosed).

## Command

```
INF_BOOT_COOL=1 INF_BOOT_DIR=$HOME/.cache/inf-m2-bench/parallel-boot INF_BOOT_REPS=3 \
  cargo bench -p inf-server --bench parallel_boot
```

8 cells × 1280 MiB steady-state ick-tail images = **11.01 GiB replay set** (data
extents + .ick). Cold cache before every rep via sync + fadvise(DONTNEED) over every
image file. Boot = the real infinityd assembly (uring drivers, control thread,
recovery board, loop-resident recovery); timed span = cell-thread spawn →
`RecoveryBoard::all_ready`.

## Result

| rep | wall to all-ready | aggregate replay |
|---|---|---|
| 0 | **9775 ms** | 1.13 GiB/s |
| 1 | **9828 ms** | 1.12 GiB/s |
| 2 | **9864 ms** | 1.12 GiB/s |

**PASS: 9.78–9.86 s < 15 s** (relative spread 0.9%). The limiter is the drive's
shared sequential ceiling (~1.2 GiB/s across 8 concurrent streams — the S15 dev
artifact measured the same ceiling); all 8 cells replay in parallel and the node
answers `-LOADING` throughout (S15 semantics, unchanged).
