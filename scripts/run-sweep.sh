#!/usr/bin/env bash
# Sharded DST sweep runner for the `just *-sweep` recipes (ADR-0106 D7).
#
# Review 2026-08-30 (batch-9 follow-up): every sweep recipe launched eight
# shards with `&` and then a bare `wait`, whose status is 0 regardless of
# the shards' exit codes — a shard reporting violations (exit 1) or dying
# left the recipe green, and the manifests had to be read by eye. This is
# F-L19-01's class in a `just` recipe. Now each shard is waited on by pid,
# each manifest must exist and carry ` violations=0 `, and the recipe's
# exit status is the verdict. Manifests are still printed: refusals
# (`refused=N`, the ADR-0018 taxonomy) and coverage counters are disclosed,
# not gated, exactly as before.
#
# Usage: run-sweep.sh <scenario> <seeds> <base-seed> [extra inf-sim args…]
# Env:   INF_SIM_BIN — a prebuilt simulator (skips `cargo build`; the
#        self-test uses a stub); INF_SWEEP_SHARDS — shard count (default 8).
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "$SCRIPT_DIR/.."

if [ "$#" -lt 3 ]; then
    echo "usage: $0 <scenario> <seeds> <base-seed> [extra inf-sim args...]" >&2
    exit 2
fi
scenario=$1
seeds=$2
base=$3
shift 3

shards=${INF_SWEEP_SHARDS:-8}
if [ -n "${INF_SIM_BIN:-}" ]; then
    sim=$INF_SIM_BIN
else
    # ADR-0107: a DST binary is built with its `dst` feature (fault points +
    # collision oracle) — never through the workspace graph.
    cargo build --release -p inf-sim --features dst --bin inf-sim
    sim=./target/release/inf-sim
fi
[ -x "$sim" ] || { echo "run-sweep: simulator not executable: $sim" >&2; exit 2; }

out=$(mktemp -d)
pids=()
i=0
while [ "$i" -lt "$shards" ]; do
    "$sim" --scenario "$scenario" --sweep "$seeds" --seed "$base" \
        --shard "$i/$shards" --out "$out" "$@" &
    pids+=("$!")
    i=$((i + 1))
done

fail=0
i=0
for pid in "${pids[@]}"; do
    if ! wait "$pid"; then
        echo "run-sweep: $scenario shard $i/$shards exited non-zero"
        fail=1
    fi
    i=$((i + 1))
done

i=0
while [ "$i" -lt "$shards" ]; do
    manifest="$out/manifest-shard-$i.txt"
    if [ ! -f "$manifest" ]; then
        echo "run-sweep: $scenario shard $i/$shards wrote no manifest ($manifest)"
        fail=1
    elif ! grep -q " violations=0 " "$manifest"; then
        echo "run-sweep: $scenario shard $i/$shards reports violations:"
        grep VIOLATION "$out/results-shard-$i.txt" 2>/dev/null | head -5 || true
        fail=1
    fi
    i=$((i + 1))
done

cat "$out"/manifest-shard-*.txt 2>/dev/null || true
if [ "$fail" -ne 0 ]; then
    echo "run-sweep: $scenario sweep FAILED (results under $out)"
    exit 1
fi
echo "run-sweep: $scenario sweep OK ($shards shards, $seeds seeds from $base; manifests under $out)"
