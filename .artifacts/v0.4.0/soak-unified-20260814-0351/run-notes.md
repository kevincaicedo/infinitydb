# Unified soak attempt 5 — run notes and §19 disclosures

**Verdict: PASS — discharges the M2.5 stability soak, the M3 §7 document
soak, and the M4 §7 memory-honesty / 24 h endurance gate.**
Full 32.14 h, not operator-terminated (attempts 3 and 4 both ended early).
Zero alerts, zero crashes, no `tier-leg-broken.txt`, no `early-abort.txt`.

- Launch: 2026-08-14 03:51 → 2026-08-15 12:00 local, `soak-unified.sh 32`
  at `SOAK_MEM_BUDGET_MB=2048`, `SOAK_MULTIPLE=10` (ADR-0069 citation
  form: 8 h declared warm-up + 24 h steady window).
- Tree: `82a81571a350e9007bea817485fb88ed79e8ebc6`; `infinityd.log`
  self-reports `infinityd 82a8157 (git 82a81571a350)` — the `build.rs`
  SHA provenance freeze confirms the binary is that commit.
- Launch mode `scoped` (systemd-run cgroup scope, ADR-0069 D3), 4 cells,
  io_uring backend, all four cells recovered clean at boot.
- 11,264 samples at a 10 s cadence, max observed interval 17 s, **zero
  gaps over 60 s** — no sampler stall.

## Disclosure 1 — `env-check` git-dirty-tree FAIL at launch (benign, named)

