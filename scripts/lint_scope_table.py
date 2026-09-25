"""docs/lint-scopes.tsv — its one parser (ADR-0144 D2/D3).

`check-lint-scopes.sh` (the attribute is the table's reflection) and
`check-lint-ratchet.sh` (which diagnostics a baseline row counts) both read
the table through `load`, so a scope means the same thing to both gates.

A scope is `(file, item)` — `item` is "" for a whole file — and maps each
lint family to one tier. Casts may be denied where arithmetic still
ratchets, so the tier is per (scope, family): a scope is split over two
rows when its families differ.
"""
import re

FAMILIES = {
    "cast": ("cast_possible_truncation", "cast_sign_loss", "cast_possible_wrap"),
    "arith": ("arithmetic_side_effects",),
}
TIERS = ("deny", "ratchet")
LINT_FAMILY = {lint: fam for fam, lints in FAMILIES.items() for lint in lints}

FN = re.compile(
    r"^\s*(pub(\([^)]*\))?\s+)?(default\s+)?(const\s+)?(async\s+)?(unsafe\s+)?"
    r'(extern\s+("[^"]*"\s+)?)?fn\s+([A-Za-z_][A-Za-z_0-9]*)'
)
DENY = re.compile(r"^#(!?)\[cfg_attr\(\s*not\(test\)\s*,\s*deny\((.*)\)\s*,?\s*\)\]$")
CHAR = re.compile(r"'(\\[^\n']+|[^\\'\n])'")
RAW = re.compile(r'b?r(#*)"')


class Table:
    def __init__(self):
        self.targets = set()  # every fuzz target a row names
        self.scopes = {}  # (file, item) -> {family: tier}
        self.errors = []

    def files(self):
        return sorted({file for file, _ in self.scopes})

    def families(self, tier):
        """[(file, item, family)] at `tier`."""
        return sorted(
            (file, item, fam)
            for (file, item), fams in self.scopes.items()
            for fam, t in fams.items()
            if t == tier
        )


def load(path):
    table = Table()
    seen = set()
    for n, line in enumerate(open(path, encoding="utf-8").read().split("\n"), 1):
        if not line.strip() or line.startswith("#"):
            continue
        at = f"{path}:{n}"
        cols = line.split("\t")
        if len(cols) != 4 or not all(cols):
            table.errors.append(f"{at}: malformed row (target, file[::item], lints, tier)")
            continue
        target, where, lints, tier = cols
        table.targets.add(target)
        if where.startswith("none:"):
            if len(where) < len("none: ") + 8:
                table.errors.append(f"{at}: a `none:` row states its reason")
            if (lints, tier) != ("-", "-"):
                table.errors.append(f"{at}: a `none:` row names no lint and no tier (`-`)")
            continue
        file, _, item = where.partition("::")
        if tier not in TIERS:
            table.errors.append(f"{at}: tier `{tier}` is not deny | ratchet")
            continue
        fams = lints.split(",")
        unknown = [f for f in fams if f not in FAMILIES]
        if unknown:
            table.errors.append(f"{at}: lint family `{unknown[0]}` is not cast | arith")
            continue
        if len(fams) != len(set(fams)):
            table.errors.append(f"{at}: a lint family is listed twice")
            continue
        for fam in fams:
            if (target, file, item, fam) in seen:
                table.errors.append(f"{at}: `{where}` {fam} is listed twice for `{target}`")
            seen.add((target, file, item, fam))
            was = table.scopes.setdefault((file, item), {}).setdefault(fam, tier)
            if was != tier:
                table.errors.append(f"{at}: `{where}` {fam} is `{tier}` here and `{was}` in another row")
    whole = {file for file, item in table.scopes if not item}
    for (file, item), fams in sorted(table.scopes.items()):
        where = f"{file}::{item}" if item else file
        if item and file in whole:
            table.errors.append(f"{path}: `{where}` overlaps the whole-file scope of `{file}`")
        for fam in sorted(set(FAMILIES) - set(fams)):
            table.errors.append(f"{path}: `{where}` names no tier for {fam} — a decoder scope states both")
    return table


def attribute_end(lines, start):
    """Index one past the attribute opening at `lines[start]`."""
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


