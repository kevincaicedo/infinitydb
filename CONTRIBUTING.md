# Contributing to InfinityDB

Thanks for your interest in InfinityDB. This is an early-alpha project under
active development; internal interfaces change between milestones. Issues,
questions, and pull requests are welcome.

- [Ground rules](#ground-rules)
- [The reading order](#the-reading-order)
- [Development setup](#development-setup)
- [Onboarding: prove the toolchain (~30 minutes)](#onboarding-prove-the-toolchain-30-minutes)
- [The validation ladder](#the-validation-ladder)
- [Architecture & design laws](#architecture--design-laws)
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

## The reading order

These are governing documents, not background reading — reviews are run
against them. Read in this order before your first substantive PR:

1. [`ARCHITECTURE.md`](ARCHITECTURE.md) — the design-decision overview: the
   problem, the shared-nothing cell model, and why the system is shaped
   this way. Then [`docs/architecture.md`](docs/architecture.md), the
   finer-grained single-node walkthrough.
2. The master plan (design laws L1–L11, gates, and the milestone train —
   planning repository; ask a maintainer if your change is
   milestone-scoped).
3. [`docs/INFINITY_STYLE.md`](docs/INFINITY_STYLE.md) — the **normative**
   engineering style: what a reviewer will hold your PR to. The
   [PR checklist](.github/PULL_REQUEST_TEMPLATE.md) is its operational
   form.
4. For milestone-scoped work: the owning milestone plan and its review
   ledger (planning repository) — story lifecycle, budgets, and the
   definition of done live there; a story is claimed in the ledger
   before code.

## Development setup

Requirements:

- **Rust 1.95+** (the toolchain is pinned in `rust-toolchain.toml`).
- **Linux with `io_uring`** to run the server (kernel 5.15+, 6.1+ recommended).
  macOS builds and tests via `kqueue` for development/correctness, but is not
  a performance target.
- **`redis-server` 8.x** on `PATH` for the compatibility-diff tests.
- [`just`](https://github.com/casey/just) for the task runner (optional but
  convenient), and `cargo-deny` for the dependency-policy check.

```bash
git clone https://github.com/kevincaicedo/infinitydb
cd infinitydb
just check          # the full local ladder (see below)
cargo run -p infinityd -- --port 6379
```

## Onboarding: prove the toolchain (~30 minutes)

Day-one exercise (M2.5-S23): run these four steps end-to-end from a fresh
clone. When they all pass you have proven the entire toolchain — build,
validation ladder, deterministic simulator, seed replay, and the bench
harness — and you know the loop every story runs in. If anything here
fails or a doc step is unclear, that is a bug in the docs: open an issue
(or fix it in your first PR).

```bash
# 1. The validation ladder (~5 min): fmt, dep-DAG law, cell deny-list,
#    fault-point/fsync/panic-policy/safety-inventory greps, clippy, tests.
just check

# 2. Determinism smoke (~1 min): same seed => byte-identical traces.
just sim-smoke

# 3. One seeded DST replay (~1 min): the debugging workflow — every sim
#    failure is a seed; this is how you replay one exactly.
cargo run --release -p inf-sim -- --scenario m2-durable --seed 0xC0FFEE --verify-determinism

# 4. One dev-tier bench row (~3 min): the measurement loop. Dev-tier
#    numbers prove your toolchain, never a claim (L10) — only the pinned
#    reference box backs published numbers.
cargo run --release -p infinityd -- --port 7777 &
cargo run --release -p inf-bench -- load --port 7777 --conns 64 --pipeline 16 --fill 100000 --duration 10
kill %1
```

## The validation ladder

Run this before opening a PR — CI runs the same checks:

```bash
just check          # = fmt + dep-DAG law + cell deny-list + clippy + tests
```

which expands to:

```bash
cargo fmt --all --check
./scripts/check-dep-dag.sh        # internal dependency-edge law
./scripts/check-cell-denylist.sh  # data-plane crates may not use banned APIs
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Layer-specific checks, run them when you touch the relevant area:

```bash
just loom           # SPSC-ring concurrency model-check (touching inf-fabric)
just compat         # Redis byte-diff suite (needs redis-server on PATH)
just sim-smoke      # deterministic simulator trace-identity check
cargo deny check    # dependency licenses/advisories
cargo +nightly miri test -p inf-alloc -p inf-fabric   # unsafe leaves
```

If you touch a protocol decoder, run the fuzz smoke
(`cargo +nightly fuzz run resp_parse -- -max_total_time=60`, likewise
`fabric_codec`, `glob_match`, `scan_cursor`).

## Architecture & design laws

Read [docs/architecture.md](docs/architecture.md) first. InfinityDB is built
on a small set of non-negotiable invariants; a change that weakens one needs
discussion in an issue, not just a PR. The most important for contributors:

- **L1 — shared-nothing data plane.** No locks, shared atomics, or shared
  mutable state between cells on the hot path. Cells talk only over the fabric.
- **L3 — batch every boundary.** Syscalls, fabric hops, and cache misses are
  batched, never one-per-item.
- **L6 — resumable commands.** Command handlers are state machines; the local
  fast path must not pay for suspension capability.
- **L9 — layered safety.** `#![forbid(unsafe_code)]` everywhere except the
  four unsafe leaves.

Mechanical guards you will hit if you cross a line:

- **Dependency-DAG law.** `scripts/check-dep-dag.sh` fails on any internal
  crate edge not listed in `docs/dep-dag.toml`. Adding an edge is a deliberate
  decision, not an accident.
- **Cell deny-list.** `scripts/check-cell-denylist.sh` forbids data-plane
  crates from using `tokio`, `std::sync::Mutex`/`RwLock`, `thread::sleep`,
  blocking filesystem calls, ambient clocks, ambient randomness, etc. Time,
  randomness, and I/O are *injected* (L7) so the whole system runs
  deterministically in the simulator.

Coding style is normative, not advisory:
[`docs/INFINITY_STYLE.md`](docs/INFINITY_STYLE.md) is the document reviews
are run against. The short form: prefer flat control flow (`?`, `let-else`,
early returns), make invalid states unrepresentable with the type system,
keep modules narrow, prefer static dispatch on hot paths, panic only for
violated internal invariants. `rustfmt` and `clippy -D warnings` are
enforced mechanically; the rest is enforced in review via the
[PR checklist](.github/PULL_REQUEST_TEMPLATE.md).

## Unsafe code

`unsafe` is allowed only in the four leaf crates (`inf-simd`, `inf-alloc`,
`inf-fabric`, `inf-runtime`). If you add or change unsafe code:

- Add a `// SAFETY:` comment on every `unsafe` block explaining the invariant
  (the `undocumented_unsafe_blocks` clippy lint is denied).
- Update the crate's `SAFETY.md` inventory.
- Add tests, and where applicable run Miri (`cargo +nightly miri test -p <crate>`)
  and the Loom model.

## Evidence & performance claims

InfinityDB has a strict claim discipline (L10):

- **Correctness changes** (bug fixes, compatibility, determinism) may merge
  with tests; label them as correctness work.
- **Performance changes** are a hypothesis until measured. State the bottleneck
  hypothesis, the target metric, and the workload; after the change, record
  before/after numbers and the artifact. Dev-laptop numbers are never
  citation-grade — only a pinned Linux reference box can back a published
  number.
- Never add a performance number to docs or comments without reproducible,
  reference-box-grade evidence behind it.

## Commits & pull requests

- Keep PRs focused; one logical change per PR.
- Write clear commit messages (imperative mood, explain the *why*).
- Run `just check` locally first.
- Fill in the [PR checklist](.github/PULL_REQUEST_TEMPLATE.md) — the
  reviewer affirms [INFINITY_STYLE](docs/INFINITY_STYLE.md) conformance as
  part of the merge, so unchecked boxes block review, they don't skip it.
- For changes to a crate's behavior, update that crate's docs and the
  compatibility matrix is regenerated automatically — if you add or change a
  command, run `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`
  and commit the regenerated `docs/compat-matrix.md`.
- The CI must be green before review.

## Reporting bugs

Open a GitHub issue with:

- What you ran (commands, config, client library + version).
- What you expected vs what happened (include exact error text / RESP replies).
- Your environment (OS, kernel version, how you ran InfinityDB — Docker or
  binary).

For a **determinism or simulator** failure, include the scenario and seed —
that is a complete, replayable reproduction
(`cargo run -p inf-sim -- --scenario <s> --seed <seed>`). See
[bins/inf-sim/README.md](bins/inf-sim/README.md).
