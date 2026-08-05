# M4-S11 demotion storm — real TierFlush pipeline (replaces the S07 MemFs-harness deviation)

date: 2026-07-18 · HomeLab reference box · governor performance, turbo off · taskset -c 4 · ADATA LEGEND 700 (Gen3 DRAM-less, disclosed) · ext4 · git 147c33a + S11 tree
Gate context: §4.1 S07 'foreground p99.9 < 2 ms during the storm'; flush = TierFlush, slice 4 KiB, capacity 4 MiB, drain-loop maintain cadence.

## MemFs substrate (3 pinned reps)
rep 1:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 60 ns | p99 1796 ns | p99.9 4193 ns | max 407282 ns
rep 2:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 63 ns | p99 1727 ns | p99.9 4293 ns | max 386586 ns
rep 3:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 63 ns | p99 1740 ns | p99.9 4166 ns | max 378745 ns

## Real device, Direct mode (INF_STORM_DIR on NVMe; 3 pinned reps)
rep 1:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 87 ns | p99 1855 ns | p99.9 1332881 ns | max 7563547 ns
rep 2:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 89 ns | p99 1901 ns | p99.9 1286411 ns | max 8219840 ns
rep 3:
ops 400000 (+ maintain slices) | cold candidates 30 | demote slices 3243 | sealed 13283553 B | flush slices 3233 | flushed 13279272 B | files sealed 3 | stalls 0
committed 8396800 B (budget 33554432 B + slice 4096 B) | demoted-to-disk head 13279272
foreground+slice latency: p50 87 ns | p99 1893 ns | p99.9 1366777 ns | max 229692129 ns

Verdict: device-loaded foreground+slice p99.9 = 1.29–1.37 ms < 2 ms (3/3 reps); one rep max 229 ms — a single maintain-slice riding a known device stall episode (billed into the same histogram by design, disclosed). The binding §7 sub-gate row remains S22's (command-level).
