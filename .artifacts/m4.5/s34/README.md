# M4.5-S34 — FUA-class durable frames (ADR-0086) — dev-tier preview artifacts

**Dev tier. Not §19-valid** (no governor/thermal log, no competitors in-run,
no `gate-run`). Inputs to the reference-box campaign, never claims.

Box: the ADR-0022 D1 reference box (i7-13700KF, ADATA LEGEND 700 DRAM-less
Gen3, ext4, kernel 7.0.0-30), governor `performance`, cells pinned 0,2,4,6,
generator pinned 8,10,12,14. Binary: this branch (S34 tree), same binary
both arms; `--barrier-class flush|fua` is the only difference.

Shape: the F2 discriminator — 200k × 1 KB load at 64 conns, then 128k ops
at 32 conns, zipfian, 100 % write, `FSYNC always`, `tri-bench --shape kv`.

| file | what |
|---|---|
| `probe-results.md`, `fuaprobe.c` | the 2026-08-20 C barrier probe (the diagnosis input) |
| `io-properties.reference-device.toml` | `inf probe-device` output on this device (1 s rows) |
| `preview-20260821-*.txt` | first A/B, 1 MiB unpaced zero-fill, runs back-to-back — **bimodal p99** |
| `preview-paced-*.txt` | 256 KiB paced zero-fill, still back-to-back — still bimodal |
| `seg-discriminator-*.txt` | 16 MiB / 256 MiB / 1 GiB segments, 1 cell — the 1 GiB arm isolates the bad mode |
| `preview-trim-*.txt` | **20 s idle between runs** (fstrim unavailable without sudo): stable |

## Result (preview-trim, 4 cells, 3 replicates each)

| arm | ops/s | p50 ms | p99 ms | max ms | barrier p50 µs |
|---|---|---|---|---|---|
| flush (today) | 5,100–5,606 | 5.59–6.17 | 9.8–10.2 | 16.8–19.1 | 3,071–3,263 |
| fua | **28,073–28,621** | **1.10–1.13** | **2.54** | **10.3–11.3** | **591** |

1 cell (preview-paced, fua): 37.8–38.6k ops/s, p50 0.73–0.76, p99 1.5–1.7
(good replicates) — 1-cell/4-cell p50 ratio ≈ 1.5 (AC ≤ 1.3 is S35's
"two windows" fix; the barrier itself is 0.39 → 0.59 ms, 1.5×).

## What the bad mode was

Back-to-back runs alternate good/bad p99 (2.5 vs 19–27 ms) with identical
counters. The segment-size discriminator isolates it: 16 MiB and 256 MiB
segments are clean in every replicate; 1 GiB segments (2–3× the bytes
written to the device per run, through pre-zeroing) reproduce it every
time, with `log_write_stall_p99` 22 ms and `fsyncs_linked > 0`. On a
DRAM-less SLC-cached drive, total bytes written per unit time — the
pre-zeroing write amplification ADR-0086 D4 discloses, on top of the
previous run's undigested writes — is what moves the tail; spacing runs
20 s apart removes it at 256 MiB. Campaign rule (already in the ledger
memory): fstrim + idle between legs; the `zero_fill_bytes` row is the
disclosure. Segment recycling (S36) removes the second write entirely.
