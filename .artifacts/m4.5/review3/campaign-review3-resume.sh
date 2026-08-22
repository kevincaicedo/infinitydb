#!/usr/bin/env bash
# Resume after the dirty-tree refusal (a docs-only commit 4d7678f restored the
# clean tree; engine byte-identical to 5e162b7). Legs: A base-3..fillA-5, all of B.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review3
DATA=$HOME/bench-data/s35-gate/data
LOG=$ROOT/campaign-review3.log
BENCH="taskset -c 8,10,12,14 ./target/release/inf-bench"
hdr() { echo "# $(date -Is) RESUME binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) strays=$(pgrep -c infinityd || true)"; }
hdr | tee -a "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class fua --duration 10 --data-root $DATA"
A_COMMON="$COMMON --frames-in-flight 1 --staging-mib 4 --leg-idle-s 60 --fill-target-kib 16"
for spec in base-3 fillA-3 fillA-4 base-4 base-5 fillA-5; do
  arm=${spec%-*}; if [ "$arm" = base ]; then fill=0; else fill=1000; fi
  echo "=== $(date -Is) A s36 $spec (fill-window-us $fill) ===" | tee -a "$LOG"
  $BENCH gate-run m4.5 --only-s36 $A_COMMON --fill-window-us $fill \
    --artifacts-root "$ROOT/A-s36-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
B_COMMON="$COMMON --replicates 1 --leg-idle-s 40 --fill-window-us 0"
PAIRINGS=("k1s4:--frames-in-flight 1 --staging-mib 4" "k3s2:--frames-in-flight 3 --staging-mib 2" "k3s4:--frames-in-flight 3 --staging-mib 4")
for round in 0 1 2 3 4; do
  for i in 0 1 2; do
    idx=$(( (round + i) % 3 ))
    entry=${PAIRINGS[$idx]}; name=${entry%%:*}; arms=${entry#*:}
    echo "=== $(date -Is) B s35 round$round $name ($arms) ===" | tee -a "$LOG"
    $BENCH gate-run m4.5 --only-s35 $B_COMMON $arms \
      --artifacts-root "$ROOT/B-s35-$name-r$round" >> "$LOG" 2>&1
    echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
  done
done
hdr | tee -a "$LOG"
echo "# done-resume $(date -Is)" | tee -a "$LOG"
