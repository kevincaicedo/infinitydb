#!/usr/bin/env bash
set -euo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT="$HOME/bench-data/s40/corrected"
DATA="$HOME/bench-data/s40/data-corrected"
LOG="$ROOT/campaign.log"
PROBE="$PWD/.artifacts/m4.5/s36/io-properties.reference-device.schema2.toml"
mkdir -p "$ROOT/runs" "$ROOT/diagnostic"
hdr() {
  date -Is
  ./target/release/infinityd --version
  redis-server --version
  memtier_benchmark --version 2>&1 | head -1
  git rev-parse --short HEAD
  echo "dirty=$(git status --porcelain | wc -l)"
  cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
  echo "infinityd running: $(pgrep -c infinityd || true) redis running: $(pgrep -c redis-server || true)"
  cat /sys/block/nvme0n1/stat
}
COMMON=(
  --generator memtier --workload set --pipeline 1 --threads 4 --clients 8
  --data-size 1024 --keyspace 1000000 --duration 60 --rate 100000
  --durability everysec --data-root "$DATA" --probe-file "$PROBE"
  --device-stat nvme0n1 --pin-start 0 --reference-box --port-base 7400
)
hdr >> "$LOG"
for order in redis,infinitydb infinitydb,redis redis,infinitydb; do
  echo "=== $(date -Is) idle 40 s, then order $order" >> "$LOG"
  sleep 40
  taskset -c 8,10,12,14 ./target/release/inf-compare run \
    --engines "$order" "${COMMON[@]}" --out "$ROOT/runs" >> "$LOG" 2>&1
done
echo "=== $(date -Is) non-production Redis no-auto-rewrite diagnostic" >> "$LOG"
sleep 40
taskset -c 8,10,12,14 ./target/release/inf-compare run \
  --engines redis "${COMMON[@]}" --redis-no-auto-rewrite \
  --out "$ROOT/diagnostic" >> "$LOG" 2>&1
hdr >> "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
