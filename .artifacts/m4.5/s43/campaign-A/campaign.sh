#!/usr/bin/env bash
# S43 campaign A (2026-08-25): FLUSH-class group hold, base (0) vs arm (250 µs),
# three rounds, order alternated. See README.md for the predeclared rule.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s43/campaign-A
DATA=$HOME/bench-data/s43/data
LOG=$ROOT/campaign.log
BENCH="taskset -c 8,10,12,14 ./target/release/inf-bench"
hdr() { echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) strays=$(pgrep -c infinityd || true) load=$(cut -d' ' -f1-3 /proc/loadavg)"; }
hdr | tee "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class flush --replicates 1 --duration 10 --leg-idle-s 40 --data-root $DATA --model-absent"
for spec in base-0 arm-0 arm-1 base-1 base-2 arm-2; do
  arm=${spec%-*}; if [ "$arm" = base ]; then w=0; else w=250; fi
  echo "=== $(date -Is) s35 $spec (flush-group-window-us $w) ===" | tee -a "$LOG"
  $BENCH gate-run m4.5 --only-s35 $COMMON --flush-group-window-us $w \
    --artifacts-root "$ROOT/s35-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
hdr | tee -a "$LOG"
echo "# done $(date -Is)" | tee -a "$LOG"
