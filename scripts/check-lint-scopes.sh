#!/usr/bin/env bash
# ADR-0144 (architecture review 2026-09-17, W5): style rules the compiler
# carries need three things clippy cannot check about itself —
#
#   * D1: every crate root (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`
#     under crates/ and bins/) carries the wildcard deny, so a new crate
#     is born under it;
#   * D2: no attribute silences a lint of the ADR except a reasoned
#     `#[allow]` at that lint's scope whose reason is in that lint's
#     classes. The audit is structural (ADR-0125 A3's): attributes are
#     read whole; an inner attribute, an `expect`, a `cfg_attr`, a group
#     (`clippy::pedantic`, `warnings`, …), a missing reason, a reason of
#     another class, or a wrong scope is a violation;
#   * D1's frozen exemptions: the `ADR-0143:` allows are exactly the rows
#     of docs/lint-exemptions.tsv, one allow per row, at most
#     ADR0143_EXEMPTIONS_MAX, and the table only shrinks against its
#     approved copies (HEAD, the base branch tip, its introducing commit).
#
# The lint table below is the one home of "which attribute may silence
# which lint"; later slices add their lints as rows, not as new scans.
#
# INF_LINT_BASE_REF=<ref> names the base branch tip (default origin/main).

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
EXEMPTIONS="${INF_LINT_EXEMPTIONS:-docs/lint-exemptions.tsv}"
BASE_REF="${INF_LINT_BASE_REF:-origin/main}"
# Only a lower number may replace this one (ADR-0144 D1).
ADR0143_EXEMPTIONS_MAX=15

for dir in crates bins; do
    [ -d "$dir" ] || { echo "LINT-SCOPES SCOPE ERROR: $dir/ is missing"; exit 1; }
done

INF_EXEMPTIONS="$EXEMPTIONS" INF_BASE_REF="$BASE_REF" INF_MAX="$ADR0143_EXEMPTIONS_MAX" \
    INF_SELF="scripts/check-lint-scopes.sh" python3 - <<'PY'
import os
import re
import subprocess
import sys
from pathlib import Path

EXEMPTIONS = os.environ["INF_EXEMPTIONS"]
BASE_REF = os.environ["INF_BASE_REF"]
MAX = int(os.environ["INF_MAX"])
SELF = os.environ["INF_SELF"]

# lint -> (scope, reason classes). Scope `fn`: the attribute sits on a
# function.
LINTS = {
    "wildcard_enum_match_arm": ("fn", ("ADR-0143:", "foreign:")),
    "match_wildcard_for_single_variants": ("fn", ("ADR-0143:", "foreign:")),
}
GROUPS = ("clippy::pedantic", "clippy::restriction", "clippy::style", "clippy::all", "warnings")
ROOT_ATTR = re.compile(
    r"#!\[cfg_attr\(\s*not\(test\),\s*deny\(\s*clippy::wildcard_enum_match_arm,\s*"
    r"clippy::match_wildcard_for_single_variants,?\s*\)\s*,?\s*\)\]"
)
FN = re.compile(
    r"^\s*(pub(\([^)]*\))?\s+)?(default\s+)?(const\s+)?(async\s+)?(unsafe\s+)?"
    r'(extern\s+("[^"]*"\s+)?)?fn\s+([A-Za-z_][A-Za-z_0-9]*)'
)
REASON = re.compile(r'reason\s*=\s*"((?:[^"\\]|\\.)*)"')

errors, oks = [], []


def attribute_end(lines, start):
    depth, in_str = 0, False
    for i in range(start, len(lines)):
        text, j = lines[i], 0
        while j < len(text):
            ch = text[j]
            if in_str:
                if ch == "\\":
                    j += 1
                elif ch == '"':
                    in_str = False
            elif ch == '"':
                in_str = True
            elif ch == "[":
                depth += 1
            elif ch == "]":
                depth -= 1
                if depth == 0:
                    return i + 1
            j += 1
    return len(lines)


def next_item(lines, at):
    i = at
    while i < len(lines):
        s = lines[i].strip()
        if not s or s.startswith("//"):
            i += 1
        elif s.startswith("#[") or s.startswith("#!["):
            i = attribute_end(lines, i)
        else:
            return lines[i]
    return ""