`env-start.txt` records `git-dirty-tree FAIL — 1 uncommitted entry`.
The entry is `?? .artifacts/v0.4.0/soak-20260814.log`: the operator's own
untracked console-log redirect. `tree.txt` carries the full
`git status --porcelain` and contains **no modified (` M`) source entry**
— the source tree was clean and the binary provenance is exact. §19
requires the dirty tree be disclosed rather than invisible; this is that
disclosure. The end-of-run probe re-fails for the documented reason the
script prints itself (the run's own artifacts are untracked by then).
Governor, EPP and thermal-throttle probes are **PASS at both ends**.

## Disclosure 2 — the measured tree carries one M4.5-era commit

HEAD at launch is `82a8157`, *"fix(m4): read_ick_counts hop fallback
recognizes BLOCK_BLOBREF (0x05), regression-pinned"*, authored under
**ADR-0073 D4 (M4.5-S00)**. It is a decoder-correctness fix on the ICK
read path, not M4.5 feature work, and it is the only commit on the
release branch beyond the M4 set. Named here so a reader who finds an
M4.5 ADR reference in a v0.4.0-alpha tag finds it **declared, not
discovered** (L10). Consequence for the release act: the tag must be cut
at `82a8157` or later, or this endurance evidence describes a binary the
release does not ship.

## Disclosure 3 — every error reply in the run is the designed `-BUSY`

The durable legs returned error replies at volume: `kv_esec`
**208,296,482**, `doc_esec` **14,071,111**, `kv_alw` **5,969**. Classified
exhaustively: **100% are `-BUSY durable log staging is full, retry`**
(`busy_retryable` equals `errors` in every leg block). This is the
explicit bounded admission refusal the design law prescribes, not an
error class — the everysec durable plane refusing admission while
saturated. Rate in context: the final `kv_esec` leg moved 96,115,367 ops
at **320,384 ops/s** with 390,323 BUSY, a **0.41%** refusal rate
(≈0.55% run-wide). `kv_mem` returned **zero** errors of any kind.

Two non-BUSY lines exist and are both teardown artifacts: one
`Connection reset by peer (os error 104)` each in `kv_mem` and `kv_esec`,
at the moment the script killed the legs at hour 32. `doc_mut` logged
2,000 `ERR ... key that doesn't exist` in its first iteration only — the
mutation pipe racing the ingest pipe at startup, self-clearing.

**Tiered rows: 156 rows across 78 legs, `errors = 0` and `nils = 0` in
every one.**

## Disclosure 4 — the reclaim series is cumulative, and the ratio line is not the engine's trigger

`tiering_dead_bytes` is **increment-only** (`inf-store/src/address_space.rs:386,687`
— `+=` only, never decremented) and `tiering_live_bytes` held **constant
at 21.98 GB** (the fill's dataset) for all 11,264 samples. So the
verdict's line

> `peak dead ratio 79.97% vs the 50.0% compaction trigger`

divides a **32 h cumulative production counter** by that counter plus a
**static gauge**. It climbs monotonically toward 100% in any healthy
long run — this run crossed 50% at hour ~12 *while compaction was
running normally* (1.56 M slices by then). It is **not** the per-file
`dead / (dead + live)` ratio the engine triggers on. See readiness F24.

This changes **nothing about this verdict**: the reclaim series is
disclosure-only by ADR-0071 D4, and the binding gate — `compact_slices`
must advance across the measured window — is directly measured
(**+9,072,308 slices**) and passes on its own terms. The defect is
latent, in the message a *future* zero-compaction run would print.

## Disclosure 5 — the hot-set instrument refused all 156 rows (expected here)

Every tiered row reports `NOT gate-eligible — separation check FAILED`:
derived memory-hit p99.9 (≈6,399 µs) exceeds server cold p50 (≈1,311 µs),
so the ADR-0071 D2 truncation cannot separate the two populations. This
is the eligibility check **working**: under the unified profile the node
is saturated (legs run `GENERATOR-LIMITED at 8 conns`) and client-side
queueing inflates the memory-hit tail past cold service. A soak was never
the hot-set gate's venue — but it is now evidence that **S24 phase 4's
reference-leg comparison must run unsaturated**, or the §7 hot-set row
stays PENDING there too.

## Gate-by-gate result

| Gate | Threshold | Measured | Verdict |
|---|---|---|---|
| RSS slope, steady window | < 0.5%/24 h | **+0.234%** | PASS |
| Accounted slope, steady window (hard sub-gate) | < 0.5%/24 h | **−0.016%** | PASS |
| Attribution residual | disclosure | +1.2% (41.1 MB on 3.34 GB) | disclosed |
| Disk bounded | ≤ 85.90 GB budget | peak **38.73 GB**, end 30.30 GB | PASS |
| Write amplification | < 3× | **1.920× max**, flat 1.45–1.50 after warm-up | PASS |
| DISKFULL refusals | 0 | **0** | PASS |
| Crashes | 0 | **0** | PASS |
| Document plane live | > 0 docs | 13,875 → 14,240 | PASS |
| Checkpoints completed | > 0 | **22,574**, `ckpts_aborted:0` | PASS |
| Tier liveness — cold reads | must advance | +119,288,727 | PASS |
| Tier liveness — flush slices | must advance | +70,431 | PASS |
| Tier liveness — compaction slices | must advance | **+9,072,308** | PASS |
| Alerts | empty | **0 lines** | PASS |

## What the run additionally establishes

- **Compaction outran attempt 2 at double the budget.** 9.59 M slices
  total (attempt 2: 6.50 M at 1024 MiB), 265 GB of user bytes node-wide
  (attempt 2: 125.8 GB). The per-leg seed advance (ADR-0064 D7) is doing
  exactly what the 20260812 shake projected.
- **Reclaim is real, not just counted.** `disk_used_bytes` is
  non-monotonic — **70 decreases** — peaking at 38.73 GB and ending at
  30.30 GB, including a 6.8 GB net reclaim over hours 28→32. Space comes
  back; the counter is not bookkeeping.
- **Compaction onset ≈ hour 5–6**, once individual files crossed their
  own dead ratio. `compact_idle_pressure` delta **0** — compaction was
  never pressured and found nothing.
- **RSS is 99.8% anonymous.** End `smaps_rollup`: RSS 3,341,076 kB,
  `Anonymous` 3,335,260 kB, `Pss_File` **3,868 kB**, `Swap` **0**. The
  ADR-0069 D3 cgroup scope kept the loadgens' file cache out; there is no
  page-cache-inflated false pass here.
- **Tiered latency did not degrade over 32 h.** Cold p99 first-quarter
  median **60.4 ms** → last-quarter **63.5 ms** (+5%) while cumulative
  dead space grew to 87 GB and 9.6 M compaction slices ran.
- **Throughput settles, it does not decay.** `ycsb-a` quartile medians
  5394 / 5300 / 5476 / 5147 ops/s (flat within a 14% relative stdev);
  `ycsb-b` 8142 / 7033 / 6918 / 6886 — one warm-up step down as the tier
  displaced the still-RAM-resident remainder, then flat to within 2%.
- **Tripwires at end:** `recv_dropped:0`, `cold_queue_full:0`,
  `tiering_tail_alloc_stalls:0`, `tiering_write_amp_undefined_ns:0`,
  `manifests_aborted:0`, `loop_iter_p999_us:247`.

## Observation carried forward, not a gate here

The saturated `kv_esec` leg shows `p50 187 µs · p99 911 µs · p999 8,959 µs
· p9999 401,407 µs` at 320 k ops/s. The deep tail belongs to a durable
everysec namespace under sustained admission refusal inside the unified
profile; it is **not** the M4 §7 foreground-protection row (which is the
tiered plane under demotion/compaction storms, S24 phase 5). Recorded so
the unified profile's tail is on the record before anyone quotes the p50.
