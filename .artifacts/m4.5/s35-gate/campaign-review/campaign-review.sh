#!/usr/bin/env bash
# M4.5 E4.7 review follow-up campaign — 2026-08-21 evening.
# Binary infinityd/inf-bench 2cb6074 (the review-fix tree, clean).
# (1) gate-run m2 --only-always, K=3 / 2 MiB, THREE replicates, 40 s idle
#     before each — the claim-ledger replicates the S35 entry owes.
# (2) gate-run m4.5 --only-s35, the K=3 / 4 MiB arm (unmeasured until now:
#     keeps the durable record bound at 4 MiB − 56 B, +8 MiB/cell) —
#     the arm the default-K decision lacks.
# Governor performance; fstrim: run by the owner before this campaign
# (manual runs do not appear in the journal — disclosed, not verified).
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-review
DATA=$HOME/bench-data/s35-gate/data
mkdir -p "$ROOT" "$DATA"
LOG=$ROOT/campaign.log
{
  echo "# $(date -Is) host=$(hostname) kernel=$(uname -r)"
  echo "# governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) epp=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null)"
  echo "# binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
  echo "# data=$DATA fstype=$(findmnt -T $DATA -no FSTYPE) device=$(findmnt -T $DATA -no SOURCE)"
  echo "# thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')"
} | tee "$LOG"
for rep in 1 2 3; do
  echo "=== $(date -Is) m2 --only-always K=3 staging-mib=2 replicate $rep, 40 s idle first ===" | tee -a "$LOG"
  sleep 40
  ./target/release/inf-bench gate-run m2 --only-always --reference-box \
    --barrier-class fua --frames-in-flight 3 --staging-mib 2 \
    --data-root "$DATA" --artifacts-root "$ROOT/k3s2-m2always-r$rep" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
echo "=== $(date -Is) m4.5 --only-s35 arm k3s4 (K=3 staging-mib=4) ===" | tee -a "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 --reference-box \
  --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 4 \
  --duration 10 --replicates 3 --leg-idle-s 40 \
  --data-root "$DATA" --artifacts-root "$ROOT/k3s4" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee -a "$LOG"
