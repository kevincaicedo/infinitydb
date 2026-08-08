# M3-S25 server-side json_set slice — opening profile (2026-07-17, dev tier)

Environment: `env.txt` (i7-13700KF, performance governor + EPP,
`no_turbo=1`; server cores 0-7 = P-cores 0-3 both hyperthreads → 4
cells; memtier cores 12-23). Desktop session live (chrome/gnome sampled
at ≤ 0.5% total — disclosed). Nothing citable (dev tier, L10).

## Wire rows this session (gate shape, blessed 4t/25c/pipe16 config)

| Row | ops/s | Note |
|---|---|---|
| set-1k (profile-run, 3 reps) | 1,852,074 mean, rsd 1.4% | fresh server |
| jset-1k (profile-run, 3 reps) | 1,204,038 mean, rsd 8.9% | rep 3 collapsed to 1.080M (disclosed below) |
| set-1k (matrix, warm) | 1,871,641 | highest observed |
| jset-1k (matrix, warm) | 1,275,104 | highest observed |
| ratio (warm, best-per-lane) | **0.6813** | yesterday's wire2: 0.6900 |

## Finding 1 — generator saturation ruled out; warm-up identified

Suspicion: yesterday's decisive observation (−6.2% parse → +0.02pp
wire) could mean the memtier `--command` lane was generator-bound.
Tested with a thread/cpuset sensitivity matrix (`matrix-*.txt`):

| Lane | 4t/25c mix | 8t/13c mix | 12t/9c mix | 8t/13c E-cores |
|---|---|---|---|---|
| set-1k | **1,871,641** | 1,775,277 | 1,583,989 | 1,819,451 |
| jset-1k | **1,275,104** | 1,252,568 | 1,258,749 | 1,253,964 |

Adding generator threads helps neither lane (set degrades; jset flat
within ~2%) → **both lanes are server-bound at the blessed config; the
§19 load-gen validity concern is retired.** The early 4t→8t jset gain
(+6.5%, `genchk-*`) was server warm-up: first legs on a fresh server
read low (jset ~1.20M) and settle ~1.25–1.28M. An interim "honest
0.7831" reading (`SUMMARY-honest.txt`) is **invalid**: its 8t set legs
collapsed (1.80/1.52/1.47M, rsd 11.4% — thread placement over mixed
P-HT/E cores); kept for the record, not for claims.

## Finding 2 — perf attribution (perf-jset-1k.data / perf-set-1k.data)

Cells carry ≈ 98.6% of samples in both legs. Converting to ns/op-cell
(pct × 4 / throughput): shared machinery is **equal per op** between
lanes (send_apply 56 vs 55 ns, memmove 161 vs 168, resolve_hashed 61
vs 70, top kernel symbol 75 vs 76, malloc family 114 vs 132) — the
wire/dispatch/fabric stack adds nothing document-specific.

The jset-only cost decomposes as:

| Bucket | ns/op-cell | Symbols |
|---|---|---|
| parse family | ≈ 797 | parse_into 448, parse_number_value 96, fixstr copy 91, classify 78, parse_number 32, utf8 23, long-copy 18, literal 11 |
| doc-store plumbing | ≈ 61 | json_write_value 34, json_set 12, DocStore::release 9, prefetch_root delta 6 |
| diffuse residual | ≈ 140 | spread across shared symbols (larger argv through OwnedCmd/fabric, accounting, cache effects) |
| **total measured delta** | **≈ 1,000** | set 2.137 µs/op-cell → jset 3.137 µs/op-cell (warm matrix rows) |

The model **fits**: parse (797 ns, pinned-bench-consistent) + ~200 ns
server-side. Yesterday's "+0.02pp on a −6.2% parse win" is inside
cross-leg noise (±2%), not evidence of a hidden bottleneck — the
over-read is corrected here.

## Gate arithmetic

0.70 × set means jset needs −47 ns/op-cell at yesterday's wire2 rows,
−84 ns at today's warm matrix rows. The parse pool is exhausted
(ADR-0049); the server-side pool is ~200 ns.

## Named levers (each its own A/B)

1. **L1 — in-place tape-blob overwrite** (`json_write_value`): today
   every root JSON.SET over an existing tape-blob doc allocs a fresh
   doc-arena blob, copies, rewrites the record, frees the old blob and
   re-shifts accounting. SET, by contrast, overwrites same-size records
   in place (`resize_in_place`). Same-size-class blob overwrites can
   rewrite blob bytes in place: no alloc/free churn, no release pass,
   single accounting shift. Estimated 40–70 ns.
2. **L2 — root-path fast slot in `ProgramCache`**: `$` bypasses the
   hash + LRU touch (a pre-compiled slot inside the cache so hit
   metrics stay truthful). Estimated 5–15 ns.
3. **Harness: per-lane warm-up leg** in `bench-m3-wire.sh` (discarded
   before reps) — kills the fresh-server first-rep bias; replicate rsd
   should drop.

## Disclosures

- One jset rep collapsed to 1.080M (−15%) for a full 10 s leg
  (`jset-1k-rep3.txt`); set collapses appeared only under the 8t mixed
  cpuset. Server logs clean in all runs. Not reproduced under the 4t
  blessed config since; watch in the A/B (ABAB ordering + rsd guard).
- Cross-leg drift: fresh-server legs read −2…−6% vs warm; the A/B
  protocol must interleave arms on one warm server or warm both arms.
- Desktop session live during all runs (dev tier only).
