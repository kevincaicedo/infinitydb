# M4-S17 AC2 — 1 GiB blob-extent round trip, staging budget + blob WA

- Date: 2026-07-30T04:19Z · kernel 7.0.0-28-generic · governor: performance
- git: 43412cb (S17 working tree — dirty by definition, evidence for the in-flight story)
- Device: ADATA LEGEND 700 (Gen3, DRAM-less — the standing ADR-0022 D4 disclosure); ext4; TierIoMode::Direct; taskset -c 4
- Command: INF_BLOB_DIR=$HOME/.cache/inf-blob-bench taskset -c 4 cargo bench -p inf-log --bench blob_roundtrip
- The bench asserts in-process, per chunk: writer and reader staging <= 2x BLOB_CHUNK_BYTES (2,097,152 B), and blob WA < 1.010x. A failing run fails the command (AC 2's attribution assert).

## replicate 1
blob_roundtrip[nvme-direct]: 1024 MiB · staging peak 1056768 B (budget 2097152 B) · device 1074798592 B → blob WA 1.001x · write 377.08 MiB/s · read 367.07 MiB/s
## replicate 2
blob_roundtrip[nvme-direct]: 1024 MiB · staging peak 1056768 B (budget 2097152 B) · device 1074798592 B → blob WA 1.001x · write 378.16 MiB/s · read 367.95 MiB/s
## replicate 3
blob_roundtrip[nvme-direct]: 1024 MiB · staging peak 1056768 B (budget 2097152 B) · device 1074798592 B → blob WA 1.001x · write 152.01 MiB/s · read 367.67 MiB/s

## Reading

- **AC 2 holds with 2x headroom**: peak staging 1,056,768 B — one 257-frame read window (1 MiB + 4 KiB;
  the writer's batch window is smaller) — is 0.50x of the 2,097,152 B budget, identical across replicates
  because the bound is structural (fixed windows), not workload-dependent.
- **Blob WA 1.001x by construction**: 1,074,798,592 device bytes / 1,073,741,824 value bytes — frame CRC
  4/4092 + one 4 KiB header. The §4.1 "blob WA ≈ 1x" half, measured.
- Replicate 3's write leg (152 MiB/s vs ~377) is the documented DRAM-less sustained-write collapse of this
  device (box profile / ADR-0022 D4) — throughput context only; the AC terms are unaffected. Dev-tier.
