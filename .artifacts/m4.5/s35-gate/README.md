# M4.5-S35 — reference-box campaign (ADR-0087 D8) — 2026-08-21

**Tier: reference box (binding).** ADR-0022 D1 box (i7-13700KF, ADATA
LEGEND 700 DRAM-less Gen3, ext4 on `nvme0n1p3`, kernel 7.0.0-30),
governor `performance` / EPP `performance` (set by the owner), cells pinned
0,2,4,6 (`--pin-start 0`), the load generator pinned 8,10,12,14
(`taskset` on `inf-bench` — `gate-run m4.5` has a pin flag, so the outer
affinity never reaches the cells), data root on the device
(`~/bench-data/s35-gate/data`, ext4), `env-check` OK on every invocation
(clean tree). **Not run: `fstrim`** (sudo unavailable to the agent); the
S34/S35 drive-state rule was applied as 40 s of idle before every durable
leg. Server binary `infinityd 0f990be` (the S35 tree + the tooling fix)
in every arm; `inf-bench` `0f990be` (campaign 1) / `23393f3` (campaign 2,
the fill-free row).

| dir | what |
|---|---|
| `campaign.sh` / `campaign2.sh` | the drivers (rotated arm order: 1 = k4→k1→k3s2, 2 = k3s2→k4→k1) |
| `artifacts/campaign.log`, `artifacts/{k1,k3s2,k4}/…/report.md` | **campaign 1** — `gate-run m4.5 --only-s35` per arm, the S35 row *with* a 200k-key fill before the AC leg (a measurement flaw: the fill's 64 × 4-in-flight frames lift the whole-session barrier histogram — visible as the 1-cell `p50/barrier = 0.97`; the ratio gate is lenient by ≤ one histogram bucket in that campaign) |
| `artifacts/{k4,k1,k3s2}-m2always/…/report.md` | **campaign 1** — `gate-run m2 --only-always` per arm on the device: the ADR-0022 D3 `always ≥ 300k w/s` gate row (64 conns × P16) |
| `artifacts2/campaign.log`, `artifacts2/{k3s2,k4,k1}/…/report.md` | **campaign 2** — the fill-free row (`23393f3`): the AC leg runs first on a fresh server, so the barrier histogram holds only its own frames; every leg reports group size (`acks/fsync`), frames, parked admissions — **the binding set** |

## Arms (same binary, `--barrier-class fua` everywhere)

| arm | flags | staging resident/cell |
|---|---|---|
| `k1` | `--frames-in-flight 1 --staging-mib 4` | 8 MiB — S34's arm, the baseline |
| `k3s2` | `--frames-in-flight 3 --staging-mib 2` | 8 MiB — the L5-neutral reference arm (record bound 2 MiB − 56 B) |
| `k4` | `--frames-in-flight 4 --staging-mib 4` | 20 MiB (+12 MiB/cell attributed, `log_staging_bytes`) |

## Gate verdicts (`m4.5-gates.toml`, `s35_*` rows; medians of 3)

| arm | campaign 1 p50/barrier · 4c/1c | campaign 2 (fill-free) p50/barrier · 4c/1c |
|---|---|---|
| k1 | 1.95 **FAIL** · 1.50 **FAIL** | 1.89 **FAIL** · 1.46 **FAIL** |
| **k3s2** | **1.19 PASS · 1.25 PASS** | **1.21 PASS · 1.28 PASS** |
| k4 | 1.12 PASS · 1.28 PASS | 1.14 PASS · 1.31 FAIL (device-state legs, below) |

K = 1 is the expected baseline failure (ADR-0086's "two windows"); the
gates discriminate. K = 3 / 2 MiB passes both gates in both campaigns.
K = 4's campaign-2 4-cell AC legs were all degraded by drive state (two
with a device barrier p99 of 16–17 ms — flagged by the row — and one at
9.3 k ops/s with a 121 ms client p99 against a 1.6 ms device p99, a shape
the row now flags as "engine tail", `write_stall_p99_us` per leg added
after this campaign); its campaign-1 verdicts stand, its campaign-2 ratio
is contaminated and not cited.

## The binding numbers (campaign 2, 4 cells, 32 conns closed-loop, 100 % write, `always`, 1 KiB)

| arm | ops/s | p50 ms | p99 ms | max ms | barrier p50 µs | p50 ÷ barrier | acks/fsync | frames/leg |
|---|---|---|---|---|---|---|---|---|
| k1 | 27.1–28.4k | 1.09–1.12 | 2.56 | 10.5–108 | 591 | 1.84–1.89 | 4.3 | 70–73k |
| **k3s2** | **39.2–39.5k** | **0.735** | **1.92–1.95** | 10.9–11.3 | 607–623 | **1.18–1.21** | 2.2 | 193–197k |
| k4 | 9.3–29.5k (device-state) | 0.75–0.78 | 16–121 | 30–186 | 655–687 | 1.12–1.15 | 2.1 | 45–156k |

1 cell, 32 conns: k1 39.3–39.6k · 0.767–0.783 ms · barrier 383 µs ·
p50/barrier 2.00–2.04 · 16 records/frame; k3s2 49.3–50.6k · 0.575 ms ·
barrier 559–575 · 1.00–1.03 · 16–18 records/frame; k4 49.5–50.9k · 0.575 ·
0.97–1.03 · 15.3–15.7. **4c/1c p50: k1 1.46, k3s2 1.28, k4 1.31**.

Reads (64 conns × P16, 100 % GET over the written keyspace): k1
1.58–1.59 M ops/s · p99.9 1.47–1.60 ms; k3s2 1.58–1.60 M · 1.50–1.73;
k4 1.58–1.59 M · 1.47–1.50 — **within ±1 % across arms** (the ±2 % AC).

256 conns (the `max` row): k1 134–175 k ops/s · p99 3.1–18.4 ms · max
45–344 ms · 33.8 records/frame; k3s2 158–198 k · 2.9–3.5 · 45–77 · 14.8;
k4 187–208 k · 2.9–5.6 · 37–69 · 12.2. The @256 tail is bimodal in every
arm (drive state); no arm is consistently worse than K = 1 — campaign 1's
K = 4 @256 reading (127–131 k, p99 24–26 ms) did **not** reproduce. The
group-size counters show the mechanism the seal pacer (ADR-0088 D2b)
targets — K > 1 issues 2–3× the barriers per second at 256 conns — and
the device sustained it here (~16 k frames/s aggregate, p99 2.9 ms).

## `gate-run m2 --only-always` (campaign 1; 64 conns × P16 pipelined, 10 s, one replicate per arm)

| arm | ops/s | p50 | p99 | p99.9 | max | gate `always ≥ 300k w/s` |
|---|---|---|---|---|---|---|
| k1 | 376 k | 1.79 ms | 27.6 ms | 33.8 | 49.8 | **PASS (binding)** |
| **k3s2** | **639 k** | **1.47** | **3.65** | **5.5** | **14.8** | **PASS (binding)** |
| k4 | 613 k | 1.47 | 3.65 | 12.0 | 212 | PASS (binding) |

The ADR-0022 D3 `always` gate — `Evidence-pending (Gen4)` since M2 — is
**met on the reference box under the FUA class at every K**, with one
replicate per arm (the campaign's shape); a 3-replicate confirmation is
owed before it is carried into the claim ledger.

## Limitations, stated

- No `fstrim` between arms (sudo); the 40 s idle rule was applied. The
  drive-state bad mode still hit campaign 2's K = 4 arm (all three 4-cell
  AC legs) and single @256 legs elsewhere — every affected leg is marked
  in the reports' notes and in the tables above.
- The @256 `max` row is device-state dominated in every arm; it neither
  confirms nor refutes a K effect on the tail.
- The m2 `always` rows are single replicates.
- Campaign 1's barrier histogram includes the fill's frames (lenient by
  ≤ one bucket); campaign 2 is the binding set.
- The three `s35-k{1,3,4}` reports under `.artifacts/m4.5/` dated
  2026-08-21 13:13–13:23 ran on tmpfs (the `m2` flow read
  `--pressure-data-root`, not `--data-root`) and are **invalid for any
  device claim** — kept with a README saying so; the tooling now refuses
  that shape (`0f990be`).

## Campaign "review" (2026-08-21 evening, `campaign-review/`, binary `2cb6074` — the review-fix tree)

Same box, governor `performance` / EPP `performance`, clean tree,
`env-check` OK; **`fstrim` run by the owner before this campaign** (a
manual run leaves no journal entry — disclosed, not verified by the
agent); 40 s idle before every durable leg. Two purposes: the three
replicates of the K = 3 / 2 MiB `m2` `always` row the claim ledger owed,
and the **K = 3 / 4 MiB** arm the default-K decision lacked.

### `gate-run m2 --only-always`, K = 3 / 2 MiB, three replicates (64 conns × P16)

| replicate | w/s | p50 | p99 | p99.9 | max | verdict (≥ 300k) |
|---|---|---|---|---|---|---|
| r1 | **634 776** | 1.50 ms | 3.8 ms | 6.5 ms | 14.1 ms | PASS |
| r2 | **541 119** | 1.50 ms | 17.4 ms | 31.7 ms | 56.9 ms | PASS |
| r3 | **572 150** | 1.54 ms | 5.6 ms | 29.2 ms | 69.6 ms | PASS |

Median 572k, min 541k — with campaign 1's 639k, four reference-box
readings of this arm, every one ≥ 1.8× the ADR-0022 D3 gate. p50 is flat
across replicates; the p99/max spread is the device's write-through tail
(the S34 drive-state mode: r2's barrier histogram carries a 17 ms p99),
which `fstrim` did not remove. The ADR-0022 D3 `always ≥ 300k w/s` gate is
**met on the reference box with three replicates** (claim ledger C18).

### `gate-run m4.5 --only-s35`, K = 3 / 4 MiB (`k3s4`, 20 MiB/cell resident: +12 MiB attributed)

| gate | measured | verdict | note |
|---|---|---|---|
| p50 ÷ barrier (≤ 1.3) | **1.21** (1.17 / 1.21 / 1.24) | PASS | identical to K = 3 / 2 MiB (1.19–1.21) — the buffer size does not enter at 2.2 records per frame |
| 4c/1c p50 (≤ 1.3) | 1.38 | FAIL — **device row** | the 4-cell legs read p50 735–767 µs, the same as the 2 MiB arm's 735; the 1-cell legs read **543 µs** (barrier p50 431–471 µs) against campaign 2's 575 (barrier 559–575): a faster device for the 1-cell legs, not a slower engine at 4 cells. The harness flagged one leg's barrier p99 at 16 ms (rep2 @256: 103k ops/s, p99 25.6 ms) — "a device row, not an engine row; re-run with fstrim + a longer idle before citing" |
| reads | 1.58–1.60 M ops/s | ±1 % | (rep2 read leg 5 457 nils — the @256 leg before it ran 103k under the bad mode and populated fewer keys) |
| @256 | 201k / 201k / 103k; p99 2.9 / 2.9 / 25.6 ms | — | bimodal again, the third repeat in the bad mode |

**Reading for the default-K decision:** K = 3 behaves the same at 2 MiB
and 4 MiB buffers on every engine-attributable figure (4-cell p50 735 µs,
39.1–39.3k ops/s at 32 conns, 1.21 × barrier). The 4 MiB pairing keeps
the durable record bound at **4 MiB − 56 B** for +8 MiB/cell resident
over the 2 MiB pairing (12 MiB over K = 1's 8 MiB); the 2 MiB pairing is
L5-neutral and moves the bound to 2 MiB − 56 B (now disclosed in the
compat matrix). The 4c/1c gate's 1.38 on this arm is a device-state
reading and is not cited; the arm's p50/barrier gate is.
