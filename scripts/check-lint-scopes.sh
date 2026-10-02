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
#     approved copies (HEAD, the base branch tip, its introducing commit);
#   * ADR-0163 D2's container ban: a cell crate names a std `HashMap`,
#     `HashSet` or `VecDeque` only under a `container: <record>` allow whose
#     row of docs/container-exemptions.tsv counts what it covers — the
#     distinct primary spans of the resolved diagnostics inside the allowed
#     item. The table is generated from the tree (the census below), its
#     counts per file and record only shrink against the approved copies,
#     and `container: capped-backing` is allowed exactly
#     CAPPED_BACKING_SITES times, with no row. clippy.toml owns the banned
#     paths and the ban's sentence; this gate reads both from it. A banned
#     name in cell production code outside every `container:` allow is red
#     from the text too, so code this host's builds do not compile (another
#     `target_os`, a feature neither build enables) is held the same.
#
# The lint table below is the one home of "which attribute may silence
# which lint"; later slices add their lints as rows, not as new scans.
#
# INF_LINT_BASE_REF=<ref> names the base branch tip (default origin/main).
# INF_CONTAINER_CENSUS=1 prints the container census (file, item, count,
# record — the table's generated columns) from the ratchet's two clippy
# passes and judges nothing else.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
EXEMPTIONS="${INF_LINT_EXEMPTIONS:-docs/lint-exemptions.tsv}"
CONTAINERS="${INF_CONTAINER_EXEMPTIONS:-docs/container-exemptions.tsv}"
BASE_REF="${INF_LINT_BASE_REF:-origin/main}"
# Only a lower number may replace this one (ADR-0144 D1).
ADR0143_EXEMPTIONS_MAX=15
# The sum of the container table's counts: only a lower number may replace
# it. The capped-backing allows are exact: 1 once `struct CappedDeque`
# lands (ADR-0151 D6).
CONTAINER_EXEMPTIONS_MAX=104
CAPPED_BACKING_SITES=0
if [ "${INF_CONTAINER_CENSUS:-0}" = 1 ] && [ -z "${INF_LINT_API_DIAGNOSTICS:-}" ]; then
    # The census reads the ratchet's two passes; the ratchet calls back here.
    exec "$SCRIPT_DIR/check-lint-ratchet.sh"
fi
# Cell membership has one owner, shared with the runtime safety gates.
. "$SCRIPT_DIR/cell-crates.sh"
CELL_DIRS=$(cell_crate_dirs)

for dir in crates bins; do
    [ -d "$dir" ] || { echo "LINT-SCOPES SCOPE ERROR: $dir/ is missing"; exit 1; }
done

INF_CELL_DIRS="$CELL_DIRS" INF_SCOPES="${INF_LINT_SCOPES:-docs/lint-scopes.tsv}" INF_EXEMPTIONS="$EXEMPTIONS" INF_BASE_REF="$BASE_REF" INF_MAX="$ADR0143_EXEMPTIONS_MAX" \
    INF_CONTAINERS="$CONTAINERS" INF_CONTAINER_MAX="$CONTAINER_EXEMPTIONS_MAX" INF_BACKING_SITES="$CAPPED_BACKING_SITES" \
    INF_SELF="scripts/check-lint-scopes.sh" INF_SCRIPT_DIR="$SCRIPT_DIR" python3 -B - <<'PY'
import bisect
import datetime
import os
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
import lint_scope_table as scope_table
from lint_scope_table import FN, attribute_end

EXEMPTIONS = os.environ["INF_EXEMPTIONS"]
BASE_REF = os.environ["INF_BASE_REF"]
MAX = int(os.environ["INF_MAX"])
SELF = os.environ["INF_SELF"]
CONTAINERS = os.environ["INF_CONTAINERS"]
CONTAINER_MAX = int(os.environ["INF_CONTAINER_MAX"])
BACKING_SITES = int(os.environ["INF_BACKING_SITES"])
CENSUS = os.environ.get("INF_CONTAINER_CENSUS") == "1"
# ADR-0163 D2: the site records that own an exempt row (ADR-0163 D3: T E M C
# A D R F). `capped-backing` is the one sanctioned holder's allow (ADR-0151
# D6) and has no row.
RECORDS = ("T", "E", "M", "C", "A", "D", "R", "F")
BACKING, BACKING_ITEM, BACKING_HOME = "capped-backing", "struct CappedDeque", "crates/inf-foundation/src/bounded"
HOST_OS = {"linux": "linux", "darwin": "macos"}.get(sys.platform, sys.platform)

