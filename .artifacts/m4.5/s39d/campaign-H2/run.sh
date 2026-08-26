#!/usr/bin/env bash
set -euo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT="$HOME/bench-data/s39d/campaign-H2"
DATA="$HOME/bench-data/s39d/data-H2"
LOG="$ROOT/campaign.log"
mkdir -p "$ROOT" "$DATA" "$ROOT/stderr"
cp "$HOME/bench-data/s39b/data/io-properties.toml" "$DATA/"
hdr() {
  date -Is
  ./target/release/infinityd --version
  git rev-parse --short HEAD
  echo "dirty=$(git status --porcelain | wc -l)"
  cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
  echo "infinityd running: $(pgrep -c infinityd || true) redis running: $(pgrep -c redis-server || true)"
  cat /sys/block/nvme0n1/stat
}
hdr >> "$LOG"
export INF_GATERUN_STDERR_DIR="$ROOT/stderr"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 \
  --only-s39d --reference-box --cells 4 --pin-start 0 --barrier-class fua \
  --replicates 3 --leg-idle-s 40 \
  --s39d-warm-records 3000000 --s39d-tail-records 200000 \
  --device-stat nvme0n1 --data-root "$DATA" --artifacts-root "$ROOT/run" \
  >> "$LOG" 2>&1
hdr >> "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
