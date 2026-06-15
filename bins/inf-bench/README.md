# inf-bench

The InfinityDB benchmark and exit-gate harness (milestones M0-E6 / M1-S17).

`inf-bench` spawns the system under test (`infinityd`) and, where relevant,
real `redis-server` as a comparator, drives load with its own RESP client, and
produces per-gate PASS/FAIL reports against the machine-readable gate files in
`docs/milestones/`. It shares **no code** with the system under test — the
client-side RESP lives in `src/resp.rs`, so the measurement tool and the server
can never accidentally agree because they share a bug.

## Subcommands

```
inf-bench env-check [--allow-dirty]
inf-bench load --host H --port P [--threads N] [--pipeline P] [--duration S] ...
inf-bench gate-run m0|m1 [flags]
inf-bench zipfian [flags]
```

### `env-check`

Validates that the box is fit to produce citation-grade numbers (M0-S03):
refuses a dirty git tree, a non-`performance` CPU governor or EPP, and any
thermal throttling. Every other subcommand starts here — see
[Reference-box requirements](#reference-box-requirements).

### `load`

A native pipelined RESP load generator: N blocking connections at a fixed
pipeline depth, a seeded SET/GET mix, merged latency histograms, and a
deterministic `--fill N` mode (partitioned key ranges, each key written exactly
once) used by the RSS gate.

### `gate-run m0` / `gate-run m1`

Runs the milestone's whole exit-gate matrix in one command and writes a report
to `.artifacts/<milestone>/<stamp>-gate-run/report.md`.

- **`gate-run m0`** — pipelined replicates with windowed tripwire scrapes
  (raw `io_uring` counter deltas via `INFO` across all cells), cross-cell A/B
  via `--route-local-only`, an unpipelined 512-connection A/B vs Redis, and the
  fill + RSS + attribution-divergence legs, against `docs/milestones/m0-gates.toml`.
- **`gate-run m1`** — the feature-pressure matrix the M1 exit gates name:
  baseline, TTL-heavy mix, the same-instant **expiry storm** (+ drain time),
  **eviction pressure** against `maxmemory`, **FLUSHALL under load**, pub/sub
  **fan-out** and background rows, the **slow-subscriber kill**, the hardened
  ≤ 1.0× **RSS** leg, and (with `--with-zipfian`) the LFU **hit-rate parity**
  row — against `docs/milestones/m1-gates.toml`.

Common flags:

```
--replicates N      --duration S        --cells N
--reference-box     --unsafe-env        --allow-dirty   --skip-fill
--infinityd-bin P   --redis-bin P       --artifacts-root DIR
# m1 sizing:
--storm-keys N      --flushall-keys N   --fill-keys N
--maxmemory-mb N    --subs N            --sub-channels N
--with-zipfian      --zipfian-keyspace N  --zipfian-ops N  --zipfian-maxmemory-mb N
```

Example (full reference-box M1 campaign):

```bash
inf-bench gate-run m1 --reference-box --with-zipfian \
  --storm-keys 1000000 --flushall-keys 50000000 --fill-keys 10000000 \
  --maxmemory-mb 4096 --subs 100000 --sub-channels 50
```

### `zipfian`

The standalone **LFU hit-rate parity** campaign tool. It replays an *identical*
zipfian (θ≈0.99) cache-aside trace against InfinityDB and Redis 8, both under
`allkeys-lfu` at the same `maxmemory`, and reports each engine's hit rate and
the gap (`pp below Redis`, gate threshold 2.0). Unlike latency/throughput rows,
hit rate is an algorithm property — it does not depend on CPU governor or
thermal state — so a clean run reproduces anywhere, though the binding
milestone verdict still wants `--reference-box`.

```bash
inf-bench zipfian --keyspace 1000000 --ops 5000000 --maxmemory-mb 512 --theta 0.99
```

It writes a `.artifacts/m1/<stamp>-zipfian/report.md` artifact and exits
non-zero if InfinityDB trails Redis by more than the threshold.

## Tier honesty (L10)

Gates marked `tier = "linux-reference-box"` report measured values everywhere
but **bind** the milestone verdict only with `--reference-box`. Any run that
fails `env-check` (which includes every dev laptop) is labeled
`DEV-TIER`/non-binding in the report; `--unsafe-env` lets such a run proceed
but stamps it non-citation-grade. macOS runs validate plumbing only.

## Reference-box requirements

`env-check` (and therefore `gate-run` without `--unsafe-env`) requires:

| Probe | Requirement | How to satisfy |
|---|---|---|
| `git-dirty-tree` | clean working tree | commit/stash; benchmark the committed binary |
| `cpufreq-governor` | every CPU `performance` | `cpupower frequency-set -g performance` |
| `cpufreq-epp` | every CPU `energy_performance_preference=performance` | write `performance` to each `…/cpufreq/energy_performance_preference` |
| `thermal-throttle` | zero `core/package_throttle_count` events during the run | adequate cooling; disable turbo for stability |

A run that passes all four is "reference-box tier" and may bind a milestone
verdict; any other run is `DEV-TIER`/non-binding.

## What this tool deliberately is not

It is not memtier. The reference-box campaign runs memtier / redis-benchmark
*alongside* this loadgen and records both, so the in-house numbers are always
cross-checked against an independent, widely-used generator.
