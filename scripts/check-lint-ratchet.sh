#!/usr/bin/env bash
# ADR-0144 D3 (absorbs ADR-0125's fn-length ratchet): one ratchet for every
# lint whose backlog is disclosed instead of denied.
#
#   * families: `fn_length` (clippy::too_many_lines at the clippy.toml
#     threshold, every production file), `cast` (the three cast lints) and
#     `arith` (clippy::arithmetic_side_effects) — the last two only on the
#     files docs/lint-scopes.tsv lists at tier `ratchet`;
#   * docs/lint-baseline.tsv is the disclosed backlog, one row per
#     (family, file): `family<TAB>count<TAB>file`. A file above its row is
#     red, a file below its row is red too ("lower the row"), so the number
#     in the docs is always the number in the tree;
#   * "never rises" is judged against approved copies of the table — HEAD,
#     the base branch tip and the table's introducing commit: a family's
#     total above any copy's is red, and a row may rise or appear only on a
#     file git shows as added or renamed against that copy (a split carries
#     its counts; a fix does not buy a violation). Moved rows are printed;
#   * one clippy JSON pass per feature set, counted once per span (cargo
#     re-emits a crate's diagnostics per feature set — ADR-0125 A1).
#
# INF_LINT_RATCHET_INPUT=<file> replaces the cargo run with captured
# `--message-format=json` output (the self-test's fixtures).
# INF_LINT_BASE_REF=<ref> names the base branch tip (default origin/main).
# INF_LINT_RATCHET_HOST=<uname -s>: the baseline is recorded on Linux; on
# another host a row whose cfg-gated file never compiled is disclosed.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"
work=$(mktemp -d)
trap '[ -n "$work" ] && [ -d "$work" ] && rm -rf "$work"' EXIT

LINTS="-W clippy::too_many_lines -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::cast_possible_wrap -W clippy::arithmetic_side_effects"
if [ -n "${INF_LINT_RATCHET_INPUT:-}" ]; then
    cp "$INF_LINT_RATCHET_INPUT" "$work/clippy.json"
else
    if ! grep -Eq '^too-many-lines-threshold *= *70$' clippy.toml; then
        echo "LINT-RATCHET SCOPE ERROR: clippy.toml does not pin too-many-lines-threshold = 70"
        exit 1
    fi
    # The simulator's lib only builds under its `dst` feature (ADR-0107).
    # shellcheck disable=SC2086
    {
        cargo clippy --workspace --exclude inf-sim --lib --bins --message-format=json -- $LINTS &&
        cargo clippy -p inf-sim --features dst --lib --bins --message-format=json -- $LINTS
    } >"$work/clippy.json" 2>"$work/clippy.err" || {
        tail -n 40 "$work/clippy.err"
        echo "LINT-RATCHET SCOPE ERROR: cargo clippy failed"
        exit 1
    }
fi

INF_BASELINE="${INF_LINT_BASELINE:-docs/lint-baseline.tsv}" \
INF_SCOPES="${INF_LINT_SCOPES:-docs/lint-scopes.tsv}" \
INF_BASE_REF="${INF_LINT_BASE_REF:-origin/main}" \
INF_HOST="${INF_LINT_RATCHET_HOST:-$(uname -s)}" \
INF_SCRIPT_DIR="$SCRIPT_DIR" \
python3 - "$work/clippy.json" <<'PY'
import json
import os
import re
import subprocess
import sys
from collections import Counter

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
import lint_scope_table as scope_table

BASELINE, SCOPES = os.environ["INF_BASELINE"], os.environ["INF_SCOPES"]
BASE_REF, HOST = os.environ["INF_BASE_REF"], os.environ["INF_HOST"]
SELF = "scripts/check-lint-ratchet.sh"
FAMILY = {
    "clippy::too_many_lines": "fn_length",
    "clippy::cast_possible_truncation": "cast",
    "clippy::cast_sign_loss": "cast",
    "clippy::cast_possible_wrap": "cast",
    "clippy::arithmetic_side_effects": "arith",
}
PATH = re.compile(r"^(crates|bins|tests)/[^\t ]+\.rs$")
errors, notes = [], []


def scope_error(msg):
    print(f"LINT-RATCHET SCOPE ERROR: {msg}")
    sys.exit(1)


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True)


def parse_baseline(text, label, strict):
    rows = {}
    for n, line in enumerate(text.split("\n"), 1):
        if not line.strip() or line.startswith("#"):
            continue
        cols = line.split("\t")
        ok = (
            len(cols) == 3
            and cols[0] in set(FAMILY.values())
            and re.fullmatch(r"[1-9][0-9]*", cols[1])
            and PATH.match(cols[2])
        )
        if not ok:
            if strict:
                scope_error(f"malformed baseline row {label}:{n}: {line}")
            continue
        key = (cols[0], cols[2])
        if key in rows and strict:
            scope_error(f"{label} lists {cols[0]} {cols[2]} twice")
        rows[key] = int(cols[1])
    return rows


# ---- which (scope, family) ratchets (docs/lint-scopes.tsv — read through
# the one parser the scope gate uses). An item scope is its function's lines.
if not os.path.isfile(SCOPES):
    scope_error(f"{SCOPES} is missing")
scopes_tbl = scope_table.load(SCOPES)
if scopes_tbl.errors:
    scope_error(scopes_tbl.errors[0])
