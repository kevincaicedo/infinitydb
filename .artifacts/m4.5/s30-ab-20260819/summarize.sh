#!/usr/bin/env bash
# Renders the residency A/B tables from the raw leg artifacts.
# Usage: summarize.sh <residency-dir>
set -euo pipefail
DIR="${1:?usage: summarize.sh <residency-dir>}"

field() { awk -v f="$2" '$1==f{print $2}' "$1"; }

echo "## read-only passes (400k zipfian ops each; cold rate = Δcold_reads_issued / ops)"
for tag in r4-off r4-on r2-off r2-on; do
  echo "### $tag"
  echo "| pass | ops/s | p50 ms | p99 ms | cold rate | promotions Δ | promoted MiB Δ |"
  echo "|---|---|---|---|---|---|---|"
  for p in 1 2 3 4 5; do
    f="$DIR/pass-$tag-$p.txt"
    [ -f "$f" ] || continue
    ops=$(grep -oP 'throughput_ops_s=\K[0-9]+' "$f")
    p50=$(grep -oP 'p50_ms=\K[0-9.]+' "$f")
    p99=$(grep -oP '(^|\s)p99_ms=\K[0-9.]+' "$f" | head -1)
    pre="$DIR/tier-$tag-p$p-pre.txt"; post="$DIR/tier-$tag-p$p-post.txt"
    cold=$(( $(field "$post" cold_reads_issued) - $(field "$pre" cold_reads_issued) ))
    promos=$(( $(field "$post" tiering_promotions) - $(field "$pre" tiering_promotions) ))
    pbytes=$(( $(field "$post" tiering_promoted_bytes) - $(field "$pre" tiering_promoted_bytes) ))
    rate=$(awk -v c="$cold" 'BEGIN{printf "%.1f%%", 100*c/400000}')
    mib=$(awk -v b="$pbytes" 'BEGIN{printf "%.1f", b/1048576}')
    echo "| $p | $ops | $p50 | $p99 | $rate | $promos | $mib |"
  done
done

echo
echo "## read-mix legs (after the 5 passes, same order both arms)"
echo "| leg | read% | ops/s | p50 ms | p99 ms | cold Δ | promos Δ |"
echo "|---|---|---|---|---|---|---|"
for tag in r4-off r4-on r2-off r2-on; do
  for pct in 95 50 0; do
    f="$DIR/mix-$tag-$pct.txt"
    [ -f "$f" ] || continue
    ops=$(grep -oP 'throughput_ops_s=\K[0-9]+' "$f")
    p50=$(grep -oP 'p50_ms=\K[0-9.]+' "$f")
    p99=$(grep -oP '(^|\s)p99_ms=\K[0-9.]+' "$f" | head -1)
    pre="$DIR/tier-$tag-mix$pct-pre.txt"; post="$DIR/tier-$tag-mix$pct-post.txt"
    cold=$(( $(field "$post" cold_reads_issued) - $(field "$pre" cold_reads_issued) ))
    promos=$(( $(field "$post" tiering_promotions) - $(field "$pre" tiering_promotions) ))
    echo "| $tag | $pct | $ops | $p50 | $p99 | $cold | $promos |"
  done
done
