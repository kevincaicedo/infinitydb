# RSS-slope excursion analysis — soak-unified-20260807-0227

Written 2026-08-08 against `samples.csv` (8,559 samples, 24.03 h, 10 s cadence).
This is the §19 root-cause record for the one failing gate. It does **not**
change the stamped verdict in `verdict.txt` — that stands as FAIL until an
owner-signed disposition says otherwise.

## The stamped failure

```
rss slope +1.147%/24h (first-5% median 2003064 kB -> last-5% median 2026032 kB)
accounted slope +0.234%/24h (used_memory 618183 kB -> 619627 kB; diagnostic)
```

Absolute growth: **+22.4 MB on a ~2.0 GB resident set, over 24 h.**

## Shape: hourly medians

| hour | vmrss (MB) | used_memory (MB) | disk (GB) | WA |
|---|---|---|---|---|
| 0 | 1955.5 | 603.6 | 12.34 | 1.635 |
| 1 | 1964.4 | 605.2 | 14.26 | 1.574 |
| 2 | 1968.7 | 605.4 | 14.63 | 1.554 |
| 3 | 1970.6 | 605.5 | 15.00 | 1.542 |
| 4 | 1970.5 | 605.2 | 14.69 | 1.543 |
| 6 | 1973.2 | 605.1 | 15.05 | 1.532 |
| 8 | 1976.1 | 605.1 | 14.65 | 1.540 |
| 12 | 1976.3 | 605.1 | 14.89 | 1.536 |
| 16 | 1976.8 | 605.3 | 14.88 | 1.538 |
| 20 | 1978.7 | 605.2 | 15.18 | 1.538 |
| 23 | 1978.7 | 605.1 | 14.80 | 1.540 |

**+20.6 MB lands in hours 0–8. +2.7 MB lands in hours 8–24.**

## Same verdict formula, warm-up excluded

The gate's own storm-resistant first/last-5%-median formula, re-applied to
progressively later start points:

| window | rss slope | accounted slope | verdict |
|---|---|---|---|
| whole run (as stamped) | **+1.145%/24h** | +0.233% | FAIL |
| drop first 1 h | +0.730% | −0.021% | FAIL |
| drop first 2 h | +0.539% | −0.059% | FAIL |
| drop first 3 h | +0.467% | −0.065% | PASS |
| drop first 4 h | +0.496% | −0.014% | PASS |
| drop first 6 h | +0.365% | +0.003% | PASS |
| drop first 8 h | +0.204% | +0.004% | PASS |
| drop first 12 h | +0.228% | +0.006% | PASS |

Independent check, OLS regression rather than endpoint medians:
hr6→24 **+0.305%/24h**, hr12→24 **+0.338%/24h**.

Plateau noise floor (hr12+): stdev 4.0 MB, peak-to-peak 22.6 MB — i.e. the
16 h drift (+2.7 MB) is smaller than one sample-to-sample excursion, and the
OLS residual slope (+6.7 MB/24 h) is inside ~1.7σ.

## What this decomposes to

`used_memory` (accounted live bytes) is **flat to within ±0.1%** across the
entire run: 605.2 MB at hour 1, 605.1 MB at hour 23. `docs_live` 14,235 →
14,240. `doc_resident_bytes` pinned at 80 MiB throughout. So the +22.4 MB is
**not live data growth**. It is unaccounted resident bytes — allocator /
arena / region high-water — and it stops accruing by hour 8.

The readiness-doc F9 disposition pre-authorized two branches:
"accounted flat while RSS grows ⇒ leak hunt" and "accounted tracks RSS to a
plateau ⇒ steady-state-window disposition ADR". **This run is neither**: the
accounted series is flat *and* RSS reaches a plateau. F9's leak-hunt branch
assumed RSS keeps growing; it does not here. The disposition is therefore an
owner call, not a mechanical read of F9.

## What this analysis does NOT establish

1. **The plateau residual is small but not measurably zero.** +0.20–0.34%/24 h
   over the tail windows. Under the gate, but a real number: ~5 MB/day, which
   is ~150 MB over a 30-day deployment. This run cannot distinguish it from
   sampling noise (σ = 4 MB).
2. **The window was chosen after seeing the data.** Picking an exclusion
   window post-hoc to flip a FAIL to a PASS is precisely the narrowing §19
   forbids. Any window-based disposition must be declared *before* the run
   that binds it.
