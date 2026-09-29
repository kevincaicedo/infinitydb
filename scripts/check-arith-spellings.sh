#!/usr/bin/env bash
# ADR-0165 D2: the arithmetic spellings the lint cannot see. Clippy's
# `arithmetic_side_effects` flags a plain operator; it is silent on
# `saturating_*` and `wrapping_*`, and which of ADR-0165 D1's rows a call
# belongs to is intent, judged at review. This gate makes every such call in
# a decoder scope a reviewed change: docs/arith-spellings.tsv holds one row
# per (family, scope) with the exact count, for each scope that
# docs/lint-scopes.tsv puts at tier `deny` for `arith`. The table equals the
# tree — a new site, or a removed one, edits the table in the same diff.
#
# Counted: `.saturating_*` / `::saturating_*`, the `wrapping_` and
# `overflowing_` forms, and the `Saturating` / `Wrapping` types (their
# operators pass the lint), in production code: test modules blanked by
# strip-test-modules.awk, and strings, chars and comments skipped by
# lint_scope_table.code_chars, the gates' one Rust lexer. An item scope
# counts inside its `fn` only. A field or fn named `saturating_*` counts
# too: the table names it, and review sees it.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"

INF_SCOPES="${INF_LINT_SCOPES:-docs/lint-scopes.tsv}" \
    INF_SPELLINGS="${INF_ARITH_SPELLINGS:-docs/arith-spellings.tsv}" \
    INF_SCRIPT_DIR="$SCRIPT_DIR" python3 -B - <<'PY'
import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, os.environ["INF_SCRIPT_DIR"])
import lint_scope_table as scope_table

SCOPES, TABLE = os.environ["INF_SCOPES"], os.environ["INF_SPELLINGS"]
STRIPPER = str(Path(os.environ["INF_SCRIPT_DIR"]) / "strip-test-modules.awk")
FAMILIES = ("saturating", "wrapping", "overflowing")
CALL = re.compile(r"(?:\.|::)\s*(saturating|wrapping|overflowing)_[a-z0-9_]+\b"
                  r"|\b(Saturating|Wrapping)\b")


def code_lines(lines):
    """Each line with everything but code blanked (one lexer, L13)."""
    out = [[" "] * len(l) for l in lines]
    for i, j, ch in scope_table.code_chars(lines):
        out[i][j] = ch
    return ["".join(l) for l in out]


for path in (SCOPES, TABLE):
    if not Path(path).is_file():
        print(f"ARITH-SPELLINGS SCOPE ERROR: {path} is missing")
        sys.exit(1)
errors = []
scopes = scope_table.load(SCOPES)
errors += scopes.errors
denied = sorted((f, i) for (f, i), fams in scopes.scopes.items() if fams.get("arith") == "deny")
if not denied:
    print(f"ARITH-SPELLINGS SCOPE ERROR: {SCOPES} puts no scope at deny for arith")
    sys.exit(1)

tree = {}
for file, item in denied:
    where = f"{file}::{item}" if item else file
    if not Path(file).is_file():
        errors.append(f"{SCOPES}: `{file}` does not exist")
        continue
    production = subprocess.check_output(["awk", "-f", STRIPPER, file], text=True).split("\n")
    production = code_lines(production)
    first, last = 1, len(production)
    if item:
        found = scope_table.locate_item(Path(file).read_text(encoding="utf-8").split("\n"), item)
        if isinstance(found, str):
            errors.append(f"{SCOPES}: `{where}`: {found}")
            continue
        _, first, last = found
    for line in production[first - 1:last]:
        for method, kind in CALL.findall(line):
            fam = method or kind.lower()
            tree[(fam, where)] = tree.get((fam, where), 0) + 1

table = {}
scope_names = {f"{f}::{i}" if i else f for f, i in denied}
for n, line in enumerate(Path(TABLE).read_text(encoding="utf-8").split("\n"), 1):
    if not line.strip() or line.startswith("#"):
        continue
    cols = line.split("\t")
    if len(cols) != 3 or cols[0] not in FAMILIES or not cols[1].isdigit() or cols[1] == "0":
        errors.append(f"{TABLE}:{n}: malformed row ({'|'.join(FAMILIES)}, count ≥ 1, scope)")
        continue
    fam, count, where = cols[0], int(cols[1]), cols[2]
    if where not in scope_names:
        errors.append(f"{TABLE}:{n}: `{where}` is not a deny-arith scope of {SCOPES}")
    elif (fam, where) in table:
        errors.append(f"{TABLE}:{n}: `{where}` {fam} is listed twice")
    else:
        table[(fam, where)] = count

for key in sorted(set(tree) | set(table)):
    have, want = tree.get(key, 0), table.get(key, 0)
    if have != want:
        fam, where = key
        errors.append(f"{where}: {have} `{fam}_*` call(s), {TABLE} says {want} — "
                      "the table changes with the site, reviewed against ADR-0165 D1")

if errors:
    for e in errors:
        print(e)
    print(f"arith-spellings: FAILED ({len(errors)} problem(s))")
    sys.exit(1)
totals = {fam: sum(c for (f, _), c in tree.items() if f == fam) for fam in FAMILIES}
print(f"arith-spellings: OK — {totals['saturating']} saturating, {totals['wrapping']} wrapping, "
      f"{totals['overflowing']} overflowing in {len(denied)} deny-arith scope(s); "
      "the row of each is judged at review (ADR-0165 D1)")
PY
