# InfinityDB Documentation

Public documentation for InfinityDB. Start with the
[project README](../README.md) for an overview and quickstart.

## Guides

- **[Architecture](ARCHITECTURE.md)** — how InfinityDB works: the
  thread-per-core, shared-nothing design, the life of a request, the fabric,
  the log and the storage engine.
- **[Deployment](deployment.md)** — running with Docker (and the `io_uring` /
  seccomp requirement), server options, and configuration. Prebuilt binaries
  come with the first release; until then, build from source.
- **[Roadmap](roadmap.md)** — the milestones and their status.
- **[Contributing](../CONTRIBUTING.md)** — development setup, the validation
  ladder, and the design laws contributors must respect.

## Tools

- **[Validation and reference box](validation.md)** — runnable tests, DST,
  performance harnesses, baseline requirements and local output policy.
- **[inf-bench](../bins/inf-bench/README.md)** — the benchmark and exit-gate
  harness (`env-check`, `load`, `gate-run`, `zipfian`).
- **[inf-sim](../bins/inf-sim/README.md)** — the deterministic simulator
  (seeded scenarios, invariant oracles, replayable failures).

## Reference

- **[Compatibility matrix](compat-matrix.md)** — every command's declared
  Redis compatibility status and the deviations recorded for it. *Generated
  artifact — do not edit by hand.*
- **[`JSON.*` reply shapes](json-reply-shapes.md)** — the reply shape of every
  document command. *Generated artifact.*
- **[JSONPath subset](jsonpath-subset.md)** and
  **[PartiQL subset](partiql-subset.md)** — the accepted grammars and their
  semantics.
- **[InfinityStyle](INFINITY_STYLE.md)** — the engineering style guide; normative
  for every design and every change.
- **[Interfaces (M0)](interfaces-m0.md)** — the frozen internal seams between
  crates (engineering reference).
- **[Interfaces (M2)](interfaces-m2.md)** — the log-spine formats and seams
  (record/frame/LSN/segment lifecycle), frozen when M2 finished; a later
  change is a recorded design decision.
- **[Interfaces (indexes and query)](interfaces-m4.5.md)** — the ordered-index,
  path-program and query seams (draft).

## Operations artifacts

- **[`../deploy/seccomp/infinitydb-seccomp.json`](../deploy/seccomp/infinitydb-seccomp.json)**
  — the hardened Docker seccomp profile that enables `io_uring`.
