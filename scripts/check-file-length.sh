#!/usr/bin/env bash
# Review 2026-08-30 lane L18 R1 (batch 64, ADR-0125): a source file is at
# most 3000 production lines. Tests do not count — `strip-test-modules.awk`
# blanks every `#[cfg(test)] mod … { }` region and names the test-only
# module files, exactly as the panic-policy and release-assert gates see
# the tree — so a file over the bar splits into a subfolder, never into a
# smaller test module.
#
# Scope: crates/*/src and bins/*/src. A missing directory or an empty file
# set is a failure, not a skip (ADR-0106 D2).

set -euo pipefail
STRIP="${INF_STRIP_AWK:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/strip-test-modules.awk}"
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
LIMIT="${INF_FILE_LINES_MAX:-3000}"

INF_STRIP_AWK="$STRIP" INF_FILE_LINES_MAX="$LIMIT" python3 - <<'PY'
import os
import subprocess
import sys
from pathlib import Path

strip = os.environ["INF_STRIP_AWK"]
limit = int(os.environ["INF_FILE_LINES_MAX"])
roots = [Path("crates"), Path("bins")]
errors = []
sizes = []
files = 0
for root in roots:
    if not root.is_dir():
        errors.append(f"FILE-LENGTH SCOPE: missing {root}")
        continue
    for crate in sorted(root.iterdir()):
        source = crate / "src"
        if not source.is_dir():
            continue
        rust = sorted(source.rglob("*.rs"))
        if not rust:
            errors.append(f"FILE-LENGTH SCOPE: empty {source}")
            continue
        reports = {}
        test_only = set()
        for file in rust:
            out = subprocess.run(["awk", "-v", "mode=report", "-f", strip, str(file)],
                                 capture_output=True, text=True, check=True).stdout
            reports[file] = out
            for line in out.splitlines():
                key, _, value = line.partition(" ")
                if key == "modfile":
                    test_only.add(file.parent / f"{value}.rs")
                    test_only.add(file.parent / value / "mod.rs")
        for file in rust:
            if file in test_only:
                continue
            files += 1
            stripped = 0
            for line in reports[file].splitlines():
                key, _, value = line.partition(" ")
                if key == "unterminated":
                    errors.append(f"FILE-LENGTH SCOPE ERROR: {file} — a test-only module never closed at its own indent")
                if key == "stripped":
                    stripped = int(value)
            total = sum(1 for _ in file.open())
            production = total - stripped
            sizes.append((production, total, file.as_posix()))
            if production > limit:
                errors.append(f"FILE-LENGTH violation: {file} has {production} production lines "
                              f"({total} with tests) — the bar is {limit}; split it into a subfolder")
if files == 0:
    errors.append("FILE-LENGTH SCOPE: no source files")
sizes.sort(reverse=True)
top = ", ".join(f"{p} {f}" for p, _, f in sizes[:3])
scope = f"{files} production files, bar {limit} lines, largest: {top}"
if errors:
    print("\n".join(errors))
    print(f"file-length FAILED: {scope}")
    sys.exit(1)
print(f"file-length OK: {scope}")
PY
