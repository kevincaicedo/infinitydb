#!/usr/bin/env bash
# S42 campaign L (2026-08-25): the stock first boot, three replicates. README.md carries the predeclared rule.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s42/campaign-L
DATA=$HOME/bench-data/s42/data-L
LOG=$ROOT/campaign.log
mkdir -p "$DATA" "$ROOT/stderr"
rm -f "$DATA/io-properties.toml"
export INF_GATERUN_STDERR_DIR=$ROOT/stderr
hdr() { echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) no_turbo=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo) strays=$(pgrep -c -x infinityd || true) load=$(cut -d' ' -f1-3 /proc/loadavg) devstat=$(cat /sys/block/nvme0n1/stat | tr -s ' ')"; }
hdr | tee "$LOG"
taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 \
  --only-s42 --reference-box --cells 4 --pin-start 0 \
  --replicates 3 --duration 10 --leg-idle-s 40 \
  --data-root "$DATA" --artifacts-root "$ROOT/run" >> "$LOG" 2>&1
echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
hdr | tee -a "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
