# inf-bench

The M0 benchmark harness (milestone M0-E6).

## Subcommands

- `env-check [--allow-dirty]` — refuses dirty trees, powersave governors,
  throttled CPUs (M0-S03). Every other subcommand's evidence quality starts
  here.
- `load` — native pipelined RESP load generator: N blocking connections,
  fixed pipeline depth, seeded SET/GET mix, merged latency histograms, and a
  deterministic `--fill N` mode (partitioned key ranges, exactly once) for
  the RSS gate.
- `gate-run m0` — the whole M0 evidence package in one command: env-check
  refusal (`--unsafe-env` records a non-citation-grade override), spawns
  `infinityd` + `redis-server`, pipelined replicates with windowed tripwire
  scrapes (raw counter deltas via INFO across all cells), cross-cell A/B via
  `--route-local-only`, unpipelined 512-conn A/B vs Redis, fill + RSS +
  attribution divergence, and per-gate verdicts against
  `docs/milestones/m0-gates.toml` written to `.artifacts/m0/<stamp>-gate-run/`.

## Tier honesty (L10)

Gates marked `tier = "linux-reference-box"` report measured values
everywhere but bind the milestone verdict only with `--reference-box`.
Dev-box runs (this includes any box failing env-check) are labeled
DEV-TIER/non-binding in the report. macOS runs validate plumbing only.

## What this tool deliberately is not

It shares no code with the system under test (client-side RESP lives in
`src/resp.rs`), and it is not memtier: the reference-box campaign (M0-S21)
runs memtier/redis-benchmark alongside this loadgen and records both.