# lint -> (scope, reason classes). Scope `fn`: the attribute sits on a
# function.
LINTS = {
    "wildcard_enum_match_arm": ("fn", ("ADR-0143:", "foreign:")),
    "match_wildcard_for_single_variants": ("fn", ("ADR-0143:", "foreign:")),
    "cast_possible_truncation": ("fn", ("bound:",)),
    "cast_sign_loss": ("fn", ("bound:",)),
    "cast_possible_wrap": ("fn", ("bound:",)),
    "arithmetic_side_effects": ("fn", ("bound:",)),
    # ADR-0125 D2's opt-out, audited here since the ratchet absorbed it.
    "too_many_lines": ("fn", ("shape:",)),
    "disallowed_methods": ("statement", ("clock:", "fs-seam:", "boot:", "control-thread:")),
    "disallowed_types": ("item", ("fs-seam:", "boot:", "control-thread:", "container:")),
}
if os.environ.get("INF_LINT_RULES") == "1":
    for lint, (scope, classes) in LINTS.items():
        example = next(cls for cls in classes if cls != "ADR-0143:")
        print(f"{lint}\t{scope}\t{example}")
    sys.exit(0)
# The banned container paths and their sentence: clippy.toml's
# `disallowed-types` under `std::collections::` (the probe's judge pins their
# number, so a lost or added path is red there too).
if not Path("clippy.toml").is_file():
    print("LINT-SCOPES SCOPE ERROR: clippy.toml is missing — the banned container paths cannot be read")
    sys.exit(1)
CONTAINER_REASON = {row["path"]: row.get("reason", "")
                    for row in tomllib.loads(Path("clippy.toml").read_text()).get("disallowed-types", [])
                    if row.get("path", "").startswith("std::collections::")}
if not CONTAINER_REASON:
    print("LINT-SCOPES SCOPE ERROR: clippy.toml bans no std::collections path — the container class has no scope")
    sys.exit(1)
CONTAINER_PATHS = set(CONTAINER_REASON)
CONTAINER_BY_NAME = {path.rsplit("::", 1)[1]: path for path in CONTAINER_PATHS}
API_LINTS = {lint for lint in LINTS if lint.startswith("disallowed_")}
CELL_DIRS = [Path(p) for p in os.environ["INF_CELL_DIRS"].splitlines()]
ITEM = re.compile(r"^\s*(pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(impl|mod|trait|struct|enum|type|use|const|static|fn)\b")
IMPL = re.compile(r"^\s*(?:unsafe\s+)?impl\b")
TRAIT = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+([A-Za-z_][A-Za-z_0-9]*)")
NAMED = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(struct|enum|union|const|static)\s+(?:mut\s+)?([A-Za-z_][A-Za-z_0-9]*)")
USE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+(.*)$")
SCOPES = os.environ["INF_SCOPES"]
GROUPS = ("clippy::pedantic", "clippy::restriction", "clippy::style", "clippy::all", "warnings")
ROOT_ATTR = re.compile(
    r"#!\[cfg_attr\(\s*not\(test\),\s*deny\(\s*clippy::wildcard_enum_match_arm,\s*"
    r"clippy::match_wildcard_for_single_variants,?\s*\)\s*,?\s*\)\]"
)
REASON = re.compile(r'reason\s*=\s*"((?:[^"\\]|\\.)*)"')

errors, oks, deny_sites, api_sites = [], [], [], []
# (file, first line, last line, item key, record, site) per `container:` allow
container_sites = []
production_of = {}  # cell file -> its production lines (test modules blanked)


def next_item_at(lines, at):
    i = at
    while i < len(lines):
        s = lines[i].strip()
        if not s or s.startswith("//"):
            i += 1
        elif s.startswith("#[") or s.startswith("#!["):
            i = attribute_end(lines, i)
        else:
            return i
    return len(lines) - 1


def next_item(lines, at):
    return lines[next_item_at(lines, at)]


def impl_type(header):
    """`Type` of an `impl<…> [Trait<…> for] path::Type<…> … {` header."""
    s = re.sub(r"^\s*(?:unsafe\s+)?impl\s*", "", header)
    if s.startswith("<"):
        depth = 0
        for k, ch in enumerate(s):
            depth += (ch == "<") - (ch == ">")
            if depth == 0:
                s = s[k + 1:]
                break
    s = s.split("{", 1)[0]
    s = re.split(r"\s+for\s+", s, maxsplit=1)[-1].strip()
    m = re.match(r"[A-Za-z_][A-Za-z_0-9:]*", s)
    return m.group(0).rsplit("::", 1)[-1] if m else s


