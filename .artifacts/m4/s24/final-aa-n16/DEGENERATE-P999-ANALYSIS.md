# The degenerate hard sub-gate's p99.9 rows cannot be adjudicated against a 1% bar

Four legs, 2026-08-15, same binary set, quiet box, env-check green.
"A/A" means the **same binary in both slots** — any non-zero delta it
reports is instrument, by construction.

| leg | pipelined p99.9 | unpipelined p99.9 | ttl-heavy p99.9 | ops rows | peak RSS |
|---|---|---|---|---|---|
| A/B n=6 | **2.44 FAIL** | 0.00 | **17.65 FAIL** | 0.11 / 0.10 / 0.79 | 0.42 |
| A/A n=6 (identical binaries) | **2.44 FAIL** | 0.00 | **2.86 FAIL** | 0.14 / 0.00 / 0.00 | 0.22 |
| A/B n=16 | 0.00 | 0.00 | **7.94 FAIL** | 0.00 / 0.13 / 0.75 | 0.49 |
| A/A n=16 (identical binaries) | 0.00 | **2.70 FAIL** | 0.00 | 0.00 / 0.00 / 0.00 | 0.28 |

## What the controls prove

**The same-binary control fails a p99.9 row in both of its runs** — at
n=6 on pipelined (2.44%, reproducing the A/B's value *exactly*) and
ttl-heavy (2.86%), and at n=16 on unpipelined (2.70%). *Which* row fails
moves between legs. Identical code cannot regress against itself, so the
≤ 1% p99.9 sub-gate is measuring the instrument, not the binary.

**The mechanism is structural, not statistical.** The percentiles come
from a `LogHistogram` with 32 sub-buckets per octave ≈ **3% per bucket**;
the report line says so itself ("nonzero spans >= 1 bucket"). The smallest
delta the instrument can express other than zero is one bucket, ≈ 2.4–3%
— **already above the 1% threshold**. The row therefore has only two
reachable states: `0.00 PASS` (both medians in the same bucket) or a value
that fails its own bar. There is no measurable "non-zero but passing"
state. A 1% threshold on a 3%-quantised measurement is not a strict gate;
it is an unreadable one.

This is what M4-S24 phase 1 concluded on 2026-08-10/11 in different
words, when it adjudicated PASS on "p99.9 same-bucket-or-better every
row" after an A/A control reproduced same-binary deltas of 2.44% and
5.88%.

## What still deserves an answer

Read on bucket identity — the only reading the instrument supports —
tonight's A/B at n=16 is same-bucket on pipelined and unpipelined, and
**ttl-heavy sits at 7.94% ≈ 2.6 buckets**. No same-binary control tonight
exceeded ~1 bucket. So ttl-heavy p99.9 is the one residue that is larger
than any observed instrument excursion, while that same row's *throughput*
(0.75%) and the node's *peak RSS* (0.49%) reproduce comfortably, and the
tiering-counter audits are identically zero.

A tail regression of 2.6 buckets with no throughput and no memory cost, on
a memory-mode namespace whose tiering code paths are asserted zero, is
not impossible — but it is also exactly the shape two other rows took
tonight before higher n dissolved them (F19's `sqes/submit`, C19's
everysec penalty). It halved from 17.65% at n=6 to 7.94% at n=16, which
is the signature of a statistic still converging.

## Recommendation (owner decision — not an agent's to take)

**Do not record the hard sub-gate as passed on tonight's p99.9 rows, and
do not record it as failed either.** The ops, RSS and zero-counter rows
pass decisively and those are the rows that carry the sub-gate's actual
meaning (memory-mode is the identical instruction path). For the p99.9
half, two honest options:

1. **Express the p99.9 sub-gate in bucket terms** — "same bucket or
   better" — by ADR, which is what phase 1 already did in practice and
   what the instrument can actually measure. This is a *re-expression to
   match the instrument's resolution*, not a narrowing of the threshold's
   intent, and it must say so explicitly with this table as its evidence.
2. **Fund a finer tail instrument** (more sub-buckets per octave, or an
   exact-quantile reservoir for the A/B rows only) and re-run. Real
   engineering, and the better long-term answer.

Either way, **ttl-heavy p99.9 is owed one more higher-n A/B + A/A pair**
before the tag, because it is the only row whose residue exceeds the
observed instrument floor.
