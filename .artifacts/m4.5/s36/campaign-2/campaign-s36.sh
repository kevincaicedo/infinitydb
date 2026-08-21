#!/usr/bin/env bash
# M4.5-S36 reference-box campaign (ADR-0088 D9) — 2026-08-21.
# Box: ADR-0022 D1 reference box; governor performance; binary from the
# committed tree (printed below). Same binary every arm; every arm runs the
# S35 default-flip candidate shape (--barrier-class fua --frames-in-flight 3
# --staging-mib 2); 40 s idle before every device leg; fstrim NOT run
# (sudo unavailable to the agent — disclosed). Probe file: schema 2,
# written by `inf probe-device` at 15:44 on the campaign root.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s35-gate/artifacts-s36b
DATA=$HOME/bench-data/s35-gate/data
mkdir -p "$ROOT"
LOG=$ROOT/campaign.log
{
  echo "# $(date -Is) host=$(hostname) kernel=$(uname -r)"
  echo "# governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) epp=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null)"
  echo "# binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
  echo "# data=$DATA fstype=$(findmnt -T $DATA -no FSTYPE) device=$(findmnt -T $DATA -no SOURCE)"
  echo "# probe: $(grep -E '^(barrier_class|write_bytes_per_s_256k|write_ops_per_s_4k|write_ops_per_s_4k_qd4|probe_schema)' $DATA/io-properties.toml | tr '\n' ' ')"
  echo "# thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')"
} | tee "$LOG"
run() {
  local tag=$1; shift
  echo "=== $(date -Is) $tag: $* ===" | tee -a "$LOG"
  taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --reference-box \
    --cells 4 --pin-start 0 --barrier-class fua --frames-in-flight 3 --staging-mib 2 \
    --duration 10 --replicates 3 --leg-idle-s 40 --offered-ops 100000 \
    --data-root "$DATA" --artifacts-root "$ROOT/$tag" "$@" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
}
# Campaign 2 (7750bfa: pre-zeroing gated on an always namespace):
# budget arms interleaved twice to separate drive state from the arm,
# then the seal-pace arms (the S36 row and the S35 row's @256 legs).
run s36-budget-on-1  --only-s36
run s36-budget-off-1 --only-s36 --model-absent
run s36-budget-on-2  --only-s36
run s36-budget-off-2 --only-s36 --model-absent
run s36-seal-pace    --only-s36 --seal-pace probe
run s35-seal-pace    --only-s35 --seal-pace probe
echo "# done $(date -Is) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ')" | tee -a "$LOG"
