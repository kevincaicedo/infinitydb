# M4.5-S31 dev-tier A/B verdict — tier-flush driver-op conversion (ADR-0084)

Date 2026-08-19 · i7-13700KF, governor `performance`, `no_turbo=1`,
env-check OK (clean tree per leg) · ADATA LEGEND 700 Gen3 DRAM-less NVMe
(the §19 device deviation, disclosed as always) · cells 4 `--pin-start 4`,
loadgen taskset 12–23 · instrument `inf-bench-s31` (3db039f + `--only-s29`
whitelist commit) — ONE instrument for every arm.

Arms (same night, in run order, evidence committed between legs):
- `base` = infinityd 7328da7 (pre-S31), 3 replicates — ran FIRST, on the
  post-build/post-sweep drive state
- `fix` = 3db039f (driver conversion + ColdDisplace rider), 3 replicates
- `driver` = 3db039f-dirty (rider disabled: `moved = true`), 3 replicates
- `base2` = 7328da7 re-run, 1 replicate — the same-night drive-state
  discriminator

## 1. Gate row (`gate-run m4.5 --only-s29`, medians of 3)

| arm | tiered@64 | tiered@256 | flat@64 | flat@256 | parity@256 | slope | p99 ratio |
|---|---|---|---|---|---|---|---|
| base (run 1) | 9,052 | 18,571 | 10,804 | 41,493 | 0.45 | 2.05 | 1.30 |
| fix | 10,273 | 30,337 | 13,327 | 51,752 | **0.59** | 2.95 | 1.79 |
| driver | 10,302 | 30,184 | 13,037 | 52,167 | **0.58** | 2.93 | 1.79 |
| base2 (repeat) | 10,259 | 29,560 | 13,182 | 52,295 | **0.57** | 2.88 | 2.00 |

**Verdict: no arm effect on throughput or parity.** The settled baseline
(base2) sits within noise of both fix arms at every point; base run 1's
0.45 was drive state (its rep0 flat@256 read 18.9k — a collapse leg),
not the binary. The parity@256 row stays red (~0.57–0.59 vs ≥ 0.7)
**unchanged through the conversion** — the blocking-flush mechanism
(ADR-0082 D4 residual 1) is **excluded** as the parity mechanism at this
shape, and the rider A/B (fix vs driver) excludes residual 3 as well.
The re-attribution lands on residual 2 — the unconditional cold resolve
on the tiered write path — owned by S30.

Reactor-drive engagement proof: `flush_rounds=110–118` per tiered@256
leg on the fix arms (the new INFO counter; 0 on base = absent field).

Foreground tail at the gate shape (tiered@256, settled arms):
p99 fix 13.3 ms (3 reps: 13.1/13.3/14.1) vs base2 15.1 ms (n=1);
p99.9 fix median 16.9 ms vs base2 20.5 ms (n=1) — **directional only**
(base2 is a single replicate; collapse legs appear in all arms).

## 2. Provoked-sealing tail leg (`sealstorm/`, ABAB ×3)

8 KiB inserts, keyspace ≫ budget (`MEM-BUDGET 32mb`/cell, FSYNC always,
128 conns) ⇒ demote ≈ ack rate ≈ 55–150 MB/s — the sealing-bound regime.

- **Client tails: drive-state dominated, no verdict either way.**
  Within-arm spread 2.5–2.8× on ops/s and 6–8× on p99.9, both arms,
  interleaved (base p999: 23.6/139/96 ms · fix p999: 73.7/17.4/108.5 ms)
  — the DRAM-less device's sustained-write collapse owns these legs,
  exactly the recorded everysec-leg discipline. Recorded, not cited.
- **The direct reactor-stall observable separates fully.**
  `loop_iter_p999_us` (INFO tripwires, one cell, post-leg) —
  base: 93 / 111 / 91 · fix: **71 / 73 / 81** (fix max < base min).
  Quantization note: at ~1 MiB slices, blocking-flush iterations are
  rarer than 1/1000 iterations, so p99.9 understates the stall (the
  stall lives at p99.99+/max, which INFO does not export) — the full
  separation at p99.9 is the indirect footprint of removing them.

## 3. Rider (fix vs driver)

No measurable throughput or tail delta at either shape (differences
within replicate noise). The rider is kept on its structural merits:
one staged WAL record per in-place overwrite instead of two, marker
bytes returned as S27 staging headroom, replay pairing proven
(`tiered_replay_routing.rs`, both halves). Its throughput hypothesis is
recorded as a **null result** at these shapes, not a win.

## Deviations / invalidity notes

- Dev tier, non-binding; the reference-box `gate-run m4.5` owns the
  binding verdict (unchanged posture).
- No fstrim available this session (no sudo): drive-state drift was
  handled by run-order interleaving + the base2 repeat instead. Base
  run 1 is therefore NOT comparable to the other arms; base2 is the
  baseline of record.
- base2 is n=1 (drift discriminator, not a full arm): tail deltas
  against it are directional.
- `s31:tiered_p999_c256_us` / `s31:flat_p999_c256_us` are set by the
  instrument but not rendered in the gate table (no gate row); the raw
  per-leg lines carry p50/p99/p999 per leg.
