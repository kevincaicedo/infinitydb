# M3-S05 optimization slice — parse throughput levers (dev-tier, 2026-07-11)

- Box: HomeLab i7-13700KF, kernel 7.0.0-27-generic, governor=performance,
  EPP=performance, `no_turbo=1` (binding env, 3.4 GHz P-core base),
  pinned `taskset -c 4`. No concurrent load during bench runs.
- Tree: post-S05 commit 23541f8 + this slice's working tree.
- Baseline artifact: `.artifacts/m3/s05-parse-20260710/` (gate 383 MiB/s,
  medium 395 MiB/s). Fresh same-box baseline replicate before any lever:
  gate **384.7 MiB/s**, medium **395.6 MiB/s** (`raw/baseline-r1.txt`) —
  reproduces within 0.5%.
- Raw logs under `raw/` (`baseline-r1`, `leverA-r1`, `leverB-r1`,
  `leverC-r1`, …); criterion `change:` lines are vs the immediately
  preceding lever (cumulative chain from the baseline).
- Tier: **dev** — no public claim exists from this artifact (L10).
- Output-byte identity enforced at every step: goldens (byte-exact),
  12 edge suites, differential proptests, `json_parse` fuzz oracles.

## Optimization log (each lever measured separately, in order)

### Lever A — fused emission core + separator-fused grammar loop

Hypothesis: the ~38% `parse_indexed` + ~9% `emit_str` buckets are
builder-plumbing and dispatch-round-trip overhead, not essential work.

Change: new `emit.rs` — single-source canonical encoding primitives
(fixint/varint width, fixstr/str8/str24 selection, u24 backpatch)
consumed by both `TapeBuilder` (checked wrapper, unchanged semantics)
and `JsonParser` (direct drive: the grammar machine is the invariant
authority; per-value `claim_value_slot`/`Result` plumbing and the
duplicated `BFrame` stack drop out; container kind + placeholder offset
pack into one u32 stack word). Grammar loop consumes `:` and `,` fused
with the values they frame; container closes cascade in an inner loop
(no dispatch round trip per closer). Header patched in place
(allocation removed from every parse and every `TapeBuilder::finish`).
S07 incremental idoc-byte guard preserved exactly (exact pre-check on
variable-length payloads, worst-case pre-check on fixed tokens).

| row | before | after | thrpt Δ |
|---|---|---|---|
| parse/gate-1KiB | 384.7 MiB/s | **465.6 MiB/s** | **+20.9%** |
| parse/medium-2KiB | 395.6 MiB/s | **466.3 MiB/s** | **+18.2%** |
| parse/small-200B | 346.9 MiB/s | 405.4 MiB/s | +15.3% |
| parse/large-64KiB | 289.5 MiB/s | 318.9 MiB/s | +10.1% |
| parse/deep-32 | 379.6 MiB/s | 481.5 MiB/s | +27.1% |

**Disposition: Accepted.**

Post-A profile (gate shape, perf 4 kHz dwarf): `parse_indexed` 28.7%
self, `emit_string` 19.9%, stage 1 ~15% (`flush_block`+`avx2_scan`+
`masks_bytewise`), numbers ~14% (`parse_number` 8.2% + `dec2flt` 2.5% +
`from_str` 1.7% + `emit_i64_checked` 1.6%), `emit::str` 6.7%,
`from_utf8` 3.5%, `memmove` 3.0%, `memcmp` (dup-key scans) 2.9%.

### Lever B — string scan/copy word tricks

Hypothesis: the `emit_string` + `emit::str` + `memmove` ≈ 30% bucket
pays memcpy-call and bytewise-tail overhead on the short strings that
dominate document keys.

Change: `find_special` loses its bytewise tail (masked stack word for
< 8 bytes, overlapped final word for ≥ 8 — the re-covered prefix
already scanned clean); escape-free payloads copy as overlapped 8-byte
words riding the input's own slack (`append_from_input`), exact-copy
fallback at end-of-input.

| row | before | after | thrpt Δ |
|---|---|---|---|
| parse/gate-1KiB | 465.6 MiB/s | 476.3 MiB/s | +2.4% |
| parse/medium-2KiB | 466.3 MiB/s | 461.4 MiB/s | **−0.95%** |
| parse/small-200B | 405.4 MiB/s | 408.2 MiB/s | +0.8% |
| parse/large-64KiB | 318.9 MiB/s | 325.5 MiB/s | +2.2% |
| parse/deep-32 | 481.5 MiB/s | 466.8 MiB/s | −3.0% |

