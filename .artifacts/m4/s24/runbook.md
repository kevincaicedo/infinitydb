# M4-S24 · Gate campaign runbook (prepared 2026-07-31, M2.5-S19 shape)

Campaign target: every §7 gate with citation-grade discipline on the
reference box (this box, ADR-0022 D1 — NVMe is Gen3 DRAM-less, disclose in
every artifact per D4). The machine gate table is `docs/milestones/
m4-gates.toml` (now the **full** §7 encoding — every row renders in every
`gate-run m4` / `ycsb` report as PASS/FAIL/PENDING; a PENDING row is a
visible gap, never a pass).

## Current campaign state (what is already banked, dev tier)

| leg | state | artifact |
|---|---|---|
| Instrument fix (slot asymmetry) | **DONE** — crossover A/B landed; A/A control clean (all clamped deltas 0.00, the 1279→1311 signature gone) | `.artifacts/m4/s24/instrument-aa-20260731/` |
| YCSB harness validation | **DONE** — 10 rows a–f + uniform, zipf self-check 57.66% vs 56.70% analytic, seed-verify, loader integrity | `.artifacts/m4/s22/1785480456-gate-run/` |
| S23 mechanics sample (30 min) | **DONE** — samplers/alerts/legs validated; verdict pipeline proven by an honest short-window FAIL (+3.5 MB warm-up drift ×48 extrapolation), MECHANICS-ONLY stamp | `.artifacts/m4/s23/soak-20260731-0255/` |
| Everything below | **BLOCKED on M4-S26 (command wiring)** or **user-run (binding env)** | — |

## Preconditions

1. **M4-S26 merged** for every tiered leg (phases 4–7). Phases 1–3 can run
   before wiring — they are the memory-mode/degenerate half.
2. Binding env (sudo): `sudo ./setup-infinity-benchmark-env.sh` from the
   outer root after any reboot; verify `inf-bench env-check` → OK,
   `perf_event_paranoid=-1`, `no_turbo=1`. Clean tree, artifacts committed
   between legs (`git add -f .artifacts/... && git commit`).
3. `touch bins/infinityd/build.rs` before the campaign build (version-stamp
   staleness); build once; copy binaries aside
   (`scratchpad/bin/`) and pass explicit `--infinityd-bin` paths so a
   mid-campaign rebuild cannot break fingerprint continuity.
4. M3 baseline binary: rebuild commit `a1ebcb9` as a **whole-workspace**
   `cargo build --release` from a worktree at a recorded absolute path
   (feature unification + debuginfo path embedding — a `-p infinityd`
   build differs by ~2 KB and the fingerprint will not reproduce). Record
   the new fingerprint in the report notes; the pin lives in the ledger row.
5. `INF_GATERUN_STDERR_DIR` must exist before every run (inf-bench does
   not mkdir it).

## Phase 0 — env + placement

- `inf-bench env-check` → OK; record governor/EPP/turbo in every header.
- Loadgen on cpus 12–23 (`taskset`), cells `--pin-start 4` (4 P-cores).
  Any row near ~7.0M ops/s is the E-core generator ceiling (named §19
  instrument limit) — label, don't celebrate.

## Phase 1 — the hard sub-gate: S03 degenerate A/B, final binding re-run

```bash
mkdir -p .artifacts/m4/s24/degenerate stderr && INF_GATERUN_STDERR_DIR=$PWD/stderr \
taskset -c 12-23 ./target/release/inf-bench gate-run m4 --reference-box \
  --replicates 6 --pin-start 4 \
  --infinityd-bin scratchpad/bin/infinityd-m4 \
  --baseline-bin scratchpad/bin/infinityd-m3-a1ebcb9 \
  --artifacts-root .artifacts/m4/s24/degenerate
```

- Fresh samples only (never reuse week-4 runs). The crossover instrument
  is a precondition **discharged 2026-07-31**; keep replicates even.
- Δ ≤ 1% on every row; tiering counters ≡ 0 (auto-aborts otherwise).
- A 1.2% delta is a STOP even in week 8 — no narrowing mid-campaign.

## Phase 2 — M3 regression rows (memory-mode namespaces)

