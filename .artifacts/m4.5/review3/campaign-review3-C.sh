#!/usr/bin/env bash
# Campaign C (combined validation): S36 row 3 ABBA pairs (fill 0 vs 1000) at
# the chosen K/staging, S35 row 3 reps fill 1000 vs fill 0 at the chosen K,
# plus a K=1/fill-0 S36 control pair when K != 1. Usage: C.sh <K> <staging-mib>
set -uo pipefail
K=${1:?K}; S=${2:?staging-mib}
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review3
DATA=$HOME/bench-data/s35-gate/data
LOG=$ROOT/campaign-review3.log
BENCH="taskset -c 8,10,12,14 ./target/release/inf-bench"
hdr() { echo "# $(date -Is) C(K=$K,S=$S) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) strays=$(pgrep -c infinityd || true)"; }
hdr | tee -a "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class fua --duration 10 --data-root $DATA --fill-target-kib 16"
C36="$COMMON --frames-in-flight $K --staging-mib $S --leg-idle-s 60"
for spec in base-1 fillA-1 fillA-2 base-2 base-3 fillA-3; do
  arm=${spec%-*}; if [ "$arm" = base ]; then fill=0; else fill=1000; fi
  echo "=== $(date -Is) C s36 k${K}s${S} $spec (fill-window-us $fill) ===" | tee -a "$LOG"
  $BENCH gate-run m4.5 --only-s36 $C36 --fill-window-us $fill --artifacts-root "$ROOT/C-s36-k${K}s${S}-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
if [ "$K" != 1 ]; then
  for spec in ctl-base-1 ctl-fillA-1; do
    arm=${spec%-*}; arm=${arm#ctl-}; if [ "$arm" = base ]; then fill=0; else fill=1000; fi
    echo "=== $(date -Is) C s36 control k1s4 $spec (fill-window-us $fill) ===" | tee -a "$LOG"
    $BENCH gate-run m4.5 --only-s36 $COMMON --frames-in-flight 1 --staging-mib 4 --leg-idle-s 60 --fill-window-us $fill --artifacts-root "$ROOT/C-s36-k1s4-$spec" >> "$LOG" 2>&1
    echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
  done
fi
C35="$COMMON --frames-in-flight $K --staging-mib $S --replicates 3 --leg-idle-s 40"
for fill in 1000 0; do
  echo "=== $(date -Is) C s35 k${K}s${S} fill $fill (3 reps) ===" | tee -a "$LOG"
  $BENCH gate-run m4.5 --only-s35 $C35 --fill-window-us $fill --artifacts-root "$ROOT/C-s35-k${K}s${S}-fill$fill" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
hdr | tee -a "$LOG"
echo "# done-C $(date -Is)" | tee -a "$LOG"