**Disposition: mixed — untangled below (B-split) before accept/reject.**
Suspect: the < 8-byte masked-word path trades a ≤ 6-iteration predicted
byte loop for a variable-length `copy_from_slice` (memcpy call); the
overlapped ≥ 8 tail and the payload word-copy are the likely wins.

### Lever C — number fast paths

Hypothesis: the ~14% number bucket pays std `dec2flt`'s full re-parse
for every f64 and byte-at-a-time digit walks.

Change: Clinger short-decimal fast path (mantissa exact in f64 — ≤ 2⁵³
— scaled by an exact power of ten, |e| ≤ 22, rounds exactly once →
bit-identical to Eisel–Lemire by the classic argument; std fallback
outside the bounds); exponent value parsed inline with saturation; all
digit runs scanned word-at-a-time (`digit_run_end`, carry-safe SWAR
classify).

| row | before | after | thrpt Δ |
|---|---|---|---|
| parse/gate-1KiB | 476.3 MiB/s | 510.7 MiB/s | +7.3% |
| parse/medium-2KiB | 461.4 MiB/s | 502.9 MiB/s | +8.8% |
| parse/small-200B | 408.2 MiB/s | 435.3 MiB/s | +6.6% |
| parse/large-64KiB | 325.5 MiB/s | 344.7 MiB/s | +5.8% |
| parse/deep-32 | 466.8 MiB/s | 506.3 MiB/s | +8.3% |
| parse/wide-array | — | 385.1 MiB/s | +1.4% |

**Disposition: Accepted.** (Bit-exactness re-proven by the 10⁶
differential + fuzz hour on the final code — see validation.)

### Lever B-split — revert the < 8-byte masked word to the byte loop

Hypothesis (from B's mixed result): the short-string masked word is the
regression; the ≥ 8 overlapped tail and payload word-copy are the wins.

| row | before | after | thrpt Δ |
|---|---|---|---|
| parse/gate-1KiB | 510.7 MiB/s | **532.4 MiB/s** | +4.2% |
| parse/medium-2KiB | 502.9 MiB/s | **514.7 MiB/s** | +2.3% |
| parse/small-200B | 435.3 MiB/s | 440.6 MiB/s | +1.2% |
| parse/large-64KiB | 344.7 MiB/s | 368.3 MiB/s | +6.9% |
| parse/deep-32 | 506.3 MiB/s | 528.5 MiB/s | +4.4% |
| parse/wide-array | 385.1 MiB/s | 401.9 MiB/s | +4.3% |

Every shape improved — hypothesis confirmed. **Lever B final
disposition: Accepted as revised** (overlapped ≥ 8 tail + payload
word-copy kept; < 8 stays the predicted byte loop; the masked-word
variant is a recorded loss, not merged).

### Lever D — dup-scan padded key-word compare — **Rejected**

Hypothesis: the duplicate-key scan's per-entry memcmp (2.9% of the
post-A profile) collapses to one u64 compare per entry if each
`ObjEntry` caches its first ≤ 8 key bytes as a zero-padded word.

Measured (vs B-split): gate **−2.5%**, deep-32 −7.8%, wide-array
**−11.7%**, large −1.8%; small-200B +10.2%, medium +1.4%. The per-key
word build (≤ 8 shift-ors, unconditional) outweighs the memcmps it
saves whenever objects have few prior keys to scan — wide-array's
3-key × 10⁴ objects pay the build ~30k times for ~2 compares each.
Only the 9-key single-object small shape wins.

**Disposition: Rejected (M0-S14 — recorded, reverted, not merged).**
Raw log: `raw/leverD-r1.txt`; the revert restoration is `final-r1`.

## Final rows (shipped code, `final-r1/r2/r3`)

