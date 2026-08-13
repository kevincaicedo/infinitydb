# Attempt 4 — unified soak run notes (§19)

- **Launched** 2026-08-11 16:14 as the ADR-0069 citation form:
  `SOAK_MEM_BUDGET_MB=2048 scripts/soak-unified.sh 32`, tree `23610ff`,
  env-check green at start (thermal counters 0/0 post-reboot),
  `mode.txt` = FULL (tiered plane wired, three-plane form).
- **Ended** 2026-08-12 22:35 by **operator termination at 30.35 h**, not
  by the script. The run is therefore ~1.65 h short of its declared 32 h.
  Nothing crashed: `infinityd.log` carries no fault lines, `alerts.log`
  is empty (0 bytes), and no `tier-leg-broken.txt` sentinel was written.
- **Consequence of the short run, stated before the numbers:** the
  ADR-0069 D1 steady-state window is `run − 8 h warm-up` = **22.35 h, not
  the declared 24 h**. The window rule is still applied exactly as
  declared (prospective 8 h warm-up, never fitted); it is simply 1.65 h
  narrower than the citation form. Every slope below is normalized to
  %/24 h over the window's own timestamp span, so the *rate* is
  comparable; the *duration of evidence* is not the full form.

## Verdict (replayed, `verdict-replay.txt`)

The script was killed before its verdict stage, so the verdict was
replayed offline from `samples.csv` through the unmodified in-tree
verdict body (`scripts/soak-unified.sh` lines 424–570), with the actual
span (30.35 h), `SERVER_ALIVE=1`, disk budget 80 GiB, `MODE=FULL`,
warm-up 8 h, `ALERT_LINES=0`, `TIER_BROKEN=0`.

**Result: FAIL — discharges: NOTHING**, on exactly one gate:

```
tier liveness: compact_slices 0 -> 0 over the steady window (delta 0)
  - tiered plane served no compaction slices across the steady window
```

Every other row passed, most of them with the best margins of any
attempt to date:

| row | attempt 4 | gate | vs attempt 3 |
|---|---|---|---|
| RSS slope, steady window | **+0.151 %/24 h** | < 0.5 | +0.208 → better |
| RSS slope, whole run | **+0.440 %/24 h** | (disclosure) | +0.248 → worse but still < 0.5 |
| Accounted slope, steady | **−0.001 %/24 h** | < 0.5 | +0.066 → flat |
| Attribution residual (end) | **+1.2 %** (40.8 MB of 3.35 GB) | disclosure | +0.9 % → comparable |
| Disk max | 23.90 GB | ≤ 80 GiB budget | bounded |
| Write amp max | 1.920× | < 3× | 1.92× → identical |
| DISKFULL refusals | 0 | 0 | same |
| Crashes | 0 | 0 | same |
| Checkpoints | 19,761 | > 0 | 36,814 (shorter run) |
| Doc plane | 13,407 → 14,240 live | live | live |
| **cold_reads_issued** | **0 → 99,855,922** | must advance | **0 → 0 (dead tier)** |
| **flush_slices** | **0 → 22,772** | must advance | **static** |
| **compact_slices** | **0 → 0** | must advance | 0 → 0 |

**The F17 regression is fixed and proven fixed.** Attempt 3's tiered leg
was byte-static for 31.4 of 32.1 h. Attempt 4's tier served **99.9 M cold
reads** — 850/s sustained for the whole steady window — and 22,772 flush
slices. The ADR-0071 harness repair did what it was written to do; the
tiered plane was unambiguously under load this time.

## The one failure: compaction never ran

The tier was alive, and compaction still recorded exactly zero slices
across all 10,706 samples. The shape of the run explains *what* happened
even though the bundle cannot prove *why*:

| window | disk | flush slices | cold reads | WA |
|---|---|---|---|---|
| warm-up (0–8 h) | **+23.67 GB** | **22,553** | 31.4 M | 1.474 → 1.196 |
| steady (8–30.35 h) | **+0.23 GB** | **219** (9.8/h) | 68.4 M (850/s) | 1.196 → **1.086** |

