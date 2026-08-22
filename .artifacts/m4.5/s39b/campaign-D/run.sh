#!/usr/bin/env bash
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s39b/campaign-D
DATA=$HOME/bench-data/s39b/data
LOG=$ROOT/campaign.log
hdr() { date -Is; ./target/release/infinityd --version; git rev-parse --short HEAD; echo "dirty=$(git status --porcelain | wc -l)"; cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | paste -sd' '; cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor; echo "infinityd running: $(pgrep -c infinityd)"; cat /sys/block/nvme0n1/stat; }
hdr >> "$LOG"
export INF_GATERUN_STDERR_DIR=$ROOT/stderr; mkdir -p "$INF_GATERUN_STDERR_DIR"
echo "=== $(date -Is) S39b row" >> "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s39b --reference-box \
  --cells 4 --pin-start 0 --barrier-class fua --duration 150 --replicates 3 --leg-idle-s 40 \
  --device-stat nvme0n1 --data-root "$DATA" --artifacts-root "$ROOT/D-s39b" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is)" >> "$LOG"
hdr >> "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
