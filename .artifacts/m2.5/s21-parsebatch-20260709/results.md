# M2.5-S21 parse-batch staged prefetch — binding A/B results (2026-07-09)

Env: designated box (i7-13700KF), governor performance, env-check OK
every cited leg, 4 cells pinned, loadgen disjoint, Gen3 DRAM-less NVMe
(memory-mode m0 rows — device not in play). Binaries built at inner
`af64cd1` (the lever commit); tree commits during the campaign window
are evidence/docs/test-only (no shipped-binary change) — the arms differ
exactly by `--parse-batch-prefetch`.

## Citable legs (gate-run m0 --reference-box, n=3 replicates each, tripwires green)

| leg | arm | natural ops/s | all-local ops/s (derived: natural/(1−penalty)) | penalty | sqes/submit | anchor |
|---|---|---|---|---|---|---|
| A1 `m0/1783651989` | off | 2,905k | **6,480k** | 55.17% | 16.66 | 1.71× |
| A4 `m0/1783652820` | off | 2,923k | **6,626k** | 55.88% | 17.83 | 1.74× |
| B1 `m0/1783652142` | on  | 2,881k | **7,975k** | 63.88% | 16.01 | 1.67× |
| B2 `m0/1783652295` | on  | 2,956k | **8,222k** | 64.05% | 18.39 | 1.69× |

**All-local +22.6%…+26.9% binding, zero overlap between arms** (hypothesis
target: ≥ +15%; acceptance bar: ≥ +10%). Natural row flat (2.88–2.96M on
vs 2.82–2.92M off — overlapping; the origin-local quarter's gain dilutes
into the fabric-bound mix). Anchor ≥ 1.25× with wide margin every leg.
The penalty *ratio* rose 55% → 64% exactly as the hypothesis predicted —
all-local grew faster than natural; the ≤ 40% staged gate must not be
read against this lever (its lever class — de-async dispatch — is
carried, ADR-0030 D4 / ADR-0033).

At 4 of the spec'd 8 cells, all-local now clears the ≥ 6M pipelined gate
value at **8.2M (+37% headroom)**; the gate row itself still binds on the
natural number (2.9M — the S21 penalty identity, ADR-0029).

## Discarded / non-citable legs (disclosed)

- `m0/1783651780` (off): flake-diagnosis agent's test loops overlapped
  the window (its disclosed load windows 22:35–22:49 EDT) — discarded.
- First-launch leg A1 + a `cargo check` burst overlapped the rerun's
  first minutes — that leg (script-killed by the gate-FAIL exit before
  evidence commit) and the overlap are why A1 was re-run clean.
- A2 (off) and B8 (on, 8 cells): server died mid-row ("connection reset")
  — the known ADR-0026 D3 spawn/ENOMEM fail-stop class; no data, not
  replicates. A3 (off, `m0/1783652590`): completed but sqes/submit 14.72
  < 16 — tripwire red, corroborating only (its all-local 6.38M agrees
  with A1/A4).
- A8 (off, 8 cells): natural replicates ran (3.59–3.96M) but
  sqes/submit 6.1–7.9 (red) and the all-local restart refused
  ("connection refused") — **the 8-cell spec shape is blocked on the
  ADR-0026 D3 ENOMEM class** (8 rings × registered buffers vs
  RLIMIT_MEMLOCK = 8 MiB on this box — the raise is a sudo/limits.conf
  change, named as a user task). S19's `--cells 8` re-read depends on it.

## Verdict

**Accepted — ships default-on in `infinityd`** (`--no-parse-batch-prefetch`
= the A/B off arm), the ADR-0030 D2 precedent. ADR-0033 records the
decision; the ADR-0005 parse-batch question is now closed end-to-end
(both halves of the demoted pipeline are adopted: fabric-apply at
ADR-0030, parse-batch here).
