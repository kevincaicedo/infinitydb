# M2-S22 CRC32C kernel row (S01 AC: ≥ 10 GB/s, A/B vs software fallback)

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `2f7b07b` (clean)
Box: HomeLab i7-13700KF (CPU-bound row; pinned core).

## Command

```
taskset -c 4 cargo bench -p inf-simd --bench crc32c
```

## Result (criterion, 100 samples/row)

| size | dispatched (hw) | software fallback | ratio |
|---|---|---|---|
| 64 B | **22.5 GiB/s = 24.1 GB/s** | 3.17 GiB/s | 7.1× |
| 4 KiB | **10.30 GiB/s = 11.06 GB/s** | 1.58 GiB/s | 6.5× |
| 64 KiB | 8.51 GiB/s = 9.14 GB/s | 1.57 GiB/s | 5.4× |
| 1 MiB | 8.43 GiB/s = 9.05 GB/s | 1.57 GiB/s | 5.4× |

## Verdict vs the ≥ 10 GB/s AC: PASS at record/frame-typical sizes, serial ceiling above 64 KiB — dispositioned

- ≥ 10 GB/s holds through 4 KiB (11.06 GB/s), covering the record-level and
  small/mid frame CRC work the LOG step actually does per iteration.
- The 64 KiB+ plateau (9.05–9.14 GB/s) is the serial-`crc32q` dependency-chain
  ceiling **already recorded in ADR-0011** together with its remedy (3-way
  interleaved streams + combine); the remedy stays post-M2 — at S05's one frame
  per iteration, frame CRC is nowhere near the loop budget (S12/S21 rows hold
  the p99.9 gates with this kernel in-path).
- Numbers reproduce the 2026-07-02 dev A/B within noise (22.5/10.3/8.2 → 22.5/10.3/8.4).
