# M3-S25 development-tier gate legs — 2026-07-16

**This is NOT the S25 campaign.** It is the development-tier validation of
every §7 exit gate that this box can honestly measure, run to answer "what
blocks v0.3.0-alpha.1" (see `reviews/release-readiness-v0.3.0-alpha.md`).
Dirty tree, dev box (i7-13700KF, performance governor, 3.4 GHz turbo-off
profile), background load disclosed per row. **No number here is citable
publicly or as gate passage** (L10, §19); the reference-box campaign
re-runs everything.

## Harness disclosure (no silent substitution)

The inf-compare `json` lanes and `inf-bench gate-run m3` rows do not exist
yet (named S25 pre-work). Wire rows were hand-run with memtier_benchmark
directly (`wire-rows.sh`): infinityd release pinned to CPUs 0–7, memtier
to 12–23, 4 threads × 25 conns, pipeline 16, 10 s × 3 replicates, port
6400. RSS rows (`rss-ab.sh`, `rss-shape.sh`): identical RESP pipe loads
into infinityd and the pinned redis-stack image
(`sha256:798ab84d…`, ReJSON 2.8.9), incremental `ps` RSS, 71,200-document
mixed corpus (207,171,400 serialized bytes) plus single-shape runs.
Corpus: `inf-bench doc-corpus --seed 0x1D0C2026`.

## Results

### Wire rows (`wire-rows/SUMMARY.txt`)

- `JSON.SET` vs `SET` (1 KiB, pipelined): 1,376,572 vs 2,286,576 ops/s =
  **0.602× — FAIL vs ≥ 0.70** (confirms the S05/S11 ≈ 58% projection;
  replicate rsd 0.2%/3.6%).
- `JSON.GET $.path` vs `GET` p50: 1 KiB **1.199×**, 200 B **1.066×** —
  **PASS vs ≤ 1.5** at both shapes (the 2026-07-11 ≈ 1.8× dev projection
  did not materialize at the wire; the S20 root-prefetch + evaluator work
  closed it). rsd ≤ 0.5%/1.9% except set-1k 3.6%/6.0%.
- Path-mutation row: `JSON.NUMINCRBY` 3,257,698 ops/s wire.
- Mixed 50/50 KV+JSON: 913,297 + 1,114,604 ops/s concurrent.

### Read-path profile (`wire-rows/jget-read-report.txt`)

perf on the live `JSON.GET` wire row (1,997 Hz, dwarf, 8 s, server CPUs):
**zero JSON-text-parser symbols** in 1,659 report rows (grep:
JsonParser|json_scan_structurals|parse_into|json::parse = 0). Hot document
symbols are tape traversal (`ObjIter::next` ≈ 3.3%/cell, `read_value`) —
the parse-free traversal proof at the wire level. Raw perf.data (2 GB) not
retained; the report + methodology are.

### Memory A/B (`rss-ab/`) — **FAIL, STOP-class**

| load | RSS/serialized (≤ 1.5) | vs redis-stack (≤ 0.7) |
|---|---|---|
| mixed 207 MB corpus | 2.155 | 0.949 |
| small-200B ×40k | 1.488 | 0.738 |
| gate-1KiB ×40k | 1.174 | 0.885 |
| large-64KiB ×2k | 1.854 | 3.335 |
| wide-array ×300 | 3.288 | 0.938 |

The S19-recorded tree-form amplification is confirmed at engine level;
the 0.7×-RedisJSON axis fails on every shape. Loads are write-once (no
mutation slack — the S25 methodology note). Finding recorded in the M3
ledger: `INFO` publishes the serving cell's attribution beside
process-wide RSS (`docs_live` 17,801 on a 71,200-doc node;
`mem_fragmentation_ratio` mixes scopes) — attribution internals are
correct, the publication needs a cross-cell sum or per-cell label.

### Command-layer criterion (in-process, same session)

`SET` 55.9 ns · `JSON.SET` root 1.145 µs (−15.9% vs 2026-07-11) ·
`GET` 42.8 ns · `JSON.GET $.path` 1.271 µs · `INCR` 85.1 ns ·
`NUMINCRBY` 109.7 ns (**1.29× ≤ 1.3 budget ✓**).

### Regression re-pass (`regression/`)

`gate-run m0|m1|m2 --unsafe-env` (dirty-tree dev runs — the harness
itself enforces non-citability). See per-milestone report dirs; compare
against the 2026-07-10 S19-closure archived reports.

## Gate scoreboard after this session (dev tier)

| §7 gate | verdict |
|---|---|
| doc read p50 ≤ 1.5× | PASS (both shapes) |
| doc write ≥ 70% SET | **FAIL (0.602×)** — the named unsafe-emit-core-vs-re-derivation ADR decision is now live |
| doc memory (both axes) | **FAIL (STOP-class)** — format/placement remediation before any release |
| crash atomicity | PASS (S18 + S24, 10k seeds) |
| compat | PASS (S21 + S22 audit) |
| replay equivalence | PASS (S23) |
| regression ≤ 5% | see regression/ |
| decoder fuzz ≥ 24 h | OPEN — machine time (live runner never registered; M2.5-S02) |
| 24 h doc soak | OPEN — harness + wall time |
