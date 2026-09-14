#!/bin/bash
A=/home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/review/batch55
cd /home/kcaicedo/Documents/Projects/databases/infinitydb/crates/inf-log || exit 1
for t in frame_decode segment_read; do
  cargo +nightly fuzz run $t -- -max_total_time=300 -rss_limit_mb=4096 > "$A/fuzz-$t.log" 2>&1
  echo "fuzz $t exit=$?" >> "$A/fuzz-$t.log"
done
echo "FUZZ DONE" >> "$A/fuzz-segment_read.log"
