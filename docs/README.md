# InfinityDB Documentation

Public documentation for InfinityDB. Start with the
[project README](../README.md) for an overview and quickstart.

## Guides

- **[Architecture](architecture.md)** — the thread-per-core, shared-nothing
  design, the life of a request, the fabric, and the storage engine, with
  diagrams.
- **[Deployment](deployment.md)** — running with Docker (and the `io_uring` /
  seccomp requirement), prebuilt binaries, server options, and configuration.
- **[Roadmap](roadmap.md)** — the milestone train and what lands when.
- **[Contributing](../CONTRIBUTING.md)** — development setup, the validation
  ladder, and the design laws contributors must respect.

## Tools

- **[inf-bench](../bins/inf-bench/README.md)** — the benchmark and exit-gate
  harness (`env-check`, `load`, `gate-run`, `zipfian`).
- **[inf-sim](../bins/inf-sim/README.md)** — the deterministic simulator
  (seeded scenarios, invariant oracles, replayable failures).

## Reference

- **[Compatibility matrix](compat-matrix.md)** — every command's Redis
  compatibility status, with documented deviations. *Generated artifact —
  do not edit by hand.*
- **[Interfaces](interfaces-m0.md)** — the frozen internal seams between
  crates (engineering reference).

## Operations artifacts

- **[`../deploy/seccomp/infinitydb-seccomp.json`](../deploy/seccomp/infinitydb-seccomp.json)**
  — the hardened Docker seccomp profile that enables `io_uring`.
