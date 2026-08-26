# S42 campaign L — the stock first boot on the reference device (ADR-0091 D6)

Written 2026-08-25 **before** the first leg. Reference box; tier per the
run's own `env-check` header.

## Row

`gate-run m4.5 --only-s42 --reference-box --cells 4 --pin-start 0
--replicates 3 --duration 10 --leg-idle-s 40 --data-root
~/bench-data/s42/data-L` — per replicate: a fresh data directory with
**no** `io-properties.toml`; `infinityd --data-dir <dir> --cells 4
--pin-start 0 --device-probe auto` (the product default, named on the
spawn so the harness rule cannot turn it into the dev tier; no other
flag) booted and timed to `loading:0` on every cell (the probe runs
before the listener); the provenance INFO read there; the server
killed; the same command booted again and timed the same way (the file
the first boot wrote); then the S35 AC leg (32 conns, pipeline 1, 10 s,
`always`) and the read leg (64 conns × P16) on that server. Harness on
cores 8,10,12,14; cells from 0; 40 s idle before the first boot and
before the AC leg.

The row refuses every arm flag — it measures the stock boot or nothing.

## Predeclared rule (plan S42 AC, ADR-0091 D6)

- **Binding:** `s42:probe_overhead_s` (first − second boot, median)
  ≤ 15 s; `first_boot_probed` = 1 (every cell `probed-at-boot`);
  `second_boot_from_file` = 1 (every cell `file`, identity `verified`);
  `schema` = 3; `s42:p50_over_barrier_x` ≤ 1.3 (S35's bar on the class
  the probe chose).
- **Comparison (the AC's "inside the probed arm's replicate spread"):**
  the stock boot's c32 ops/s, p50 ÷ barrier and read ops/s medians
  against the **same-night explicit probed arm** — campaign M's three
  `--barrier-class fua` S35 legs (the schema-2 reference file, K auto)
  — inside that arm's replicate spread, or within ± 10 % of its median
  where the spread is narrower than 10 %. The plan's "≈ 1.2, ≈ 39 k" is
  the K = 3 / 2 MiB campaign's figure; the shipping K = 3 / 4 MiB read
  32.5–35.0 k in campaign G (2026-08-22) — the same-night arm is the
  comparator, those numbers are context.
- **The class verdict is a reading, not a bar:** the in-boot probe is
  one second per row; if the stock boot rules `flush` on this device
  after a 40 s idle, that is a finding against ADR-0091 D1's probe
  robustness (the CLI probe and the smoke ruled `fua` here: 293 vs
  918 µs), recorded as such, and the AC is not met.
- **Falsifier:** any binding clause missed on the median ⇒ S42 stays
  `Evidence-pending` with the clause named; the lifecycle is not
  re-dispositioned by this run.

## Prediction on the record

First boot ≈ 10–11 s (the smoke read 10.27 s at 2 cells), second boot
< 1 s; `fua`, `fua_max_frame_bytes 262144`, K = 3, model probed;
c32 ≈ 32–40 k ops/s at p50 ÷ barrier ≈ 1.2; reads ≈ 1.5–1.6 M/s.
