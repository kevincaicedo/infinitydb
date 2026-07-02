# Sequential log read throughput — `SegmentReader` full CRC-validating pass

- **Story:** M2-S04 (AC: sequential read ≥ 2 GB/s on the reference NVMe — headroom over the 1 GB/s/cell replay gate; L4)
- **Date:** 2026-07-02 · **Tier:** dev box (Linux 7.0.0-27, 24 threads, governor `performance`; NVMe: ADATA LEGEND 700) — **indicative only, non-citable (L10)**; the gate value binds on the reference box
- **Command:** `cargo bench -p inf-log --bench reader_seq` (raw output: `criterion-output.txt`)
- **Workload:** one 255 MiB segment written by `StagingRing` + `SegmentRotor` (frames of 128 × 256 B-value records ≈ 36 KiB); full `apply_frames` pass = read + peek + CRC32C validate + record iteration per frame
- **Caveat:** the segment is freshly written, so the pass reads from the **warm page cache** — this measures the reader's software ceiling (decode + CRC + memcpy), not cold-device bandwidth. The reference-box run must include a cold-cache (`echo 3 > drop_caches`) row before the AC can close.

| Read-ahead window | Throughput (median) |
|-------------------|--------------------:|
| 256 KiB | 3.63 GiB/s |
| 1 MiB (default) | 3.62 GiB/s |
| 4 MiB | 3.49 GiB/s |

## Reading

- The software path clears the 2 GB/s AC value with ~1.8× headroom even
  in the warm-cache regime where CRC dominates: consistent with the
  crc32c A/B (~8.2 GiB/s at large inputs) plus one window copy and record
  decode.
- Window size is flat 256 KiB→1 MiB and slightly worse at 4 MiB (L2/L3
  cache pressure from the larger resident window) — the 1 MiB default
  stands; no configuration cliff to document.
- **Disposition: Evidence-pending** for the reference-box AC (cold-cache
  NVMe row required). If the cold row lands below 2 GB/s, the first lever
  is overlapping read-ahead (issue the next window's read before decoding
  the current one), which the S05 `BackendDriver` migration enables for
  free — only with its own A/B (L4).
