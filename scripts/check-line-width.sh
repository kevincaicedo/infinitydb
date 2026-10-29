#!/usr/bin/env bash
# Review 2026-08-30 lane L18 R2 (batch 64, ADR-0125): 100 columns is a
# hard limit ("nothing hides past a horizontal scrollbar" — INFINITY_STYLE
# §Formatting). `cargo fmt --check` enforces none of it for string
# literals, comments, attributes and macro bodies (rustfmt leaves those
# alone, and the options that would error are nightly-only), so the lane
# found 141 lines over with the formatter green. This gate counts
# characters, not bytes, on every Rust file the workspace builds.
#
# Scope: every *.rs under crates/, bins/ and tests/. A missing directory
# or an empty file set is a failure, not a skip (ADR-0106 D2).

set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"
LIMIT="${INF_LINE_WIDTH_MAX:-100}"

INF_LINE_WIDTH_MAX="$LIMIT" python3 - <<'PY'
import os
import sys
from pathlib import Path

limit = int(os.environ["INF_LINE_WIDTH_MAX"])
roots = [Path("crates"), Path("bins"), Path("tests")]
errors = []
files = lines = 0
for root in roots:
    if not root.is_dir():
        errors.append(f"LINE-WIDTH SCOPE: missing {root}")
        continue
    rust = sorted(p for p in root.rglob("*.rs") if "target" not in p.parts)
    if not rust:
        errors.append(f"LINE-WIDTH SCOPE: no Rust files under {root}")
        continue
    for file in rust:
        files += 1
        for number, line in enumerate(file.read_text(encoding="utf-8").split("\n"), 1):
            lines += 1
            width = len(line.rstrip("\r"))
            if width > limit:
                errors.append(f"LINE-WIDTH violation: {file}:{number} is {width} columns (bar {limit})")
if files == 0:
    errors.append("LINE-WIDTH SCOPE: no source files")
scope = f"{files} files, {lines} lines, bar {limit} columns"
if errors:
    print("\n".join(errors))
    print(f"line-width FAILED: {len(errors)} lines over — {scope}")
    sys.exit(1)
print(f"line-width OK: {scope}")
PY
