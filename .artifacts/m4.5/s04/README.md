# M4.5-S04 — maintenance-hook budget + prune A/B (dev-tier)

Date: 2026-08-13 · Box: dev (24-core, see `reviews/` dev-box profile) ·
Pinned `taskset -c 4` · governor/EPP `performance` · load < 0.25 ·
`cargo bench -p inf-store --bench index_maint` (release, medians over 15
rounds × 20k ops; replicates `index_maint-r{1,2,3}.txt`).

**Dev-tier, non-citable** (the house evidence rule): these rows prove the
§4.1 budget and the prune's contribution; binding reference-box rows are
S17/S18's.

## Rows (replicate medians, spread < 1%)

| row | r1 | r2 | r3 | meaning |
|---|---|---|---|---|
| `json_set_zero_index` | 44.4 | 44.8 | 45.5 ns | root `JSON.SET` (~60 B doc), no indexes anywhere — the baseline and the degenerate-case row |
| `json_set_one_index_10m` | 509.9 | 509.4 | 512.7 ns | the same mutation through the bracket against one f64 index holding 10,000,001 entries — each op is exactly one (remove + insert) pair through the whole hook |
| **`maintenance_pair_at_10m`** | **465.6** | **464.6** | **467.3 ns** | the difference — **≤ 600 ns/pair budget: met** (§4.1 row; disposition Accepted) |
| `bracket_4idx_unpruned` | 1935.5 | 1936.6 | 1950.6 ns | bracket around a no-op mutation, four indexes (`$.price` f64, `$.name` utf8, `$.qty` i64, `$.tags[*]` utf8 wildcard): both evaluations per index, empty diff |
| `bracket_4idx_pruned` | 111.1 | 110.9 | 111.4 ns | the same bracket with a provably-disjoint `$.other` mutation path — the static prune skips every evaluation (ADR-0076 D6) |
| `prune_delta` | 17.4× | 17.5× | 17.5× | the prune's contribution (§4.1: reported beside the unpruned row so the write-overhead gate number is an artifact, not folklore) |

## Reading

- The pair cost (465 ns) includes peek, path evaluation, encode,
  reservation arithmetic, the sorted diff, and both tree ops at 10M
  tree entries — inside the 600 ns budget with ~22% headroom.
- The pruned 4-index bracket costs ~28 ns/index — the "~one branch per
  index" §4.1 arithmetic (a decoded-step comparison, no evaluation, no
  encode). Root-set mutations pay the unpruned chain by design; the S17
  gate workload must state its root-set/path-mutation mix (plan §4.1).
- Zero-index namespaces measure byte-identical to the M4 baseline shape
  (one cached branch — see `asm-zero-index.md` in this directory).
