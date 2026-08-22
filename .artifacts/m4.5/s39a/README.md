# M4.5-S39a — frame fill at small groups — reference-box A/B (ADR-0089 D6) — 2026-08-22 (23:51 → 00:38 local)

**Tier: reference box (binding).** Governor `performance`, cells pinned
0,2,4,6, generator 8,10,12,14, ext4 NVMe data root, clean tree, `env-check`
OK, `fstrim` by the owner before the session (disclosed, not verified),
40 s idle before every device leg. Engine `22af6ed`; the third part
(`campaign-s39a-3.sh`: base-4, fillA-5, base-5, the S35 arm-B row) on
`ba23666`, which changes one `inf-bench` parser line — `infinityd` is
engine-identical. Every arm at `--barrier-class fua --frames-in-flight 3
--staging-mib 2`. Arm A = `--fill-window-us 1000 --fill-target-kib 16`;
arm B = arm A `+ --fill-window-always` (held barrier-carrying frames
behind in-flight ones; removed from the tree with this verdict).

## S36 row (`gate-run m4.5 --only-s36`) — baseline vs arm A, interleaved, five pairs

| report | arm | closed-loop `everysec` ops/s | CPU % of 400 | `log_padding_pct` | write-amp (milli) | offered 100k: p99 · max · parked |
|---|---|---|---|---|---|---|
| `s36-base-1` | base | 219 756 | 223 | 24.3 | 1 589 | 121 µs · 3.03 ms · 0 |
| `s36-fillA-1` | A | **304 388** | 274 | **12.6** | **1 360** | 89 µs · 2.89 ms · 0 |
| `s36-base-2` | base | 254 300 | 259 | 29.9 | 1 805 | 117 µs · 3.02 ms · 0 |
| `s36-fillA-2` | A | **313 222** | 279 | **12.5** | **1 382** | 93 µs · 3.66 ms · 0 |
| `s36-base-3` | base | 266 264 | 275 | 29.7 | 1 774 | 113 µs · 3.00 ms · 0 |
| `s36-fillA-3` | A | **304 061** | 275 | **12.5** | **1 359** | 319 ms · **413 ms** · 106 |
| `s36-fillA-4` | A | **314 514** | 285 | **12.6** | **1 387** | 95 µs · 3.10 ms · 0 |
| `s36-base-4` | base | 257 024 | 263 | 29.1 | 1 736 | 113 µs · 3.67 ms · 0 |
| `s36-fillA-5` | A | **312 526** | 281 | **12.5** | **1 355** | 97 µs · 3.10 ms · 0 |
| `s36-base-5` | base | 269 461 | 274 | 29.8 | 1 848 | 117 µs · 3.80 ms · 0 |

tmpfs control 432–476 k in every row; the 0.85× tmpfs gate stays red in
both arms (0.46–0.57 → 0.64–0.66). Closed-loop `max` 187–523 ms in every
arm (device writeback stalls under a 20 s saturating write — the S27 D6
shape). Write-stall p99 0.94–1.15 → 1.44–1.70 ms (larger frames).

**fillA-3's offered leg** caught a 413 ms device stall (106 admission
parks); the other four arm-A offered legs read 2.9–3.7 ms with a lower
p99 than every baseline leg, and the same stall class appears in every
arm's closed-loop leg and in the 08-21 S36 campaign-1 baseline offered
leg (301 ms). Cited at the median (3.1 ms); a named residual (ADR-0089
amendment).

## S35 row (`gate-run m4.5 --only-s35`) — baseline / arm A / arm B, 3 replicates, 1-cell leg interleaved

| report | arm | p50 ÷ barrier @32 (≤ 1.3) | 4c ÷ 1c p50 (≤ 1.3) | 4-cell c32 ops/s · p50 · padding | 1-cell c32 p50 · padding | c256 ops/s · padding | reads |
|---|---|---|---|---|---|---|---|
| `s35-base` | base | 1.21 PASS | 1.38 FAIL | 38.9–39.1 k · 735–751 µs · 51.6 % | 543–575 µs · 10–11 % | 19–145 k · 12.4 % | 1.58–1.63 M |
| `s35-fillA` | A | 1.21 PASS | 1.31 FAIL | 36.9–37.7 k · 735–751 µs · 51.2 % | 575 µs · 10.6–11.1 % | 113–151 k · 12.4 % | 1.60–1.63 M |
| `s35-fillB` | B | **1.89 FAIL** | 1.67 FAIL | 20.4–27.3 k · 1 087–1 119 µs · 34.8 % | 655–687 µs · 11.6 % | 121–213 k · 10.7 % | 1.60–1.61 M |

Arm A leaves the `always` row inside the replicate spread on every
column (the barrier-carrying c32 frames stay at 2.2 records / 51 %
padding — the policy never holds them). Arm B is the predicted K = 1
"two windows" shape (ADR-0089 D2) — **Rejected**, flag removed.

**The 4c/1c gate tonight:** the *baseline* K = 3 / 2 MiB row reads 1.38
(1.25–1.28 on 08-21) with the same 4-cell p50 and a faster 1-cell leg —
the gate at ≤ 1.3 moves with the 1-cell leg's drive state by one client-
histogram bucket and is red for both pairings tonight; see the review
ledger's default-K item.
