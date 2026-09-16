#!/usr/bin/env bash
# The DST smoke lane (F-L19-03, review 2026-08-30): every scenario the
# `inf-sim` binary accepts, once, at a fixed seed, determinism-verified.
# One registry for `just sim-smoke`, the PR CI `sim-smoke` job and
# `bins/inf-sim/tests/lanes.rs` (which fails when a scenario in
# `inf_sim::SCENARIOS` has no row here, or a row names no scenario).
# Rows are `<scenario> [flags…]`; a row's flags replace the default
# `--verify-determinism`. Usage: scripts/sim-smoke.sh [seed]
set -euo pipefail
cd "$(dirname "$0")/.."
seed=${1:-0xC0FFEE}

cargo build --release -p inf-sim --features dst --bin inf-sim
bin=target/release/inf-sim

# F-L11-05: an accept-path error is a counter, never connection teardown.
# Group 0 (review §5.5): adversarial key/value lengths at 4 cells.
# F-L19-05/06: namespace-bound + SELECTed clients, the served surface and
# the stored content under audit at 4 cells.
# F-L12-02: one hot owner, the binary's mesh sizing, deep pipelines.
# F-L15-05 (ADR-0123): maxclients refusal at accept + the idle reaper.
# ADR-0124 (F-L15-08): a graceful stop before the cut keeps every ack.
# F-L17-14: the M4.5 index crash scenarios.
# F-L04-02 (ADR-0119): one EIO under a cold read is one typed reply.
# F-L19-03: the m2 device/mode/window/recycle oracles and the three
# m2 fill/hold/pending scenarios ran in no automated lane.
rows=(
  "m0-smoke"
  "m0-smoke --plant accept-error"
  "m0-adversarial --cells 4 --verify-determinism"
  "m0-surface --cells 4 --verify-determinism"
  "m0-fabric-fairness"
  "m0-admission"
  "m1-cache"
  "m2-durable"
  "m2-clean-stop"
  "m2-device-budget"
  "m2-mode-transition"
  "m2-reorder-window"
  "m2-fill-tick"
  "m2-group-hold"
  "m2-fua-pending"
  "m2-ckpt-refused"
  "m2-recycle"
  "m2-combined"
  "m2-ns-create-window"
  "m2-ns-ddl-race"
  "m3-document"
  "boot-storm"
  "m4-steel"
  "m4-pressure"
  "m4-cold"
  "m4-recovery"
  "m4-diskfull"
  "m4-tiered"
  "m4-tiered --plant tier-read-eio"
  "m45-backfill"
  "m45-sidecar"
)

ran=0
start=$SECONDS
for row in "${rows[@]}"; do
  # shellcheck disable=SC2206
  parts=($row)
  name=${parts[0]}
  flags=("${parts[@]:1}")
  if [ ${#flags[@]} -eq 0 ]; then flags=(--verify-determinism); fi
  echo "== sim-smoke: $name ${flags[*]} (seed $seed)"
  "$bin" --scenario "$name" --seed "$seed" "${flags[@]}"
  ran=$((ran + 1))
done
echo "sim-smoke: $ran rows green in $((SECONDS - start)) s (seed $seed)"