def item_key(lines, at, last):
    """The container table's key of the item at `lines[at]` (ADR-0163 D2):
    the keyword and name; a method qualified by its `impl` (or trait) type;
    a `use` by its path text, a brace group written `{…}` so a path added to
    the group is a higher count on the same row, not a new row."""
    text = lines[at]
    fn = FN.match(text)
    if fn:
        for i in range(at - 1, -1, -1):
            owner = TRAIT.match(lines[i])
            if not (owner or IMPL.match(lines[i])):
                continue
            end = scope_table._body_end(lines, i)
            if end is not None and end >= at:
                name = owner.group(1) if owner else impl_type(" ".join(lines[i:i + 8]))
                return f"fn {name}::{fn.group(9)}"
        return f"fn {fn.group(9)}"
    use = USE.match(text)
    if use:
        path = re.sub(r"\s+", " ", " ".join([use.group(1)] + lines[at + 1:last + 1]))
        path = re.sub(r"\{.*\}", "{…}", path).split(";", 1)[0].strip()
        return f"use {path}"
    named = NAMED.match(text)
    return f"{named.group(1)} {named.group(2)}" if named else text.strip()


def production_lines(path):
    key = str(path)
    if key not in production_of:
        stripper = str(Path(os.environ["INF_SCRIPT_DIR"]) / "strip-test-modules.awk")
        production_of[key] = subprocess.check_output(["awk", "-f", stripper, key], text=True).split("\n")
    return production_of[key]


def container_names(lines):
    """(line, column, name, path), 1-based, of each banned name in the code of
    `lines` (comments, strings and char literals are not code) and of each use
    of a same-file `as` rename of one; the rename's own token is not a use.
    The text census of a file this host does not compile, and the backstop.
    A rename re-exported from another file is not followed (review's)."""
    banned = "|".join(map(re.escape, sorted(CONTAINER_BY_NAME)))
    if not re.search(rf"\b({banned})\b", "\n".join(lines)):
        return []
    code = [list(" " * len(line)) for line in lines]
    for i, j, ch in scope_table.code_chars(lines):
        code[i][j] = ch
    code = ["".join(row) for row in code]
    starts = [0]
    for line in code:
        starts.append(starts[-1] + len(line) + 1)
    paths, renames = dict(CONTAINER_BY_NAME), set()
    for m in re.finditer(rf"\b({banned})\s+as\s+([A-Za-z][A-Za-z0-9_]*|_[A-Za-z0-9_]+)\b", "\n".join(code)):
        paths[m.group(2)] = CONTAINER_BY_NAME[m.group(1)]
        i = bisect.bisect_right(starts, m.start(2)) - 1
        renames.add((i, m.start(2) - starts[i]))
    name = re.compile(r"\b(" + "|".join(map(re.escape, sorted(paths))) + r")\b")
    return [(i + 1, m.start() + 1, m.group(1), paths[m.group(1)])
            for i, line in enumerate(code) for m in name.finditer(line) if (i, m.start()) not in renames]


def host_gated(file):
    """The `target_os` this module file is declared under, if any: its
    `mod` item's attributes in the parent file (one level, as the tree
    declares `kqueue` and `uring`)."""
    p = Path(file)
    stem = p.parent.name if p.name == "mod.rs" else p.stem
    here = p.parent.parent if p.name == "mod.rs" else p.parent
    parents = [here / n for n in ("lib.rs", "main.rs", "mod.rs")] + [here.with_suffix(".rs")]
    for parent in parents:
        if not parent.is_file() or parent == p:
            continue
        lines = parent.read_text(encoding="utf-8", errors="replace").split("\n")
        for i, line in enumerate(lines):
            if re.match(rf"^\s*(pub(\([^)]*\))?\s+)?mod\s+{re.escape(stem)}\s*;", line):
                j = i
                while j > 0 and lines[j - 1].strip().startswith("#["):
                    j -= 1
                m = re.search(r'target_os\s*=\s*"([A-Za-z0-9_]+)"', " ".join(lines[j:i]))
                return m.group(1) if m else None
    return None


