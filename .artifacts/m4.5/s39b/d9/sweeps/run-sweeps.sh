#!/usr/bin/env bash
# M4.5-S39b / ADR-0090 D9 correctness rows (A8: the pool wait under every sweep), run sequentially (one sweep at
# a time — the box froze twice under stacked sims), each under a virtual
# memory cap. Manifests land beside this script.
set -uo pipefail
cd "$(dirname "$0")/../../../../.."
ulimit -v 16000000
OUT=.artifacts/m4.5/s39b/d9/sweeps
cargo build --release --bin inf-sim >/dev/null 2>&1 || { echo "build failed"; exit 1; }
echo "engine $(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) $(date -Is)" > "$OUT/header.txt"
run() {
  local name=$1 scenario=$2 seeds=$3 base=$4
  local out; out=$(mktemp -d)
  echo "=== $name $scenario $seeds @ $base $(date -Is)" >> "$OUT/log.txt"
  for i in 0 1 2 3 4 5 6 7; do
    ./target/release/inf-sim --scenario "$scenario" --sweep "$seeds" --seed "$base" --shard "$i/8" --out "$out" >/dev/null 2>>"$OUT/$name.stderr" &
  done
  wait
  cat "$out"/manifest-shard-*.txt > "$OUT/$name.manifest.txt"
  cat "$out"/results-shard-*.txt | grep -v " ok$" > "$OUT/$name.non-ok.txt"
  echo "=== done $name $(date -Is) violations=$(grep -o 'violations=[0-9]*' "$OUT/$name.manifest.txt" | cut -d= -f2 | paste -sd+ | bc)" >> "$OUT/log.txt"
}
run recycle-10k      m2-recycle          10000 0xD5EE0000
run durable-10k      m2-durable          10000 0xD5EE0000
run transition-4k    m2-mode-transition   4000 0x7A4E0000
run reorder-2k       m2-reorder-window    2000 0x2E0D0000
run budget-1k        m2-device-budget     1000 0xB0D6E700
echo "ALL DONE $(date -Is)" >> "$OUT/log.txt"
