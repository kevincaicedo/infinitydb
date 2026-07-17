# ADR-0047 slice 2 — stage fusion + ADR-0049 emit region (dev tier, 2026-07-17)

Environment: `env.txt` (i7-13700KF, `performance` governor + EPP,
`no_turbo=1`, bench pinned `taskset -c 4`; wire rows server 0-7 /
memtier 12-23, 3 replicates). Baseline = same-day tree at commit
`53cd74c` (the K1–K3 state). Dirty tree during lever iteration;
disclosed. Nothing citable (dev tier, L10).

## Lever chain (gate-1KiB row; criterion 100 samples, pinned)

| Arm | gate ns | gate thrpt | disposition |
|---|---|---|---|
| session baseline (K3 state) | 883.7 | 1.0792 GiB/s | — |
| F1 stage fusion, first cut | 1608.0 | 607 MiB/s | **repaired** — `Vec::push`-per-block classify loop made LLVM SLP-vectorize the mask combining into a `vpinsrb` storm (50% of cycles, perf-annotated); ptr-write loop (K3 shape) restored it |
| F1 fixed (`json_classify_blocks` + `JsonTokenCursor`) | 844.4 | 1.1294 GiB/s (+4.6%) | **Accepted** as carrier — medium +6.2 / large +3.8 / small −1.8 / wide −1.6 / **deep −9.6** (token-dense shapes pay the per-token pull) |
| F2 pull-through cursor (peek = pure read) | 856.4 | 1.1136 | **Accepted** — deep +2.2 recovers part of the F1 loss; gate −1.1 inside the same-binary band |
| K4 key-fingerprint filter + `#[inline]` note_key | 847.0 | 1.1260 | **Accepted** — kills the O(k²/2) duplicate scans; flat alone on gate (the bucket was bookkeeping, not scans) but prerequisite for D3's win |
| D3 emit region first cut (ADR-0049, incl. `fixstr_swar`) | 892.5 | 1.0685 | **decomposed** — the SWAR fixstr lost −5% on gate; deep +4.8 showed the rest of the region winning |
| D3 final (region minus fixstr_swar; K2 kernel restored) | 827.0 | 1.1531 | **Accepted** |
| lazy-entries note_key (count+filter, walk-on-demand) | 887.2 | 1.0749 | **Rejected, removed** — weak fp collided on `"name"`/`"note"` (walks every gate object); the strong-fp variant fixed gate but destabilized medium/large/wide (−12–18%, layout-dominated). Rows: `d4-lazykeys-parse.txt`, `d5-fp2-parse.txt` |
| revert to D3 final | 823.7 / 829.2 / 831.7 (r1/r2/r3) | 1.15 GiB/s | 1.0% same-binary spread; first wire leg ran here → **0.6898** |
| K2b header-fused fixstr kernel (`json_copy_unescaped_fixstr`) | 808.7 | 1.1793 (+2.3%) | **Accepted** — folds the tag push (a `Vec` branch per short string) into the K2 call's one reservation; small +4.8, medium +5.5 |
| string_close byte-check elision | 778.5 | 1.2250 (+3.9%) | **Accepted** — the token after an open quote is provably its close quote (in-string masking); the per-string load+branch becomes a debug assertion; differential re-run green |
| **shipped** | **772.6 / 777.1 / 776.6 (r1/r2/r3)** | **≈ 1.23 GiB/s** | quotable same-binary replicates, 0.6% spread |

## Cumulative (all six shapes, shipped binary vs session baseline)

| Shape | Baseline | Final | Δ |
|---|---|---|---|
| gate-1KiB | 883.7 ns / 1.0792 GiB/s | 772.6 ns / **1.2344 GiB/s** | **+14.4%** |
| medium-2KiB | 1.6990 µs / 1.1227 | 1.4087 µs / 1.3540 | +20.6% |
| large-64KiB | 38.525 µs / 1.5843 | 32.997 µs / 1.8497 | +16.8% |
| small-200B | 223.0 ns / 855.2 MiB/s | 197.1 ns / 968.0 MiB/s | +13.2% |
| deep-32 | 1.4990 µs / 407.8 MiB/s | 1.3468 µs / 453.9 MiB/s | +11.3% |
| wide-array | 832.6 µs / 480.2 MiB/s | 723.4 µs / 552.8 MiB/s | +15.1% |

No shape below baseline. `scan/simd` (batch stage-1, still used by the
scalar-oracle arm and benches) unchanged at 4.42 GiB/s — the extraction
refactor is codegen-neutral.

## What landed (code)