def audit(path, exempt_sites):
    lines = path.read_text(encoding="utf-8", errors="replace").split("\n")
    cell = any(path.is_relative_to(root) for root in CELL_DIRS)
    production = []
    if cell:
        stripper = str(Path(os.environ["INF_SCRIPT_DIR"]) / "strip-test-modules.awk")
        report = subprocess.check_output(["awk", "-v", "mode=report", "-f", stripper, str(path)], text=True)
        if "unterminated" in report:
            errors.append(f"{path}: test-only module never closed; API audit cannot establish scope")
        production = subprocess.check_output(["awk", "-f", stripper, str(path)], text=True).split("\n")
        production_of[str(path)] = production
        if path.name != "limits.rs" and "Cap::entries" in "\n".join(production):
            code = {(n, c) for n, c, _ in scope_table.code_chars(production)}
            for n, line in enumerate(production):
                for m in re.finditer(r"\bCap::entries\b", line):
                    if (n, m.start()) in code:
                        errors.append(f"{path}:{n + 1}: `Cap::entries` outside a `limits.rs` — a cap is a "
                                      "named const of its crate's `limits` module (ADR-0163 D2)")
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
        deny = scope_table.DENY.match(text)
        if deny and scope_table.denied([text], inner=bool(deny.group(1))):
            fn = FN.match(next_item(lines, end))
            deny_sites.append((str(path), int(site.rsplit(":", 1)[1]), bool(deny.group(1)), fn.group(9) if fn else ""))
        if not re.search(r"\b(allow|expect)\s*\(", head):
            continue
        named = [l for l in LINTS if re.search(rf"\b{l}\b", head)
                 and (l not in API_LINTS or (cell and production[next_item_at(lines, end)].strip()))]
        group = [g for g in GROUPS if re.search(rf"(^|[(,\s]){re.escape(g)}([),\s]|$)", head)]
        if not named and not group:
            continue
        if group:
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
            if not reason:
                errors.append(f"{site}: allow of {named[0]} without a reason")
            else:
                why = reason.group(1)
                valid = True
                for lint in named:
                    scope, classes = LINTS[lint]
                    if not why.startswith(classes) or not why.partition(":")[2].strip():
                        errors.append(f"{site}: reason class of {lint} must be one of {', '.join(classes)}")
                        valid = False
                    if scope == "fn" and not fn:
                        errors.append(f"{site}: allow of {lint} is not on a function")
                        valid = False
                    if scope == "statement" and (fn or ITEM.match(item)) and not (fn and why.startswith("clock:")):
                        errors.append(f"{site}: {lint} needs a statement; only clock: permits a function")
                        valid = False
                    kind = ITEM.match(item)
                    if scope == "item" and not fn and (not kind or kind.group(2) in ("impl", "mod", "trait", "type")):
                        errors.append(f"{site}: {lint} needs a narrow item, never a module, impl, trait or alias")
                        valid = False
                if valid:
                    oks.append(f"{site} — {why}")
                    api = set(named) & API_LINTS
                    if api:
                        first = next_item_at(lines, end)
                        last = scope_table._body_end(lines, first, statement=not bool(fn))
                        if last is None or last < first:
                            errors.append(f"{site}: API allow has no narrow scope the audit can close")
                        else:
                            api_sites.append((str(path), first + 1, last + 1, api, why.split(":", 1)[0]))
                            if "disallowed_types" in api and why.startswith("container:"):
                                record = why.partition(":")[2].strip()
                                key = item_key(lines, first, last)
                                container_sites.append((str(path), first + 1, last + 1, key, record, site))
                    if why.startswith("ADR-0143:") and fn:
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
for top in ("crates", "bins", "tests"):
    if not os.path.isdir(top):
        continue
    for dirpath, dirnames, names in os.walk(top):
        dirnames[:] = sorted(d for d in dirnames if d not in ("target", "fuzz"))
        for name in sorted(names):
            if name.endswith(".rs"):
                files += 1
                audit(Path(dirpath) / name, exempt_sites)

# ---- D2/D3: the scope table — every fuzz target is named by a row, every
# row's target and file exist, and the source reflects the table exactly:
# a scope's `deny` families are denied on that scope (the file's own inner
# attribute, or the outer attribute on the item), its `ratchet` families are
# not, and no decoder deny sits where no row names it
targets = set()
for crate in sorted(Path("crates").iterdir()):
    fuzz = crate / "fuzz" / "fuzz_targets"
    if fuzz.is_dir():
        targets |= {f"{crate.name}/{t.stem}" for t in fuzz.glob("*.rs")}
if not Path(SCOPES).is_file():
    print(f"LINT-SCOPES SCOPE ERROR: {SCOPES} is missing")
    sys.exit(1)
scopes_tbl = scope_table.load(SCOPES)
errors += scopes_tbl.errors
for t in sorted(scopes_tbl.targets - targets):
    errors.append(f"{SCOPES}: fuzz target `{t}` does not exist")
for t in sorted(targets - scopes_tbl.targets):
    errors.append(f"fuzz target `{t}` is named by no row of {SCOPES} — a decoder chooses its lint scope")
if not targets:
    print("LINT-SCOPES SCOPE ERROR: no fuzz target found under crates/*/fuzz/fuzz_targets")
    sys.exit(1)
