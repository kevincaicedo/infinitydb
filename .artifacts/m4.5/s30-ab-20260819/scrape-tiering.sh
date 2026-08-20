#!/usr/bin/env bash
# Sums cell-scope INFO tiering counters across all cells (the INFO
# scope rule: Tiering is a per-cell slice; REUSEPORT spreads fresh
# connections until every cell answered). Usage:
#   scrape-tiering.sh <port> <cells> > snapshot.txt
# Output: "<field> <sum>" lines for the S30 counters of interest.
set -euo pipefail
PORT=$1
CELLS=$2
declare -A CELL_BLOB
for _ in $(seq 1 96); do
  txt=$(redis-cli -p "$PORT" INFO 2>/dev/null || true)
  cell=$(printf '%s\n' "$txt" | tr -d '\r' | awk -F: '/^cell:/{print $2; exit}')
  [ -n "${cell:-}" ] || continue
  CELL_BLOB[$cell]="$txt"
  [ "${#CELL_BLOB[@]}" -ge "$CELLS" ] && break
done
if [ "${#CELL_BLOB[@]}" -lt "$CELLS" ]; then
  echo "SCRAPE-INCOMPLETE cells=${#CELL_BLOB[@]}/$CELLS" >&2
  exit 1
fi
for field in cold_reads_issued cold_reads_enqueued tiering_promotions \
  tiering_promoted_bytes tiering_promote_first_touch \
  tiering_promote_skip_window tiering_promote_skip_pinned \
  tiering_promote_skip_disk tiering_promote_skip_stale \
  tiering_promote_skip_cap tiering_flush_rounds tiering_flush_bytes \
  tiering_compaction_bytes; do
  total=0
  for cell in "${!CELL_BLOB[@]}"; do
    v=$(printf '%s\n' "${CELL_BLOB[$cell]}" | tr -d '\r' | awk -F: -v f="$field" '$1==f{print $2; exit}')
    total=$((total + ${v:-0}))
  done
  echo "$field $total"
done
