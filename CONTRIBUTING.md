# Contributing to InfinityDB

Thanks for your interest in InfinityDB. This is an early-alpha project under
active development; internal interfaces change between milestones. Issues,
questions, and pull requests are welcome.

- [Ground rules](#ground-rules)
- [Development setup](#development-setup)
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

Coding style follows the Rust API guidelines plus: prefer flat control flow
(`?`, `let-else`, early returns), make invalid states unrepresentable with the
type system, keep modules narrow, and prefer static dispatch on hot paths.
`rustfmt` and `clippy -D warnings` are enforced.

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