reflected = set()  # (file, item) scopes whose source was read
for (file, item), fams in sorted(scopes_tbl.scopes.items()):
    where = f"{file}::{item}" if item else file
    if not Path(file).is_file():
        errors.append(f"{SCOPES}: `{file}` does not exist")
        continue
    lines = Path(file).read_text(encoding="utf-8", errors="replace").split("\n")
    if item:
        found = scope_table.locate_item(lines, item)
        if isinstance(found, str):
            errors.append(f"{SCOPES}: `{where}`: {found}")
            continue
        have = scope_table.denied(found[0], inner=False)
    else:
        have = scope_table.denied(scope_table.leading_attributes(lines), inner=True)
    reflected.add((file, item))
    want = {l for fam, tier in fams.items() if tier == "deny" for l in scope_table.FAMILIES[fam]}
    place = f"on `fn {item}`" if item else "among the file's own inner attributes"
    for lint in sorted(want - have):
        errors.append(f"{SCOPES}: `{where}` is tier deny for clippy::{lint} and lacks `cfg_attr(not(test), deny(…))` {place}")
    for lint in sorted(have - want):
        errors.append(f"{where}: denies clippy::{lint}, which {SCOPES} still ratchets — move the row to deny")
for file, line, inner, fn_name in deny_sites:
    scope = (file, "" if inner else fn_name)
    if scope not in scopes_tbl.scopes:
        errors.append(f"{file}:{line}: a decoder deny that no row of {SCOPES} names — the table is its one home")
    elif inner and scope in reflected:
        lines = Path(file).read_text(encoding="utf-8", errors="replace").split("\n")
        if line >= scope_table.first_item_line(lines):
            errors.append(f"{file}:{line}: a decoder deny inside a nested module — the row names the file")
denied_n, ratchet_n = len(scopes_tbl.families("deny")), len(scopes_tbl.families("ratchet"))

# ---- D5: the same audit checks resolved API paths from the ratchet's two
# existing JSON passes. --force-warn exposes even allowed calls, so a clock:
# allow cannot hide a file acquisition, nor a boot: allow an ambient clock.
diagnostics = os.environ.get("INF_LINT_API_DIAGNOSTICS")
container_spans = set()  # (file, line, column) of the banned paths, cell production code
unallowed = set()  # (file, line) of a banned path the audit already reports
if diagnostics:
    completed, resolved, in_build = 0, set(), set()
    expected = {lint for _, _, _, lints, _ in api_sites for lint in lints}
    for raw in Path(diagnostics).read_text().splitlines():
        try:
            message = json.loads(raw)
        except ValueError:
            continue
        if message.get("reason") == "build-finished":
            completed += 1
            if message.get("success") is not True:
                errors.append(f"API audit build {completed} failed")
            missing = expected - in_build
            if missing:
                errors.append(f"API audit build {completed} lacks force-warn witnesses for {sorted(missing)}")
            in_build.clear()
        if message.get("reason") != "compiler-message":
            continue
        d = message["message"]
        code = (d.get("code") or {}).get("code", "").removeprefix("clippy::")
        if code not in API_LINTS:
            continue
        match = re.search(r"use of a disallowed (?:method|type) `([^`]+)`", d["message"])
        if not match:
            errors.append(f"resolved API diagnostic has no path: {d['message']}")
            continue
        path = match.group(1)
        fs = path.startswith(("std::fs::", "std::path::Path::", "std::os::unix::fs::"))
        clock = path.startswith(("std::time::", "core::arch::")) or path in {
            "libc::clock_gettime", "libc::gettimeofday", "libc::time"
        }
        container = code == "disallowed_types" and path in CONTAINER_PATHS
        classes = ({"fs-seam", "boot", "control-thread"} if fs else
                   {"clock"} if clock else {"container"} if container else set())
        for span in d["spans"]:
            file, line = span["file_name"], span["line_start"]
            if Path(file).is_absolute() and Path(file).is_relative_to(Path.cwd()):
                file = str(Path(file).relative_to(Path.cwd()))
            if not span.get("is_primary") or not any(Path(file).is_relative_to(root) for root in CELL_DIRS):
                continue
            resolved.add((file, line, code, path))
            if container:
                container_spans.add((file, line, span["column_start"]))
            in_build.add(code)
            if not any(file == f and first <= line <= last and code in lints and cls in classes
                       for f, first, last, lints, cls in api_sites):
                errors.append(f"{file}:{line}: {path} lacks a narrow allow of its own API class")
                if container:
                    unallowed.add((file, line))
    if completed != 2:
        errors.append(f"API audit needs two completed feature-set builds, got {completed}")
    if in_build:
        errors.append("API audit has diagnostics after its last completed build")
    if not resolved:
        errors.append("API audit saw no resolved call; a missing force-warn carrier is not a clean audit")
    print(f"API call-class audit: {len(resolved)} resolved production sites, {completed} completed builds")

