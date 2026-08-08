# M2.5-S08 — recovery read-ahead/apply overlap: verdict (2026-07-06)

- box: HomeLab i7-13700KF (designated reference box, ADR-0022 D1), governor
  performance, turbo off, ADATA LEGEND 700 (Gen3 DRAM-less — disclosed
  deviation from the §18 Gen4 profile). Cold cache per rep
  (`sync` + `fadvise(DONTNEED)`, the S22 method). Server bench pinned
  `taskset -c 4`; the prefetch worker unpins itself (that is part of the
  design — see below).
- images: `recovery_replay` 1 GiB/cell (tail-only = full-log worst case;
  ick-tail = checkpoint@50% + tail, the steady-state boot shape);
  `parallel_boot` 8 cells × 1280 MiB = 11.0 GiB.
- legs: replay ABBA off/on/on/off × 5 reps (`replay-*.txt`); boot off/on
  × 3 reps (`boot-*.txt`). L7: state digest identical across every
  replicate of every row (printed per row).

## Mechanism selection (cheapest-decisive-first, L4)

1. **`posix_fadvise(WILLNEED)` — Rejected.** On this kernel (7.0)/device,
   hinting the next window *doubled* per-window cold read latency
   (0.50 → 1.00 ms/MiB; every depth/batch variant lost —
   `willneed_test2.py`, in this dir). It defeats the kernel's own
   sequential-readahead ramp.
2. **Prefetch thread — Accepted** (`prefetch_thread_test.py`: pread
   0.50 → 0.07 ms/chunk, synthetic 0.70 → 1.00 GiB/s). Shipped as
   `inf-server::ReadAheadFs` (per-`open_read` boot-scoped worker, second
   handle by path, atomics + park/unpark, unpins from the inherited
   cell-core mask via the audited `inf_runtime::unpin_current_thread` —
   pinned, it timeshares with the apply loop and overlaps nothing, the
   first smoke measured exactly that).
3. **Direct `.ick` footer probe** (unconditional, both arms): the counts
   presize hint hopped every section header — a chain of *dependent* small
   reads, cold ≈ one synchronous page fault each, measured as the dominant
   cold ick cost. A well-formed `.ick` ends at its footer and the footer
   length is computable from the header, so two reads replace the hop
   (CRC-validated; falls back to the hop; fuzz oracle extended —
   `ick_decode` 13.6 M runs / 300 s, 0 findings).

## Cold replay A/B (5 reps × 2 legs per arm, zero overlap anywhere)

| row | off (probe only) | on (probe + prefetch) | Δ |
|---|---|---|---|
| tail-only | 0.66–0.68 GiB/s | **0.91–0.92 GiB/s** | **+37 %** |
| ick-tail | 0.81–0.82 GiB/s | **1.07–1.09 GiB/s** | **+32 %** |

(The probe alone moved cold ick-tail 0.64 → 0.81 vs the pre-change
baseline legs in this dir's first ABBA set.)

Overlap is **complete**: cold-with-prefetch equals the measured *warm*
apply ceiling at binding clocks (tail-only warm 0.87–0.92, ick-tail warm
1.06–1.10). The remaining bound is apply CPU at no-turbo base clock, not
the device or composition.

## Gate reading (per-cell cold replay ≥ 1.0 GB/s)

- **ick-tail (steady-state boot shape): 1.07–1.09 GiB/s = 1.15–1.17 GB/s
  — PASS with margin.**
- tail-only (full-log worst case): 0.91–0.92 GiB/s = 0.98–0.99 GB/s —
  **Narrowed**: overlap eliminated the I/O composition entirely; the row
  now sits on the apply-CPU ceiling of this box at base clock (its own
  warm number). Naming the ceiling, not carrying an I/O debt.

## Parallel boot (8 cells, 11 GiB, cold) — the regime split

| arm | all-ready | aggregate |
|---|---|---|
| off | **7.30–7.39 s** | 1.49–1.51 GiB/s |
| on (prefetch forced) | 9.98–10.05 s | 1.10 GiB/s |

Eight concurrent reader streams already saturate the device; eight more
prefetch streams destroy sequential locality — the lever **loses** the
multi-cell regime (both arms still clear the < 15 s boot gate).
**Shipped policy:** `infinityd` enables prefetch iff the node recovers a
single cell (`ReadAheadFs::new(fs, cells == 1)`); multi-cell keeps the
off-arm. Re-evaluate the multi-cell arm on the Gen4 box at S18.
10 GB boot: **improves or holds** — the shipped default is the off-arm
(7.3 s), and the footer probe helps both arms.
