# M4-S04 steel-thread cold-read histogram — env snapshot
2026-07-18
git: b8b21c5 + S04/S05/S06 work in progress (disclosed dirty tree)
kernel: 7.0.0-28-generic
cpu: 13th Gen Intel(R) Core(TM) i7-13700KF (pinned: taskset -c 4)
governor(cpu4): performance · epp: performance
device: ADATA LEGEND 700 (nvme0n1, ext4 /home) — **dev box, NOT the
reference NVMe**; DRAM-less budget drive
tier: dev — **prototype/informational, never quotable as a claim (L10)**;
the risk-gate input is the reference-box re-run of this harness.

## Harness
`INF_STEEL_DIR=<dir on nvme0n1> taskset -c 4 cargo test -p inf-runtime
--features uring --release --test steel_thread -- --ignored
cold_read_histogram --nocapture`

- Real io_uring `TierRead` through `IoGate` executor suspension (the full
  steel-thread GET: plan → suspend → re-resolve → CRC verify → decode).
- `posix_fadvise(DONTNEED)` on the tier file before every read — each
  sample hits the device, not the page cache.
- Includes pump overhead (submit_and_reap park ≤ 5 ms, woken by CQE) —
  an upper bound on the read path itself.
- 300 rounds per replicate over the demoted 32-record corpus.

## 3 pinned replicates (µs)

| rep | p50 | p90 | p99 | max |
|-----|-----|-----|-----|-----|
| 1 | 169.8 | 171.0 | 176.4 | 256.8 |
| 2 | 169.9 | 171.3 | 177.1 | 249.6 |
| 3 | 169.2 | 170.0 | 174.6 | 259.4 |

Context (informational): the §22 cold-read gate is p99 < 1.5 ms on the
reference NVMe under **loaded zipfian** — this idle-drain prototype number
(~0.18 ms p99, dev tier) is a different shape and exists only to show the
suspension path carries no hidden millisecond; the gate verdict belongs to
S22/S24 rows on the reference box.
