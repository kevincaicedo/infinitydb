#!/usr/bin/env bash
# M4.5-S30 dev-tier — the parity@256 discriminator control (ADR-0085
# D5). Replicates the S29 gate leg shape (200k × 1 KiB fill, 100% SET
# pipeline-1 closed loop at 256 conns, FSYNC always, MEM-BUDGET 128mb)
# with one lever per arm:
#   demote   — the gate shape as-is (sealing + cold resolves active)
#   nodemote — MUTABLE-FRACTION 999: nothing seals, nothing goes cold,
#              no flush I/O (the whole tiered write-path machinery off
#              at the same budget — bounds residual 2 + flush I/O from
#              above)
# Per-leg INFO snapshots carry cold_reads_issued (the direct
# engagement counter for the write path's cold resolve).
# Usage: parity-control.sh <out-dir>
set -euo pipefail
OUT="${1:?usage: parity-control.sh <out-dir>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ENGINE="$(cd "$HERE/../../.." && pwd)"
BIN="$ENGINE/target/release/infinityd"
BENCH="$ENGINE/target/release/inf-bench"
DATA="$HOME/.cache/inf-tmp/s30-parity"
PORT=7411
CELLS=4
REPS=3
mkdir -p "$OUT"

snapshot() {
  "$HERE/scrape-tiering.sh" $PORT $CELLS > "$1"
}

leg() { # leg <demote|nodemote> <rep>
  local shape=$1 rep=$2
  pkill -f "infinityd.*--port $PORT" 2>/dev/null || true
  sleep 1
  rm -rf "$DATA"; mkdir -p "$DATA"
  taskset -c 4,6,8,10 "$BIN" --cells $CELLS --pin-start 4 --port $PORT \
    --data-dir "$DATA" > "$OUT/srv-$shape-$rep.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 100); do
    redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break
    sleep 0.2
  done
  local extra=()
  if [ "$shape" = nodemote ]; then
    extra=(MUTABLE-FRACTION 999)
  fi
  redis-cli -p $PORT INF.NS CREATE s30ctl MODE durable FSYNC always \
    MEM-BUDGET 128mb DISK-BUDGET 10gb TIER-IO-MODE direct "${extra[@]}" \
    > /dev/null
  for _ in $(seq 1 16); do
    redis-cli -p $PORT INF.NS USE s30ctl > /dev/null
  done
  # Deterministic fill: the gate's shape (200k × 1 KiB, pipeline 4).
  taskset -c 12-23 "$BENCH" load --port $PORT --conns 64 --pipeline 4 \
    --keys 200000 --fill 200000 --value-size 1024 --key-prefix "s30ctl:" \
    --setup "INF.NS USE s30ctl" > "$OUT/fill-$shape-$rep.txt" 2>&1
  sleep 5 # MAINTAIN settles (head advances on the demote shape)
  snapshot "$OUT/tier-$shape-$rep-pre.txt"
  taskset -c 12-23 "$BENCH" load --port $PORT --conns 256 --pipeline 1 \
    --duration 10 --mix 1:0 --keys 200000 --value-size 1024 \
    --key-prefix "s30ctl:" --setup "INF.NS USE s30ctl" \
    > "$OUT/leg-$shape-$rep.txt" 2>&1
  snapshot "$OUT/tier-$shape-$rep-post.txt"
  kill $pid 2>/dev/null || true
  wait $pid 2>/dev/null || true
  rm -rf "$DATA"
  sleep 3
}

for rep in $(seq 1 $REPS); do
  leg demote "$rep"
  leg nodemote "$rep"
done
echo S30-PARITY-CONTROL-DONE