# ---- ADR-0163 D2: the container census. A `container:` allow covers the
# distinct primary spans (file, line, column) of the banned paths inside its
# item; a span inside two allowed items belongs to the narrower. A file this
# host does not compile (a `mod` under another `target_os`) is counted from
# its text instead (a same-file rename counted), and disclosed.
def holder(file, line):
    """The key of the narrowest `container:`-allowed item of `file` holding
    `line`, or None."""
    holders = [s for s in container_sites if s[0] == file and s[1] <= line <= s[2]]
    return min(holders, key=lambda s: s[2] - s[1])[3] if holders else None


census, text_counted = {}, {}
if diagnostics:
    for f, _, _, key, _, _ in container_sites:
        census[(f, key)] = 0
    spans_of = {}
    for f, line, column in container_spans:
        spans_of.setdefault(f, set()).add((line, column))
    for f in sorted({site[0] for site in container_sites}):
        gate_os = host_gated(f)
        if gate_os and gate_os != HOST_OS:
            text_counted[f] = gate_os
            spans_of[f] = {(line, column) for line, column, _, _ in container_names(production_lines(f))}
    for f, spans in spans_of.items():
        for line, _ in spans:
            key = holder(f, line)
            if key is not None:
                census[(f, key)] += 1
    # The backstop inside an exempt item of a compiled file: its text names
    # no more containers than its compiled spans, so a field under a feature
    # neither build enables is not hidden by the row.
    for f in sorted({site[0] for site in container_sites} - set(text_counted)):
        written = {}
        for line, _, _, _ in container_names(production_lines(f)):
            key = holder(f, line)
            if key is not None:
                written[key] = written.get(key, 0) + 1
        for key, n in sorted(written.items()):
            if n > census[(f, key)]:
                errors.append(f"{f}: `{key}` names {n} container(s) in its text, {census[(f, key)]} in "
                              "the compiled spans — one neither build compiles is still a container, and a "
                              "container added to an exempt item is red: use a capped type")
if CENSUS:
    if not diagnostics:
        print("LINT-SCOPES SCOPE ERROR: the container census needs the ratchet's passes")
        sys.exit(1)
    record_of = {(s[0], s[3]): s[4] for s in container_sites}
    rows = [(f, key, n, record_of[(f, key)]) for (f, key), n in sorted(census.items())
            if record_of[(f, key)] != BACKING]
    for row in rows:
        print("\t".join(str(col) for col in row))
    gated = "".join(f"; {f} counted from its text (compiled only on {os_name})"
                    for f, os_name in sorted(text_counted.items()))
    print(f"# container census: {len(rows)} rows, sum {sum(r[2] for r in rows)}{gated}")
    sys.exit(0)

# ---- ADR-0163 D2's backstop, read from the text and so the same on every
# host: a banned name in cell production code (test modules stripped) outside
# every `container:` allow is red, whether or not either build compiles it.
# A line the audit above already reports is not reported twice.
backstop_files = 0
for f, production in sorted(production_of.items()):
    backstop_files += 1
    for line, _, name, path in container_names(production):
        if (f, line) not in unallowed and holder(f, line) is None:
            errors.append(f"{f}:{line}: `{name}` names {path} outside every `container:` allow (read from "
                          f"the text, whatever the host compiles) — {CONTAINER_REASON[path]}")

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

# ---- ADR-0163 D2: the container exemption table.
# Columns: file, item, count, record, expiry. The first four are the
# census's; the expiry is assigned by hand and only moves earlier.
DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
GATE_MAX = re.compile(r"^CONTAINER_EXEMPTIONS_MAX=([0-9]+)[ \t]*$", re.M)


def container_rows(text, label):
    rows = {}
    for n, line in enumerate(text.split("\n"), 1):
        if not line.strip() or line.startswith("#"):
            continue
        cols = line.split("\t")
        at = f"{label}:{n}"
        if len(cols) != 5 or not all(cols):
            errors.append(f"{at}: malformed row (file<TAB>item<TAB>count<TAB>record<TAB>expiry)")
            continue
        file, item, count, record, expiry = cols
        if not re.fullmatch(r"[1-9][0-9]*", count):
            errors.append(f"{at}: count `{count}` is not a positive integer")
            continue
        if record not in RECORDS:
            errors.append(f"{at}: record `{record}` is not a site record ({' '.join(RECORDS)})")
            continue
        try:
            due = datetime.date.fromisoformat(expiry) if DATE.match(expiry) else None
        except ValueError:
            due = None
        if due is None:
            errors.append(f"{at}: expiry `{expiry}` is not a date (YYYY-MM-DD)")
            continue
        if (file, item) in rows:
            errors.append(f"{at}: {file} `{item}` is listed twice")
            continue
        rows[(file, item)] = (int(count), record, due)
    return rows


