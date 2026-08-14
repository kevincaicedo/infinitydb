# M4.5-S05 backfill gate artifacts (dev-tier)

Budget (plan §4.1 / S05 AC 1): backfill sustains **≥ 500k entries/s/cell
with foreground p99.9 < 2 ms in the same run** — the co-gate, both
numbers from one run.

## Method

`crates/inf-store/benches/index_backfill.rs` (custom harness): a 10M-doc
corpus written with **no** index declared, one f64 chain index
(`$.price`, one entry per document — walk rate ≡ entry rate) declared,
then plane-shaped MAINTAIN ticks (`max_docs = 1024`, the plane's
`MAX_BACKFILL_DOCS_PER_TICK`) driven to cell-`Ready` while foreground
traffic (bracketed `JSON.SET` + `GET` on corpus keys, 4 ops/slice)
races the walk between slices.

Foreground latency uses the **queued-arrival model**: each sampled op
reports (preceding slice duration + own execution) — the worst-case
latency of a command that arrived as the slice began. The p99.9 of that
distribution is the co-gate number.

Completeness is asserted on the tree (10,000,000 entries at
convergence); the walk's fresh-insert count runs ~4.8k below the corpus
because racing foreground SETs inserted those entries through the hook
first — the ADR-0077 D1 idempotent convergence, visible in the numbers.

## Runs (3 replicates, 2026-08-13)

Pinned `taskset -c 4`, governor + EPP `performance`, load < 0.35,
`ulimit -v 16 GiB`, binary `index_backfill-47a8c63d2d117b80`
(inner tree at S05 head). Files: `index_backfill-r{1,2,3}.txt`.
(A discarded first invocation ran a prior binary whose completeness
assert compared the walk's fresh-insert count to the corpus — wrong by
exactly the raced share explained above; the assert now binds tree
cardinality and r1–r3 are the corrected binary.)

| replicate | walk entries/s | wall entries/s | fg p50 | fg p99 | fg p99.9 | fg max | slice max |
|-----------|---------------:|---------------:|-------:|-------:|---------:|-------:|----------:|
| r1 | 1,737,901 | 1,726,316 | 0.604 ms | 0.705 ms | **0.904 ms** | 6.047 ms | 6.045 ms |
| r2 | 1,735,862 | 1,724,314 | 0.605 ms | 0.707 ms | **0.932 ms** | 6.144 ms | 6.142 ms |
| r3 | 1,731,284 | 1,719,768 | 0.606 ms | 0.742 ms | **0.965 ms** | 5.951 ms | 5.950 ms |

**Verdict: PASS ×3** — walk-time rate 3.46–3.48× the 500k gate (spread
< 0.4%), foreground p99.9 at 0.90–0.97 ms vs the 2 ms bound. Wall-clock
rate (walk + interleaved foreground) also clears the gate on its own.

## Disclosures

- **One ~6 ms slice per run** (consistent across replicates — the max
  sample in both the slice and foreground distributions). Suspected
  tree-pool doubling at large capacity; it sits beyond p99.9
  (1/9721 slices) so the co-gate holds, but a command arriving at that
  slice would see ~6 ms. Named follow-up lever (not built): pre-size the
  tree from the store's live count at job start. Re-enters if S17's
  latency rows need it.
- Dev-tier numbers (this box): non-citable per the evidence rules;
  reference-box rows bind at S17.
- Corpus docs are small (~93 B attributed each); the S17 gate workload
  binds the 1 KiB shape.