Re-run the M3 gate set on the tiering build (same harness the M3 campaign
used; worst gate ≤ 5%). Carry into the campaign table:
`--m3-regression-pct <worst> --campaign-note "<m3 artifact dir>"`.

## Phase 3 — inf-compare memory-mode rows (the wedge must not erode)

```bash
inf-bench compare --engines redis,dragonfly,infinityd --generator both \
  --workload set,get,mixed,memory --pipeline 1,16 --reference-box
```

Competitors in-run or the row does not exist (L10). memtier drives only
this memory-mode surface; every tiered row stays `inf-bench`-labeled.
Note: `.gitignore` ignores `.artifacts/compare` — `git add -f` the
report + logs.

## Phase 4 — YCSB tiered rows (needs S26)

```bash
# Tiered leg (10× RAM) + the RAM-resident reference leg, same campaign:
taskset -c 12-23 ./target/release/inf-bench ycsb --reference-box \
  --mem-budget-mb 2048 --dataset-multiple 10 --duration 60 --verify-seed \
  --data-root $HOME/.cache/inf-tmp --artifacts-root .artifacts/m4/s24/ycsb
taskset -c 12-23 ./target/release/inf-bench ycsb --reference-box \
  --mem-budget-mb 2048 --dataset-multiple 1 --duration 60 \
  --data-root $HOME/.cache/inf-tmp --artifacts-root .artifacts/m4/s24/ycsb-ref
```

- Produces `ycsb:cold_read_p99_ms` directly; the hot-set deltas
  (`ycsb:hot_set_p{50,99,999}_delta_pct`) compare the tiered leg's
  memory-hit split against the reference leg's split — same instrument,
  same campaign (the plan-recorded interpretation of "memory-only").
- The harness hard-errors if the wired plane does not emit the
  `SPLIT_FIELDS` INFO schema (the S26 contract in `ycsb.rs`).
- QD-cap/weight calibration (ADR-0055 D2/D3) and the ADR-0063 D3 reserve
  re-read happen here; a tripwire bound change is an ADR, not an edit.

## Phase 5 — mixed-node re-audit + recovery + matrices (needs S26)

```bash
taskset -c 12-23 ./target/release/inf-bench mixed-audit --reference-box \
  --duration 25 --data-root $HOME/.cache/inf-tmp        # full 3-workload form
# recovery row: M2 gate re-run with tiering on (S12 shape, 10 GB)
# DST: 10k-seed sweeps m4-recovery + m4-diskfull + m4-pressure + m4-cold
#   (sequential, ulimit -v per sweep — the box-freeze discipline)
# crash matrix: cargo test -p crash-matrix (m4.toml rows all green)
```

The mixed-audit gate rows carry in via `--mixed-attribution-pct` /
`--cache-isolation-pct`; recovery via `--recovery-gbps-per-cell` /
`--recovery-boot-s`; sweeps via `--dst-violations` / `--crash-failures`;
storms via `--foreground-p999-ms` — each with `--campaign-note` naming its
artifact (enforced, L10). Also the S20 named-ns `MAXMEMORY` enforcement
finding still needs an owner before any multi-cache claim.

## Phase 6 — 24 h endurance (user wall time)

`.artifacts/m4/s23/RUNBOOK-24h.md` — run after S26 + a clean 1 h tiered
soak. Carry `--endurance-rss-slope-pct` / `--endurance-crashes`.

## Phase 7 — flamegraphs + attribution per row

One `perf record -C 4,6,8,10 -F 1997 -g --call-graph dwarf,16384` window
per row class (degenerate, ycsb hot, ycsb cold-flood, mixed); extract
reports, **delete perf.data immediately** (tmpfs RAM); every cited row
names its flamegraph. `kptr_restrict=1`: bound the kernel bucket, don't
split it.

## Final assembly

One `gate-run m4` invocation with every external carrier + the in-run rows
= the campaign gate table for S25's verdict ADR. Then the ledger walk:
draft one row per §7 gate (Allowed/Narrowed wording per the S19-closure
worksheet shape), dev-tier rows stay Evidence-pending, and the release
re-validation rule applies (a row not re-run this campaign reverts to
Evidence-pending for the v0.4.0 announcement).
