# M4.5-S30 dev-tier A/B verdict — read-driven promotion (ADR-0085)

**Tier: dev (non-binding).** i7-13700KF, 30 GB, ADATA LEGEND 700 Gen3
DRAM-less NVMe; governor `performance`, `no_turbo=1`; **no fstrim this
session (no sudo)** — interleaved arms per ratio stand in (the S31
deviation, unchanged), and the drive visibly degraded across the night
(disclosed below where it bites). Binary: engine `75bd3f8`, one binary
for both arms — the `tiered-promote-on-read` CONFIG key is the toggle
(`no` = the pre-S30 read path, proven inert by
`disabled_promotion_is_inert` + all-zero counters in every off leg).
Generators: tri-bench (family A — exploratory harness, NOT §19-valid;
trends and same-night arm deltas are the evidence) and `inf-bench`
(families B/C). Reference-box rows stay owed for every citable number.

## Family A — the finding's repro (`residency.sh`, `residency/`)

Server 4 cells pinned 0,2,4,6; tri-bench on 8,10,12,14; ns `readonly`
durable/everysec/DISK-BUDGET 20gb/direct; bulk load 4M × 1 KB; five
identical zipfian(0.99) read-only passes (400k ops, 32 conns); then
95/50/0 mix legs, same order both arms. Ratios: r4 = MEM-BUDGET
256mb/cell (4:1, the finding's shape), r2 = 512mb/cell (2:1). Cold
rate = Δ`cold_reads_issued`/ops (device reads per op; records
straddling 4 KiB frames issue >1, so >100% is legal — a different
denominator than the finding's ~30% command-lane figure; compare
trends, not scales).

| arm | pass 1 | pass 3 | pass 5 | cold-rate trend | promotions/pass |
|---|---|---|---|---|---|
| r4-off | 58,558 @ p50 .384 | 57,894 @ .378 | 58,241 @ .366 | 135.1→134.7% **flat** | 0 |
| r4-on | 127,117 @ .183 | 160,895 @ .111 | **176,882 @ .070** | 67.4→51.5% **falling** | 30.0k→7.0k decaying |
| r2-off | 60,429 @ .348 | 60,682 @ .350 | 61,174 @ .357 | ~128% **flat** | 0 |
| r2-on | 50,447 @ .203 | 160,576 @ .104 | **190,061 @ .060** | falling | 17.7k→9.4k decaying |

**H1 confirmed at both ratios.** The off arms reproduce the finding
exactly (flat throughput, flat p50, no cold-rate trend over 5 passes).
The on arms converge monotonically; pass 5 beats the finding's
write-warmed control (121,566 @ 0.051) at 3.0×/5.2× the off plateau's
throughput/p50 (r4). Promotion volume decays pass-over-pass — the
ADR-0085 D3 self-extinguishing bound, observed. Cold rate is still
descending at pass 5 (the mid-frequency zipf band warms slowly under
second-touch admission — the designed trade against one-touch
pollution). One honest transient: r2-on pass 1 ran *below* its off arm
(50.4k vs 60.4k) — the first-pass promotion burst plus demote churn is
a front-loaded warming cost; passes 2–5 repay it.

**H4 confirmed** (mix legs, beyond-RAM): 95/5 off 109.9–114.7k @ p50
.211–.225 → on 170.1–184.5k @ **.067–.076** (+55–61%, ~3× p50, p99
0.78–0.84 vs 1.09–1.11 ms); 50/50 off 45.7–60.0k → on 152.4–158.4k.
The on arms' mixed-leg cold volume drops 20–40% — writes find warmed
keys, avoiding their own cold resolves (the S31-named coupling,
measured).

**H2 (write path):** 100%-write legs are fsync/drive-state dominated
on this box (r2-off 50/50's p99 17.9 ms is the signature): r4 0%-leg
off 85.1k vs on 158.2k, r2 off 133.6k vs on 129.4k — recorded, not
cited, in either direction. `promotions=0` on every 0%-read leg (the
mechanism is disengaged by construction). The clean H2 instrument is
family B.

## Family B — the S29 gate row (`gate/1787195039-gate-run/report.md`)

`gate-run m4.5 --only-s29`, 3 replicates, env-check OK, promotion
binary (default on):

| row | S31 base2/fix (same box, prior night) | this run | verdict |
|---|---|---|---|
| tiered@256 median | 29,560 / 30,337 | 29,174 | **unchanged** |
| flat@256 median | 52,295 / 51,752 | 36,326 (spread 29.2–41.3k, p999 to 770 ms) | **collapsed — drive state** |
| parity@256 | 0.57 / 0.59 | 0.80 "PASS (dev)" | **not citable as progress** |
| slope / parity@64 / p99-ratio | 2.88–2.95 / — / ≤2.0 | 2.92 / 1.20 / 0.63 | green |

**H3 confirmed mechanically:** `promotions=0` on every leg (100% SET —
nothing to promote) and tiered throughput unchanged vs S31's arms —
promotion neither helps nor hurts this row, by counter, not by
inference. The 0.80 parity read is the *flat denominator collapsing*
on degraded drive state (no fstrim), not a tiered improvement; the
row stays owned by the reference box. New per-leg discriminators:
`cold_reads=100,329–107,493` per tiered@256 leg ≈ **0.35 cold resolves
per SET** — residual 2's magnitude, now measured in-row.

## Family C — the parity decomposition (`parity-control.sh`, `parity/`)

The gate shape (200k × 1 KiB, always, 128mb/cell, 100% SET @256,
pipeline 1) with one lever: `MUTABLE-FRACTION 999` (nothing seals ⇒
nothing goes cold ⇒ no cold resolves AND no tier-flush I/O), 3
interleaved reps/arm:

| arm | ops/s (reps) | p99 µs | cold reads | flush rounds |
|---|---|---|---|---|
| demote (gate shape) | 29,447 / 29,367 / 20,851 | 13.8k / 13.8k / 41.0k | 105–107k (rep3 78k) | 84–115 |
| nodemote | **51,921 / 52,550 / 47,620** | 7.8k / 7.6k / 25.1k | **0** | **0** |

The nodemote arm sits at the healthy flat@256 level (~52k — S31's
same-shape flat medians): **a tiered namespace whose demote pipeline is
idle serves 100%-SET at parity ≈ 1.0.** The whole parity gap is
therefore the *active demote pipeline* — the ~0.35/SET foreground cold
resolves plus the tier-flush device I/O sharing the WAL's fsync device
— and none of it is staging admission, tiered dispatch, MAINTAIN
no-op overhead, or promotion. This run does **not** separate the
cold-resolve share from the flush-device-contention share; that split
(a cold-read-QD or io-mode arm) is named for the reference-box session.
rep3 of each arm carries the night's drive-state degradation (p999
270 ms / 32 ms) — the medians are reps 1–2-dominated and the arms are
interleaved, so the relative verdict stands.

## Verdict

- **Adopted (ADR-0085): read-driven promotion closes the finding's
  workload class** — dev-tier, both ratios, monotone convergence,
  self-extinguishing volume, write path provably untouched where it
  can be proven (counters) and drive-state-dominated where it cannot.
- **Parity@256 is decomposed, not moved:** promotion is excluded by
  counter; residual 2 is measured at ~0.35 cold resolves/SET; the
  idle-pipeline control brackets the whole gap as cold-resolve +
  flush-device contention. Remediation of the cold resolve stays
  rejected as unsound (ADR-0085 D5).
- Losing arms: none — no result in this session argues against the
  merge. The r2-on pass-1 transient and the drive-state disclosures
  are recorded above.
