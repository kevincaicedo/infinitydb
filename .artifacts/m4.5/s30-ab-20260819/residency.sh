#!/usr/bin/env bash
# M4.5-S30 dev-tier A/B — read-driven promotion (ADR-0085).
# Family A: the finding's repro (tri-bench, exploratory tier), two arms
# (tiered-promote-on-read yes/no via CONFIG — same binary), two
# dataset:budget ratios, 5 identical zipfian read-only passes each,
# then 95/50/0 read-mix legs. Per-pass INFO tiering snapshots (cell-sum
# via scrape-tiering.sh) carry the cold-read/promotion counters.
# Usage: residency.sh <out-dir>
set -euo pipefail
OUT="${1:?usage: residency.sh <out-dir>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ENGINE="$(cd "$HERE/../../.." && pwd)"
BIN="$ENGINE/target/release/infinityd"
TRI="$HOME/bench-harness/target/release/tri-bench"
DATA="$HOME/bench-data/s30-ab/data"
PORT=7385
CELLS=4
RECORDS=4000000
OPS=400000
mkdir -p "$OUT"

snapshot() { # snapshot <file>
  "$HERE/scrape-tiering.sh" $PORT $CELLS > "$1"
}

leg() { # leg <on|off> <budget> <tag>
  local arm=$1 budget=$2 tag=$3
  pkill -f "infinityd.*--port $PORT" 2>/dev/null || true
  sleep 1
  rm -rf "$DATA"; mkdir -p "$DATA"
  taskset -c 0,2,4,6 "$BIN" --cells $CELLS --pin-start 0 --port $PORT \
    --data-dir "$DATA" > "$OUT/srv-$tag.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 100); do
    redis-cli -p $PORT ping 2>/dev/null | grep -q PONG && break
    sleep 0.2
  done
  if [ "$arm" = off ]; then
    redis-cli -p $PORT CONFIG SET tiered-promote-on-read no > /dev/null
  fi
  redis-cli -p $PORT INF.NS CREATE readonly MODE durable FSYNC everysec \
    MEM-BUDGET "$budget" DISK-BUDGET 20gb TIER-IO-MODE direct > /dev/null
  # DDL fan settle: USE must succeed on fresh connections repeatedly.
  for _ in $(seq 1 16); do
    redis-cli -p $PORT INF.NS USE readonly > /dev/null
  done
  taskset -c 8,10,12,14 "$TRI" --engine infinity --phase load \
    --records $RECORDS --conns 16 --port $PORT --ns readonly --shape kv \
    > "$OUT/load-$tag.txt" 2>&1
  sleep 8 # let MAINTAIN flush/release the tail (drive the head up)
  for pass in 1 2 3 4 5; do
    snapshot "$OUT/tier-$tag-p$pass-pre.txt"
    taskset -c 8,10,12,14 "$TRI" --engine infinity --phase run \
      --records $RECORDS --ops $OPS --conns 32 --read-pct 100 \
      --dist zipfian --port $PORT --ns readonly --shape kv --warmup 3 \
      > "$OUT/pass-$tag-$pass.txt" 2>&1
    snapshot "$OUT/tier-$tag-p$pass-post.txt"
  done
  for pct in 95 50 0; do
    snapshot "$OUT/tier-$tag-mix$pct-pre.txt"
    taskset -c 8,10,12,14 "$TRI" --engine infinity --phase run \
      --records $RECORDS --ops $OPS --conns 32 --read-pct $pct \
      --dist zipfian --port $PORT --ns readonly --shape kv --warmup 3 \
      > "$OUT/mix-$tag-$pct.txt" 2>&1
    snapshot "$OUT/tier-$tag-mix$pct-post.txt"
  done
  kill $pid 2>/dev/null || true
  wait $pid 2>/dev/null || true
  rm -rf "$DATA"
  sleep 3
}

# Interleaved arms per ratio (drive-state fairness). Ratio names:
# r4 = 4:1 dataset:budget (the finding's shape), r2 = 2:1.
leg off 256mb r4-off
leg on  256mb r4-on
leg off 512mb r2-off
leg on  512mb r2-on
echo S30-RESIDENCY-DONE