def audit(path, exempt_sites):
    lines = path.read_text(encoding="utf-8", errors="replace").split("\n")
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if not (s.startswith("#[") or s.startswith("#![")):
            i += 1
            continue
        end = attribute_end(lines, i)
        text = " ".join(l.strip() for l in lines[i:end])
        site = f"{path}:{i + 1}"
        i = end
        head = REASON.sub("", text)  # lint names; never the reason text
        if not re.search(r"\b(allow|expect)\s*\(", head):
            continue
        named = [l for l in LINTS if re.search(rf"\b{l}\b", head)]
        group = [g for g in GROUPS if re.search(rf"(^|[(,\s]){re.escape(g)}([),\s]|$)", head)]
        if not named and not group:
            continue
        if group and not named:
            # A group that contains a lint of the table hides it.
            errors.append(f"{site}: group suppression `{group[0]}` hides the ADR-0144 lints")
            continue
        if s.startswith("#!["):
            errors.append(f"{site}: inner attribute silences {named[0]} for a whole scope")
        elif "cfg_attr" in head:
            errors.append(f"{site}: cfg_attr suppression of {named[0]} is conditional and hidden")
        elif re.search(r"\bexpect\s*\(", head):
            errors.append(f"{site}: `expect` silences {named[0]}; use allow with a reason")
        else:
            reason = REASON.search(text)
            item = next_item(lines, end)
            fn = FN.match(item)
            classes = LINTS[named[0]][1]
            if not reason:
                errors.append(f"{site}: allow of {named[0]} without a reason")
            elif not reason.group(1).startswith(classes):
                errors.append(
                    f"{site}: reason class of {named[0]} must be one of {', '.join(classes)}"
                )
            elif not fn:
                errors.append(f"{site}: allow of {named[0]} is not on a function")
            else:
                why = reason.group(1)
                oks.append(f"{site} — {why}")
                if why.startswith("ADR-0143:"):
                    exempt_sites.append((str(path), fn.group(9), why.split("column", 1)[-1].strip()))


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True)


def rows_of(text, label):
    rows = []
    for n, line in enumerate(text.split("\n"), 1):
        if not line.strip() or line.startswith("#"):
            continue
        cols = line.split("\t")
        if len(cols) != 3 or not all(cols):
            errors.append(f"{label}:{n}: malformed row (file<TAB>fn<TAB>column)")
            continue
        rows.append(tuple(cols))
    return rows


# ---- D1: every crate root carries the attribute
roots = 0
for top in (Path("crates"), Path("bins")):
    for crate in sorted(top.iterdir()):
        src = crate / "src"
        if not (crate / "Cargo.toml").is_file() or not src.is_dir():
            continue
        crate_roots = [p for p in (src / "lib.rs", src / "main.rs") if p.is_file()]
        crate_roots += sorted((src / "bin").glob("*.rs")) if (src / "bin").is_dir() else []
        for root in crate_roots:
            roots += 1
            if not ROOT_ATTR.search(root.read_text()):
                errors.append(f"{root}: crate root lacks the ADR-0144 D1 wildcard deny")
if roots == 0:
    print("LINT-SCOPES SCOPE ERROR: no crate root found under crates/ and bins/")
    sys.exit(1)

# ---- D2: the suppression audit
files, exempt_sites = 0, []
for top in ("crates", "bins"):
    for dirpath, dirnames, names in os.walk(top):
        dirnames[:] = sorted(d for d in dirnames if d not in ("target", "fuzz"))
        for name in sorted(names):
            if name.endswith(".rs"):
                files += 1
                audit(Path(dirpath) / name, exempt_sites)

# ---- D1: the frozen exemption table
if not Path(EXEMPTIONS).is_file():
    print(f"LINT-SCOPES SCOPE ERROR: {EXEMPTIONS} is missing")
    sys.exit(1)
table = rows_of(Path(EXEMPTIONS).read_text(), EXEMPTIONS)
if len(table) != len(set(table)):
    errors.append(f"{EXEMPTIONS}: a row is listed twice")
if len(table) > MAX:
    errors.append(f"{EXEMPTIONS}: {len(table)} rows, the frozen maximum is {MAX}")
