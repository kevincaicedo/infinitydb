# Reproducing validation

The source of truth is the code and its executable checks. Commit regression
tests, fixtures, minimized fuzz inputs, DST seeds and maintained harnesses.
Generated logs, profiles, benchmark reports and database images stay local or
in temporary CI storage. Both `.artifacts/` and `artifacts/` are ignored at
any depth; there is no tracked gate, claim, comparison, review or release
output archive. `scripts/check-doc-artifacts.sh` also rejects force-added
output directories.

## Where things belong

| Content | Location |
|---|---|
| Unit, property and integration tests | Owning crate's `src/` or `tests/` |
| Client/reference and crash checks | `tests/compat/`, `tests/crash-matrix/` |
| Deterministic scenarios and retained seeds | `bins/inf-sim/src/`, `bins/inf-sim/seeds/` |
| Fuzz targets and minimized regression inputs | Owning crate's `fuzz/` source and regression corpus |
| Benchmark instruments | `bins/inf-bench/`, `bins/inf-compare/`, crate `benches/` |
| Reusable validation/campaign drivers | `scripts/` |
| Commands, assumptions, decisions and result summaries | `docs/` and the owning ledger |
| Generated output | Ignored `.artifacts/`, `target/` or an explicit external directory |

Do not replace a failing test with its transcript. Record a defect's trigger,
oracle, test name and command. A DST failure needs the scenario, seed and
configuration; a decoder failure needs the minimized input beside its test.
Distinguish model coverage, client/binary coverage and real-reference checks.

## Correctness and STOP gates

Run from the Rust repository root:

```bash
just check
cargo deny check
just compat
just sim-smoke
```

`just check`, workspace tests and `just compat` require Redis **8.0.5** on
PATH or `INF_COMPAT_ORACLE_ADDR` pointing to that version. An absent,
broken or wrong-version oracle fails; there is no successful skip. The
compat corpus changes `maxclients` to 20,000, so raise the shell's soft
file-descriptor limit if necessary (`ulimit -n 65536`). `just compat`
also requires the real InfinityDB binary it builds. CI's build matrix
explicitly excludes the compat package; the separate `compat-diff` job
executes it with pinned oracles. Full reference checks may need additional tools or pinned oracle
images; see [CONTRIBUTING](../CONTRIBUTING.md). A local prerequisite failure
is not a passing gate. Workloads and required sweep sizes remain those of the
owning gate; a smoke run does not discharge a full campaign.

`cargo test -p crash-matrix` requires Python 3.11+ and runs every node row
in `m2.toml`, `m4.toml` and `m45.toml` by exact package/target/function.
Each invocation must pass one non-ignored test and emit its point/verdict
receipt after the assertions. Missing, ignored or empty carriers and
invented verdicts fail. Cargo resolves current test binaries offline;
receipts live only in captured child output. Linux reactor rows are
explicitly unsupported on other hosts and must pass on Linux CI.

Run the smallest relevant check while developing, then the required suite:

```bash
cargo test -p inf-log
cargo run --release -p inf-sim --features dst --bin inf-sim -- \
  --scenario m2-durable --seed 0xC0FFEE --verify-determinism
just durable-sweep 10000 0xD5EE0000
```

For unsafe leaves use the applicable Miri/Loom checks; for decoders run the
owning fuzz target. Their commands and prerequisites live in
[CONTRIBUTING](../CONTRIBUTING.md), crate `SAFETY.md` files and the
[simulator guide](../bins/inf-sim/README.md). Keep checks in CI where practical.

## Performance and the reference box

Anyone can run the harness on their own machine. Results describe that
machine and workload; hardware differences can change throughput, latency
and the bottleneck. A reference-box designation does not turn a failed
environment probe or saturated generator into a valid measurement.

The project's designated HomeLab reference profile was recorded in
ADR-0022; these are reference details, not a live host-health assertion:

