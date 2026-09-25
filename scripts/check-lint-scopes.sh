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
# Cell membership has one owner, shared with the runtime safety gates.
. "$SCRIPT_DIR/cell-crates.sh"
CELL_DIRS=$(cell_crate_dirs)

for dir in crates bins; do
    [ -d "$dir" ] || { echo "LINT-SCOPES SCOPE ERROR: $dir/ is missing"; exit 1; }
done

INF_CELL_DIRS="$CELL_DIRS" INF_SCOPES="${INF_LINT_SCOPES:-docs/lint-scopes.tsv}" INF_EXEMPTIONS="$EXEMPTIONS" INF_BASE_REF="$BASE_REF" INF_MAX="$ADR0143_EXEMPTIONS_MAX" \
    INF_SELF="scripts/check-lint-scopes.sh" INF_SCRIPT_DIR="$SCRIPT_DIR" python3 -B - <<'PY'
import os
import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
import lint_scope_table as scope_table
from lint_scope_table import FN, attribute_end

EXEMPTIONS = os.environ["INF_EXEMPTIONS"]
BASE_REF = os.environ["INF_BASE_REF"]
MAX = int(os.environ["INF_MAX"])
SELF = os.environ["INF_SELF"]

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
    "disallowed_types": ("item", ("fs-seam:", "boot:", "control-thread:")),
}
if os.environ.get("INF_LINT_RULES") == "1":
    for lint, (scope, classes) in LINTS.items():
        example = next(cls for cls in classes if cls != "ADR-0143:")
        print(f"{lint}\t{scope}\t{example}")
    sys.exit(0)
API_LINTS = {lint for lint in LINTS if lint.startswith("disallowed_")}
CELL_DIRS = [Path(p) for p in os.environ["INF_CELL_DIRS"].splitlines()]
ITEM = re.compile(r"^\s*(pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:impl|mod|trait|struct|enum|type|use|const|static|fn)\b")
SCOPES = os.environ["INF_SCOPES"]
GROUPS = ("clippy::pedantic", "clippy::restriction", "clippy::style", "clippy::all", "warnings")
ROOT_ATTR = re.compile(
    r"#!\[cfg_attr\(\s*not\(test\),\s*deny\(\s*clippy::wildcard_enum_match_arm,\s*"
    r"clippy::match_wildcard_for_single_variants,?\s*\)\s*,?\s*\)\]"
)
REASON = re.compile(r'reason\s*=\s*"((?:[^"\\]|\\.)*)"')

errors, oks, deny_sites, api_sites = [], [], [], []


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
                    if scope == "item" and (not ITEM.match(item) or re.search(r"\b(impl|mod|trait|type)\b", item)):
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
        classes = {"fs-seam", "boot", "control-thread"} if fs else ({"clock"} if clock else set())
        for span in d["spans"]:
            file, line = span["file_name"], span["line_start"]
            if Path(file).is_absolute() and Path(file).is_relative_to(Path.cwd()):
                file = str(Path(file).relative_to(Path.cwd()))
            if not span.get("is_primary") or not any(Path(file).is_relative_to(root) for root in CELL_DIRS):
                continue
            resolved.add((file, line, code, path))
            in_build.add(code)
            if not any(file == f and first <= line <= last and code in lints and cls in classes
                       for f, first, last, lints, cls in api_sites):
                errors.append(f"{file}:{line}: {path} lacks a narrow allow of its own API class")
    if completed != 2:
        errors.append(f"API audit needs two completed feature-set builds, got {completed}")
    if in_build:
        errors.append("API audit has diagnostics after its last completed build")
    if not resolved:
        errors.append("API audit saw no resolved call; a missing force-warn carrier is not a clean audit")
    print(f"API call-class audit: {len(resolved)} resolved production sites, {completed} completed builds")

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
scope = f"{roots} crate roots under the wildcard deny, {files} files audited, {len(oks)} reasoned allow(s), {len(table)}/{MAX} ADR-0143 exemptions, {len(targets)} fuzz targets scoped, {len(scopes_tbl.scopes)} scope(s): {denied_n} (scope, family) denied, {ratchet_n} ratcheted"
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