The fill wrote the dataset; then tier **writes stopped** while tier
**reads continued at full rate**. WA decaying to 1.086 is the same fact
from the accounting side: 1.0× is WAL-only, so essentially nothing was
reaching a tier file. No new tier writes ⇒ no displaced (dead) records
⇒ nothing for compaction to reclaim ⇒ `compact_slices` 0. The gate is
not misfiring; it is correctly reporting that this run did not exercise
the mechanism its story names ("24 h endurance **with compaction
active**").

### The engine is not the defect — measured, same day

A short instrumented run on the same tree (dev-tier, non-citation;
`~/.cache/inf-campaign/diag*`) drove the identical `a` workload against a
freshly filled tiered namespace at 64 MiB budget / 10× dataset:

- after fill: `dead_bytes` **368 B**, `compact_slices` **0**, WA 1.915
  (pure inserts create no dead space — correct)
- after ~2 min of the update-heavy `a` row: `dead_bytes`
  **244,195,584**, `compaction_bytes` **62,317,224**, `compact_slices`
  **16,276**, `cold_reads_issued` 338,859, WA 1.543

So the update → displacement → dead-space → compaction path **works, and
fires within two minutes** when updates reach cold records. Whatever
suppressed compaction in the soak is in the soak's workload delivery or
its scale/configuration, **not** in the compaction driver. That
distinction is the whole difference between a release blocker and a
harness finding, and it is why the diagnostic was run before writing
this note.

### What the bundle cannot answer, and the exact missing instrument

`samples.csv` carries `cold_reads_issued`, `flush_slices` and
`compact_slices` (ADR-0071 D4) but **not** `dead_bytes` or
`compact_idle_pressure` — both of which `INFO tiering` already emits,
per-namespace and node-wide. Without them the run cannot distinguish:

1. **no dead space was ever created** (updates hit RAM-resident records;
   benign, a workload/scale property), from
2. **dead space accumulated and compaction never fired** (an engine
   defect) — which the diagnostic above makes unlikely but which this
   run's own data cannot exclude.

**Named missing artifact:** `tiering_dead_bytes` and
`compact_idle_pressure` columns in `samples.csv`, plus one 30-minute
tiered leg at the soak's own scale (2048 MiB / 10×) scraping them. That
is a ~40-line sampler change and a half-hour run; it converts this
finding from "unexplained" to adjudicated.

### Leading hypothesis, recorded as a hypothesis

`YCSB_COMMON` passes a **fixed seed** (`--seed 486541350`) to *every*
tiered leg, and the run executed 145 rows. Every leg therefore replays
the identical scrambled-zipfian draw sequence over the identical
keyspace. After the first leg, the keys that leg updates already hold
their newest version near the tail, so later legs' updates plausibly
stop displacing cold records — which is precisely the observed
signature (writes stop, reads continue). If that is the cause, it is a
**harness** issue with a real consequence: a 32 h endurance soak that
replays one key sequence 145 times is not the endurance workload the
gate imagines. The fix (per-leg seed derived from the leg index, so the
blend stays reproducible from the run seed but the key stream advances)
is one line, and it needs an owner decision because it changes the
declared workload.

## Disposition

**FAIL by the run's own hardened verdict — discharges NOTHING.** Per
ADR-0071 D4 a failing run is not evidence for any gate, and this note
does not re-cut that verdict after the fact. Concretely:

- **M4 §7 endurance / memory honesty: NOT discharged.** (Third attempt in
  a row that this row survives — but for the first time the tiered plane
  was demonstrably alive throughout, so the remaining gap is narrow and
  named.)
- **M2.5 stability soak and M3 §7 doc soak: NOT re-discharged by this
  run either** — they were discharged by attempt 3 (2026-08-10) and that
  disposition stands on its own artifact; a failing run adds nothing to
  it and takes nothing from it.
- The memory series here are recorded as **corroborating disclosure**,
  not as gate evidence: RSS steady +0.151 %/24 h and accounted
  −0.001 %/24 h over 22.35 h on a live tiered plane are the strongest
  memory numbers the project has produced, and they are consistent with
  attempt 3's discharge rather than a substitute for it.

## Environment disclosures

- Tree `23610ff` at launch; the tree goes dirty *during* every soak
  because the artifact directory lives inside the repo, so each tiered
  leg self-stamps `DIRTY TREE — dev run only, output is not
  citation-grade`. That is the legs' own stamp, unchanged from prior
  attempts; the soak's gate rows are the samplers', not the legs'.
- Operator termination at 30.35 h (above). Not a crash, not an alert.
- Device: ADATA LEGEND 700 Gen3 DRAM-less (ADR-0022 D1); the cold-read
  p99 row inside the legs reads `FAIL (DEV-TIER, non-binding)` exactly as
  the D8 deferral predicts, and is not a claim either way here.
- Doc loadgen logs gzipped in place (679 MB → 3.1 MB); bundle 793 MB →
  6.5 MB.
