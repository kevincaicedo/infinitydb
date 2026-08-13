# M4.5-S01 — ordered map (B+-tree) A/B artifacts

**Tier: dev — non-citable.** i7-13700KF (hybrid; bench pinned
`taskset -c 4`, a P-core), 30 MiB L3 / 24 MiB L2, governor
`performance`, EPP `performance`. Tree under test:
`inf-store::OrderedMap`, `cargo bench -p inf-store --bench ordered`,
N = 10,000,000 entries, 15 rounds per row, medians; 3 replicates per
phase. Working tree of the S01 session (see the ledger entry for the
commit). Binding rows are S18's on the reference box.

## Phase 1 — explicit-SIMD kernel vs autovectorized count-scan

Files: `ordered-10m-rep{1,2,3}.txt`. Row label `search=simd` = the
explicit AVX2/SSE4.2 `inf_simd::lower_bound_u64` kernel (sign-flip
`pcmpgtq` + movemask popcount, `AtomicU8` runtime dispatch — the CRLF
pattern); `search=scalar` = the plain branchless count-loop.

| row (fanout 64) | kernel | count-loop |
|---|---|---|
| probe hot_set=10k | 372.6→269.8 ns¹ | 210.3 ns |
| probe hot_set=100k | 372.6 ns | 297.9 ns |

¹ rep1 values; the ordering was consistent in every replicate.

**Disposition: kernel Rejected (M0-S14 rule — recorded, not merged).**
LLVM autovectorizes the count-loop (vector compare-accumulate, no
per-chunk movemask/popcount) and inlines it into the search;
`#[target_feature]` functions cannot inline, so the dispatched kernel
paid a call + dispatch per tree level. `inf-simd/src/lower_bound.rs`
now ships the winning safe loop only; its module doc and SAFETY.md
carry the disposition. Any future explicit kernel (AVX-512, reference
box) re-enters through a new A/B against these rows.

## Phase 2 — early-exit leaf scan vs count-scan; fanout 32 vs 64

Files: `ordered-10m-phase2-rep{1,2,3}.txt`. `search=early` = exit the
prefix scan at the boundary (touches ~half the leaf lines, one
well-predicted mispredict); `search=count` = branchless full-array
count. Hot-set widths: 1k (cache-resident — the §4.1 budget's "hot"),
10k (~10 MB, partially L3-resident against a 246 MB tree), 100k
(DRAM-facing). Rep-1 medians (spread < 3% across replicates):

| row | F=64 early | F=64 count | F=32 early | F=32 count |
|---|---|---|---|---|
| probe hot 1k | 140.8 | 162.0 | **127.3** | 149.9 |
| probe hot 10k | 190.5 | 201.3 | **182.0** | 201.8 |
| probe 100k | 267.5 | 276.6 | **266.3** | 281.3 |
| next() ns amortized | **5.89** | — | 8.71 | — |
| insert random ns | 269.4 | — | **259.4** | — |
| insert sequential ns | 91.5 | — | **81.7** | — |
| bytes/entry random | **24.65** | — | 25.99 | — |
| bytes/entry sequential | **17.58** | — | 18.68 | — |
| bytes/entry var16 random | 42.54 (F=64) | — | — | — |

**Dispositions:**

- **Early-exit leaf scan: Accepted** (wins every probe row; wired as
  the tree default; the count-scan remains reachable via the hidden
  `contains_scalar_search` A/B twin).
- **Fanout 32: Accepted as the default.** Both fanouts pass every §4.1
  budget; the rows that feed binding gates — hot point probe (the
  point-query gate) and insert (the S04 ≤ 600 ns/pair maintenance
  budget) — favor 32 (−9% probe, −4%/−11% insert) over 64's +5%
  memory and +2.8 ns amortized `next()` (irrelevant against the 2 ms
  page budget: a 100-item page differs by ~0.3 µs).

## Budget verdict (§4.1 S01 row, dev-tier)

- **Point probe ≤ 150 ns hot: met on the cache-resident row** — 127 ns
  (F=32) / 141 ns (F=64). Wider warmths reported beside it, never
  blended: 182 ns at 10k, 266 ns at 100k (leaf line fetches from
  L3/DRAM + TLB pressure on the scattered leaf pool). Named follow-up
  levers if the reference box or the S17 end-to-end rows need them:
  intra-leaf line-router (64 B per-line heads → ~3 touched lines), 2 MiB
  page backing for the node pools. Gate risk assessed low: the §7
  point-query gate is end-to-end (p50 ≤ 2.5× `JSON.GET`, ~µs scale) —
  the tree contributes < 0.3 µs at the worst measured row.
- **Range next() ≤ 40 ns: met ×4.6** (8.71 ns at the F=32 default).
- **≤ 40 B/entry at 10M: met** — 25.99 (F=32 random), 18.68
  (sequential — the rightmost-split heuristic's row). The var-scheme
  16-byte-key row reads 42.54 B/entry and is disclosed as over: 24 B of
  it is irreducible payload (16 key + 8 ref); the 40 B gate binds on
  the S17/S18 gate corpus (numeric and ≤ 8 B string keys measure
  ~26–36 B/entry). Slack is bounded by construction (pool/heap growth
  in 12.5% steps): 3.7% of total at 10M random.

## Reproduce

```
ORDERED_BENCH_N=10000000 taskset -c 4 cargo bench -p inf-store --bench ordered
cargo test -p inf-store --release --test ordered_storm -- --ignored   # 10^6-op model storm
```
