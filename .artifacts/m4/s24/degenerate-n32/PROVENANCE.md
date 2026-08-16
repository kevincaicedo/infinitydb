# M4-S24 phase 1 — degenerate hard sub-gate at n=32 (A/B + A/A), SALVAGED

**Read this first: these two legs produced no harness report.** Both
invocations passed `--write-amp-milli 1890` without `--campaign-note`.
That combination is refused at `bins/inf-bench/src/m4rows.rs:428` — which
sits **after** every measurement row has run and **before**
`finish_report()`. So all six leg-rows completed and printed, then the
process exited non-zero and wrote nothing. `.artifacts/m4/s24/final-n32`
and `final-aa-n32` do not exist.

What survives is the operator's terminal transcript, preserved verbatim in
`ab-transcript.txt` and `aa-transcript.txt`, plus the analysis below.
**This is weaker evidence than a harness report** and is labelled as such.
Specifically missing versus a normal artifact: the report header's
binary fingerprints, the rendered gate table, the peak-RSS deltas, and the
machine-checkable `gates.json`.

What *is* established by the transcripts themselves:

- `inf-bench env-check` printed **OK** at the top of both legs, with
  `git-dirty-tree PASS`, `cpufreq-governor PASS`, `cpufreq-epp PASS`,
  `thermal-throttle PASS`.
- Every row ran to completion in both legs — the process reached the flag
  parser, which is downstream of all three rows.
- `assert_zero_tiering()` runs per row and aborts the leg on any non-zero
  tiering counter or constructed tiered table. Neither leg aborted, so the
  memory-mode invariant held in both.

## Invocations (as run, 2026-08-16)

```
taskset -c 12-23 ./target/release/inf-bench gate-run m4 --reference-box \
    --replicates 32 --pin-start 4 \
    --infinityd-bin $HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1 \
    --baseline-bin  $HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-m3-a1ebcb9 \
    --write-amp-milli 1890 --artifacts-root .artifacts/m4/s24/final-n32

taskset -c 12-23 ./target/release/inf-bench gate-run m4 --reference-box \
    --replicates 32 --pin-start 4 \
    --infinityd-bin $HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1 \
    --baseline-bin  $HOME/.cache/inf-campaign/v0.4.0-bin/infinityd-6bd25b1 \
    --write-amp-milli 1890 --artifacts-root .artifacts/m4/s24/final-aa-n32
```

The second leg passes the **same binary in both slots** — the A/A control.
Identical code cannot regress against itself, so anything it reports is
instrument noise by construction.

## Reproducing the harness arithmetic

`analyze.py` re-implements exactly what `degenerate_row()` would have done:

| harness | file:line | rule |
|---|---|---|
| `median(v)` | `gaterun.rs:227` | `sorted(v)[len(v)//2]` — the **upper** median |
| `delta_pct(a,b)` | `m2rows.rs:41` | `(b − a) / a × 100`, a = baseline, b = m4 |
| gate value | `m4rows.rs:148-150` | `max(0, delta_pct(base_p999, m4_p999))` |

`deep.py` adds three diagnostics that the harness does not compute: local
bucket width, slot-matched deltas, and a 20 000-draw bootstrap over
replicates (seed 20260816 — fixed, no ambient randomness).

Run: `python3 analyze.py ab-transcript.txt aa-transcript.txt`
and `python3 deep.py`. Both assert 32/32 samples parsed per leg per row
and that the crossover is balanced 16/16.

## Result — the gate rows

Threshold per `m4-gates.toml` after ADR-0070 D4b: p99.9 rows are
`0.0 = same histogram bucket or better`; ops rows `≤ 1.0%`.

| row | A/B p99.9 | A/A p99.9 (null) | A/B ops | A/A ops |
|---|---|---|---|---|
| pipelined 1:10 | **0.00** (m4 −2.00%, 1 bucket better) | **0.00** (±0) | 0.16% | 0.00% |
| unpipelined 512-conn | **3.57 FAIL** (+2 buckets) | **0.00** (m4 −1.76%) | 0.00% | 0.00% |
| ttl-heavy 1:1 | **0.00** (m4 −3.03%, 2 buckets better) | **0.00** (m4 −2.94%) | 0.00% | 0.30% |

**The residue ADR-0070 D4b named is discharged.** ttl-heavy p99.9 was
+17.65% at n=6 and +7.94% at n=16; at n=32 it is **−3.03%** — the M4
binary's tail median is two buckets *better* than the M3 baseline's. It
dissolved on more samples, the way F19 and C19 did. The bootstrap says why
it could never have been adjudicated: that row's 95% CI is
**[−18.06%, +22.81%]** and its same-binary A/A CI is [−14.29%, +11.11%].
A row with a ±20% noise band cannot resolve 7.94% in either direction.

