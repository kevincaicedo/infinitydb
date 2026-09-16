#!/usr/bin/env bash
# Compare the pinned before/after models without changing either checkout.
set -euo pipefail
cd "$(dirname "$0")/.."
baseline="${1:-2ced6586a09c0fbd6b896643969fbd2727d95d56}"
candidate="${2:-cfca32ab1f158a2ac0f4568036f3620d2c7935a9}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/before" "$work/after"
git archive "$baseline" bins/inf-sim/src bins/inf-sim/seeds | tar -x -C "$work/before"
git archive "$candidate" bins/inf-sim/src bins/inf-sim/seeds | tar -x -C "$work/after"
cp scripts/model-equivalence/main.rs "$work/main.rs"
rustc --edition 2024 -O "$work/main.rs" -o "$work/run"
printf 'baseline=%s\ncandidate=%s\n' "$baseline" "$candidate"
"$work/run"