1. **Stage fusion** (ADR-0047 D2 item 3, zero new unsafe in inf-simd):
   `inf_simd::json_classify_blocks` stores raw 32 B `BlockMasks` per
   64 B block through a ptr-write loop (K3 shape);
   `inf_simd::JsonTokenCursor` (pull-through: `peek` is a register
   read, `bump` pops the next token from the per-block emit mask
   in-register) replaces the `Vec<u32>` structural-index round trip.
   The grammar machine is generic over `TokenSource` — the scalar
   oracle feeds the same machine through a batch-index adapter, and the
   tier-equivalence proptests now prove the streaming consumption too.
2. **`note_key` fingerprint filter**: a 64-bit per-frame filter
   (len/first/last-byte hash) proves most keys duplicate-free without
   the linear scan; the memcmp scan stays the authority on any
   collision. Known benign collision class (`"name"`/`"note"`) costs
   one short scan — measured cheaper than every stronger hash tried.
3. **ADR-0049 emit region** (`inf_doc::emit`, the one audited unsafe
   module — SAFETY.md added, Miri-covered): `i64`, `f64`, `str_header`,
   container `begin`, and `append_overlapped` follow
   reserve-once/write-unchecked; per-byte `Vec` capacity branches and
   the per-word extend branches are gone. `inf-doc` lint moved
   `forbid(unsafe_code)` → `deny` + module allow (the inf-simd shape).
   The `fixstr_swar` attempt to also replace the K2 kernel call was
   Rejected (−5% gate) and removed; K2 stays.
4. **K2b `inf_simd::json_copy_unescaped_fixstr`**: the K2 kernel with
   the caller's fixstr tag fused in — one reservation carries header +
   payload, removing the separate tag push per short string. SAFETY.md
   (inf-simd) K2b entry; exhaustive sweep extended to the fused tiers.
5. **`string_close` elision**: the per-string close-quote byte check
   demoted to a debug assertion — the stage-1 in-string mask proves the
   token after an open quote is its close quote or nothing.

## Validation (shipped state)

- `cargo test -p inf-simd` 49 ✓ / `-p inf-doc` 15 suites ✓ (goldens
  byte-exact, exact error offsets).
- `json_differential` release, `PROPTEST_CASES=250000` — 4 properties ×
  250k = 10⁶ documents ✓ (6.98 s).
- Miri: full `inf-doc --lib` (27 tests, includes the emit-region unit
  oracles) ✓ under the new scalar-classify Miri gate.
- `json_parse` fuzz smoke 300 s: see `fuzz-smoke.txt` (run post-wire to
  keep the wire cores quiet).
- `just check` + `cargo deny check`: at session close (below).

## Wire verdict (wire-run.sh / wire-run2.sh, 3 reps each)

Two legs, both on this session's binaries (`wire/`, `wire2/`):

| Arm | JSON.SET/SET (≥ 0.70) | JSON.GET/GET p50 (≤ 1.5) |
|---|---|---|
| K1+K2+K3 (2026-07-16 final) | 0.6556 | 1.0890 |
| fusion + emit region (parse 1.158 GiB/s) | 0.6898 | 1.2050 |
| + K2b + string_close (parse 1.234 GiB/s) | **0.6900** | 1.0719 |

**The doc-write gate remains red at dev tier: 0.6900 vs ≥ 0.70** —
1.4% short. Full trajectory 0.6020 → 0.6234 → 0.6556 → 0.6898 → 0.6900.

**The decisive observation:** between the two wire legs, gate-shape
parse improved −6.2% (824 → 772 ns) and the wire ratio moved +0.02pp.
The jset-vs-set delta is no longer parse-bound — the remaining gap
lives in the server-side `JSON.SET` path (record building, doc store,
per-command machinery), outside ADR-0047's parse staging. Further
parse levers cannot close this gate.

Read-row disclosure: `get-1k` rsd 4.9–5.5% on both legs (others ≤ 2.9%);
the read gate is comfortably green either way.

## Named next steps

Per ADR-0049's own failure path (ADR-0047 D3 exhausted on honest
measurement):

1. **§7 gate re-expression ADR, ratified by the milestone owner** (the
   ADR-0035 precedent) — the 0.6900-vs-0.70 residual with the parse
   staging exhausted is the named trigger; or
2. **A server-side `json_set` slice** (S16 fast-path territory:
   record-build/store cost of `JSON.SET` vs `SET`) — a new story with
   its own budget, not a continuation of this one; the wire sensitivity
   evidence above is its opening profile question.
3. Reference-box campaign re-runs everything regardless (no dev number
   is citable, L10).