**A new residue takes its place on a different row.** unpipelined 512-conn
reads +3.57% (two 32 µs buckets at ~1.8 ms). It is not established as a
regression: its bootstrap CI is **[+0.00%, +5.36%]** — the lower bound is
zero — and the *same-binary control already failed this exact row at
2.70% (n=16)*. The observation is 1.3× the demonstrated same-binary null
on that row.

## Why the p99.9 rows behave this way — measured, not asserted

**1. One bucket is wider than the 1% bar, on every row.** Measured from
the distinct values actually emitted (n=64 per row):

| row | median | bucket width | one bucket = |
|---|---|---|---|
| pipelined | ~783 µs | 16 µs | **2.04%** |
| unpipelined | ~1855 µs | 32 µs | **1.73%** |
| ttl-heavy | ~4095 µs | 128 µs | **3.13%** |

The smallest non-zero delta the instrument can express is 1.7–3.1%
depending on the row. A 1% threshold was unreachable on all three. This
confirms ADR-0070 D4b's "~3%/bucket" with a direct measurement, and
refines it: the figure is row-dependent because bucket width doubles per
octave.

**2. The failing row is uncorrelated with the binary pair.** Six legs:

| leg | pipelined | unpipelined | ttl-heavy |
|---|---|---|---|
| A/B n=6 | 2.44 FAIL | 0.00 | 17.65 FAIL |
| A/A n=6 *(identical)* | **2.44 FAIL** | 0.00 | **2.86 FAIL** |
| A/B n=16 | 0.00 | 0.00 | 7.94 FAIL |
| A/A n=16 *(identical)* | 0.00 | **2.70 FAIL** | 0.00 |
| A/B n=32 | 0.00 | **3.57 FAIL** | 0.00 |
| A/A n=32 *(identical)* | 0.00 | 0.00 | 0.00 |

**All three rows have failed at least once under identical binaries.** A
single-leg p99.9 failure on any of them therefore carries no information
about the code under test. Note also that the A/A control is clean on all
three rows at n=32 and was not at n=6 or n=16 — more samples do stabilise
the null, which is why n=32 is the leg worth citing.

**3. The ttl-heavy row is bimodal by slot, which is why it is the noisiest.**
Within each replicate, whichever binary runs **second** pays a large tail
penalty:

| row | ran-first median | ran-second median | ratio |
|---|---|---|---|
| ttl-heavy (A/B) | 3007 µs | 4479 µs | **1.49×** |
| ttl-heavy (A/A) | 3135 µs | 4479 µs | **1.43×** |
| unpipelined (A/B) | 1823 µs | 1823 µs | 1.00× |
| pipelined (A/B) | 783 µs | 799 µs | 1.02× |

The effect is the same size in the A/A leg as in the A/B leg, so it is a
property of the box and the harness, not of either binary — most likely
device and page-cache state left behind by the first server in the pair.

The ADR-0064 D1 crossover *cancels* this in expectation: each binary
occupies each slot exactly 16 times. But it does not cancel the
**variance** it induces. Pooling 16 low-mode and 16 high-mode samples makes
the distribution bimodal, and the pooled median then sits on the boundary
between the two modes — the least stable point in the distribution. That
is the mechanism behind the ±20% CI, and behind the 17.65 → 7.94 → −3.03
march as n grew.

**A concrete improvement for the M5 instrument debt** (not applied here —
changing the estimator is an ADR, not an edit): compute the delta
*within* matched slots and combine, instead of pooling. The slot-matched
ttl-heavy deltas are +4.26% (first) and +0.00% (second) in the A/B leg —
already tighter than the pooled ±20% band, from the same samples.

## Disposition

- ttl-heavy p99.9 residue: **discharged.** It was sampling noise.
- unpipelined p99.9 at n=32: **not adjudicable** — inside the demonstrated
  same-binary null for that row, CI includes zero.
- p99.9 half of the sub-gate overall: **still unreadable by this
  instrument at n = 6, 16 and 32.** ADR-0070 D4b's reading stands and is
  strengthened; M5 inherits the finer instrument.
- ops and RSS halves: pass decisively (worst 0.30% against a 1% bar).
- **Owed:** an artifact-producing re-run. The assembly leg in
  `.artifacts/m4/s24/final-n32-assembly/` re-runs the A/B pair at n=32
  *with* `--campaign-note` and every carrier, so the campaign table cites
  a harness report rather than this salvage.

## Files

- `ab-transcript.txt` / `aa-transcript.txt` — operator terminal output, verbatim
- `analyze.py` — harness-equivalent gate arithmetic
- `deep.py` — bucket geometry, slot analysis, bootstrap
- `analysis.txt` — output of both, captured
