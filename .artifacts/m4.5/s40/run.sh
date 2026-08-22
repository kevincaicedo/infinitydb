#!/usr/bin/env bash
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s40/campaign
LOG=$ROOT/campaign.log
hdr() { date -Is; ./target/release/infinityd --version; redis-server --version; memtier_benchmark --version 2>&1 | head -1; git rev-parse --short HEAD; echo "dirty=$(git status --porcelain | wc -l)"; cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor; echo "infinityd running: $(pgrep -c infinityd) redis running: $(pgrep -c redis-server)"; }
hdr >> "$LOG"
COMMON="--generator memtier --workload set --pipeline 1 --threads 4 --clients 8 --data-size 1024 --keyspace 1000000 --duration 60 --rate 100000 --durability everysec --data-root $HOME/bench-data/s40/data --probe-file $HOME/bench-data/s39b/data/io-properties.toml --device-stat nvme0n1 --pin-start 0 --reference-box --out $ROOT/runs --port-base 7400"
for order in redis,infinitydb infinitydb,redis redis,infinitydb; do
  echo "=== $(date -Is) idle 40 s, then order $order" >> "$LOG"
  sleep 40
  taskset -c 8,10,12,14 ./target/release/inf-compare run --engines $order $COMMON >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is)" >> "$LOG"
done
hdr >> "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
