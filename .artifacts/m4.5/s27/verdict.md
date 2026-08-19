# M4.5-S27 A/B — durable admission paces instead of refusing (2026-08-19)

- box: HomeLab i7-13700KF (8P+8E), ADATA LEGEND 700 Gen3 DRAM-less NVMe,
  kernel 7.0.0-30, governor performance, 4 cells `--pin-start 0`.
- tier: **dev (non-binding)** — dirty tree (`--allow-dirty`, the S27
  changes under test are the dirt); data root `~/.cache/inf-s27-data`
  (real fs, not tmpfs). The ADR-0081 D5 bar binds on the reference box.
- instrument: `inf-bench gate-run m4.5 --only-s27` (blessed in-house
  harness; the row landed with this story). Two leg-sets per run:
  **provoked** (`--log-staging-mib 1`, 32 conns × pipeline 4, 3
  back-to-back 10 s 100 % SET repeats + 1 informational `always` leg)
  and **D5 shape** (default 4 MiB staging, 32 conns × pipeline 1, 3
  back-to-back repeats — the ADR-0081 D5 closed loop).
- arms: `pre` = inner-repo HEAD `f612001` + a 2-hunk `--log-staging-mib`
  plumb only (the regime injector; admission behaviour untouched —
  patch disclosed below); `post` = the ADR-0083 fix; `sp2` = post with
  `--sync-pipeline 2` (the ADR-0081 D4 obligation).
- sequence: pre/post interleaved (pre-1, post-1, post-2, pre-2, pre-3,
  post-3); the sp2 block re-ran after them (a shell quoting bug voided
  the interleaved sp2 slots — disclosed; drive state drifted between
  the post and sp2 blocks, so sp2 supports only an overlap/no-overlap
  read, which is all that is claimed).

## Gate keys (median of 3 replicates per arm)

| key | pre | post | sp2 (post + pipeline 2) |
|---|---|---|---|
| `s27:busy_refusals_pct` (≤ 0.05) | **0.86** (0.47 / 0.86 / 1.05) FAIL | **0.00** (0 / 0 / 0) PASS | 0.00 PASS |
| `s27:write_repeat_decay_x` (≥ 0.90) | 0.79 (0.65 / 0.79 / 1.33) | **1.00** (0.96 / 1.00 / 1.02) PASS | 0.91 (0.67 / 0.91 / 1.01) |
| `s27:max_ms` (≤ 50) | 1562 (842 / 1562 / 2830) FAIL | 697 (543 / 697 / 1961) **FAIL — left red** | 816 (734 / 816 / 3079) FAIL |

**Refusals.** Pre-fix: up to 100,537 `-BUSY` per 10 s provoked leg, and
1.7k–50k per leg **even at default staging** in the D5 shape once the
session's own writes degraded drive state (the finding's regime,
recreated organically). Post-fix: **zero refusals across all 18 post
legs and all 18 sp2 legs** — the fabric parking converts every one into
bounded pacing (`log_admission_parked_total` delta 1.9k–3.8k per run,
`log_write_stall_p99_us` 239–287 µs).

**Repeat stability.** Post-fix last:first is 0.96–1.02 across all
three replicates; pre-fix swings 0.65–1.33 (refusal storms + retry
noise). The finding's monotonic-decay signature does not survive the
fix.

**Max latency — deliberately left red.** Post-fix max improved ~2× at
the median but sits at 0.4–2.0 s against the 50 ms bar: with refusals
gone, device writeback stalls surface as pacing latency (the physics
MongoDB/ScyllaDB pay, at ~3–4× their same-box throughput). The gate is
not narrowed (L10). Named lever if the reference box also fails it: an
occupancy-threshold pacer (park before staging-full) trading
throughput for tail. Whether the D5 50 ms bar applies at closed-loop
saturation or at a comparator-matched rate is the reference-box
campaign's call.

**`always` (informational, provoked regime).** No refusals in any leg;
7.4k–15.8k ops/s, device-backlog dominated. One outlier: the post-1
`always` leg completed 0 ops in its 10 s window — its first
group-commit fsync sat behind the provoked legs' ~1.5 GB writeback
backlog (the same physics as the finding's 8.39 s sample); post-2/3
and all sp2/pre `always` legs served normally, and the
`durable_pressure_always_acks_gate_through_the_pump` e2e test pins the
code path deterministically. Recorded as a device-state outlier, not
excluded from the busy denominator.

**Sync-pipeline 2 (ADR-0081 D4 → ADR-0083 D6).** Full replicate
overlap with pipeline 1 on every axis (provoked medians 492k vs 483k;
D5 medians 297k vs 380k inside a shared 208k–467k spread; identical
zero-refusal and max behaviour). **No measurable win in the reachable
slow regime; the M2.5-S07 `Rejected` disposition stands; ADR-0022 D3
is not amended.** The lever stays shipped-but-off.

## Honesty notes

- The gate row was **reshaped once mid-session, before the recorded
  A/B**: the first scouting run (`.artifacts/m4.5/s27/pre-rep1`,
  `post-rep1`) gated decay/max on the provoked leg-set, where they
  measure SLC/writeback physics rather than admission; the recorded
  row splits refusals (both sets) from decay/max (D5 set). Thresholds
  never moved.
- Throughput medians move ±40% leg-to-leg with drive state (the
  F20/F29 lesson); no throughput delta is claimed for the fix in
  either direction beyond "within drive-state noise".
- `parked_total` reads 0 on the pre arm because the counter does not
  exist in that binary (not evidence pressure was absent — the busy
  counts are).
- Baseline patch (regime injector only): `--log-staging-mib` arg parse
  + `StagingConfig{capacity_bytes}` plumb in `bins/infinityd/src/main.rs`,
  nothing else.

## Raw

Per-leg tables: `.artifacts/m4.5/s27/{pre,post,sp2}-{1,2,3}/*/report.md`
(scouting runs: `pre-rep1`, `post-rep1`).
