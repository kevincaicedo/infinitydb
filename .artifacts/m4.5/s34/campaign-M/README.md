# S34 campaign M — the owed rows: everysec penalty, read rows, replay (C38b) — FLUSH vs FUA, same night, interleaved

Written 2026-08-25 **before** the first leg. Reference box; tier per
each run's own `env-check` header. Three rows, each 3 interleaved
replicates per arm, order alternated so drive state lands on both arms.

## Arms (the same engine binary; the class is the only difference)

- **flush** — `--barrier-class flush --model-absent`: `Buffered`
  segments, packed frames, K = 1, no device model — the pre-S34 write
  path byte-for-byte (ADR-0091 D1's `off` tier with the file absent).
- **fua** — `--barrier-class fua` with the reference device's schema-2
  `io-properties.toml` at `--data-root` (the S35/S39b campaigns' file:
  `fua_max_frame_bytes 262144`, write 510 MB/s): `Direct` segments,
  aligned v3 frames, K auto = 3 / 4 MiB, the device budget, one-slot
  recycling + quarter wait, fill 1 ms / 16 KiB — the shipping probed
  configuration.

## Rows

- **M1 — read rows + the always AC leg (`gate-run m4.5 --only-s35
  --reference-box --cells 4 --pin-start 0 --replicates 1 --duration 10
  --leg-idle-s 40`)** × 6 runs: flush, fua, fua, flush, flush, fua.
  Harness on 8,10,12,14. The read leg (64 conns × P16, 100 % GET over
  the keys the write legs populated) is the S34 AC's "every read row";
  the c32/c256 `always` legs are disclosed beside it (S35 owns them).
- **M2 — the everysec penalty row (`gate-run m2 --only-everysec
  --reference-box --cells 4 --pin-start 0 --replicates 3 --duration
  10`)** × 6 runs: flush, fua, fua, flush, flush, fua. One durable node,
  a `memory` namespace and an `everysec` namespace, the identical 1:1
  mix (64 conns × P16) against each in ABBA × 3 inside the run;
  truncation off for the run (the row's shape, ~2–4 GB per run at 10 s
  legs). Harness on cores 8–23 (the 64-thread generator must not be the
  bottleneck; cells hold 0,2,4,6).
- **M3 — the replay row (`gate-run m4.5 --only-s39d --reference-box
  --cells 4 --pin-start 0 --barrier-class fua --s39d-baseline
  flush-class --replicates 3 --leg-idle-s 40 --s39d-warm-records 3000000
  --s39d-tail-records 200000 --device-stat nvme0n1`)**: the S39d
  fixed-work shape (3 M warm + 200 k tail records × 1 KiB, the same
  `INF.CKPT WAIT` boundary, SIGKILL, 40 s idle, one boot) with the
  **baseline = the FLUSH class** (packed log) and the **arm = the FUA
  class** (aligned log, recycling on). Both arms carry the device model
  (the row copies it for every spawn; the class is the only difference
  — the model does not touch recovery). Interleaved ABBA inside the run.

## Predeclared rule (plan S34 AC; ADR-0086 D9)

- **Everysec penalty:** the `everysec` namespace's ops/s (median of the
  three runs' medians) under fua within ± 2 % of flush, and the penalty
  percentage within ± 2 points; p999 disclosed.
- **Read rows:** M1's read-leg ops/s median under fua within ± 2 % of
  flush (per-run values disclosed; the S35 read leg's own replicate
  spread is ± 1 % on the record).
- **Replay (C38b's "within 1 %"):** `s39d:phase_replay_x` (Σcells
  replay time, fua ÷ flush, per-replicate median) within 1.00 ± 0.01.
  The instrument's own replicate spread on this phase read ± 2.5 % in
  campaign H2 (420–440 ms per cell); **if the spread of the three pairs
  exceeds the 1 % bar, the clause is reported as measured with the
  instrument floor named and stays `Evidence-pending (instrument)`,
  never closed on a number the instrument cannot resolve.** The
  replay-phase rate per cell (`s39d:replay_gbps_per_cell_{arm,base}`)
  is disclosed against C38b's 1 GB/s bar — informational (C38b's own
  row is the 10 GB tiered node).
- **Falsifier:** any clause outside its band on the median ⇒ that clause
  is red, S34 stays `Implementation complete — evidence pending` with
  the clause named and the losing arm's numbers recorded (L4); the
  probed default (ADR-0091) is not re-dispositioned by this campaign
  (its bar is S42's).
- **If every clause holds ⇒ S34 `Done, reference-box evidence`** (its
  `always` clauses were discharged by S35/S39a on the record; this
  campaign discharges the penalty, read and replay clauses).

## Predictions on the record

Reads flat (± 1 %, the path is not the log); everysec within noise
(the everysec frames are plain writes on both classes; the fua arm's
per-second `fdatasync` on an `O_DIRECT` segment vs the buffered one —
unknown sign, small); replay: the aligned log carries padding at the
fill's frame size (≈ 1–3 % more bytes for the same records) and skips
zeros word-wise where the packed log stops at its end — expected within
a few percent, direction unknown.