spans = {}  # file -> [(first line, last line, {family: tier})]; whole file = (1, inf)
for (file, item), fams in sorted(scopes_tbl.scopes.items()):
    first, last = 1, float("inf")
    if item:
        if not os.path.isfile(file):
            scope_error(f"{SCOPES}: `{file}` does not exist")
        lines = open(file, encoding="utf-8", errors="replace").read().split("\n")
        found = scope_table.locate_item(lines, item)
        if isinstance(found, str):
            scope_error(f"{SCOPES}: `{file}::{item}`: {found}")
        _, first, last = found
    spans.setdefault(file, []).append((first, last, fams))


def tier_of(family, file, line):
    for first, last, fams in spans.get(file, ()):
        if first <= line <= last:
            return fams[family]
    return None


# ---- count, once per span
sites, denied_sites, finished = set(), set(), False
for raw in open(sys.argv[1], encoding="utf-8"):
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    if msg.get("reason") == "build-finished":
        finished = finished or bool(msg.get("success"))
    if msg.get("reason") != "compiler-message":
        continue
    d = msg["message"]
    family = FAMILY.get((d.get("code") or {}).get("code"))
    if family is None:
        continue
    for sp in d["spans"]:
        if sp.get("is_primary"):
            f = sp["file_name"]
            tier = "ratchet" if family == "fn_length" else tier_of(family, f, sp["line_start"])
            if tier == "ratchet":
                sites.add((family, f, sp["line_start"], sp["column_start"]))
            elif tier == "deny":
                denied_sites.add((family, f, sp["line_start"]))
if not finished:
    scope_error("clippy reported no completed build — no count is not a clean count")
counts = Counter((fam, f) for fam, f, _, _ in sites)
for fam, f, line in sorted(denied_sites):
    errors.append(f"{f}:{line} has a {fam} site in a scope {SCOPES} denies — a row cannot buy it")

# ---- tree vs the working table, both directions
if not os.path.isfile(BASELINE):
    scope_error(f"{BASELINE} is missing")
table = parse_baseline(open(BASELINE, encoding="utf-8").read(), BASELINE, True)
for key, n in sorted(counts.items()):
    base = table.get(key)
    if base is None:
        errors.append(f"{key[1]} has {n} {key[0]} site(s) and no baseline row — fix them")
    elif n > base:
        errors.append(f"{key[1]} has {n} {key[0]} site(s), baseline {base} — fix the new one, never raise the row")
    elif n < base:
        errors.append(f"{key[1]} has {n} {key[0]} site(s), baseline {base} — lower the row in {BASELINE} to {n}")
skipped = 0
for key, base in sorted(table.items()):
    if key not in counts:
        if HOST == "Linux":
            errors.append(f"{key[1]} has no {key[0]} site, baseline {base} — delete the row in {BASELINE}")
        else:
            notes.append(f"{key[1]} ({key[0]} {base}) not compiled on {HOST}")
            skipped += 1

# ---- the working table vs its approved copies
if git("rev-parse", "--git-dir").returncode != 0:
    scope_error("not a git repository — the approved copies cannot be read")
intro = git("log", "--diff-filter=A", "--format=%H", "--", BASELINE).stdout.split()
copies = [("HEAD", "HEAD"), ("base tip", BASE_REF)]
copies += [("introducing commit", intro[-1])] if intro else []
totals = Counter()
for (fam, _), n in table.items():
    totals[fam] += n
moved = set()
for label, ref in copies:
    if git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode != 0:
        if label == "base tip":
            errors.append(f"SCOPE: base ref `{ref}` does not resolve (fetch it; CI needs fetch-depth: 0)")
        continue
    shown = git("show", f"{ref}:{BASELINE}")
    if shown.returncode != 0:
        if git("cat-file", "-e", f"{ref}:{SELF}").returncode == 0:
            errors.append(f"{label} ({ref}) has the gate and no {BASELINE} — a deleted table is not a bootstrap")
        else:
            notes.append(f"{label} predates the gate (bootstrap)")
        continue
    approved = parse_baseline(shown.stdout, label, False)
    approved_totals = Counter()
    for (fam, _), n in approved.items():
        approved_totals[fam] += n
    for fam in sorted(totals):
        if totals[fam] > approved_totals[fam]:
            errors.append(f"{fam} total {totals[fam]} exceeds the {label}'s {approved_totals[fam]} — the backlog only shrinks")
    fresh = {}
    for line in git("diff", "--name-status", "-M", ref, "--").stdout.split("\n"):
        cols = line.split("\t")
        if len(cols) == 3 and cols[0].startswith("R"):
            fresh[cols[2]] = cols[1]
        elif len(cols) == 2 and cols[0] == "A":
            fresh[cols[1]] = None
    for (fam, f), n in sorted(table.items()):
        was = approved.get((fam, f), approved.get((fam, fresh.get(f)), 0))
        if n > was:
            if f in fresh:
                moved.add(f"{fam} {f} ({was} → {n})")
            else:
                errors.append(f"{fam} row of {f} rose {was} → {n} against the {label} on a file that is neither added nor renamed — a fix does not buy a violation")

if errors:
    for e in errors:
        print(f"LINT-RATCHET violation: {e}")
    print(f"lint-ratchet FAILED: {len(errors)} violation(s)")
    sys.exit(1)
scope = ", ".join(f"{fam} {totals[fam]} in {sum(1 for k in table if k[0] == fam)} file(s)" for fam in sorted(totals))
print(f"lint-ratchet OK: {scope or 'no backlog'} (the baseline backlog); {len(scopes_tbl.families('ratchet'))} (scope, family) ratcheted, {len(scopes_tbl.families('deny'))} denied")
for m in sorted(moved):
    print(f"    moved: {m}")
for n in notes:
    print(f"    note: {n}")
PY
