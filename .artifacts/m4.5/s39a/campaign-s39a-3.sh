#!/usr/bin/env bash
# S39a campaign, third part (2026-08-22): the rows the dirty-tree refusal
# skipped (base-4, fillA-5, base-5) and the S35 arm-B row (its first
# attempt died at the bench parser — bare flag unregistered).
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review2/s39a
DATA=$HOME/bench-data/s35-gate/data
LOG=$ROOT/campaign-s39a-3.log
echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 2 --duration 10 --leg-idle-s 40 --data-root $DATA"
ARM_A="--fill-window-us 1000 --fill-target-kib 16"
for spec in base-4 fillA-5 base-5; do
  arm=${spec%-*}; if [ "$arm" = base ]; then extra=""; else extra="$ARM_A"; fi
  echo "=== $(date -Is) s36 $spec ===" | tee -a "$LOG"
  taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s36 $COMMON $extra \
    --artifacts-root "$ROOT/s36-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
echo "=== $(date -Is) s35 fillB (3 replicates) ===" | tee -a "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 $COMMON $ARM_A --fill-window-always --replicates 3 \
  --artifacts-root "$ROOT/s35-fillB" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee -a "$LOG"
