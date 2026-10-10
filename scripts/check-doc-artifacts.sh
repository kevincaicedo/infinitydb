#!/usr/bin/env bash
# ADR-0106 D15/D17: document identities, current paths and one compat matrix;
# ADR-0166 D5: every relative link in a published Markdown file resolves to a
# published file inside this repository.
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

python3 - <<'PY'
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath
from urllib.parse import unquote

root = Path.cwd()
errors = []

# Check the index: a force-add must not bypass the output policy.
if (root / ".git").exists():
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    output = [p for p in tracked if {".artifacts", "artifacts"}.intersection(Path(p).parts[:-1])]
    if output:
        errors.append(f"DOC OUTPUT: {len(output)} tracked generated-output paths: {output[:8]}")

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
    root / "docs/ARCHITECTURE.md": read_required(root / "docs/ARCHITECTURE.md"),
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

# A workspace document's links are the published-link check's (below); this
# loop resolves links only in governing files outside this repository.
links_checked = adr_paths = 0
for path, body in sources.items():
    governing = not path.resolve().is_relative_to(root.resolve())
    for line, text in enumerate(body.splitlines(), 1):
        location = f"{path}:{line}"
        if re.search(r"(?<![\w-])infinity/|tests/compat-suite", text):
            errors.append(f"DOC PATH: obsolete workspace/harness path at {location}")
        if "`docs/vortex-master-plan.md`" in text:
            errors.append(f"DOC PATH: deleted legacy document cited as current at {location}")
        if not governing:
            continue
        destinations = re.findall(r"\]\(\s*(?:<([^>]+)>|([^\s)]+))", text)
        destinations += re.findall(r"^ {0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|([^\s]+))", text)
        for wrapped, plain in destinations:
            target = wrapped or plain
            if re.match(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*:|//)", target):
                continue
            target = unquote(target.split("#", 1)[0])
            if not target:
                continue
            links_checked += 1
            if not (path.parent / target).resolve().exists():
                errors.append(f"DOC PATH: {location}: missing link {target}")
        # NNNN names are future deliverables; 00xx is the obsolete landed-ADR spelling.
        for target in re.findall(r"docs/adr/(?:[0-9]{4}|00xx)-[\w.-]+\.md", text):
            if "00xx-" in target:
                errors.append(f"DOC PATH: unresolved ADR placeholder {target} at {location}")
            elif governance:
                adr_paths += 1
                if not (root.parent / target).is_file():
                    errors.append(f"DOC PATH: {location}: missing ADR citation {target}")

# Published links: every relative link in a published Markdown file — one
# git tracks, or a new one git does not ignore, so a new document is judged
# before its first commit — resolves inside this repository to a published
# file or a directory holding one. A link resolves from its file's directory,
# a root-absolute one from the repository root. A link that leaves the
# repository, or names no published path (missing, or ignored by git), is
# red. Fenced and inline code hold no links; a target with a scheme
# (https:, mailto:, //) or an anchor alone is not relative. HTML pages are not
# judged here; the OK line says so.
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
CODE_SPAN = re.compile(r"(`+)(?!`).+?(?<!`)\1(?!`)")
LINKS = [
    re.compile(r"\]\(\s*(?:<([^>]+)>|([^\s)]+))"),
    re.compile(r"^ {0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|(\S+))"),
    re.compile(r"""\b(?:href|src)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))"""),
]
SCHEME = re.compile(r"(?:[a-zA-Z][a-zA-Z0-9+.-]*:|//)")


def git_text(*args):
    done = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    return done.stdout.decode(errors="replace") if done.returncode == 0 else None


md_files = md_links = 0
top = git_text("rev-parse", "--show-toplevel")
base = root.resolve()
if top is None or Path(top.strip()).resolve() != base:
    errors.append(
        f"DOC SCOPE: {root} is not the top of its own git work tree — the published "
        "Markdown set is git's"
    )
else:
    listing = git_text("ls-files", "-z", "--cached", "--others", "--exclude-standard") or ""
    published = {p for p in listing.split("\0") if p and (root / p).is_file()}
    published |= {str(d) for p in published for d in PurePosixPath(p).parents}
    for name in sorted(p for p in published if p.lower().endswith(".md")):
        md_files += 1
        fence = None  # the opening run while inside a fenced block
        for number, text in enumerate((root / name).read_text(errors="replace").splitlines(), 1):
            run = FENCE.match(text)
            if fence is not None:
                if run and run[1][0] == fence[0] and len(run[1]) >= len(fence) and not run[2].strip():
                    fence = None
                continue
            if run and not (run[1][0] == "`" and "`" in run[2]):
                fence = run[1]
                continue
            text = CODE_SPAN.sub("", text)
            for pattern in LINKS:
                for groups in pattern.findall(text):
                    target = next((g for g in groups if g), "").strip()
                    if not target or target.startswith("#") or SCHEME.match(target):
                        continue
                    md_links += 1
                    path = unquote(target.split("#", 1)[0].split("?", 1)[0])
                    start = base if path.startswith("/") else (base / name).parent
                    resolved = (start / path.lstrip("/")).resolve()
                    where = f"{name}:{number}"
                    if not resolved.is_relative_to(base):
                        errors.append(f"DOC LINK: {where}: link {target} leaves the repository")
                    elif resolved.relative_to(base).as_posix() not in published:
                        errors.append(
                            f"DOC LINK: {where}: link {target} names no published file "
                            "(missing, or ignored by git)"
                        )
    if not md_files:
        errors.append("DOC SCOPE: no published Markdown file — the link check judged nothing")
    elif not md_links:
        errors.append(
            f"DOC SCOPE: {md_files} published Markdown files hold no relative link — the "
            "link check judged nothing"
        )

scope += (
    f"; {len(sources)} governing/plan files, {links_checked} parent-record links, "
    f"{adr_paths} ADR paths; published links: {md_links} relative links in {md_files} "
    "Markdown files (git-tracked, or new and unignored; fenced and inline code excluded) "
    "judged to resolve inside this repository; HTML pages not judged here"
)
if errors:
    print("\n".join(errors))
    print(f"doc-artifacts FAILED: {scope}")
    sys.exit(1)
print(f"doc-artifacts OK: {scope}; matrix bytes checked by matrix_artifact")
PY
