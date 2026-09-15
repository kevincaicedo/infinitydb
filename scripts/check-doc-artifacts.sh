#!/usr/bin/env bash
# ADR-0106 D15: one ADR per number and one generated compatibility matrix.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import re
import sys
from pathlib import Path

root = Path.cwd()
errors = []

def read_required(path):
    if not path.is_file():
        errors.append(f"DOC SCOPE: missing file {path}")
        return ""
    text = path.read_text()
    if not text.strip():
        errors.append(f"DOC SCOPE: empty file {path}")
    return text

read_required(root / "Cargo.toml")
matrix = read_required(root / "docs/compat-matrix.md")
if "**GENERATED — do not edit.**" not in matrix:
    errors.append("DOC MATRIX: workspace matrix lacks its generated banner")

docs = root.parent / "docs"
markers = [docs / "infinity-master-plan.md", docs / "adr", docs / "compat-matrix.md"]
adrs = []
if any(path.exists() for path in markers):
    read_required(markers[0])
    pointer = read_required(markers[2])
    expected = (
        "# Compatibility matrix\n\n"
        "The generated [compatibility matrix](../infinitydb/docs/compat-matrix.md) "
        "lives in the Rust workspace.\n"
    )
    if pointer != expected:
        errors.append("DOC MATRIX: outer docs/compat-matrix.md must be the link-only pointer")
    if (docs / "../infinitydb/docs/compat-matrix.md").resolve() != (root / "docs/compat-matrix.md"):
        errors.append("DOC MATRIX: outer pointer does not resolve to this workspace")
    if not markers[1].is_dir():
        errors.append(f"DOC SCOPE: missing ADR directory {markers[1]}")
    else:
        adrs = sorted(markers[1].glob("*.md"))
    numbers = {}
    for path in adrs:
        if path.name == "README.md":
            continue
        match = re.fullmatch(r"([0-9]{4})-.+\.md", path.name)
        if not match:
            errors.append(f"DOC ADR: malformed filename {path.name}")
            continue
        number = match[1]
        if number in numbers:
            errors.append(f"DOC ADR: duplicate {number}: {numbers[number]} and {path.name}")
        numbers[number] = path.name
        body = read_required(path)
        if path.name == "0000-template.md":
            continue
        if not re.match(rf"# ADR-{number}(?:\s|:)", body):
            errors.append(f"DOC ADR: title does not match filename {path.name}")
    decisions = len(numbers.keys() - {"0000"})
    if decisions == 0:
        errors.append("DOC SCOPE: no ADR decisions")
    scope = f"workspace matrix, outer pointer, {decisions} ADR numbers in {markers[1]}"
else:
    scope = "workspace matrix; standalone checkout: parent governance absent, not validated"

if errors:
    print("\n".join(errors))
    print(f"doc-artifacts FAILED: {scope}")
    sys.exit(1)
print(f"doc-artifacts OK: {scope}; matrix bytes checked by matrix_artifact")
PY
