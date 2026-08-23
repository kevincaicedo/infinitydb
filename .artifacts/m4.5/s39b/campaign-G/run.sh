#!/usr/bin/env bash
set -euo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT="$HOME/bench-data/s39b/campaign-G"
DATA="$HOME/bench-data/s39b/data-G"
LOG="$ROOT/campaign.log"
mkdir -p "$ROOT" "$DATA" "$ROOT/stderr"
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
  --only-s39b --reference-box --cells 4 --pin-start 0 --barrier-class fua \
  --duration 150 --replicates 3 --leg-idle-s 40 --segment-recycle-slots 1 \
  --s39b-baseline wait-off --recycle-wait quarter \
  --device-stat nvme0n1 --data-root "$DATA" --artifacts-root "$ROOT/run" \
  >> "$LOG" 2>&1
hdr >> "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
