#!/usr/bin/env bash
# M4.5 E4.7 review follow-up 2 — 2026-08-22. The clean K=3 / 4 MiB rerun
# the review asked for, after the completion-ledger reorder-window fix
# (6fb1f01, clean tree): fstrim by the owner before the session, 40 s idle
# before every durable leg, the 1-cell leg interleaved per replicate (the
# S35 row's new shape), FIVE replicates. Both S35 gates must read green
# before the default flips (ADR-0087 amendment of this date).
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review2
DATA=$HOME/bench-data/s35-gate/data
mkdir -p "$ROOT" "$DATA"
LOG=$ROOT/campaign-k3s4-rerun.log
{
  echo "# $(date -Is) host=$(hostname) kernel=$(uname -r)"
  echo "# governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) epp=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null)"
  echo "# binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
  echo "# data=$DATA fstype=$(findmnt -T $DATA -no FSTYPE) device=$(findmnt -T $DATA -no SOURCE)"
  echo "# thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')"
  echo "# fstrim: run by the owner before this session (disclosed, not verified)"
} | tee "$LOG"
echo "=== $(date -Is) m4.5 --only-s35 arm k3s4 rerun (K=3 staging-mib=4, 5 replicates, interleaved 1c) — 60 s idle first ===" | tee -a "$LOG"
sleep 60
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 --reference-box \
  --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 4 \
  --duration 10 --replicates 5 --leg-idle-s 40 \
  --data-root "$DATA" --artifacts-root "$ROOT/k3s4-rerun" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee -a "$LOG"
