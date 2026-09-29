#!/usr/bin/env bash
# ADR-0164: the parent repository's document gates — ADR back-links and
# interface notes (check-adr-links.sh), record shape and the 600-line bound
# (check-drr.sh). Both live beside the governance they read. A standalone
# engine checkout has no parent and prints the skip (ADR-0106: a scope is
# stated, never assumed); a parent without its gates is a scope failure.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
parent=$(cd .. && pwd)
if [ ! -f "$parent/docs/infinity-master-plan.md" ]; then
    echo "parent doc gates: SKIPPED — standalone checkout, no parent governance at $parent"
    exit 0
fi
for gate in check-adr-links.sh check-drr.sh; do
    if [ ! -x "$parent/scripts/$gate" ]; then
        echo "parent doc gates: SCOPE — $parent/scripts/$gate missing or not executable" >&2
        exit 1
    fi
    "$parent/scripts/$gate"
done
