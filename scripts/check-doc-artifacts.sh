#!/usr/bin/env bash
# ADR-0106 D15/D17: document identities, current paths and one compat matrix.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import re
import sys
from pathlib import Path
from urllib.parse import unquote

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
sources = {
    root / "ARCHITECTURE.md": read_required(root / "ARCHITECTURE.md"),
    root / "docs/INFINITY_STYLE.md": read_required(root / "docs/INFINITY_STYLE.md"),
}
matrix = read_required(root / "docs/compat-matrix.md")
if "**GENERATED — do not edit.**" not in matrix:
    errors.append("DOC MATRIX: workspace matrix lacks its generated banner")

docs = root.parent / "docs"
markers = [docs / "infinity-master-plan.md", docs / "adr", docs / "compat-matrix.md"]
adrs = []
governance = any(path.exists() for path in [*markers, docs / "milestones"])
if governance:
    sources[markers[0]] = read_required(markers[0])
    plans = sorted((docs / "milestones").glob("*.md"))
    if not plans:
        errors.append("DOC SCOPE: missing or empty parent milestone directory")
    for path in plans:
        sources[path] = read_required(path)
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

links_checked = parent_links = adr_paths = 0
for path, body in sources.items():
    for line, text in enumerate(body.splitlines(), 1):
        location = f"{path}:{line}"
        if re.search(r"(?<![\w-])infinity/|tests/compat-suite", text):
            errors.append(f"DOC PATH: obsolete workspace/harness path at {location}")
        if "`docs/vortex-master-plan.md`" in text:
            errors.append(f"DOC PATH: deleted legacy document cited as current at {location}")
        destinations = re.findall(r"\]\(\s*(?:<([^>]+)>|([^\s)]+))", text)
        destinations += re.findall(r"^ {0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|([^\s]+))", text)
        for wrapped, plain in destinations:
            target = wrapped or plain
            if re.match(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*:|//)", target):
                continue
            target = unquote(target.split("#", 1)[0])
            if not target:
                continue
            resolved = (path.parent / target).resolve()
            if not governance and not resolved.is_relative_to(root):
                parent_links += 1
                continue
            links_checked += 1
            if not resolved.exists():
                errors.append(f"DOC PATH: {location}: missing link {target}")
        # NNNN names are future deliverables; 00xx is the obsolete landed-ADR spelling.
        for target in re.findall(r"docs/adr/(?:[0-9]{4}|00xx)-[\w.-]+\.md", text):
            if "00xx-" in target:
                errors.append(f"DOC PATH: unresolved ADR placeholder {target} at {location}")
            elif governance:
                adr_paths += 1
                if not (root.parent / target).is_file():
                    errors.append(f"DOC PATH: {location}: missing ADR citation {target}")

scope += (
    f"; {len(sources)} governing/plan files, {links_checked} local links, "
    f"{adr_paths} ADR paths, {parent_links} parent links unvalidated"
)
if errors:
    print("\n".join(errors))
    print(f"doc-artifacts FAILED: {scope}")
    sys.exit(1)
print(f"doc-artifacts OK: {scope}; matrix bytes checked by matrix_artifact")
PY
