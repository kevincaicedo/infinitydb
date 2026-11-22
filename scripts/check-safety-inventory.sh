#!/usr/bin/env bash
# SAFETY.md inventory vs code (INFINITY_STYLE §Unsafe Rust).
#
# One direction is load-bearing and enforced hard: every file under a
# crate's src/ that USES unsafe (blocks, fns, impls, extern) must be named
# in that crate's SAFETY.md — new unsafe in an unnamed module fails the
# build until the inventory covers it. The complementary direction (stale
# inventory entries) is a docs nit, not a soundness hole, and stays a
# review concern.
#
# Scope: shipped surface only (crates/*/src, bins/*/src). Test/bench
# unsafe lives outside src/ and is covered by clippy's
# undocumented_unsafe_blocks = deny (workspace-wide). The grep matches
# unsafe *usage* tokens, not the word in prose or `forbid(unsafe_code)`. A
# directory the walk cannot read (scripts/source_files.py) is a scope error.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"

INF_SCRIPT_DIR="$SCRIPT_DIR" python3 - <<'PY'
import os
from pathlib import Path
import re
import sys

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
from source_files import WalkError, subdirectories, tree_listing


def walked(listed, *args):
    """A walk's listing, or the scope error: a directory the walk cannot read fails the gate."""
    try:
        return listed(*args)
    except WalkError as err:
        print(f"SAFETY INVENTORY SCOPE ERROR: {err}")
        sys.exit(2)


unsafe = re.compile(r"\bunsafe\s+(?:\{|fn\b|impl\b|extern\b)")
path_char = r"A-Za-z0-9_./-"
crates = files = unsafe_files = 0
errors = []
for root in [Path("crates"), Path("bins")]:
    if not root.is_dir():
        errors.append(f"SAFETY INVENTORY SCOPE: missing {root}")
        continue
    for crate in walked(subdirectories, root):
        source = crate / "src"
        rust = walked(tree_listing, source, "*.rs") if source.is_dir() else []
        if not source.is_dir() or not rust:
            errors.append(f"SAFETY INVENTORY SCOPE: missing or empty {source}")
            continue
        crates += 1
        files += len(rust)
        inventory = crate / "SAFETY.md"
        text = inventory.read_text() if inventory.is_file() else ""
        for file in rust:
            if not unsafe.search(file.read_text()):
                continue
            unsafe_files += 1
            relative = file.relative_to(source).as_posix()
            names = [relative, f"src/{relative}", file.as_posix()]
            literal = "|".join(re.escape(name) for name in names)
            pattern = rf"(?<![{path_char}])(?:{literal})(?![{path_char}])"
            if not re.search(pattern, text):
                errors.append(f"SAFETY INVENTORY GAP: {file} uses unsafe but {inventory} never names {relative}")
if crates == 0:
    errors.append("SAFETY INVENTORY SCOPE: no source crates")
scope = f"{crates} crates, {files} Rust files, {unsafe_files} unsafe-bearing files"
if errors:
    print("\n".join(errors))
    print(f"safety-inventory FAILED: {scope}")
    sys.exit(1)
print(f"safety-inventory OK: {scope}; every unsafe file has an exact inventory path")
PY
