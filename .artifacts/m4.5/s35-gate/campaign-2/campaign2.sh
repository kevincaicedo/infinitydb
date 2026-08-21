#!/usr/bin/env bash
# M4.5-S35 reference-box campaign (ADR-0087 D8) — 2026-08-21.
# Box: ADR-0022 D1 reference box (i7-13700KF, ADATA LEGEND 700, ext4 on
# nvme0n1p3, kernel 7.0.0-30). Governor: performance (set by the owner).
# Binary: target/release/{infinityd,inf-bench} from the committed tree
# (version printed below). Same binary every arm; arm flags are the only
# difference; every arm runs --barrier-class fua.
#
# Arms: k1 (K=1, 4 MiB — S34's arm), k3s2 (K=3, 2 MiB — L5-neutral),
#       k4 (K=4, 4 MiB — depth trend, +12 MiB/cell attributed).
# Rows: gate-run m4.5 --only-s35 (cells pinned 0,2,4,6; generator
#       taskset 8,10,12,14; 40 s idle before every durable leg — the S34
#       drive-state rule; fstrim NOT run: sudo unavailable to the agent),
#       then gate-run m2 --only-always (no outer taskset — m2 has no pin
#       flag; the 64×16 pipelined 300k-w/s gate row).
# Outputs land outside the tree (env-check demands a clean tree for every
# invocation) and are copied into .artifacts/m4.5/s35-gate/ afterwards.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts2
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
run_arm() {
  local tag=$1 k=$2 mib=$3
  echo "=== $(date -Is) m4.5 --only-s35 arm $tag (K=$k staging-mib=$mib) ===" | tee -a "$LOG"
  taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 --reference-box \
    --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight "$k" --staging-mib "$mib" \
    --duration 10 --replicates 3 --leg-idle-s 40 \
    --data-root "$DATA" --artifacts-root "$ROOT/$tag" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
}
# Campaign 2 (fill-free row, inf-bench 23393f3; infinityd 0f990be): order k3s2 → k4 → k1.
run_arm k3s2 3 2
run_arm k4   4 4
run_arm k1   1 4
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '
' ' ')" | tee -a "$LOG"
