#!/usr/bin/env bash
# ADR-0164: the parent repository's document gates — ADR back-links and
# interface notes (check-adr-links.sh), record shape and the 600-line bound
# (check-drr.sh), the claim-ledger lint (check-claim-ledger.sh): the recipes,
# jobs, gates, cargo targets, tests, scripts, flags, paths and links a claim
# row cites exist in this checkout, a cited gate is run by a recipe or a
# GitHub-hosted workflow step (a self-hosted job is manual), and an
# Allowed/Narrowed row cites enforcement; what it does not judge is listed in
# its own header — and the public-docs gate (check-public-doc-links.sh,
# ADR-0166): this checkout's published documents link only to its published
# files and point at nothing it does not publish. All live beside the records
# they read; each runs its own self-test before its verdict. The checkout
# under test is passed as INF_ENGINE_ROOT, so each gate judges it, not the
# parent's own infinitydb/. A standalone engine checkout has no parent and
# prints the skip (ADR-0106: a scope is stated, never assumed); its relative
# links are still judged by check-doc-artifacts.sh. A parent without its
# gates is a scope failure.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
parent=$(cd .. && pwd)
if [ ! -f "$parent/docs/infinity-master-plan.md" ]; then
    echo "parent doc gates: SKIPPED — standalone checkout, no parent governance at $parent"
    exit 0
fi
for gate in check-adr-links.sh check-drr.sh check-claim-ledger.sh check-public-doc-links.sh; do
    if [ ! -x "$parent/scripts/$gate" ]; then
        echo "parent doc gates: SCOPE — $parent/scripts/$gate missing or not executable" >&2
        exit 1
    fi
    INF_ENGINE_ROOT="$PWD" "$parent/scripts/$gate"
done
