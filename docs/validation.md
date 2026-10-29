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
| Commands, assumptions, decisions and result summaries | The pull request description; `docs/` when a decision changes what a document says |
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

Full CI runs on every push to `main` and Mondays at 07:23 UTC, as well as
the `ci-full` PR label, maintainer `/ci-full` comment and manual dispatch.
It runs strict-provenance Miri on `inf-alloc` and `inf-fabric`, all 14
existing fuzz smoke targets for 300 seconds each, and benchmark test/build
sanity. Automatic runs use their event SHA and have separate concurrency
groups. The PR lane retains its smaller Miri and RESP-fuzz canaries;
the nightly fuzz campaign remains separate. Benchmark sanity on hosted
runners validates the instruments, without producing performance evidence.

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

The NVMe is an explicit deviation from the Gen4 profile the device-bound
gates were specified against. Keep that limitation beside device-bound
results; do not silently relax a gate.
Inspect `lscpu`, `uname -a`, the filesystem/device, governor/EPP and thermal
state for each run. Four-cell reference campaigns use server CPUs 0,2,4,6 and
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

### Differences of a few percent

Two separately built binaries place the same code at different addresses,
and that alone moves timings. On the reference host, a cross-binary
Criterion comparison moved rows whose code had not changed by up to about
6 % in either direction — for example the `scan/simd` row of
`crates/inf-doc/benches/parse.rs` and `incr_plain` in
`crates/inf-server/benches/json_cmd.rs`. A same-binary A/A control cannot
see this, because both of its legs share one layout. A cross-binary timing
A/B is therefore no evidence for or against a change smaller than that band.

Before calling a small loss or gain:

1. **Count instructions.** Drive the operation for a fixed number of
   iterations `n` and for zero iterations, read exact user-space
   instructions for each (`perf stat -e instructions:u`), and report
   `(I(n) − I(0)) / n` per operation. Code placement does not move this
   count. An equal count shows that the change executes no more
   instructions. It does not show equal time: cache misses and branch
   behavior can change with no new instruction, so a timing claim still
   needs step 2.
2. **Time both variants in one binary.** Compile the old and the new
   implementation side by side into one benchmark binary and alternate their
   order across legs on one pinned CPU. The shared binary removes differences
   in the rest of the layout, and alternation cancels order effects and drift.
   Each variant's own code still sits at its own addresses, so its placement
   still differs; pair the timing with step 1's instruction count.

Record which of these the conclusion rests on. A cross-binary timing result
is context, not the verdict.

### Rows that fail a spread budget on an unchanged binary

Some rows fail a spread budget with no change in the code. Three causes have
been seen on the reference host, each with a known instance:

- **Two stable modes.** `numincrby_json` and `numincrby_json_forced_tree` in
  `crates/inf-server/benches/json_cmd.rs` read about 117 and 108 ns in most
  legs, and about 125 and 118 ns (6–10 % slower) in about one leg in four.
  The same two values appeared in two separate campaigns.
- **One outlier leg near the budget.** The ordered-map point probe at
  fanout 32, hot set 1 000, early-exit search
  (`crates/inf-store/benches/ordered.rs`): in one set of four legs, one read
  112.0 ns against 109.4–109.9 ns for the other three. The row's spread was
  2.3–2.4 % in three separate runs.
- **Timer resolution.** `direct_call` in
  `crates/inf-runtime/benches/executor.rs` is a 0.6 ns row, so a spread of a
  few percent is at the resolution of the timer.

Re-running such a row until it passes is not a measurement. Its repair is
specified before the run that uses it: a discarded warm-up leg or more legs
for an outlier, a declared rule for choosing a mode for a bimodal row, and an
instruction count or more work per sample for a row at resolution. A
comparison that depends on one of these rows states which repair it used.
Until a repair is in place, an over-budget result on these rows alone leaves
the comparison pending; it neither passes nor fails the change.

## Harness entry points

| Question | Maintained instrument |
|---|---|
| Cache, memory, durability and tiered gate matrices | [`inf-bench gate-run`](../bins/inf-bench/README.md) |
| Comparisons with independent generators | [`inf-compare`](../bins/inf-compare/README.md), `just benchmark` |
| Write accounting versus block-device counters | `cargo bench -p inf-store --bench write_accounting` |
| Write-amplification invariants | `cargo test -p inf-store --test tiered_write_amp` |
| Loading admission and recovery completion | `cargo test -p inf-server --test node_e2e loading_` |

Long-running reference-box campaigns — document wire and RSS shapes, the
parser-free read profile, the device-barrier protocol, recovery brackets and
the soaks — are driven by tools that are not part of this repository. A
result from one of them reaches public copy only as described in
[Recording a claim or gate result](#recording-a-claim-or-gate-result), with
its revision, box and command.

Harness source and `--help` define supported knobs and defaults. For example:

```bash
./target/release/inf-bench gate-run m0 --reference-box --replicates 3 \
  --artifacts-root .artifacts/gates/m0
just benchmark --reference-box --workload mixed --duration 15 \
  --out .artifacts/compare
```

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

