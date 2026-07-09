# M2.5-S07 sync-pipeline A/B — verdict (2026-07-06)

- box: HomeLab i7-13700KF, ADATA LEGEND 700 Gen3 DRAM-less NVMe, kernel per report headers
- tier: **dev (non-binding)** — governor powersave, env-check overridden
  (`--unsafe-env --allow-dirty`, user gap-run artifacts untracked); the
  directional verdict is unambiguous (zero overlap between arms), the
  absolute numbers are not citable.
- design: `gate-run m2 --only-always` (10 s saturating always row, 4 cells,
  pin-start 4), sync-pipeline **1 vs 2**, ABBA × 3 (12 legs), pressure data
  root on the real device (`~/.cache/inf-m2-press`).
- binary: post-S01/S07 tree, commit 46591fd.

## legs

| leg | bound | w/s | acks/fsync | fsync p50 µs | fsync p99 µs | group p50 | formation |
|---|---|---|---|---|---|---|---|
| 0 | 1 | 155459 | 99.2 | 2495 | 4095 | 101 | 0.39x |
| 1 | 2 | 149340 | 55.6 | 2751 | 5631 | 58 | 0.23x |
| 2 | 2 | 149636 | 55.7 | 2751 | 5631 | 59 | 0.23x |
| 3 | 1 | 154176 | 99.0 | 2495 | 4223 | 99 | 0.39x |
| 4 | 1 | 154740 | 99.0 | 2495 | 4095 | 99 | 0.39x |
| 5 | 2 | 149124 | 55.6 | 2751 | 5759 | 58 | 0.23x |
| 6 | 2 | 151264 | 55.9 | 2751 | 5503 | 59 | 0.23x |
| 7 | 1 | 157034 | 100.0 | 2495 | 4095 | 101 | 0.39x |
| 8 | 1 | 156473 | 99.5 | 2495 | 4031 | 101 | 0.39x |
| 9 | 2 | 149977 | 55.5 | 2751 | 5503 | 58 | 0.23x |
| 10 | 2 | 150309 | 55.6 | 2751 | 5631 | 58 | 0.23x |
| 11 | 1 | 155541 | 99.3 | 2495 | 4031 | 101 | 0.39x |

medians: bound 1 — 155.5k w/s, formation 0.39x, group p50 101, fsync
p50/p99 2495/4095 µs. bound 2 — 149.8k w/s (−3.7%), formation 0.23x,
group p50 58, fsync p50/p99 2751/5631 µs (p99 +37%).

## verdict: **Rejected** (bound 1 remains the default)

The two-in-flight pipeline loses on every axis, with no replicate overlap:
on this DRAM-less Gen3 device the flush itself serializes, so a second
in-flight sync cannot overlap device work — it only *splits* each group
(~half the records per fsync, double the fsync count for the same acks)
and lengthens the tail by queueing behind its sibling.

The bound-1 arm also **refutes the ADR-0022 D8.6 dead-time hypothesis**
by identity: per-cell arrival rate × fsync p50 = 38.9k/s × 2.495 ms ≈ 97
records, and the measured group p50 is 99–101. Group size already equals
arrivals-during-flush — the LOG-step issue point adds no measurable dead
time (the loop iterates orders of magnitude faster than a 2.5 ms flush),
so there is nothing for completion-CQE issue to recover at bound 1
(consistent with ADR-0026 D4's SQE-batching analysis).

Consequence for the S07 formation gate (≥ 0.8× available in-flight): at
this arrival rate the gate is unreachable by *any* issue-timing change —
0.8 × 256 = 205 records/group would require ~5.3 ms flushes (a worse
device) or ~2× the arrival rate (more offered load — the Gen4/S18
territory, same home as the 300k absolute row). The honest formation
currency on this device is `group ≈ per-cell throughput × fsync latency`,
which the mechanism achieves at ~100% efficiency (ratio 99–100 acks/fsync).

Disposition per the M0-S14 rule: the lever ships implemented but **off**
(`--sync-pipeline` default 1 — byte-identical to ADR-0022 D3); re-evaluate
at S18 (Gen4) where flush latency and arrival rate change together. The
formation observable (fsync_group_p50/p99 + tripwire:group_formation_x)
stays as the standing instrument this A/B validated.
