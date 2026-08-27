# S34 close-out campaign P (2026-08-27) — after `sudo fstrim`: the 10k sweep, the everysec row, the cold replay row, the S18 10 GB row

Written **before** the first leg. Reference box (`env-check` per run);
the drive was `fstrim`med by the owner before this session (disclosed
maintenance, `devbox-tmp-usrquota-drive-state`); no other job runs on
the box for the chain's duration (the `linux-devbox-profile` rule —
nothing compiles during a leg). Engine: the inner commit that lands the
S42 lifecycle fixes, the probe read row, the derived replay cap and
`inf cache-evict` (named in `campaign.log`'s header).

## Legs, in order (one job on the box at a time)

- **P0 — `just durable-sweep` at 10 000 seeds** (the S43 default-on
  AC's owed sweep: the FLUSH-class group hold shipping at 250 µs since
  `6756643`): 8 shards × 1 250 seeds of `m2-durable`, base seed
  `0xD5EE0000`, each shard under `ulimit -v 4 GiB` + `nice 10` (the
  heavy-runs rule). **Rule:** 0 violations; refusals only of the
  ADR-0021 D3 class, each disclosed.
- **P1 — the everysec penalty row, post-trim (`gate-run m2
  --only-everysec --reference-box --cells 4 --pin-start 0 --replicates 3
  --duration 10`)** × 6 runs: flush, fua, fua, flush, flush, fua
  (campaign M's M2 recipe verbatim; harness on cores 8–23; `flush` =
  `--barrier-class flush --model-absent`, `fua` = `--barrier-class fua`
  + the data-root's probe file). **Rule (plan S34 AC, ADR-0086 D9):**
  the everysec namespace's ops/s median under fua within ± 2 % of
  flush **and** the penalty percentage within ± 2 points, on the three
  runs' medians; **validity:** the drive-state regime absent — everysec
  fsync p999 < 1 s on both arms and each arm's in-run penalty spread
  < 10 points; otherwise the clause stays `Evidence-pending (drive
  state)` with the numbers on the record.
- **P2 — the cold replay row (`gate-run m4.5 --only-s39d --reference-box
  --cells 4 --pin-start 0 --barrier-class fua --s39d-baseline
  flush-class --s39d-cold-boot --replicates 3 --leg-idle-s 40
  --s39d-warm-records 3000000 --s39d-tail-records 200000 --device-stat
  nvme0n1`)**: campaign M3's shape with **both arms booted cold** (every
  file of the crashed image synced + `fadvise(DONTNEED)` by `inf
  cache-evict` after the 40 s idle, immediately before the one boot) —
  the C38b clause compared packed-vs-aligned, not warm-vs-cold. **Rule
  (C38b "within 1 %"):** `s39d:phase_replay_x` (Σcells replay time,
  fua ÷ flush, per-replicate median) within 1.00 ± 0.01 — with the
  instrument floor named (H2's ± 2.5 % replicate spread on this phase):
  if the three pairs' spread exceeds the bar, the clause is reported as
  measured and stays `Evidence-pending (instrument)`; `replay_gbps_per_
  cell_{arm,base}` disclosed (the ADR-0088 D4 term). **Validity:**
  `cold_boot` non-zero on every leg; `records_recovered_match` 1.
- **P3 — the S18 recovery row at its 10 GB shape (`gate-run m4.5
  --only-s39d --reference-box --cells 4 --pin-start 0 --barrier-class
  fua --s39d-baseline flush-class --s39d-cold-boot --replicates 3
  --leg-idle-s 40 --s39d-warm-records 10000000 --s39d-tail-records
  200000 --device-stat nvme0n1`)**: 10 M × 1 KiB warm records (≈ 10 GB
  of checkpoint image — the §7 "10 GB node" shape on four cells reading
  cold), the same boundary/tail/SIGKILL/idle/cold boot. **Rule (the S18
  < 15 s STOP gate re-read):** `s39d_recovery_first_boot_s_arm` ≤ 15 s
  (process launch → `loading:0` on every cell, arm, per-replicate
  median); the phase decomposition names the term. **Informational:**
  the ADR-0088 D4 falsifier — the slowest cell's replay phase ≤ 5 s
  (`replay_budget_s`) under the derived cap; `write_amp_milli_log_
  checkpoint` at this shape (the amendment's ≈ 8.4 consequence);
  `ckpt_replay_bytes_per_s` / `ckpt_cap_bytes` from INFO on the record.
  Both arms carry the probe file (the derived cap applies to both).

## The probe file

`data-P/io-properties.toml` is written fresh by this engine's `inf
probe-device data-P --seconds 2` at the start of the chain (the read row
measured on the trimmed drive), and copied into every spawn's data dir
by the harness (`copy_probe_file`). Its `read_bytes_per_s_256k` decides
the derived cap for P2/P3; the value is recorded in `campaign.log`.

## What the chain does not do

No comparator-in-run row (none of these rows is comparative); no
compile or test during a leg; no mid-leg re-runs. Artifacts land under
`infinitydb/.artifacts/m4.5/s34/campaign-P/` after `CHAIN DONE`.

## Results (2026-08-27, engine `9c75b94`; rules above were written first)

Chain restarts, disclosed: chain a's P1 legs at 16:57 and chain c's P3 at
17:24 were refused by `env-check` (dirty tree — an S37 edit, then two
governing-doc edits, landed in the checkout during the chain); the
refused legs wrote no report; P0 stands from chain a, P1–P2 from chain
b, P3 from chain c, all in the predeclared order (`campaign.log` carries
every header).

- **P0 — `m2-durable` × 10 000 (`p0-sweep/`):** 0 violations, 0
  refusals on every shard; the FLUSH-class group hold engaged (`waits_
  group` 184 k on shards 0/4). The S43 default-on closure's owed sweep.
- **P1 — everysec after `fstrim` (`p1-esec-*/`):** penalty flush 18.8 /
  57.1 / 41.4 %, fua 55.6 / 59.7 / 48.7 %; in-run everysec spread
  56–107 points on every run; flush fsync p99 0.1–4.7 s, fua p99
  0.33–0.37 s; memory-ns legs 2.36–2.41 M ops/s. **Validity clause
  fired — not adjudicable; `Evidence-pending (drive state)` stands**
  (drive at 72 % fill; trim did not clear the regime). Class fact
  again: fua per-second fdatasync p50 4.7–5.1 ms vs 60–76 ms buffered.
- **P2 — cold replay (`p2-s39d-cold/`):** both arms evicted (22 files,
  6.38 GB per leg). Replay-phase rate on the slowest cell, per-replicate
  median: **arm 0.29 GB/s, baseline 0.27 GB/s per cell** (1.07 × — at
  parity or better inside the ± 2.5 % phase floor): the C38b clause is
  met cold-vs-cold; campaign M3's 3.23 GB/s baseline was the page cache.
  Arm cold boot wall 3.51–3.62 s vs 3.30–3.38 s (the recycled log's slack
  audit, 1.1–1.2 s vs 0.09 s); `recovery_total_x` 1.05 (diagnostic);
  records match 1.00. The probe's read row (967 MB/s ÷ 4 = 242 MB/s per
  cell) is conservative against the measured replay.
- **P3 — S18 at 10 GB, cold (`p3-s18-10g/`):** 10.2 M records × 1 KiB,
  four cells, every image evicted (arm 22 files / 13.76 GB, base 18
  files / 12.68 GB). **Arm first boot to `loading:0` 8.34 / 8.38 / 9.02 s
  — median 8.38 s ≤ 15 s: the S18 STOP gate re-read PASSES on the
  shipping default, cold**; baseline 7.62–7.76 s. Slowest cell: checkpoint
  load 6.82–6.93 s (≈ 82 % of every boot, both arms), replay 0.54–0.89 s
  (arm) / 0.70–0.73 s (base) at 0.35 / 0.37 GB/s per cell, the arm's slack
  audit 0.74–1.21 s vs 0.05 s; records match 1.00; `recovery_total_x`
  1.09 (diagnostic). ADR-0088 D4's falsifier is silent (replay ≤ 5 s
  under the derived cap). The next recovery term at this shape is the
  checkpoint load (`.ick` read-ahead / block size — ADR-0090 A13.4), not
  replay and not recycling.
