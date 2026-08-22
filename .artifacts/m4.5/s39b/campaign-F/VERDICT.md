# S39b campaign F verdict

The corrected first-boot recovery gate failed, so the one-slot product default
is not ratified and returns to zero. Recycling remains available explicitly as
`--segment-recycle-slots 1`.

The three paired first-boot recovery results were:

| replicate | order | baseline | one slot | arm / baseline |
|---|---|---:|---:|---:|
| 0 | baseline, arm | 3.925 s | 3.535 s | 0.901x |
| 1 | arm, baseline | 3.601 s | 5.225 s | 1.451x |
| 2 | baseline, arm | 3.834 s | 4.957 s | 1.293x |

The predeclared paired median is **1.293x**, above the `<= 1.05` ratification
limit. Every first boot completed, and the correctness suites were green, but
the recovery performance condition is binding. Campaigns D/E's recovery
numbers remain withdrawn because they timed a second boot of the same image.

The mechanism's benefit remains real but opt-in: the warmed median accounted
host-write ratio fell from about 2.20 to 1.42 bytes per log-frame byte (about
35% lower), p50 was 0.97x baseline, and p99 was 0.20x baseline. The warmed
zero-fill share was 0.22, still above the ADR's 0.10 target. Read throughput's
median ratio rounded to 0.98 but fell just below its binding `>= 0.98` gate.

The gate-run process exited nonzero because recovery, warmed zero-fill, and the
read control were red. This was an expected possible campaign outcome, not a
harness failure. The clean-tree environment gate passed and the row completed
all six legs.
