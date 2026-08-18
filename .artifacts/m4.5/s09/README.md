# M4.5-S09 — PartiQL compile + statement-cache rows (dev-tier)

**Non-citable dev-box evidence** (house rules: `reviews/` + the dev-box
profile). Gate-grade re-runs ride the S17/S18 campaign on the reference
box. Box state: `taskset -c 4`, governor + EPP `performance`, idle load,
2026-08-18.

Budget (plan §4.1 S09 row): **parse+compile ≤ 20 μs typical statement;
statement cache ≥ 99 % hit on the hot query mix.**

## Rows (criterion medians, 3 replicates)

| bench | rep1 | rep2 | rep3 | verdict |
|---|---|---|---|---|
| `compile_typical` (2-conjunct SELECT, index range + residual + LIMIT) | 1.2663 µs | 1.2716 µs | 1.2645 µs | **≤ 20 µs gate: 15.7× headroom; spread < 0.6 %** |
| `compile_point` (`$key = …`) | 337.7 ns | 341.8 ns | 344.4 ns | steel-thread point compile |
| `compile_reject` (`ORDER BY`) | 185.8 ns | 178.8 ns | 173.0 ns | rejection floor (uncached by design) |
| `cache_hit` (same statement) | 52.7 ns | 86.0 ns | 52.6 ns | rep2 is a wide-interval outlier (transient contention); median 52.7 ns |
| `cache_hot_mix` (20 statements round-robin) | 60.3 ns | 60.7 ns | 61.1 ns | the ≥ 99 %-hit steady state costs ~61 ns/lookup |

## The hit-rate half of the row

A rate is workload arithmetic, not a latency: the property test
`cache_hot_mix_hits_over_99_percent` (`src/partiql/cache.rs`) pins one
cold miss per distinct statement and zero evictions for any hot set
inside the default 1024-entry capacity — 20 statements × 5 000 lookups
⇒ 99.6 %+ measured in-test; steady state approaches 100 %. The bench
row above prices the hit path the rate multiplies.

## Provenance

- Bench source: `benches/partiql_compile.rs` (fixture catalog: 4 ready
  indexes incl. one multi-valued).
- Replicates: `partiql-compile-rep{1,2,3}.txt` (this directory), each a
  full fresh criterion run on the pinned core.
- Correctness evidence backing these numbers: the 303-case golden suite
  (`tests/golden/partiql_suite.txt`), the bound-vs-VM oracle
  (`tests/partiql_bounds.rs`, 10⁶ release lane incl. the two-conjunct
  fold property), COUNT paging (`tests/count_paging.rs`), fuzz smokes
  `fuzz_partiql_parse` 34,700,867 runs / 301 s and
  `fuzz_access_program` 74,799,445 runs / 301 s, both zero findings.
