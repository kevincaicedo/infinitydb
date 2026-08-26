#!/usr/bin/env bash
# S34 campaign M1b (2026-08-26): the S35 read leg on a filled, quiesced namespace, flush vs fua interleaved. README.md carries the rule.
set -uo pipefail
cd /home/kcaicedo/Documents/Projects/databases/infinitydb
ROOT=$HOME/bench-data/s34/campaign-M1b
DATA=$HOME/bench-data/s34/data-M
LOG=$ROOT/campaign.log
mkdir -p "$DATA" "$ROOT/stderr"
cp "$HOME/bench-data/s39b/data/io-properties.toml" "$DATA/"
export INF_GATERUN_STDERR_DIR=$ROOT/stderr
hdr() { echo "# $(date -Is) binary=$(./target/release/infinityd --version) git=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) thermal: $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | tr '\n' ' ') gov=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) no_turbo=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo) strays=$(pgrep -c -x infinityd || true) load=$(cut -d' ' -f1-3 /proc/loadavg) devstat=$(cat /sys/block/nvme0n1/stat | tr -s ' ')"; }
armflags() { if [ "$1" = flush ]; then echo "--barrier-class flush --model-absent"; else echo "--barrier-class fua"; fi; }
hdr | tee "$LOG"
for spec in flush-0 fua-0 fua-1 flush-1 flush-2 fua-2; do
  arm=${spec%-*}
  echo "=== $(date -Is) M1b s35 $spec ===" | tee -a "$LOG"
  taskset -c 8,10,12,14 ./target/release/inf-bench gate-run m4.5 --only-s35 --reference-box --cells 4 --pin-start 0 \
    --replicates 1 --duration 10 --leg-idle-s 40 --read-leg-fill --data-root "$DATA" $(armflags $arm) \
    --artifacts-root "$ROOT/s35-$spec" >> "$LOG" 2>&1
  echo "=== exit $? $(date -Is) ===" | tee -a "$LOG"
done
hdr | tee -a "$LOG"
echo "CAMPAIGN DONE" >> "$LOG"
