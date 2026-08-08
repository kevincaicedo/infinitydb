# M4-S13 write accounting vs the block layer — ≤ 10% divergence AC

date: 2026-07-25 · box: HomeLab dev box (i7-13700KF, ADATA LEGEND 700
Gen3 DRAM-less NVMe — §19 device deviation disclosed, ADR-0022 D4 class)
· kernel 7.0.0-28-generic · governor `performance`, EPP `performance`,
turbo off (`no_turbo=1`) · `taskset -c 4` · ext4 on `/dev/nvme0n1p3`
(diskstats device 259:3) · `INF_ACCT_DIR=$HOME/.cache/inf-bench-acct` ·
tier I/O mode `Direct` (ADR-0054 default) · git `4145216` + the S13 tree
(tree dirty with exactly the S13 change set — disclosed, §19)

Command:

```
INF_ACCT_DIR=$HOME/.cache/inf-bench-acct INF_ACCT_MIB=512 \
  taskset -c 4 cargo bench -p inf-store --bench write_accounting
```

**Tier: dev.** Single box, device deviation disclosed. The campaign
re-read on the reference box is S22/S24's.

## Verdict

| | |
|---|---|
| **AC** | accounting vs `iostat`/blktrace within 10% on a controlled run |
| **Measured divergence** | **−1.49% / −1.57% / −1.52%** (3 replicates; median **−1.52%**) |
| **Result** | **PASS** — 6.5× inside the AC window |
| **Write amplification measured** | **1.999×** (identical across all three replicates) |

The number the story exists to state, measured rather than asserted: a
user byte on a durable-tiered namespace is written **twice** — once as a
WAL record, once as tier-file bytes. 1.999×, not "about two".

## Method

One tiered namespace on one cell, driven end to end through the real
path — no simulated disk, no harness stand-in for either write leg:

- **WAL leg:** `StagingRing` → `SegmentRotor::commit_frame` into real
  `seg-NNNNNN.ilog` files (`StdSegmentFs`, ftruncate preallocation),
  1024 records per frame. Buffered, then fsynced at the end of the run so
  every byte has reached the device before the instrument is read.
- **Tier leg:** the real `TierFlush` pipeline in `Direct` mode — frame
  CRCs, capacity rotation, footers, fdatasync barriers (ADR-0056), one
  commit page (1 MiB) per MAINTAIN slice for both the seal step and the
  flush barrier.
- **Checkpointing quiesced** (the plan's named attribution trap): a
  checkpoint writes bytes belonging to the checkpoint domain, which
  `INFO persistence` reports; leaving it on would surface here as an
  unexplained device-byte surplus.
- **Instrument:** `/proc/diskstats` field 10 (sectors written × 512) for
  the device backing the run directory — the same counter `iostat`
  reports as `kB_wrtn`, read directly so the measurement needs no
  external tool and no root.

Workload: 1,032,444 records × (8 B key + 512 B value) = 512 MiB of user
bytes, i.e. **8× the 64 MiB namespace memory budget**, so the residue
still resident in the mutable region is a rounding error rather than the
measurement.

## Replicates

| rep | counters `written_bytes` | device (diskstats) | divergence | idle noise floor |
|-----|--------------------------|--------------------|------------|------------------|
| 0 | 1,073,136,772 | 1,089,396,736 | **−1.49%** | 0 B/s |
| 1 | 1,073,136,772 | 1,090,285,568 | **−1.57%** | 243 kB/s |
| 2 | 1,073,136,772 | 1,089,679,360 | **−1.52%** | 1.4 kB/s |

Counter values are byte-identical across replicates (the workload is
deterministic); only the device reading varies, by 0.08% peak-to-peak.
The noise floor is the box's background write rate measured immediately
before each run with this process writing nothing — rep 1's 243 kB/s over
3 s is 0.7 MB against a 1.09 GB measurement (0.07%), so no replicate is
invalidated by background activity.

## Counter breakdown (identical in every replicate)

```
user_bytes             536,903,324   key + value, at the record boundary
wal_bytes              543,097,988   encoded log records (length prefix included)
flush_bytes            530,038,784   tier device bytes (frames + CRCs + headers + footers)
compaction_bytes                 0   M4-S15 seam — copy-forward does not exist yet
written_bytes        1,073,136,772   = the write-amp numerator
write amplification          1.999×
```

## The residual −1.5%, term by term

The counters under-report the device by ~16 MB on 1.09 GB. Every term is
named; none of them is unexplained:

Exact residual, rep 2: 1,089,679,360 − 1,073,136,772 = **16,542,588 B**
(1.52%).

| term | bytes | why it is not in the counters |
|---|---|---|
| WAL frame envelope | 44,396 (measured) | frame header + trailer per frame. One frame carries records from every namespace the cell wrote that iteration, so pro-rating ~44 B of envelope per namespace would be a lie dressed as precision (`write_accounting.rs` states this as a deliberate asymmetry). The harness measures and prints it. |
| WAL writeback block rounding | ≤ 4 MB (bounded, not measured) | the last partial 4 KiB block of each frame is written whole by the page cache; ≈1008 frames × ≤ 4 KiB. |
| ext4 journal + inode/extent metadata | the remainder (≈ 12 MB) | filesystem bookkeeping for the frame appends, the tier files, and the directory entries. Not a database write, and correctly outside a database counter. |
| background noise | ≤ 0.7 MB | measured per replicate (table above). |

All four push in the same direction (device > counters), which is why
every replicate lands on the same side of zero. Adding the one term the
harness can measure exactly (the frame envelope) moves the divergence by
0.01% — the rest is filesystem, not accounting.

## Related finding (recorded here, owned by S16/S19/S22)

The tier leg's amplification is **set by the MAINTAIN slice budget**, not
by the format. A tier file's partial tail frame is rewritten in place at
every barrier until it fills (ADR-0056 D5), so a slice quantum near the
4 KiB frame size pays for most frames twice. Measured in
`inf-store/tests/tiered_accounting.rs`
(`flush_amplification_follows_the_slice_budget`), same 8.5 MB workload:

| MAINTAIN slice | `flush_bytes` | vs the run's user bytes |
|---|---|---|
| 4 KiB (one commit page in that rig) | 16,105,472 | 1.89× |
| 256 KiB | 9,490,432 | 1.11× |

The 512 MiB run above uses the ADR-0052 D4 default 1 MiB slice and lands
at `flush_bytes / user_bytes` = **0.987×** — the tier leg costs
essentially one copy of the data, which is the whole design intent.

Consequence for operators: `tiering_flush_bytes` running near 2× the data
written is a slice budget to raise, not a bug to file. Stated in
`infinitydb/docs/ops-tiered-storage.md`; the knob itself is S19's
(`INF.NS`), and S22 owns tuning it against the WA gate.

## Reproduction

```
mkdir -p $HOME/.cache/inf-bench-acct
INF_ACCT_DIR=$HOME/.cache/inf-bench-acct INF_ACCT_MIB=512 \
  taskset -c 4 cargo bench -p inf-store --bench write_accounting
```

The harness refuses tmpfs (a RAM "device byte" is not a device byte),
prints its own methodology block, and asserts the ±10% AC itself — a run
that fails the AC fails the command.
