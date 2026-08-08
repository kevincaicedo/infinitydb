# M3-S02 traversal-budget slice — depth-4 leaf fetch (dev-tier, 2026-07-11)

- Env: `env.txt` (binding env re-verified; same-binary replicate spread
  < 0.3%, cross-binary layout spread ±3.5–4% applies — memory-note rule:
  per-step chain values are directional, the shipped binary's replicates
  are the quotable numbers).
- Baseline: clean 51bd16b, same session — tape **216.4–217.0 ns**
  (budget ≤ 200, 9% over), arena **196.0–197.1 ns**.
- Tier: **dev** — the S25 reference-tier re-run stands; no public claim
  exists from this artifact (L10).

## Diagnosis before levers (perf, the L4 step)

`perf stat` on the criterion row: **branch-misses ≈ 0** (42 k over 3 s vs
7.2 B branches — the predictor learns the bench pattern), **IPC 3.02** —
the row is instruction-count-bound, not latency- or misprediction-bound.
`perf annotate` on `ObjRef::get` (out-of-line): PLT `memcpy` for the ≤ 7-
byte needle build + stack round-trip re-read (~10%), a jump table for the
skip dispatch (~9%), and a 32-byte `Option<ValueRef>` sret stack copy per
match (~16%). The scan loop itself was minor — the row was paying **call-
boundary overhead**, invisible until instruction-level attribution.

## Lever chain (tape row, cumulative; each step rebuilt → directional)

| step | tape ns | note |
|---|---|---|
| baseline 51bd16b | 216.4–217.0 | 3 replicates |
| word-compare key scan (masked 8-byte tag+key test) | 213.0–213.1 | −1.6% — far under the ≥ 10% hypothesis |
| + window-fused value skip (`skip_from_window`) | 214.1–214.4 | no gain |
| + shift-or needle build, branch-chain dispatch | 215.1–215.9 | no gain out-of-line |
| + `#[inline]` on `get`/scan/`skip_value`/`read_value` + cursor `get` | **175.7–176.1** | **the step: −18.7%** |
| cursor-`#[inline]` removed (ablation) | 229.1–230.1 | both rows worse — restored |
| window-skip removed (ablation on the inlined config) | 169.2 | **faster** → window-skip Rejected |
| word-scan removed (ablation → plain scalar loop) | 164.1 | **faster** → word-scan Rejected |

## Dispositions (M0-S14)

- **Accepted — `#[inline]` on the tape lookup chain** (`ObjRef::get`,
  `skip_value`, `read_value`, `ObjCursor::get`, arena `ObjRef::get`):
  callers integrate the scan; sret copies, PLT calls, and prologue
  spills vanish. This is the whole tape-row win.
- **Accepted — arena key-only scan**: `get` compared via `entry(i)`,
  which paid a second slot read + full value `deref` (`DocValue`
  construction, container header reads) per *scanned* entry (49% + 17%
  of the arena row in `entry`/`deref` by annotate). Scan keys only,
  deref the value once on match.
- **Rejected — word-compare key scan** (needle = tag + first ≤ 7 key
  bytes, one xor/mask test per entry): −1.6% out-of-line, **+4.0% vs the
  plain scalar loop once inlined** (164.1 vs 170.6). The inlined loop is
  instruction-throughput-bound; the mask/shift work costs more than the
  compare it fuses. Recorded, not merged.
- **Rejected — window-fused value skip** (`skip_from_window`: compute
  the value extent from the already-loaded 8-byte window): +5.2% vs the
  inlined `skip_value` (169.2 vs 178.4). Same reason — extra shifts beat
  the pipelined reload they save. Recorded in-code at the call site.

## Final rows (shipped binary, same-binary r1–r3)

| row | baseline | shipped | Δ | budget |
|---|---|---|---|---|
| depth4_leaf_fetch/**tape** | 216.4–217.0 | **163.9–164.0 ns** | **−24.2%** | ≤ 200 ns — **PASSES, 18% under** (dev tier; was 9% over) |
| depth4_leaf_fetch/**arena** | 196.0–197.1 | **121.1–121.9 ns** | **−38.1%** | (feeds the morph-threshold A/B, ADR-0036 D8) |

Adversarial key placement unchanged (target key 7th of 8 per level).
The read-side morph comparison flips: arena is now **faster** than tape
on this shape (121.9 vs 164.0) — S16's mutation-side A/B inherits this
corrected read-side input when it fixes `doc_morph_bytes_min`.

## Validation (this slice)

- `cargo test -p inf-doc` green, both feature lanes (10 suites each;
  goldens byte-exact incl. duplicate-key first-match pinning).
- `cargo clippy -p inf-doc --all-targets` clean.
- No format byte, validator rule, or public API shape changed — read
  path only. The `idoc_decode` fuzz smoke runs in the session-close
  validation set (recorded in the ledger entry).