`final-r1` measured the pre-fmt build; `final-r2` recompiled after
`cargo fmt` (the shipped text) and `final-r3` re-ran the same binary.
Same-binary run spread < 0.3%; across semantically identical rebuilds
the gate row moved 514–532 MiB/s (±3.5% **code-layout noise**,
disclosed — the quoted numbers are the shipped binary's r2/r3 pair).

| row | S05 baseline | slice final | Δ | budget |
|---|---|---|---|---|
| **parse/gate-1KiB** | 384.7 MiB/s | **525.6 MiB/s** | **+36.6%** | ≥ 2.5 GB/s as written — still MISSED (≈ 4.9×); ≈ 1.05 GB/s re-derived — ≈ 2.0× |
| **parse/medium-2KiB** | 395.6 MiB/s | **519.5 MiB/s** | **+31.3%** | ≥ 1 GB/s floor — still MISSED (≈ 1.9×) |
| parse/small-200B | 346.9 MiB/s | 440.4 MiB/s | +27.0% | — |
| parse/large-64KiB | 289.5 MiB/s | 371.5 MiB/s | +28.3% | — |
| parse/deep-32 | 379.6 MiB/s | 529.5 MiB/s | +39.5% | — |
| parse/wide-array | 302.1 MiB/s | 400.9 MiB/s | +32.7% | — |

## Remaining profile (gate shape, final code)

`parse_indexed` 29.0% (grammar loop + dup bookkeeping; `memcmp` dup
scans another 3.1%) · `emit_string` 28.3% + `emit::str_header` 4.5%
(string handling is now the largest stage-2 bucket) · stage 1 ~16.6%
(`flush_block` 11.6 + `avx2_scan` 3.0 + `masks_bytewise` 2.9 — its
share ceiling: the 3.03 GiB/s scan costs 17% at 526 MiB/s e2e) ·
`parse_number` 8.7% + `emit_i64_checked` 1.5% · `from_utf8` 2.1%.

## Projection row (same run, `ingest-final`)

| row | S05 | now |
|---|---|---|
| parse gate-1KiB (in-store harness) | 2.09 µs | **1.456 µs** (−30.5%) |
| `CellStore::set` 1 KiB | 48.3 ns | 47.4 ns |
| `json_set` prebuilt idoc | 63.6 ns | 62.5 ns |
| parse + `json_set` e2e | 2.19 µs | 1.587 µs |

Parse is 92% of ingest. Under the measured-M2.5 server-SET anchor
(~1.75 µs at 1 KiB): JSON.SET ≈ 1.75/(1.75+1.46) ≈ **55% of SET** (was
45% before the slice, 26% under the plan's original assumption). The
70%-gate parse bar under that anchor stays ≈ 0.75 µs ≈ **1.05 GB/s**
on the gate shape — the shipped code is 2.0× short at dev tier. S11's
end-to-end rows settle the true denominator; S25 re-runs at reference
tier (this box is 3.4 GHz turbo-off — reference clocks shift absolute
MiB/s, not the ratio).

## Disposition and remaining named levers

Cumulative: **+37% gate / +31% medium**, three levers Accepted, two
variants Rejected-and-recorded (M0-S14). Throughput ACs stay **open
(Evidence-pending)** — the floor is ≈ 1.9× away at dev tier. Remaining
levers, in expected-value order:

1. Dedicated fixstr fast path in `emit_string` (33% bucket: fuse
   `string_close`/slice/`str_header` for ≤ 31-byte escape-free strings).
2. `parse_into(&mut Vec)` at the S03/S11 ingest seam — `json_set` can
   recycle the output allocation (~30–50 ns/parse, ~2–3%).
3. Object-entry loop fusion (key→colon→value→comma as one inner loop).
4. Stage-1 typed `(offset, class)` index (kills stage-2 byte reloads).
5. SIMD UTF-8 validation kernel in `inf-simd` (2.1% + enables dropping
   the whole-input `&str` framing).
6. If those exhaust short of the S11-derived bar: the ADR question
   (vetted unsafe emit core in `inf-doc` — an L9/§3.3 decision — vs an
   evidence-based gate re-derivation at S25).

## Validation (this artifact's run set)

- `just check` green from clean (fmt · dep-dag · cell-denylist ·
  fault-points · fsync grep · panic-policy · safety-inventory · clippy
  `-D warnings` · workspace 576 + intern lane 151 = 727 tests passed,
  0 failed — count unchanged from the S05 close).
- `cargo deny check` green (advisories/bans/licenses/sources).
- 10⁶-doc differential release run green (`PROPTEST_CASES=250000`,
  4 properties, 7.1 s — bit-exact f64 incl. the Clinger path).
- `json_parse` fuzz, 1 h on the shipped code: **37,473,488 runs, zero
  findings**, coverage 2539 edges (pre-slice hour: 2373) — E-cores,
  concurrent with test/deny validation, never with bench rows.
- `doc_ingest` projection re-run same-session (table above).
- Output-byte identity between the S05 baseline parser and the
  slice-final parser is enforced by the goldens + canonical-stability
  fuzz oracle + the differential — the slice changed no output byte.