def _attributes(lines, start, stop):
    """Attribute texts in lines[start:stop], skipping blanks and comments;
    stops at the first line that is neither."""
    out, i = [], start
    while i < stop:
        s = lines[i].strip()
        if not s or s.startswith("//"):
            i += 1
        elif s.startswith("#[") or s.startswith("#!["):
            end = attribute_end(lines, i)
            out.append(" ".join(l.strip() for l in lines[i:end]))
            i = end
        else:
            break
    return out


def leading_attributes(lines):
    """The file's own inner attributes: the ones before its first item."""
    return [a for a in _attributes(lines, 0, len(lines)) if a.startswith("#![")]


def first_item_line(lines):
    """1-based line of the first thing that is not a blank, comment or attribute."""
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if not s or s.startswith("//"):
            i += 1
        elif s.startswith("#[") or s.startswith("#!["):
            i = attribute_end(lines, i)
        else:
            break
    return i + 1


def denied(attributes, inner):
    """Governed lints under `cfg_attr(not(test), deny(…))` of the wanted form."""
    out = set()
    for text in attributes:
        m = DENY.match(text)
        if m and bool(m.group(1)) == inner:
            out |= {l for l in LINT_FAMILY if re.search(rf"\bclippy::{l}\b", m.group(2))}
    return out


def _body_end(lines, fn_line, statement=False):
    """Last line of a function body, or a narrow API-allow statement/item.

    Statement mode also stops at a top-level semicolon or the containing
    block's close. A nested braced expression may shorten a statement's
    audited range; that fails closed when the compiler reports a later call.
    """
    depth, opened, block = 0, False, 0
    parens, brackets = 0, 0
    i, j = fn_line, 0
    raw_close = None
    in_str = False
    while i < len(lines):
        text = lines[i]
        while j < len(text):
            if raw_close is not None:
                end = text.find(raw_close, j)
                if end < 0:
                    break
                j, raw_close = end + len(raw_close), None
                continue
            if in_str:
                if text[j] == "\\":
                    j += 1
                elif text[j] == '"':
                    in_str = False
                j += 1
                continue
            if block:
                if text.startswith("*/", j):
                    block, j = block - 1, j + 2
                elif text.startswith("/*", j):
                    block, j = block + 1, j + 2
                else:
                    j += 1
                continue
            if text.startswith("//", j):
                break
            if text.startswith("/*", j):
                block, j = 1, j + 2
                continue
            raw = RAW.match(text, j)
            if raw and (j == 0 or not (text[j - 1].isalnum() or text[j - 1] == "_")):
                raw_close, j = '"' + raw.group(1), raw.end()
                continue
            ch = text[j]
            if ch == '"':
                in_str = True
            elif ch == "'":
                lit = CHAR.match(text, j)
                if lit:
                    j = lit.end()
                    continue
            elif ch == "{":
                depth, opened = depth + 1, True
            elif ch == "}":
                if statement and not opened:
                    return i - 1
                depth -= 1
                if opened and depth == 0:
                    return i
            elif ch == "(":
                parens += 1
            elif ch == ")":
                parens -= 1
            elif ch == "[":
                brackets += 1
            elif ch == "]":
                brackets -= 1
            elif ch == ";" and not opened and parens == brackets == 0:
                return i if statement else None
            j += 1
        i, j = i + 1, 0
    return None


def locate_item(lines, item):
    """(attributes, first_line, last_line) of the one `fn item`, 1-based and
    inclusive of its attributes; or an error string."""
    found = [i for i, l in enumerate(lines) if (m := FN.match(l)) and m.group(9) == item]
    if not found:
        return f"no `fn {item}`"
    if len(found) > 1:
        return f"{len(found)} functions are named `{item}` — an item scope names exactly one"
    fn_line = found[0]
    # walk back over the contiguous attribute / comment block above the fn
    start = fn_line
    while start > 0:
        s = lines[start - 1].strip()
        if s.startswith(("#[", "//")) or _inside_attribute(lines, start - 1):
            start -= 1
        else:
            break
    end = _body_end(lines, fn_line)
    if end is None:
        return f"`fn {item}` has no body the gate can close"
    return _attributes(lines, start, fn_line), start + 1, end + 1


def _inside_attribute(lines, at):
    """True when line `at` continues an attribute opened above it."""
    for back in range(at, max(at - 12, -1), -1):
        s = lines[back].strip()
        if s.startswith("#[") or s.startswith("#!["):
            return attribute_end(lines, back) > at
        if not s:
            return False
    return False
