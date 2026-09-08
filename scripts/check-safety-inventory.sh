#!/usr/bin/env bash
# M2.5-S16: SAFETY.md inventory vs code (§17.3, INFINITY_STYLE §Unsafe Rust).
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
# unsafe *usage* tokens, not the word in prose or `forbid(unsafe_code)`.

set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
from pathlib import Path
import re
import sys

unsafe = re.compile(r"\bunsafe\s+(?:\{|fn\b|impl\b|extern\b)")
path_char = r"A-Za-z0-9_./-"
crates = files = unsafe_files = 0
errors = []
for root in [Path("crates"), Path("bins")]:
    if not root.is_dir():
        errors.append(f"SAFETY INVENTORY SCOPE: missing {root}")
        continue
    for crate in sorted(root.iterdir()):
        if not crate.is_dir():
            continue
        source = crate / "src"
        rust = sorted(source.rglob("*.rs"))
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
