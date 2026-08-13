# M4.5-S02 — typed index-key encoding budget artifacts

**Tier: dev — non-citable.** i7-13700KF (hybrid; bench pinned
`taskset -c 4`, a P-core), governor `performance`, EPP `performance`.
`cargo bench -p inf-store --bench index_key`, 100k-value corpora, 15
rounds per row, medians; 3 replicates (`index-key-rep{1,2,3}.txt`),
spread < 4% on every row (< 1% on the numeric rows). Working tree of
the S02 session (see the ledger entry for the commit). The §4.1 budget
is **encode ≤ 30 ns typical scalar**.

| row | rep1 | rep2 | rep3 | budget verdict |
|---|---|---|---|---|
| encode_i64 | 10.07 | 10.07 | 10.07 | ✓ ≤ 30 |
| encode_f64 | 10.38 | 10.38 | 10.38 | ✓ |
| encode_f64_from_i64 (coerced arm) | 5.90 | 5.89 | 5.90 | ✓ |
| encode_bool | 10.05 | 10.02 | 10.06 | ✓ |
| encode_utf8_16 (fast path) | 12.62 | 12.58 | 12.63 | ✓ |
| encode_utf8_64 (fast path) | 31.34 | 31.38 | 31.33 | disclosed¹ |
| encode_utf8_16_nul (escape slow path) | 20.05 | 19.33 | 19.60 | ✓ |
| compare_i64_f64 (informational) | 10.05 | 10.03 | 10.14 | — |

¹ A 64 B string is not the budget's "typical scalar" (the §7 gate
corpus indexes numerics and short strings); the row is reported for
honesty — string encode is byte-throughput bound (~0.5 ns/B: scan +
copy + terminator) and scales linearly, not a fixed-cost regression.
The escape slow path (NUL every 5th byte) costs ~7 ns over the 16 B
fast path.

Numbers include the full harness path (corpus load, `black_box`, the
checksum fold) — they are upper bounds on the bare encode. No row
required optimization work; no A/B alternatives were in play (the
layout is fixed by ADR-0074; this artifact proves the budget, it does
not pick a design).
