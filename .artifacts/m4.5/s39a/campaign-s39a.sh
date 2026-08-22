#!/usr/bin/env bash
# M4.5-S39a A/B campaign (ADR-0089 D6) — 2026-08-22. All arms at the S35
# default-flip candidate (--barrier-class fua --frames-in-flight 3
# --staging-mib 2, the pairing with both S35 gates green). fstrim by the
# owner before the session (disclosed, not verified); governor performance.
# (1) S36 row — the padding/throughput row: baseline vs arm A
#     (--fill-window-us 1000 --fill-target-kib 16), interleaved three times.
# (2) S35 row — the scope proof: baseline, arm A, arm B
#     (--fill-window-always), 3 replicates each, 1-cell leg interleaved.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review2/s39a
DATA=$HOME/bench-data/s35-gate/data
mkdir -p "$ROOT" "$DATA"
LOG=$ROOT/campaign-s39a.log
{
  echo "# $(date -Is) host=$(hostname) kernel=$(uname -r)"
  echo "# governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) epp=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null)"
  echo "# binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
  echo "# data=$DATA fstype=$(findmnt -T $DATA -no FSTYPE) device=$(findmnt -T $DATA -no SOURCE)"
  echo "# thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')"
} | tee "$LOG"
COMMON="--reference-box --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 2 --duration 10 --leg-idle-s 40 --data-root $DATA"
ARM_A="--fill-window-us 1000 --fill-target-kib 16"
for pair in 1 2 3; do
  for arm in base fillA; do
    if [ "$arm" = base ]; then extra=""; else extra="$ARM_A"; fi
    echo "=== $(date -Is) s36 $arm-$pair ===" | tee -a "$LOG"
    taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s36 $COMMON $extra \
      --artifacts-root "$ROOT/s36-$arm-$pair" >> "$LOG" 2>&1
    echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
  done
done
for arm in base fillA fillB; do
  case $arm in base) extra="";; fillA) extra="$ARM_A";; fillB) extra="$ARM_A --fill-window-always";; esac
  echo "=== $(date -Is) s35 $arm (3 replicates) ===" | tee -a "$LOG"
  taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 $COMMON $extra --replicates 3 \
    --artifacts-root "$ROOT/s35-$arm" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee -a "$LOG"
