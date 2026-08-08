# M2-S07 log I/O tier A/B — buffered+fdatasync vs O_DIRECT (dev box, 2026-07-02)

**Status: dev-tier engineering run — NON-CITABLE (L10).** The S07 AC binds
on the reference NVMe Gen4 box (PLP-class device, where the flush
arithmetic changes completely); this artifact answers the A/B at the
driver+device layer on the dev box with the measurement discipline the AC
requires (fsync histogram on every row, 3 replicates, pinned governor).
Verdict recorded in **ADR-0014** (buffered ships; O_DIRECT rejected for
the M2 log write path; O_DSYNC-class barrier narrowed to a reference-box
follow-up).

## What was measured

`crates/inf-runtime/benches/log_io_tier.rs`
(`cargo bench -p inf-runtime --features uring --bench log_io_tier`,
`INF_BENCH_SECS=5`, `taskset -c 2`): the same grouped-write workload as
the S06 rehearsal (`log_fsync.rs` — one `IoOp::LogWrite` frame per
iteration, exactly one frame in flight, sequential offsets in a 1 GiB
fallocated file), across three policies:

| policy | write path | durability barrier |
|---|---|---|
| `buffered` | page-cache write | linked fdatasync (writeback + FLUSH) |
| `direct` | O_DIRECT (frames padded to 4 KiB) | linked fdatasync (FLUSH only) |
| `direct-dsync` | O_DIRECT \| O_DSYNC (padded) | the write itself (FUA-class) |

plus an **interference phase**: grouped writer (group 256) racing a
sequential reader on a second 1 GiB file, fadvise(DONTNEED)-cooled per
pass (the checkpoint/recovery-read stand-in), and a **page-cache
footprint** delta (`/proc/meminfo` Cached+Dirty, indicative).

## Environment (disclosures)

- Dev box (M0 Linux dev-box profile), kernel 7.0.0-27, ext4 on consumer
  NVMe (ADATA LEGEND 700 — no power-loss protection: every barrier pays a
  full media flush ≈ 1.2–1.5 ms; a FUA write is cheaper than a flush).
- **Governor `performance`, EPP `performance`** (pinned via
  `setup-infinity-benchmark-env.sh`; `inf-bench env-check` green on all
  probes except `git-dirty-tree` — mid-implementation session, dev tier
  only, disclosed).
- 5 s/row, 3 replicates (`replicates.txt`), record = 70 B.

## Results (replicate means; barriers/s = fsyncs/s or FUA writes/s)

| group | buffered barr/s | direct barr/s | dsync barr/s | buffered w/s | direct w/s | dsync w/s |
|---|---|---|---|---|---|---|
| 1 | 974 | 712 | 779 | 974 | 712 | 779 |
| 64 | 731 | 850 | 1,070 | 46,800 | 54,400 | 68,500 |
| 256 | 769 | 865 | 1,096 | 196,800 | 221,500 | 280,500 |
| 1024 | 721 | 774 | 943 | 738,600 | 792,100 | 965,400 |

- Latency (p50 per frame commit): buffered 1.25–1.54 ms, direct 1.25–1.34 ms,
  direct-dsync 1.25–1.34 ms. p99.9 2.2–4.5 ms all policies.
- **Replicate spread:** direct/dsync < 1.5% on every row; buffered rows
  vary up to ~13% across replicates (replicate 1 consistently slower —
  writeback-state sensitivity is itself a buffered-tier property; noted,
  dev tier).
- **Tail excursions:** rare ~50–90 ms max-latency events appear on BOTH
  policies (direct replicate 3 at small groups: p99.9 46 ms; buffered
  replicate 3 at group 1024: max 88 ms) — consumer-device housekeeping
  stalls, no policy advantage demonstrated.
- **Page-cache footprint:** buffered +230–259 MiB per 5 s row (≈ bytes
  written stay resident); direct/dsync ≈ 0. Real, but page cache is
  reclaimable and no foreground cost was measurable (below).
- **Write amplification (padding):** direct policies write 4 KiB-padded
  frames: 58× at group 1 (70 B → 4 KiB), 1.8× at 64, 1.14× at 256, 1.04×
  at 1024 (`devMiB/s` vs `payMiB/s` columns).

### Interference row (group 256 writer vs cooled sequential reader)

| policy | writes/s | writer p99.9 | reader MiB/s (baseline 1,637) |
|---|---|---|---|
| buffered | 100,400 | 4.6–5.9 ms | 389 |
| direct | 100,400 | 4.5–5.5 ms | 390 |

**No measurable difference.** The reader collapse (1,637 → ~390 MiB/s) is
device queue contention, identical under both policies; the buffered
writer's page-cache occupancy did not translate into foreground cost at
this dataset/RAM ratio.

## Reading

1. The workload is barrier-bound on this device, so policy deltas are
   barrier-cost deltas: buffered pays writeback+FLUSH, direct pays
   FLUSH-only (data already on device) — **+12–19% barriers/s for
   direct** at mid groups; FUA writes skip the flush — **+25–46% for
   direct-dsync**. Directionally real, mechanically explained.
2. The wins do NOT carry to M2's ship decision: a production O_DIRECT
   writer needs 4 KiB-padded frames, and the frozen frame format v1 reads
   zeros as end-of-log (`ReadEnd::ZeroTail`) — interior padding is a
   **format revision**, not a flag. Plus the group-1 write-amp floor
   (58×) lands exactly on the low-concurrency `always` shape. ADR-0014
   disposition: **Rejected for M2; O_DSYNC-class barrier Narrowed** to a
   reference-box re-evaluation (with PLP, flush ≈ free and all three
   policies likely converge; if the reference box still shows a FUA win,
   it re-opens with S10's checkpoint-streaming format work).
3. On this device the 300k gate value needs group ≈ 330–390 under any
   policy — unchanged from the S06 reading.

## Files

- `replicates.txt` — full 3-replicate output, all phases.
- `environment.txt` — env-check + governor/EPP/kernel/git capture.
- Disposition: **buffered + linked fdatasync ships (cut line honored);
  reference-box A/B row = Evidence-pending**, tracked on the S07 AC.
