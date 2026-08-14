# M4.5-S06 — checkpoint sidecar + recovery interplay (dev-tier artifacts)

Story: `docs/milestones/m4.5-indexes-query.md` S06 · ADR-0078 (schema +
semantics) under the ADR-0073 constraints · ledger
`reviews/infinity-m4.5-indexes-query.md`.

Box: the dev box (`linux-devbox-profile`) — dev-tier, non-citable; the
reference-box row binds at S17. Runs pinned `taskset -c 4`, governor/EPP
`performance`, sequential under `ulimit -v` per the heavy-runs
discipline. Page-cache-warm boots (the shard is written immediately
before the boots — disclosed; the S13 replay-gate precedent).

## Files

- `sidecar_recovery_10g.txt` — the binding recovery row: 10M docs ×
  ~1 KiB (≈ 10 GB of record bytes), 4 indexes (F64/Utf8/I64/I64,
  ~100k distinct values each — disclosed), 3 boot replicates per
  variant via the real `open_cell_log`:
  `sidecar` (load path, the gate), `control` (same corpus, no 0x06
  sections — the sidecar bytes' cost as a boot delta), `rebuild`
  (control boot + the S05 walk to convergence — the fallback, informational).
  Command: `INF_BENCH_DOCS=10000000 INF_BENCH_REPS=3 taskset -c 4
  cargo bench -p inf-server --bench sidecar_recovery`.
- `index_sidecar_ab_10m.txt` — the ADR-0078 D5 load-path A/B: 10M
  strictly-ascending pairs per scheme, `append` (the loader) vs
  `insert` (the general path), ns/entry + B/entry + slack, 3
  replicates. Budget: ≤ 60 ns/entry (the ledger napkin).
  Command: `taskset -c 4 cargo bench -p inf-store --bench index_sidecar`.
- `m45_sidecar_seeds.txt` — the DST sweep: `inf-sim --scenario
  m45-sidecar --seed N --verify-determinism` for seeds 1..=10 (storm
  counts, loads, refusals, hashes — every run hash-identical on its
  second execution).
- `fuzz_smoke.txt` — `cargo +nightly fuzz run fuzz_index_sidecar --
  -max_total_time=300` and the extended `ick_decode` smoke.
- `durable_sweep.txt` — `just durable-sweep` (10,000 m2-durable seeds,
  the recovery-interplay regression proof: non-indexed cells stay v1
  byte-identical and the M2 contract holds).
