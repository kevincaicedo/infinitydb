#!/usr/bin/env bash
# S39a campaign, second half (2026-08-22): two more S36 base/arm-A pairs —
# the fillA-3 offered-rate leg caught a 413 ms device stall (parked 106),
# the class the closed-loop legs show in every arm; the ADR-0089 D6 (d)
# falsifier reads on the literal leg, so the offered leg needs more
# replicates before it is cited either way.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review2/s39a
DATA=$HOME/bench-data/s35-gate/data
LOG=$ROOT/campaign-s39a-2.log
echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 2 --duration 10 --leg-idle-s 40 --data-root $DATA"
ARM_A="--fill-window-us 1000 --fill-target-kib 16"
for pair in 4 5; do
  for arm in fillA base; do
    if [ "$arm" = base ]; then extra=""; else extra="$ARM_A"; fi
    echo "=== $(date -Is) s36 $arm-$pair ===" | tee -a "$LOG"
    taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s36 $COMMON $extra \
      --artifacts-root "$ROOT/s36-$arm-$pair" >> "$LOG" 2>&1
    echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
  done
done
echo "# done $(date -Is)" | tee -a "$LOG"