| Component | Recorded reference |
|---|---|
| CPU | Intel Core i7-13700KF, 8 P-cores + 8 E-cores |
| RAM | 30 GiB available in the designated environment |
| Storage | ADATA LEGEND 700, consumer PCIe Gen3, DRAM-less NVMe |
| OS | Linux; record the exact kernel on every run (historical campaigns used 7.0.0-27 through -31) |
| CPU policy | Verified `performance` governor/EPP, thermal checks, disclosed turbo/SMT state |
| Placement | Server and load generator on disjoint physical cores; record affinity and sibling lists |
| Repeats | 3–5 under the same declared workload, with result spread and A/B ordering |

The NVMe is an explicit deviation from the plan's Gen4 profile. Keep that
limitation beside device-bound results; do not silently relax a gate.
Inspect `lscpu`, `uname -a`, the filesystem/device, governor/EPP and thermal
state for each run. Four-cell S37 campaigns use server CPUs 0,2,4,6 and
generator CPUs 8,10,12,14 on this host; verify topology before reusing those
numbers elsewhere. Measure device profiles on the target device, never copy
a historical `io-properties.toml` as portable configuration.

Build and check the measured revision before starting load:

```bash
git rev-parse HEAD
git status --short
rustc --version --verbose
cargo build --locked --release -p infinityd -p inf-bench -p inf-compare -p inf
./target/release/inf-bench env-check
```

For an A/B, build both declared revisions with the same toolchain/features
in separate clean checkouts. Record full baseline and candidate commit IDs,
binary hashes, configurations and workload/seed. Use the harness's baseline
binary option where available; otherwise alternate the two explicitly built
binaries. A/B against an unnamed cached executable is not reproducible.

The repository cleanup baseline is commit
`10155d3501dc65381a13dbad21fb4863c65cd9fd`: source-identical to the former
`2a86a3c` outside the removed output tree. It is a source baseline, not a new
performance measurement. Historical commit IDs changed during the rewrite.

## Harness entry points

| Question | Maintained instrument |
|---|---|
| Cache, memory, durability and tiered gate matrices | [`inf-bench gate-run`](../bins/inf-bench/README.md) |
| Comparisons with independent generators | [`inf-compare`](../bins/inf-compare/README.md), `just benchmark` |
| Document wire and RSS shapes | `scripts/bench-m3-wire.sh`, `scripts/bench-m3-rss.sh` |
| Parser-free document reads | `scripts/check-doc-read-profile.sh` |
| Ticketed DEL, shadow overwrite, DBSIZE | [`S37 protocol`](validation-s37.md), `scripts/run-s37-reference.sh` |
| Recovery shape and cold/warm cache | `scripts/recovery-brackets.sh` (set `BIN` to the declared build) |
| Soak with cache, documents and tiered storage | `scripts/soak-unified.sh` |
| Write accounting versus block-device counters | `cargo bench -p inf-store --bench write_accounting` |
| Write-amplification invariants | `cargo test -p inf-store --test tiered_write_amp` |
| Historical model refactor equivalence | `scripts/model-equivalence-reproduce.sh` (two pinned revisions) |
| Loading admission and recovery completion | `cargo test -p inf-server --test node_e2e loading_` |

Harness source and `--help` define supported knobs and defaults. For example:

```bash
./target/release/inf-bench gate-run m0 --reference-box --replicates 3 \
  --artifacts-root .artifacts/gates/m0
just benchmark --reference-box --workload mixed --duration 15 \
  --out .artifacts/compare
```

The historical equivalence check needs a full clone containing both pinned
revisions; it compares that refactor, not all later model changes.

Local output is useful for inspecting a run and diagnosing failures. It is
not committed. Keep correctness fixtures separate from generated data so a
fresh checkout can recreate checks without an old output directory.

## Recording a claim or gate result

Record the test/harness, exact command, revisions, tool versions, workload
and seed, reference environment, measured result and spread, expected bound,
exit status, limitations and disposition. Public numbers still need actual
valid measurements, clean-tree reference runs, same-run tripwires and release
revalidation. The availability of a harness alone proves no performance claim.
For failed attempts retain a concise reason; do not accumulate log bundles.

In the combined development checkout, the master plan, milestone plans,
ADRs and claim/review ledgers live in the parent `docs/` and `reviews/`.
ADR-0127 supersedes their older instructions to commit run output. Historical
artifact paths are retired provenance labels. Revalidate an old result with
the maintained harness before using it for a new release claim.
