# Batch 32 validation — 2026-09-11

Baseline: `c4ea42c` (batch 31). This package covers the model style split,
B18-23-R11 interface propagation, deferred 64-seed tiered/recovery sweeps,
and the S37 campaign instruments. The paired report is
`reviews/infinity-batch32-followup-20260911.md` in the parent repository.

`validation.json` records observed command exits, with logs. `just check`
passed 2,238 test executions across its repeated feature suites. Both
model runs passed 88 tests. Snapshot equivalence covers 40,000 revision
and 20,000 identity histories, comparing results, statistics and exact
error text against the baseline. Reproduce from the workspace root with
`bash .artifacts/review/batch32/model-equivalence-reproduce.sh`.

Both deferred sweeps pass all 64 seeds from `0xc10000`. Their manifest
and result files are retained under `tiered-64/` and `recovery-64/`.
These are simulator campaigns, not a production crash-fleet acceptance.
The 64 intentional DISKFULL commands in the tiered campaign are distinct
from recovery-taxonomy refusals (zero).

Final checks after the benchmark reporting edits: formatting, benchmark
Clippy, 61 benchmark tests, shipping release build and Bash syntax all
exit 0. The Python host sampler also parses. The pre-existing `_rdtscp`
Clippy configuration warning remains open. Dependency checking used the
cached advisory database (`--offline`); no live refresh was performed.

S37 development smoke, including refused trials, is retained under
`.artifacts/m4.5/s37/campaign-R/`. `smoke-binaries.json` identifies the
instrument used for the successful fresh-database DBSIZE and D9 smoke.
Final benchmark edits add checked counter arithmetic, summary notes and
write-leg CPU/error reporting; targeted checks passed without repeating
the long smoke. `final-binaries.json` identifies the final local build
before the source commit; campaign R's `reference-preflight.json` records
the shipping build at clean commit `6429fba`.
Source hashes are in `source-manifest.json`; artifact checksums are in
`artifact-sha256.txt`. The checksum list excludes itself.

Reference admission is blocked by nonzero lifetime thermal-throttle
counters. No override is reference evidence. No performance claim or
shadow-overwrite default was changed. No full soak, 10k-seed durable
sweep, kernel matrix, independent GETDEL performance row, C25 re-proof,
Loom, Miri or decoder fuzz campaign was run in this follow-up.
