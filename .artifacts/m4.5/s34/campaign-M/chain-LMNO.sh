#!/usr/bin/env bash
# 2026-08-25 campaign chain after K: L (S42) → M (S34) → N (S37 discriminator) → O (S40 corrected generator). Sequential, one job on the box.
set -uo pipefail
LOG=$HOME/bench-data/smoke-0825/chain-LMNO.log
for c in "$HOME/bench-data/s42/campaign-L" "$HOME/bench-data/s34/campaign-M" "$HOME/bench-data/s37/campaign-N" "$HOME/bench-data/s40/campaign-O"; do
  echo "=== $(date -Is) START $c ===" >> "$LOG"
  "$c/campaign.sh" >> "$c/nohup.out" 2>&1
  echo "=== $(date -Is) END $c exit=$? ===" >> "$LOG"
  sleep 40
done
echo "CHAIN DONE $(date -Is)" >> "$LOG"
