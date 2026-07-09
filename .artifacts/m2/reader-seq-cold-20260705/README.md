# M2-S22 cold-cache sequential reader row (S04 AC: ≥ 2 GB/s on reference NVMe)

Date 2026-07-05 · kernel 7.0.0-27-generic · governor `performance` · git `2f7b07b` (clean)
Box: HomeLab i7-13700KF, ADATA LEGEND 700 (consumer NVMe, PCIe Gen3, DRAM-less) —
the user-designated M2 reference box (Gen4 deviation disclosed).

## Command

```
INF_BENCH_COOL=1 INF_BENCH_DIR=$HOME/.cache/inf-m2-bench \
  taskset -c 4 cargo bench -p inf-log --bench reader_seq
```

Full CRC-validating `SegmentReader` pass over a 255 MiB segment; cold per pass via
sync + fadvise(DONTNEED) (cost included in the measured pass, ~ms).

## Result (criterion, 20 samples/row)

| window | throughput |
|---|---|
| 256 KiB | **1.43–1.44 GiB/s = 1.53–1.55 GB/s** |
| 1 MiB (default) | **1.43 GiB/s = 1.53 GB/s** |
| 4 MiB | 1.26 GiB/s (larger windows lose on this DRAM-less drive) |

## Verdict vs the ≥ 2 GB/s AC: FAIL on this device, with disposition

- Warm on the same box: **3.62 GiB/s** (`reader-seq-dev-20260702`) — the reader's
  CPU path (CRC + framing) has ~2.4× headroom over this device; cold throughput is
  pinned at the drive's single-stream sequential read ceiling (~1.5 GB/s Gen3
  DRAM-less), not the reader.
- The AC's intent — headroom over the 1 GB/s/cell replay gate — holds in the only
  sense the reader controls: it sustains > 2 GB/s whenever the device can feed it.
  The absolute 2 GB/s cold number requires the Gen4 reference device; recorded as
  device-bound in the M2 verdict ADR (no reader-side remediation indicated).
