# Unified soak attempt 2 — operator notes (2026-08-07 02:27 EDT)

Instrument: `scripts/soak-unified.sh 24` (ADR-0066 D1). Mode probe: **FULL**
— S26 wiring landed, so the tiered leg is driven for real and the verdict
stamp will claim all three planes (M2.5 stability, M3 §7 doc soak, M4 §7
memory honesty). This is the F9 re-run: attempt 1
(`soak-unified-20260805-0340`) FAILED at +0.848 %/24 h with no accounted
series to attribute it; this run carries the `used_memory_kb` column.

Launch tree: `3712006` (S27 tip), clean at launch, `env-check: OK`
(governor `performance`, EPP + thermal probes PASS) — see `env-start.txt`
and `tree.txt`.

## Deviation to disclose — tiered leg scale

| | S23 runbook gate scale | this run |
|---|---|---|
| `SOAK_MEM_BUDGET_MB` | 2048 | **1024** |
| `SOAK_MULTIPLE` | 10 | 10 |
| dataset | 20 GiB | **10 GiB** |
| disk budget cap | 80 GiB | **40 GiB** |

**Why:** `/` had 82 GB free at launch (`infinitydb/target` alone is 64 GB).
The runbook's 80 GiB disk-budget cap exceeds free space, so a compaction lag
would hit filesystem ENOSPC *outside* the S21 admission machinery — the
failure mode that kills the night rather than reporting on it. 1024 caps the
namespace at 40 GiB with ~24 GiB expected steady use (the 1 h shake ran
2.4× dataset), leaving ~45 GB headroom for an unattended run.

**What this preserves:** the 10× budget:dataset ratio the gate names, the
wired plane (demotion, compaction, cold reads, blob reclaim), and every
threshold. **What it does not:** the runbook's "dataset well over box RAM"
absolute sizing — the 10 GiB dataset sits under the box's 30 GiB. Tier reads
are `Direct` by default (ADR-0054), so page cache is not in the tier read
path and the S09 disclosure rule does not bite; the residual question is
whether 10 GiB exercises enough address space / index pressure to stand as
the §7 memory-honesty scale. **Owner call at review: accept this scale for
the M4 §7 row, or re-run item 15 at 2048 after freeing disk.** The M2.5 and
M3 §7 gates are unaffected by tier scale.

Owner decision recorded at launch (2026-08-07): run at 1024 tonight rather
than lose the night, disclose here, decide the M4 row at review.

## Timing

- 02:27:18 launch · 02:29:12 tiered fill complete (10,485,760 keys)
- The 24 h window starts at fill completion → **ends ≈ 2026-08-08 02:29**,
  verdict written a few seconds later to `verdict.txt`.
- Sampler runs from 02:27, so the first ~2 min of samples cover the fill.
  At 10 s cadence the first-5 % median window is ~72 min, so fill churn is a
  small fraction of it — but if the slope lands near the bar, check whether
  the first-5 % median is depressed by fill-time RSS before reading it as
  growth.

## First 6 minutes (all green)

`vmrss ~1.99 GB stable · docs_live 14,240 · tier disk 10.16 GB of 40 GiB ·
WA 1.897× (gate < 3) · DISKFULL refusals 0 · ckpts advancing (124 by 02:32)
· sqes/submit 7.2 · alerts.log empty`

## Root cause — durable-leg error replies (kv_esec 31.26 M, kv_alw 2,165, kv_mem 0) — added 2026-08-08 (§19 excursion rule)

Every durable-leg error reply is the single typed refusal
`-BUSY durable log staging is full, retry` (`STAGING_BUSY_ERROR`,
`crates/inf-server/src/durable.rs:46`), reproduced verbatim post-run on a
single node driven with this soak's esec shape (40 s repro, 124
occurrences of this one string and no other). Mechanism: named-namespace
writes are slot-routed per key; with 4 cells ~75% take the fabric `ApplyNs`
path, whose owner-side admission (`plane.rs:806-828`) cannot suspend inside
the fabric drain and answers a typed retryable refusal when the cell's
4 MiB staging buffer fills while the previous frame's LogWrite is still in
flight (locally-owned keys park on the drain waitlist instead —
`plane.rs:4915-4936`). Refusal windows open when device contention (tier
flush/compaction + checkpoint streaming + fsync storms; fsync p999 516 ms
this run) delays frame-write completions beyond the ~120 ms it takes esec
inflow to fill the second buffer. Implied staging-full duty cycle ≈ 0.2%.

Asymmetry is structural: esec (0.157% of SETs) fires into stall windows at
full rate because everysec acks never gate the client; alw (0.00093%) is
ack-gated, so its connections are parked during precisely the backed-up
windows and re-issue only after the commit path drains; mem (0) never runs
durable admission. Legs 1–2 (246 k / 218 k errors vs 108 k steady mean)
coincide with the 10 GiB tier fill (first ~115 s, samples.csv disk ramp)
plus the first cold-working-set YCSB-A leg and node warm-up — the run's
one-time peak device contention (leg-1 p9999 368 ms, max 1.45 s, lowest
throughput).

Verdict for this excursion: honest bounded backpressure (typed refusal
before execution; no effect applied, no ack contract violated) — not a
stability finding, and it does not change any of this run's gate verdicts
(the run's FAIL is the RSS slope, see `slope-analysis.md`). Follow-ups
filed in the readiness doc (F14): (1) ADR-0015 D6 says "no error surfaced
for transient pressure" — the fabric-path refusal deviates and takes an
ADR amendment; (2) no INFO counter records these refusals
(`StagingStats.refusals` is neither incremented on the `would_fit`
pre-check path nor surfaced) — 31.3 M client-visible refusals left zero
server-side evidence; `log_admission_busy` is being added; (3) inf-bench
prints only an error count, no strings — this diagnosis required post-hoc
reproduction; error-string sampling is being added.

## RSS attribution — the "unattributed ~1.1 GiB" resolved — added 2026-08-08

See `rss-attribution.md` in this bundle: the gap was an INFO scope trap
(Memory section is node-scope, Tiering section is cell-scope — the tier
region and tiered index are per-cell terms, ×4 cells = 1.37 GiB). The
bottom-up model reconciles end-of-run RSS to within 16.4 MB (0.8%).

## Box state during the run

`redis-server` (the compat oracle) is idle on 6379. VS Code + rust-analyzer
(2.7 GB RSS) and Chrome were resident at launch and swap was already
6.2/8 GB used — the runbook wants the box otherwise idle. Server cells pin
from cpu 4, load generators are taskset to 12–23, so cpus 0–3 and 8–11
absorb the desktop; the RSS gate reads `infinityd`'s own `/proc` and is not
polluted by other processes, but memory pressure that pushes the node into
swap would be. If the slope fails, check this first.
