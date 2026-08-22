#!/usr/bin/env bash
cd "$(dirname "$0")"
./campaign-review3-C.sh 1 4
./campaign-review3-C.sh 3 4
echo "# done-C-all $(date -Is)" >> campaign-review3.log
