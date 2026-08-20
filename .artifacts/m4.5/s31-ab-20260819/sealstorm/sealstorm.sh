#!/usr/bin/env bash
# M4.5-S31 provoked-sealing tail leg: 8 KiB inserts into a tiny-budget
# tiered namespace => demote rate ~ ack rate => flush round rate is the
# regime. Captures client tail + per-cell loop_iter p99.9 (the direct
# reactor-stall observable) per arm, ABAB order.
set -uo pipefail
CAMP=$HOME/.cache/inf-campaign
DATA=$HOME/.cache/inf-tmp/s31-seal
OUT=${1:?out dir}
mkdir -p "$OUT"
PORT=7411

leg() {
  local arm=$1 rep=$2 bin=$3
  rm -rf "$DATA"; mkdir -p "$DATA"
  "$bin" --cells 4 --pin-start 4 --port $PORT --data-dir "$DATA" \
    > "$OUT/srv-$arm-$rep.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 100); do
    [ "$(redis-cli -p $PORT ping 2>/dev/null)" = "PONG" ] && break
    sleep 0.2
  done
  redis-cli -p $PORT INF.NS CREATE sealstorm MODE durable FSYNC always \
    MEM-BUDGET 32mb DISK-BUDGET 20gb TIER-IO-MODE direct > /dev/null
  # Fan settle: USE until every cell applied the DDL.
  for _ in $(seq 1 50); do
    ok=1
    for _ in $(seq 1 8); do
      [ "$(redis-cli -p $PORT INF.NS USE sealstorm 2>/dev/null)" = "OK" ] || ok=0
    done
    [ $ok = 1 ] && break
    sleep 0.2
  done
  sleep 1
  redis-cli -p $PORT INFO tripwires > "$OUT/pre-trip-$arm-$rep.txt"
  redis-cli -p $PORT INFO tiering > "$OUT/pre-tier-$arm-$rep.txt"
  taskset -c 12-23 "$CAMP/inf-bench-s31" load --port $PORT --conns 128 \
    --pipeline 1 --duration 12 --mix 1:0 --keys 2000000 --value-size 8192 \
    --key-prefix "seal:$rep:" --setup "INF.NS USE sealstorm" \
    > "$OUT/leg-$arm-$rep.txt" 2>&1
  redis-cli -p $PORT INFO tripwires > "$OUT/post-trip-$arm-$rep.txt"
  redis-cli -p $PORT INFO tiering > "$OUT/post-tier-$arm-$rep.txt"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  rm -rf "$DATA"
  sleep 5
}

for rep in 0 1 2; do
  leg base "$rep" "$CAMP/infinityd-s31base"
  leg fix "$rep" "$CAMP/infinityd-s31fix"
done
echo SEALSTORM-DONE