for row in sorted(set(table)):
    count = exempt_sites.count(row)
    if count == 0:
        errors.append(f"{EXEMPTIONS}: row {row[0]} {row[1]} has no `ADR-0143:` allow — delete it")
    elif count > 1:
        errors.append(f"{row[0]}: {count} `ADR-0143:` allows for the one row of fn {row[1]}")
for site in sorted(set(exempt_sites)):
    if site not in table:
        errors.append(f"{site[0]}: `ADR-0143:` allow on fn {site[1]} has no row — new exemptions are closed")

# approved copies: the table only shrinks (a renamed file keeps its row)
notes = []
if git("rev-parse", "--git-dir").returncode != 0:
    print("LINT-SCOPES SCOPE ERROR: not a git repository — the approved copies cannot be read")
    sys.exit(1)
intro = git("log", "--diff-filter=A", "--format=%H", "--", EXEMPTIONS).stdout.split()
copies = [("HEAD", "HEAD"), ("base tip", BASE_REF)] + ([("introducing commit", intro[-1])] if intro else [])
for label, ref in copies:
    if git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode != 0:
        if label == "base tip":
            errors.append(f"SCOPE: base ref `{ref}` does not resolve (fetch it; CI needs fetch-depth: 0)")
        continue
    shown = git("show", f"{ref}:{EXEMPTIONS}")
    if shown.returncode != 0:
        if git("cat-file", "-e", f"{ref}:{SELF}").returncode == 0:
            errors.append(f"{label} ({ref}) has the gate and no {EXEMPTIONS} — a deleted table is not a bootstrap")
        else:
            notes.append(f"{label} predates the gate (bootstrap)")
        continue
    renames = {}
    diff = git("diff", "--name-status", "-M", ref, "--").stdout
    for line in diff.split("\n"):
        cols = line.split("\t")
        if len(cols) == 3 and cols[0].startswith("R"):
            renames[cols[2]] = cols[1]
    approved = set(rows_of(shown.stdout, f"{label}:{EXEMPTIONS}"))
    for row in table:
        was = (renames.get(row[0], row[0]), row[1], row[2])
        if row not in approved and was not in approved:
            errors.append(f"{EXEMPTIONS}: row {row[0]} {row[1]} is not in the {label}'s table — the table only shrinks (a renamed file keeps its row once the rename is staged)")

if errors:
    for e in errors:
        print(f"LINT-SCOPES violation: {e}")
    print(f"lint-scopes FAILED: {len(errors)} violation(s)")
    sys.exit(1)
scope = f"{roots} crate roots under the wildcard deny, {files} files audited, {len(oks)} reasoned allow(s), {len(table)}/{MAX} ADR-0143 exemptions"
if notes:
    scope += "; " + "; ".join(notes)
print(f"lint-scopes OK: {scope}")
for ok in oks:
    print(f"    allow: {ok}")
PY

# ---- the probe: every planted line draws its lint, by name, on its line
# (ADR-0144 D2/D5: a plant that fails to build for another reason is red).
PROBE_SRC="${INF_LINT_PROBE_SRC:-$SCRIPT_DIR/lint-scope-probe}"
if [ -n "${INF_CHECK_ROOT:-}" ] && [ "${INF_LINT_PROBE:-off}" = off ]; then
    exit 0
fi
if [ ! -f "$PROBE_SRC/src/lib.rs" ]; then
    echo "LINT-SCOPES SCOPE ERROR: probe crate missing at $PROBE_SRC"
    exit 1
fi
WS_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
mkdir -p "$WS_ROOT/target"
pwork=$(mktemp -d "$WS_ROOT/target/lint-scope-probe.XXXXXX")
[ -n "$pwork" ] && [ -d "$pwork" ] || { echo "LINT-SCOPES SCOPE ERROR: mktemp failed"; exit 2; }
trap '[ -n "$pwork" ] && [ -d "$pwork" ] && rm -rf "$pwork"' EXIT
cp -R "$PROBE_SRC/." "$pwork/probe"
(cd "$pwork/probe" && env -u CLIPPY_CONF_DIR cargo clippy --quiet --target-dir "$pwork/target" \
    --message-format=json >"$pwork/diag.json" 2>"$pwork/stderr") || true
python3 "$SCRIPT_DIR/lint-scope-probe/judge.py" "$pwork/probe/src/lib.rs" "$pwork/diag.json"
