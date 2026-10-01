#!/usr/bin/env bash
# ADR-0164: the parent repository's document gates — ADR back-links and
# interface notes (check-adr-links.sh), record shape and the 600-line bound
# (check-drr.sh), and the claim-ledger lint (check-claim-ledger.sh): the
# recipes, jobs, gates, cargo targets, tests, scripts, flags, paths and links a
# claim row cites exist in this checkout, a cited gate is run by a recipe or a
# GitHub-hosted workflow step (a self-hosted job is manual), and an
# Allowed/Narrowed row cites enforcement; what it does not judge is listed in
# its own header. All live beside the governance they read. The
# checkout under test is passed as INF_ENGINE_ROOT, so the ledger-lint judges
# it, not the parent's own infinitydb/. A standalone engine checkout has no
# parent and prints the skip (ADR-0106: a scope is stated, never assumed); a
# parent without its gates is a scope failure.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
parent=$(cd .. && pwd)
if [ ! -f "$parent/docs/infinity-master-plan.md" ]; then
    echo "parent doc gates: SKIPPED — standalone checkout, no parent governance at $parent"
    exit 0
fi
for gate in check-adr-links.sh check-drr.sh check-claim-ledger.sh; do
    if [ ! -x "$parent/scripts/$gate" ]; then
        echo "parent doc gates: SCOPE — $parent/scripts/$gate missing or not executable" >&2
        exit 1
    fi
    INF_ENGINE_ROOT="$PWD" "$parent/scripts/$gate"
done
