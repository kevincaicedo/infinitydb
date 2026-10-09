# Contributing to InfinityDB

Thanks for your interest in InfinityDB. This is an **alpha** project under
active development; internal interfaces change between milestones. Issues,
questions, and pull requests are welcome.

- [Ground rules](#ground-rules)
- [How a change is made](#how-a-change-is-made)
- [The reading order](#the-reading-order)
- [Development setup](#development-setup)
- [Onboarding: prove the toolchain (~30 minutes)](#onboarding-prove-the-toolchain-30-minutes)
- [The validation ladder](#the-validation-ladder)
- [Design laws](#design-laws)
- [Unsafe code](#unsafe-code)
- [Evidence & performance claims](#evidence--performance-claims)
- [Commits & pull requests](#commits--pull-requests)
- [Reporting bugs](#reporting-bugs)

## Ground rules

- Be respectful and constructive.
- Open an issue to discuss anything non-trivial before sending a large PR —
  the architecture has strong invariants (see below) and a change that breaks
  one needs design discussion first.
- By contributing, you agree your contributions are licensed under the
  project's [Apache-2.0](LICENSE) license.

## How a change is made

InfinityDB proves a design before it writes the code (law L12). The loop
every change runs through:

1. **Start with an issue.** Describe the problem, the behavior you expect,
   and how you would observe it. Small, local fixes (a typo, an obviously
   wrong error string with a test) can go straight to a PR.
2. **Design first.** A change that adds state, a limit, a queue, an on-disk
   or wire format, a decoder or a new crate edge starts as a short written
   design in the issue (or a design note in the PR, before any code). It
   answers the questions in
   [InfinityStyle § Design Before Code](docs/INFINITY_STYLE.md#design-before-code):
   the state machine as a table, what is validated and reserved before
   publication and what each failure leaves behind, the cost at scale
   including construction and recovery, the resource expected to saturate,
   every new limit with its unit and crossing behavior, the hostile inputs
   and the oracle that catches a violation, and the observation that would
   reject the design. Someone who did not write the design reviews it
   before code is written.
3. **Frozen things change by decision first.** A change to a frozen
   interface ([`docs/interfaces-m0.md`](docs/interfaces-m0.md),
   [`docs/interfaces-m2.md`](docs/interfaces-m2.md),
   [`docs/interfaces-m4.5.md`](docs/interfaces-m4.5.md)), an on-disk or wire
   format, a design law, or the crate dependency graph
   ([`docs/dep-dag.toml`](docs/dep-dag.toml)) needs a decision accepted by
   the maintainers **before** the code. Maintainers record it as a numbered
   decision (the `ADR-NNNN Dn` identifiers you will see in code comments),
   and the code cites it.
4. **Build it with its tests in the same change.** A defect gets a
   red-first regression test at the layer that owns the invariant (plus a
   client-level test if clients can reach it). A decoder gets a fuzz
   target. Behavior across crashes, reordering or concurrency gets a
   deterministic-simulation scenario or crash-matrix row. Reply bytes are
   byte-diffed against Redis in the compatibility corpus. Unsafe code gets
   Miri/Loom coverage. Every new oracle has a canary: a planted bug that
   turns it red.
5. **Validate.** `just check` and `cargo deny check` green, plus the layer
   checks for what you touched (see [the validation ladder](#the-validation-ladder)).
6. **Commit and open the PR.** One logical change per commit, a short
   one-line message stating the concrete change, and the
   [PR checklist](.github/PULL_REQUEST_TEMPLATE.md) filled in. The PR
   description carries the reasoning, the commands you ran, their results,
   and **what you did not run**.

A change is **done** when it is reachable from the wire at the shipped
topology (the multi-cell `infinityd`, not only a library tier), its tests
landed with it, `just check` and `cargo deny check` are green, and a fix
answers its class question: which type, table or lint makes the sibling
bug impossible — or why no such class exists.

## The reading order

These are governing documents, not background reading — reviews are run
against them. Read in this order before your first substantive PR:

1. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — how InfinityDB works:
   the shared-nothing cell model, the life of a request, the log and the
   storage engine, and why the system is shaped this way.
2. [`docs/INFINITY_STYLE.md`](docs/INFINITY_STYLE.md) — the **normative**
   engineering style for designs and code: what a reviewer will hold your
   design and your PR to. The
   [PR checklist](.github/PULL_REQUEST_TEMPLATE.md) is its operational
   form.
3. For work on a seam or on durability: the interface documents
   ([`interfaces-m0.md`](docs/interfaces-m0.md),
   [`interfaces-m2.md`](docs/interfaces-m2.md),
   [`interfaces-m4.5.md`](docs/interfaces-m4.5.md)); for a new crate edge,
   [`docs/dep-dag.toml`](docs/dep-dag.toml).
4. [`docs/validation.md`](docs/validation.md) — what to run, where tests and
   seeds live, and the rules for performance evidence.

## Development setup

Requirements:

- **Rust 1.95** (the toolchain is pinned in `rust-toolchain.toml`).
- **Linux with `io_uring`** to run the server (kernel 5.15+, 6.1+ recommended).
  macOS builds and tests via `kqueue` for development/correctness, but is not
  a performance target.
- **`redis-server` 8.0.5** on `PATH` (or `INF_COMPAT_ORACLE_ADDR` pointing at
  one): `just check`, the workspace tests and `just compat` byte-diff
  against it, and an absent or different version fails rather than skips.
  The compat corpus raises `maxclients`, so raise the shell's file-descriptor
  limit if needed (`ulimit -n 65536`).
- [`just`](https://github.com/casey/just) for the task runner, and
  `cargo-deny` for the dependency-policy check.

Clone, run the full local ladder (`just check`, described below) and start
a server:

```bash
git clone https://github.com/kevincaicedo/infinitydb
cd infinitydb
just check
cargo run -p infinityd -- --port 6379
```

## Onboarding: prove the toolchain (~30 minutes)

Run these four steps end-to-end from a fresh clone. When they all pass you
have proven the entire toolchain — build, validation ladder, deterministic
simulator, seed replay, and the bench harness — and you know the loop every
change runs in. If anything here fails or a doc step is unclear, that is a
bug in the docs: open an issue (or fix it in your first PR).

1. The validation ladder: fmt, the mechanical gates, clippy, tests.

   ```bash
   just check
   ```

2. Determinism smoke: every simulator scenario, same seed ⇒ byte-identical
   traces.

   ```bash
   just sim-smoke
   ```

3. One seeded DST replay (~1 min): the debugging workflow — a violation the
   simulator reports names its seed; this is how you replay one exactly.

   ```bash
   cargo run --release -p inf-sim --features dst --bin inf-sim -- \
     --scenario m2-durable --seed 0xC0FFEE --verify-determinism
   ```

4. One dev-tier bench row (~3 min): the measurement loop. Dev-tier numbers
   prove your toolchain, never a claim (L10) — only a pinned reference box
   backs published numbers.

   ```bash
   cargo run --release -p infinityd -- --port 7777 &
   cargo run --release -p inf-bench -- load --port 7777 --conns 64 --pipeline 16 --fill 100000 --duration 10
   kill %1
   ```

## The validation ladder

Run these before opening a PR — CI runs the same checks. `just check` is
the required baseline; `cargo deny check` checks dependency licenses and
advisories:

```bash
just check
cargo deny check
```

`just check` runs `cargo fmt --check`; every mechanical gate under
`scripts/` (dependency DAG, cell deny-list, fault points, fsync fail-stop,
panic policy, release-assert inventory, unsafe inventory and crate roots,
shipping features, file length and line width, lint ratchet and scopes,
documentation links); the gates' own self-test, which runs each gate on
planted cases and requires each to exit non-zero (a case written with
`expect_red_because` also requires its cause in the gate's output); clippy
with `-D warnings`;
and the workspace tests. The `check` recipe in the [`justfile`](justfile)
is the exact list.

Layer-specific checks, run them when you touch the relevant area:

- `just loom` — concurrency model-checks of the SPSC ring (touching `inf-fabric`) and of
  checkpoint issuance (touching `inf-foundation`'s `issue` module).
- `just compat` — Redis byte-diff suite against a real `infinityd`.
- `just sim-smoke` — deterministic simulator trace-identity check.
- `just durable-sweep` — durability seeds across crash points.
- `cargo test -p inf-runtime --features uring` — the `io_uring` backend.
- `cargo +nightly miri test -p inf-alloc -p inf-fabric` — the unsafe leaves.

If you touch a decoder, run its fuzz target for a few minutes
(`cargo +nightly fuzz run <target> -- -max_total_time=300` from the owning
crate; `cargo +nightly fuzz list` names the targets). A new decoder lands
with its fuzz target in the same change.

Run heavy checks one at a time; see [`docs/validation.md`](docs/validation.md)
for the full list and its prerequisites.

## Design laws

InfinityDB is built on a small set of non-negotiable laws; a change that
weakens one needs discussion in an issue first, not just a PR.

| Law | Rule |
|---|---|
| L1 | One core, one shard, one owner — no shared mutable data-plane state. No locks or shared atomics between cells; cells talk only over the fabric. |
| L2 | The log is the database — every projection (indexes, tiers, checkpoints) is rebuildable from the log or a checkpoint. |
| L3 | Batch every boundary — syscalls, fabric hops, fsyncs and cache misses are batched, never one-per-item. |
| L4 | Mechanical sympathy is measured — an optimization ships with A/B evidence or behind a flag. |
| L5 | Memory is the product — bytes per key, slack and RSS attribution are gates. |
| L6 | Every command is a resumable state machine — and the local fast path pays nothing for it. |
| L7 | Determinism is a feature — time, randomness and I/O are injected so the whole system runs in the simulator. |
| L8 | Compatibility is staged and honest — a per-command matrix, every deviation written down. |
| L9 | Safety is layered — unsafe code isolated and audited; every decoder fuzzed. |
| L10 | Claims follow evidence — and evidence that cannot go red is not evidence. |
| L11 | Extensibility is a seam, not a fork — engines never bypass the kernel's seams. |
| L12 | Design is proven before code. |
| L13 | One decision, one place — enforced by the strongest mechanism available: type, then lint, then generated table, then self-tested script, then review. |

Mechanical guards you will hit if you cross a line:

- **Dependency-DAG law.** `scripts/check-dep-dag.sh` fails on any internal
  crate edge not listed in `docs/dep-dag.toml`. Adding an edge is a deliberate
  decision, not an accident.
- **Cell deny-list.** `scripts/check-cell-denylist.sh` forbids data-plane
  crates from using `tokio`, `std::sync::Mutex`/`RwLock`, `thread::sleep`,
  blocking filesystem calls, ambient clocks, ambient randomness, etc.
  Clippy's `disallowed-methods` backs the clock ban with type resolution.

Coding style is normative, not advisory:
[`docs/INFINITY_STYLE.md`](docs/INFINITY_STYLE.md) is the document reviews
are run against. The short form: make invalid states unrepresentable with
the type system, check a value that crossed a trust boundary once and turn
it into a type, validate and reserve before you publish, never carry a
lease or borrow across a suspension, give every limit a unit and a crossing
behavior, prefer static dispatch on hot paths, and panic only for violated
internal invariants. `rustfmt` and `clippy -D warnings` are enforced
mechanically; the rest is enforced in review via the
[PR checklist](.github/PULL_REQUEST_TEMPLATE.md).

## Unsafe code

`unsafe` is allowed only in the four audited leaf crates (`inf-simd`,
`inf-alloc`, `inf-fabric`, `inf-runtime`'s `affinity`, `cold`, `driver`,
`executor`, `net`, `kqueue`, `uring` and `signal` modules) and in a few
named, module-scoped regions listed in
[InfinityStyle § Unsafe Rust](docs/INFINITY_STYLE.md#unsafe-rust); every
other library and binary crate root forbids it, and
`scripts/check-unsafe-roots.sh` enforces the list. If you add or change
unsafe code:

- Add a `// SAFETY:` comment on every `unsafe` block explaining the invariant
  (the `undocumented_unsafe_blocks` clippy lint is denied).
- Update the crate's `SAFETY.md` inventory (script-checked).
- Add tests, and where applicable run Miri (`cargo +nightly miri test -p <crate>`)
  and the Loom model.

## Evidence & performance claims

InfinityDB has a strict claim discipline (L10):

- **Correctness changes** (bug fixes, compatibility, determinism) may merge
  with tests; label them as correctness work.
- **Performance changes** are a hypothesis until measured. State the bottleneck
  hypothesis, the target metric, and the workload before the change; after
  it, record before/after numbers, baseline revisions and exact reproduction
  commands. A losing A/B is recorded and the change is **not merged**.
  Dev-laptop numbers are never citation-grade — only a pinned Linux reference
  box can back a published number.
- Never add a performance number to docs or comments without reproducible,
  reference-box-grade evidence behind it.

Commit tests, fixtures, DST seeds and harnesses. Keep generated logs,
profiles, reports and database images local: `.artifacts/` and `artifacts/`
are ignored, with no gate or claim exceptions. The
[validation guide](docs/validation.md) lists runnable checks and the reference
hardware. Record results and reproduction details in the pull request;
do not create an output archive for a bug fix or a release gate.

## Commits & pull requests

- One logical change per commit, so bisect and the simulator's A/B diffs work
  at the granularity of a decision. Keep PRs focused.
- A short, one-line commit message stating the concrete change; the
  reasoning, commands and results go in the PR description.
- Run `just check` locally first.
- Fill in the [PR checklist](.github/PULL_REQUEST_TEMPLATE.md) — the
  reviewer affirms [InfinityStyle](docs/INFINITY_STYLE.md) conformance as
  part of the merge, so unchecked boxes block review, they don't skip it.
- If you add or change a command, regenerate the compatibility matrix
  (`INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`) and
  commit the regenerated `docs/compat-matrix.md`; CI fails when it is stale.
- For changes to a crate's behavior, update that crate's docs in the same
  change.
- The CI must be green before review.

## Reporting bugs

Open a GitHub issue with:

- What you ran (commands, config, client library + version).
- What you expected vs what happened (include exact error text / RESP replies).
- Your environment (OS, kernel version, how you ran InfinityDB — Docker or
  binary).

For a **determinism or simulator** failure, include the scenario and seed —
that is a complete, replayable reproduction
(`cargo run --release -p inf-sim --features dst --bin inf-sim -- --scenario <s> --seed <seed>`).
See [bins/inf-sim/README.md](bins/inf-sim/README.md).