3. **Two thirds of RSS is unattributed.** End-of-run: `process_rss` 2.07 GB vs
   `used_memory` 633 MB (`mem_fragmentation_ratio` 3.28). Named contributors
   — `tiering_committed_bytes` 270 MB, `records_resident_bytes` 117 MB,
   `doc_resident_bytes` 84 MB, `wire_buffers_bytes` 16 MB — leave roughly
   1.1 GB unexplained. The §7 gate text says "RSS slope < 0.5%/24 h,
   **attribution** + (if page-cache mode) cgroup file-cache disclosed". The
   attribution half is not satisfied by this artifact regardless of the slope.

## Every other gate in this run

| gate | threshold | measured | verdict |
|---|---|---|---|
| Tiering disk bounded | ≤ 40 GiB budget | 16.77 GiB max | PASS |
| Write amplification | < 3× | 1.920× max, flat (1.53–1.64 hourly) | PASS |
| DISKFULL refusals | 0 | 0 | PASS |
| Crashes | 0 | 0 | PASS |
| Alerts | 0 | `alerts.log` empty | PASS |
| Document plane live | throughout | docs_live 14,235 → 14,240 | PASS |
| Checkpoints advancing | — | 0 → 47,871, 0 aborted | PASS |
| sqes/submit tripwire | — | median 7.7, min 7.1 | PASS |
| Governor / EPP / thermal | unchanged | `env-end.txt` PASS on all three | PASS |

## Open items this artifact raises (not gate rows)

**A. `kv_esec` error replies are unexplained.** Every one of the 287
everysec-durable KV legs reports error replies under load — 70k–246k per
300 s leg against 85–160 M ops (≈ 0.05–0.3%). `kv_mem` reports exactly zero.
§19 requires a root cause written into this artifact or the run is invalid
for claims. Most likely typed durable-path admission backpressure
(`acks_gated` reads 58.7 B at end), which would be honest and expected — but
"most likely" is not a root cause. **This must be diagnosed and recorded
here before any row from this run is cited.**

**B. `cold_read_p99_us` reports a garbage value.** `info-end.txt` carries
`cold_read_p99_us:85899345919` (= 20 × 2³² − 1, i.e. 85,899 *seconds*) while
`tiering_cold_p99_us` in the same snapshot reads 12799 µs. Both nominally
describe cold-read latency; they disagree by seven orders of magnitude, over
102 M issued reads. `info-start.txt` reads 0. This is the `ColdReads`
enqueue→delivery histogram (`cold.rs::latency_percentile_us`, INFO
`split[10]`) and it is a reporting bug, not a measurement. It sits beside a
§19-named tripwire and next to the cold-p99 §7 gate — fix before the
campaign.

**C. The binary's own provenance stamp is wrong.** `infinityd.log` line 1
reads `infinityd 147c33a (git 147c33aca509)`. `tree.txt` records the launch
tree as `3712006`, and `147c33a` is not even reachable from HEAD (it is the
pre-rebase S09/S10 commit). Cause: `bins/infinityd/build.rs` emits
`cargo::rerun-if-changed=<git-dir>/HEAD`, but on a checked-out branch
`.git/HEAD` is a symbolic ref that never changes when you commit — only
`.git/refs/heads/<branch>` does. The build script therefore never re-runs,
and the stamped SHA freezes at the last branch switch. Every artifact this
binary produces self-identifies with the wrong commit. This is an L10/§19
evidence-integrity defect and must be fixed before the binding campaign.

**D. `tiering_ram_hit_p50/p99/p999_us` all read 0** at end of run while the
cold percentiles are populated. If the RAM-hit histogram is genuinely
sub-microsecond that is plausible, but the hot-set §7 gate is *defined* on
these three fields — confirm they populate before S24 phase 4, or the gate
produces zeros and reads as a pass.

## Deviation carried from launch (unchanged)

Tiered leg ran at `SOAK_MEM_BUDGET_MB=1024` (10 GiB dataset, 40 GiB disk cap)
rather than the S23 runbook's 2048/20 GiB/80 GiB — see `run-notes.md` for the
free-space rationale. Peak tier disk was 16.77 GiB of the 40 GiB cap, so the
run had genuine compaction/demotion pressure but not the runbook's
"dataset well over box RAM" absolute sizing. Owner call at review.