if not Path(CONTAINERS).is_file():
    print(f"LINT-SCOPES SCOPE ERROR: {CONTAINERS} is missing")
    sys.exit(1)
crows = container_rows(Path(CONTAINERS).read_text(), CONTAINERS)
backing = [s for s in container_sites if s[4] == BACKING]
if len(backing) != BACKING_SITES:
    where = ", ".join(s[5] for s in backing) or "none"
    errors.append(f"{len(backing)} `container: {BACKING}` allow(s) ({where}); the gate wants exactly "
                  f"{BACKING_SITES}: the one sanctioned holder's backing (ADR-0151 D6)")
for f, _, _, key, _, site in backing:
    if key != BACKING_ITEM or not Path(f).is_relative_to(BACKING_HOME):
        errors.append(f"{site}: `container: {BACKING}` on `{key}` in {f} — the one backing is "
                      f"`{BACKING_ITEM}` under {BACKING_HOME}/ (ADR-0151 D6)")
    elif diagnostics and census.get((f, key), 0) != 1:
        errors.append(f"{site}: the backing `{key}` holds {census.get((f, key), 0)} container span(s) — "
                      "exactly one (ADR-0151 D6)")
exempt = [s for s in container_sites if s[4] != BACKING]
keys = {}
for f, _, _, key, record, site in exempt:
    if record not in RECORDS:
        errors.append(f"{site}: `container: {record}` names no site record ({' '.join(RECORDS)}) "
                      f"and is not `{BACKING}`")
    keys.setdefault((f, key), []).append(site)
for (f, key), sites in sorted(keys.items()):
    if len(sites) > 1:
        errors.append(f"{f}: {len(sites)} `container:` allows share the item key `{key}` "
                      f"({', '.join(sites)}) — the table cannot tell them apart")
today = datetime.date.today()
for (f, key), (count, record, due) in sorted(crows.items()):
    sites = [s for s in exempt if (s[0], s[3]) == (f, key)]
    if not sites:
        errors.append(f"{CONTAINERS}: row {f} `{key}` has no `container:` allow — delete it")
    elif sites[0][4] != record:
        errors.append(f"{sites[0][5]}: `container: {sites[0][4]}` but its row says record {record}")
    if due < today:
        errors.append(f"{CONTAINERS}: row {f} `{key}` expired on {due} — migrate its site")
    if diagnostics and sites:
        n = census.get((f, key), 0)
        if n == 0:
            errors.append(f"{sites[0][5]}: the allow on `{key}` covers no container — delete it and its row")
        elif n > count:
            errors.append(f"{f}: `{key}` holds {n} container span(s), its row says {count} — a container "
                          "added to an exempt item is red: use a capped type")
        elif n < count:
            errors.append(f"{f}: `{key}` holds {n} container span(s), its row says {count} — lower the row to {n}")
for f, _, _, key, record, site in exempt:
    if (f, key) not in crows:
        errors.append(f"{site}: `container: {record}` allow on `{key}` has no row in {CONTAINERS} — "
                      "new exemptions are closed")
csum = sum(count for count, _, _ in crows.values())
if csum > CONTAINER_MAX:
    errors.append(f"{CONTAINERS}: counts sum to {csum}, above CONTAINER_EXEMPTIONS_MAX = {CONTAINER_MAX}")

# approved copies: per file and record the counts only shrink, a pair the
# copy lacks is red, a date only moves earlier, the maximum only falls. A
# copy whose gate has no CONTAINER_EXEMPTIONS_MAX predates the container gate
# (bootstrap); one whose gate has it and no table is red.
cintro = git("log", "--diff-filter=A", "--format=%H", "--", CONTAINERS).stdout.split()
ccopies = [("HEAD", "HEAD"), ("base tip", BASE_REF)] + ([("introducing commit", cintro[-1])] if cintro else [])
csums = {}
for (f, _), (count, record, _) in crows.items():
    csums[(f, record)] = csums.get((f, record), 0) + count
