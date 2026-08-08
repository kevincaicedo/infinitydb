#!/usr/bin/env bash
# M2.5-S09/S21 cycle-accounting campaign: perf stat + perf record on the
# pinned cell cores during steady pipelined load, natural vs all-local.
# Cells pin to cpus 4,6,8,10 (--pin-start 4, 4 cells, even = distinct P-cores).
set -euo pipefail
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release
OUT=$1   # output dir
PORT=6400
CELLS_CPUS=4,6,8,10
mkdir -p "$OUT"

run_leg() {
  local name=$1; shift
  local server_flags=("$@")
  echo "== leg $name: infinityd ${server_flags[*]} =="
  "$BIN/infinityd" --port $PORT --cells 4 --pin-start 4 "${server_flags[@]}" \
    > "$OUT/$name-server.stdout" 2> "$OUT/$name-server.stderr" &
  local srv=$!
  sleep 1
  # 40 s of load; perf windows sit inside steady state.
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --fill 1000000 --duration 5 \
    > "$OUT/$name-fill.stdout" 2>&1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --duration 40 \
    --out "$OUT/$name-load.toml" > "$OUT/$name-load.stdout" 2>&1 &
  local load=$!
  sleep 8
  # INFO snapshot before the stat window (tripwires incl. cmds/iter).
  redis-cli -p $PORT info > "$OUT/$name-info-before.txt" 2>/dev/null || true
  perf stat -C $CELLS_CPUS \
    -e cycles,instructions,branches,branch-misses,cache-references,cache-misses,L1-dcache-loads,L1-dcache-load-misses \
    -- sleep 10 2> "$OUT/$name-perf-stat.txt"
  redis-cli -p $PORT info > "$OUT/$name-info-after.txt" 2>/dev/null || true
  perf record -C $CELLS_CPUS -F 1997 -g --call-graph dwarf,16384 \
    -o "$OUT/$name-perf.data" -- sleep 8 2> "$OUT/$name-perf-record.txt"
  wait $load || true
  kill $srv 2>/dev/null || true
  wait $srv 2>/dev/null || true
  perf report -i "$OUT/$name-perf.data" --stdio --no-children --percent-limit 0.3 \
    > "$OUT/$name-perf-self.txt" 2>/dev/null
  perf report -i "$OUT/$name-perf.data" --stdio --children --percent-limit 1 \
    > "$OUT/$name-perf-children.txt" 2>/dev/null
  sleep 2
}

run_leg natural
run_leg local --route-local-only
echo "campaign complete: $OUT"
