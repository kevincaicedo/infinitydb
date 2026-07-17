#!/usr/bin/env bash
# M2.5-S21 de-async dispatch — dev-tier ABAB sanity (hypothesis.md method).
# Natural row shape from s09-s21-cycle-split: 4 cells pinned 4,6,8,10,
# loadgen on 12,14,16-23, conns 64 x P=16, 1M keys filled, uniform mix.
set -euo pipefail
BIN=/home/kcaicedo/Documents/Projects/databases/infinitydb/target/release
OUT=$(dirname "$0")
PORT=6401

run_leg() {
  local name=$1; shift
  local server_flags=("$@")
  echo "== leg $name: infinityd ${server_flags[*]} =="
  "$BIN/infinityd" --port $PORT --cells 4 --pin-start 4 "${server_flags[@]}" \
    > "$OUT/$name-server.stdout" 2> "$OUT/$name-server.stderr" &
  local srv=$!
  sleep 1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --fill 1000000 --duration 5 \
    > "$OUT/$name-fill.stdout" 2>&1
  taskset -c 12,14,16,17,18,19,20,21,22,23 "$BIN/inf-bench" load --host 127.0.0.1 --port $PORT \
    --conns 64 --pipeline 16 --duration 20 \
    > "$OUT/$name-load.stdout" 2>&1
  redis-cli -p $PORT info > "$OUT/$name-info.txt" 2>/dev/null || true
  kill $srv 2>/dev/null || true
  wait $srv 2>/dev/null || true
  grep ops_per_sec "$OUT/$name-load.stdout"
}

run_leg A1-off --no-deasync-dispatch
run_leg B1-on  --deasync-dispatch
run_leg A2-off --no-deasync-dispatch
run_leg B2-on  --deasync-dispatch