for label, ref in ccopies:
    if git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode != 0:
        continue  # an unresolvable base tip is already a scope error above
    gate = git("show", f"{ref}:{SELF}")
    copy_max = GATE_MAX.search(gate.stdout) if gate.returncode == 0 else None
    if not copy_max:
        notes.append(f"{label} predates the container gate (bootstrap)")
        continue
    if CONTAINER_MAX > int(copy_max.group(1)):
        errors.append(f"CONTAINER_EXEMPTIONS_MAX = {CONTAINER_MAX} is above the {label}'s "
                      f"{copy_max.group(1)} — only a lower number may replace it")
    shown = git("show", f"{ref}:{CONTAINERS}")
    if shown.returncode != 0:
        errors.append(f"{label} ({ref}) has the container gate and no {CONTAINERS} — a deleted table "
                      "is not a bootstrap")
        continue
    approved = container_rows(shown.stdout, f"{label}:{CONTAINERS}")
    renames = {}
    for line in git("diff", "--name-status", "-M", ref, "--").stdout.split("\n"):
        cols = line.split("\t")
        if len(cols) == 3 and cols[0].startswith("R"):
            renames[cols[2]] = cols[1]
    approved_sums, approved_dates = {}, {}
    for (f, key), (count, record, due) in approved.items():
        approved_sums[(f, record)] = approved_sums.get((f, record), 0) + count
        approved_dates[(f, key, record)] = due
        approved_dates[(f, "", record)] = max(due, approved_dates.get((f, "", record), due))
    for (f, record), n in sorted(csums.items()):
        was = approved_sums.get((renames.get(f, f), record))
        if was is None:
            errors.append(f"{CONTAINERS}: {f} has record {record} rows and the {label}'s table has none — "
                          "an exemption never moves to a new file or record")
        elif n > was:
            errors.append(f"{CONTAINERS}: {f} record {record} counts sum to {n}, above the {label}'s {was} — "
                          "the table only shrinks")
    for (f, key), (_, record, due) in sorted(crows.items()):
        old = renames.get(f, f)
        was = approved_dates.get((old, key, record), approved_dates.get((old, "", record)))
        if was is not None and due > was:
            errors.append(f"{CONTAINERS}: row {f} `{key}` expiry {due} is later than the {label}'s {was} — "
                          "a date only moves earlier")

if errors:
    for e in errors:
        print(f"LINT-SCOPES violation: {e}")
    print(f"lint-scopes FAILED: {len(errors)} violation(s)")
    sys.exit(1)
scope = f"{roots} crate roots under the wildcard deny, {files} files audited, {len(oks)} reasoned allow(s), {len(table)}/{MAX} ADR-0143 exemptions, {len(targets)} fuzz targets scoped, {len(scopes_tbl.scopes)} scope(s): {denied_n} (scope, family) denied, {ratchet_n} ratcheted"
scope += (f"; container exemptions: {len(crows)} row(s), counts {csum}/{CONTAINER_MAX}, "
          f"{len(backing)}/{BACKING_SITES} capped-backing, "
          + ("counts judged against the census" if diagnostics else "counts judged in the ratchet's API pass")
          + f", {backstop_files} cell file(s) read as text for a banned name outside an allow")
scope += "".join(f"; {f} counted from its text (compiled only on {os_name})" for f, os_name in sorted(text_counted.items()))
if notes:
    scope += "; " + "; ".join(notes)
print(f"lint-scopes OK: {scope}")
for ok in oks:
    print(f"    allow: {ok}")
PY

if [ "${INF_LINT_RULES:-0}" = 1 ] || [ -n "${INF_LINT_API_DIAGNOSTICS:-}" ]; then
    exit 0
fi

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
# Fixture mutations may supply their own config; both judges read the exact
# config Clippy uses. The shipped probe inherits the production config.
if [ ! -f "$pwork/probe/clippy.toml" ]; then
    cp "$WS_ROOT/clippy.toml" "$pwork/probe/clippy.toml"
fi
pin=$(python3 -B - "$WS_ROOT/rust-toolchain.toml" <<'PY'
import re, sys, tomllib
pin = tomllib.loads(open(sys.argv[1]).read())["toolchain"]["channel"]
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", pin):
    sys.exit("LINT-SCOPES SCOPE ERROR: filesystem probes need a pinned stable compiler")
print(pin)
PY
)
(cd "$pwork/probe" && env -u CLIPPY_CONF_DIR -u RUSTC_BOOTSTRAP cargo +"$pin" clippy --quiet --target-dir "$pwork/target" \
    --message-format=json >"$pwork/diag.json" 2>"$pwork/stderr") || true
unstable_exit=0
env -u RUSTC_BOOTSTRAP rustc +"$pin" --edition=2021 --crate-type=lib --emit=metadata \
    --error-format=json --out-dir "$pwork" "$pwork/probe/unstable/set_times.rs" \
    >"$pwork/unstable.stdout" 2>"$pwork/unstable.json" || unstable_exit=$?
python3 -B "$SCRIPT_DIR/lint-scope-probe/judge.py" "$pwork/probe/src/lib.rs" \
    "$pwork/diag.json" "$pwork/probe/clippy.toml" "$pwork/unstable.json" "$unstable_exit"
