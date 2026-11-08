"""The target-root scan (ADR-0198 A2), run by the gates' self-test.

One script, scripts/target-root.sh, maps a checkout to the directory its
builds may use. This scan reads the justfile and every scripts/*.sh but that
helper and the self-test (which hold the rule and its plants) and is red on a
script that goes around it:

  - a read of CARGO_TARGET_DIR: `$CARGO_TARGET_DIR` or `${CARGO_TARGET_DIR…}`
    in shell, `os.environ[…]`, `.get(…)` or `getenv(…)` of it in a heredoc;
    the justfile alone may interpolate `{{…CARGO_TARGET_DIR…}}`, and only
    while its one assignment of it is the export of
    `./scripts/target-root.sh --print`;
  - a `--target-dir`, or a `CARGO_TARGET_DIR=` binding, whose directory is
    neither the helper's nor one made for this run. The helper's is
    `$TARGET_ROOT` (in a file that sources the helper and calls
    target_root_setup, and never assigns it), `$(target_root_kept <name>)`,
    or a path beneath either. A per-run one is `$(mktemp -d …)`, with or
    without a template, wherever it sits, or a path beneath one. A variable
    is either kind when the file assigns it at least once and every
    assignment is of that kind or the other: a variable assigned nowhere in
    the file (the environment's, a sourced file's) is neither. A heredoc's
    `"--target-dir", os.environ["X"]` reads X, whose every binding in the
    file must be the helper's or a per-run directory;
  - a path into a profile directory under a `target/`: under a set
    CARGO_TARGET_DIR a checkout's `target/` holds a stale build or none.

A path into a profile directory is a whole `target` path component, then a
`/` and, at any depth before the path ends (at the word's end or a `:`), a
component that is a profile directory or that an expansion, an unquoted glob
or a brace list can make one: `$x`, `${…}`, `$(…)`, a backquote and a
justfile's `{{…}}` each stand for any text. The profile directories are
`debug`, `release` and the name of each custom `[profile.<name>]` table of
the root's Cargo.toml. Whatever follows that component does not matter: a
binary's name, an expansion, a quote, a `:` or nothing. Quotes are removed
first, so `"$root/target"/release` is one path, and single-quoted text is
literal (`-path '*/target/*'` names no profile). The scan reads each word as
written, the words inside its command substitutions and parameter
expansions, and, when the word as written holds no such path, the word with
one variable replaced by each value the file gives it (its assignments, a
`for` loop's words, an expansion's default; a value that holds such a path
itself is judged where it is written): up to three replacements deep and 64
variants a word. A path is red bare or after `./`, `../` or `/`; anywhere in
a parameter's default (`${X:-…}`, `${X-…}`, `${X:=…}`, `${X=…}`); and after
any other prefix (the text before `target` from the word's start or its
last `:`, a leading `NAME=` or `--flag=` dropped) but a directory made for
this run, by the kind rules above. The helper's root is not such a prefix:
the builds it holds sit at `<root>/<profile>/<name>`, under no `target/`.

A path beneath a directory is one that does not climb out of it: a `..`
component after the variable makes it neither kind.

The scan holds no list of exempt sites: none is left. Comment lines, and a
line's words from one that starts with `#`, are not read for profile paths.
A variable is read file-wide, not by function scope, so one name holds one
kind in a file. Exit 0 with the scope line, 1 naming each red, 2 on a scope
error (no justfile or no script).
"""

import os
import re
import sys

SKIP = {"target-root.sh", "check-scripts-selftest.sh"}
NAME = r"[A-Za-z_][A-Za-z0-9_]*"
RAW_SHELL = re.compile(r"\$\{?CARGO_TARGET_DIR\b")
RAW_PY = re.compile(r"(?:environ(?:\.get)?\s*[\[(]|getenv\s*\()\s*[\"']CARGO_TARGET_DIR[\"']")
# Cargo's profile directories: `dev` and `test` build into debug, `release` and `bench`
# into release, and a custom `[profile.<name>]` of the root manifest into <name>.
BUILT_IN_PROFILES = {"dev": "debug", "test": "debug", "release": "release", "bench": "release"}
PROFILE_TABLE = re.compile(r"^\[profile\.([A-Za-z0-9_-]+)\]\s*(?:#.*)?$")
# A character that continues a name: `my-target/` or `x.target/` is another directory.
NAME_CHARS = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-")
TARGET = [("lit", c) for c in "target/"]
PARAM = re.compile(r"\$\{(" + NAME + r"|[0-9]+)(:?[-=+?])")
REFERENCE = re.compile(r"\$(" + NAME + r")$|\$\{(" + NAME + r")\}$")
PLACEHOLDERS = {"<a loop's word>", "<read>"}
VARIANT_DEPTH, VARIANT_CAP = 3, 64
LEAD = re.compile(r"^(?:" + NAME + r"\+?=|--?[A-Za-z0-9-]+=)")
DOTS = re.compile(r"^\.\.?(?:/\.\.?)*$")
CLIMB = re.compile(r"(?:^|/)\.\.(?:/|$)")
HEREDOC = re.compile(r"<<(-?)\s*(['\"]?)(" + NAME + r")\2")
PY_SITE = re.compile(r"[\"']--target-dir[\"']\s*,\s*(os\.environ\[[^\]\n]*\]|[^,\]\)\n]+)")
PY_ENV = re.compile(r"os\.environ\[\s*[\"'](" + NAME + r")[\"']\s*\]$")
SOURCES = re.compile(r"(?:^|[\s;&|])(?:\.|source)\s+[\"']?[^\s\"']*target-root\.sh[\"']?(?:\s|$)")
EXPORT = "export CARGO_TARGET_DIR := `./scripts/target-root.sh --print`"
MKTEMP = re.compile(r"^\$\(\s*mktemp\s(?:.*\s)?-d(?:\s|\)).*\)$|^\$\(\s*mktemp\s+-d\)$")
KEPT = re.compile(r"^\$\(\s*target_root_kept\s+[A-Za-z0-9_-]+\s*\)$")
REF = re.compile(r"^\$\{?(" + NAME + r")\}?(/.*)?$")


def words(line):
    """The line's shell words, quotes kept, `$( )`, `${ }` and `{{ }}` whole."""
    return [line[begin:end] for begin, end in spans(line)]


def spans(line):
    """Each shell word of the line as (begin, end_exclusive)."""
    out, i, n = [], 0, len(line)
    while i < n:
        while i < n and line[i] in " \t;|&<>()":
            i += 1
        start, depth, braces = i, 0, 0
        while i < n:
            c = line[i]
            if c == "\\":
                i += 2
                continue
            if c == "'" and depth == 0:
                j = line.find("'", i + 1)
                i = n if j < 0 else j + 1
                continue
            if c == '"':
                i += 1
                while i < n and line[i] != '"':
                    i += 2 if line[i] == "\\" else 1
                i += 1
                continue
            if line.startswith("$(", i) or line.startswith("${", i):
                depth += 1
                i += 2
                continue
            if line.startswith("{{", i):
                braces += 1  # a justfile interpolation, one word with its spaces
                i += 2
                continue
            if line.startswith("}}", i) and braces > 0 and depth == 0:
                braces -= 1
                i += 2
                continue
            if c in ")}" and depth > 0:
                depth -= 1
                i += 1
                continue
            if depth == 0 and braces == 0 and c in " \t;|&<>()":
                break
            i += 1
        if i > start:
            out.append((start, i))
    return out


def unquote(word):
    if len(word) >= 2 and word[0] == word[-1] == "'":
        return None  # single-quoted: literal text, never a directory the scan can follow
    return word.replace('"', "")


def closing(word, i, opener, closer):
    """The end_exclusive of the group opened at word[i], quoted text inside skipped;
    the word's length when the group never closes."""
    depth, n, quoted = 0, len(word), False
    while i < n:
        c = word[i]
        if c == "\\":
            i += 2
            continue
        if c == "'" and not quoted:
            j = word.find("'", i + 1)
            i = n if j < 0 else j + 1
            continue
        if c == '"':
            quoted = not quoted
        elif c == opener and not quoted:
            depth += 1
        elif c == closer and not quoted:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def top_level(text, separator):
    """The text split at each separator outside braces."""
    parts, depth, begin = [], 0, 0
    for i, c in enumerate(text):
        depth += {"{": 1, "}": -1}.get(c, 0)
        if c == separator and depth == 0:
            parts.append(text[begin:i])
            begin = i + 1
    return parts + [text[begin:]]


def expansion_end(word, i):
    """The end_exclusive of the expansion at word[i], or i + 1 for a lone `$`."""
    if word.startswith("{{", i):
        j = word.find("}}", i + 2)
        return len(word) if j < 0 else j + 2
    if word[i] == "`":
        j = word.find("`", i + 1)
        return len(word) if j < 0 else j + 1
    if word.startswith("$(", i):
        return closing(word, i + 1, "(", ")")
    if word.startswith("${", i):
        return closing(word, i + 1, "{", "}")
    name = re.match(NAME, word[i + 1:])
    if name:
        return i + 1 + name.end()
    return i + 2 if i + 1 < len(word) and word[i + 1] in "0123456789@*#?$!-" else i + 1


def inner_texts(raw):
    """The shell texts inside one expansion: (text, its `${…}` when a default)."""
    if raw.startswith("$((") or raw.startswith("{{"):
        return []
    if raw.startswith("$("):
        return [(raw[2:-1], None)]
    if raw.startswith("`"):
        return [(raw[1:-1], None)]
    param = PARAM.match(raw)
    if param and raw.endswith("}"):
        default = param.group(2).lstrip(":") in "-="
        return [(raw[param.end():-1], raw if default else None)]
    return []


def lex(word):
    """The word's atoms, its quotes removed, and the shell texts inside it.

    An atom is ("lit", c) for one literal character (single-quoted, escaped,
    double-quoted or plain), ("exp", raw) for one expansion, ("glob", raw) for
    an unquoted `*`, `?` or `[…]`, or ("brace", raw) for an unquoted `{…,…}`
    or `{a..b}` list.
    """
    atoms, inner, i, n, quoted = [], [], 0, len(word), False
    while i < n:
        c, j = word[i], i + 1
        if c == "'" and not quoted:
            j = word.find("'", i + 1)
            j = n if j < 0 else j
            atoms.extend(("lit", ch) for ch in word[i + 1:j])
            j += 1
        elif c == '"':
            quoted = not quoted
        elif c == "\\" and i + 1 < n:
            atoms.append(("lit", word[i + 1]))
            j = i + 2
        elif c in "$`" or word.startswith("{{", i):
            j = expansion_end(word, i)
            atoms.append(("exp", word[i:j]) if j > i + 1 else ("lit", c))
            inner.extend(inner_texts(word[i:j]))
        elif not quoted and (c in "*?" or (c == "[" and "]" in word[i + 1:])):
            j = word.index("]", i) + 1 if c == "[" else j
            atoms.append(("glob", word[i:j]))
        elif not quoted and c == "{" and brace_list(word, i):
            j = closing(word, i, "{", "}")
            atoms.append(("brace", word[i:j]))
        else:
            atoms.append(("lit", c))
        i = j
    return atoms, inner


def brace_list(word, i):
    """Whether word[i] opens a closed `{…,…}` or `{a..b}` brace list."""
    j = closing(word, i, "{", "}")
    body = word[i + 1:j - 1]
    return word[j - 1] == "}" and j - i > 2 and (len(top_level(body, ",")) > 1 or ".." in body)


def render(atoms):
    """The atoms as text, a literal `$`, backquote, quote or backslash escaped."""
    return "".join("\\" + v if kind == "lit" and v in "$`\"'\\" else v for kind, v in atoms)


def atom_pattern(kind, raw, inside_brace=False):
    if kind == "lit":
        return re.escape(raw)
    if kind == "brace" and not inside_brace and ".." not in raw:
        alternatives = top_level(raw[1:-1], ",")
        return "(?:" + "|".join(
            "".join(atom_pattern(k, r, True) for k, r in lex(alt)[0]) for alt in alternatives) + ")"
    return "." if kind == "glob" and raw != "*" else ".*"


def profile_directories(root):
    """debug, release and each custom profile the root's Cargo.toml defines."""
    names = sorted(set(BUILT_IN_PROFILES.values()))
    manifest = os.path.join(root, "Cargo.toml")
    if os.path.isfile(manifest):
        with open(manifest, encoding="utf-8", errors="replace") as f:
            for line in f:
                table = PROFILE_TABLE.match(line.strip())
                if table and table.group(1) not in BUILT_IN_PROFILES:
                    names.append(table.group(1))
    return tuple(names)


def profile_paths(atoms, profiles):
    """Each path into a profile directory in the word: (begin, end_exclusive) atom
    indexes, begin at its `target` component, end at the word's end or a `:`."""
    out, k, n = [], 0, len(atoms)
    while k + len(TARGET) <= n:
        whole = k == 0 or not (atoms[k - 1][0] == "lit" and atoms[k - 1][1] in NAME_CHARS)
        if not whole or atoms[k:k + len(TARGET)] != TARGET:
            k += 1
            continue
        end, components, current = k + len(TARGET), [], []
        while end < n and atoms[end] != ("lit", ":"):
            if atoms[end] == ("lit", "/"):
                components.append(current)
                current = []
            else:
                current.append(atoms[end])
            end += 1
        components.append(current)
        patterns = ["".join(atom_pattern(kind, raw) for kind, raw in c) for c in components]
        if any(re.fullmatch(p, profile, re.S) for p in patterns for profile in profiles):
            out.append((k, end))
            k = end
        else:
            k += 1
    return out


class File:
    def __init__(self, root, rel, profiles):
        self.rel, self.shell, self.body, self.assigns, self.loops = rel, [], [], {}, {}
        self.profiles = profiles
        logical, first, tag = "", 0, None
        with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as f:
            lines = f.read().split("\n")
        for number, line in enumerate(lines, 1):
            if tag is not None:
                if (line.strip() if tag[0] else line) == tag[1]:
                    tag = None
                elif not line.lstrip().startswith("#"):
                    self.body.append((number, line))
                continue
            if not logical:
                first = number
            if line.endswith("\\"):
                logical += line[:-1] + " "
                continue
            logical += line
            text, logical = logical, ""
            if text.lstrip().startswith("#"):
                continue
            self.shell.append((first, text))
            found = HEREDOC.search(text)
            if found:
                tag = (found.group(1) == "-", found.group(3))
        for number, text in self.shell:
            self.read_assignments(number, text)
        whole = "\n".join(text for _, text in self.shell)
        self.helper = bool(SOURCES.search(whole)) and "target_root_setup" in whole
        self.memo, self.sources = {}, {}

    def read_assignments(self, number, text):
        for word in words(text):
            match = re.match("(" + NAME + r")\+?=(.*)$", word, re.S)
            if match:
                self.assigns.setdefault(match.group(1), []).append((number, match.group(2)))
        for match in re.finditer(r"\bfor\s+(" + NAME + r")\s+in\b", text):
            self.assigns.setdefault(match.group(1), []).append((number, "<a loop's word>"))
            rest, last = text[match.end():], 0
            for begin, end in spans(rest):  # the loop's words, up to its `;` or `do`
                if ";" in rest[last:begin] or rest[begin:end] == "do":
                    break
                self.loops.setdefault(match.group(1), []).append((number, rest[begin:end]))
                last = end
        for match in re.finditer(r"\bread\s+(?:-[A-Za-z]+\s+)*(" + NAME + r")", text):
            self.assigns.setdefault(match.group(1), []).append((number, "<read>"))

    def kind_of(self, value, seen=()):
        """'helper', 'per-run', or the reason the value is neither."""
        word = unquote(value.strip())
        if word is None:
            return f"the single-quoted literal {value}"
        if MKTEMP.match(word):
            return "per-run"
        if KEPT.match(word):
            return "helper"
        ref = REF.match(word)
        if ref and CLIMB.search(ref.group(2) or ""):
            return f"a path that climbs out of ${ref.group(1)} ({value})"
        if not ref:
            if word.startswith(("target/", "./target/")) or word in ("target", "./target"):
                return f"a literal under target/ ({value})"
            if word.startswith("/"):
                return f"an absolute literal ({value})"
            return f"a value the scan cannot follow to a directory ({value})"
        return self.var_kind(ref.group(1), seen)

    def var_kind(self, name, seen=()):
        if name in self.memo:
            return self.memo[name]
        if name in seen:
            return f"${name}, assigned through itself"
        assigned = self.assigns.get(name, [])
        if name == "TARGET_ROOT":
            if assigned:
                kind = "$TARGET_ROOT, assigned in this file rather than by target_root_setup"
            elif not self.helper:
                kind = "$TARGET_ROOT in a file that does not source target-root.sh and call target_root_setup"
            else:
                kind = "helper"
        elif not assigned:
            kind = f"${name}, assigned nowhere in this file"
        else:
            kind = "helper"
            for number, value in assigned:
                got = self.kind_of(value, seen + (name,))
                if got not in ("helper", "per-run"):
                    kind = f"${name}, assigned at line {number} to {got}"
                    break
                if got == "per-run":
                    kind = "per-run" if kind in ("helper", "per-run") else kind
        self.memo[name] = kind
        return kind

    def binaries(self, text):
        """The line's reds for paths into a profile directory, and the count beneath a
        per-run directory. Each command substitution's and parameter expansion's text
        joins the work list once, so the walk ends with the line's nesting."""
        reds, safe, todo = [], 0, [(text, None)]
        while todo:
            chunk, default = todo.pop()
            for begin, end in spans(chunk):
                if chunk[begin] == "#":
                    break  # a comment to the line's end
                atoms, inner = lex(chunk[begin:end])
                todo.extend((body, default or own) for body, own in inner)
                found, beneath = self.judge(atoms, default)
                reds.extend(found)
                safe += beneath
        return reds, safe

    def judge(self, atoms, default):
        """One word's reds and its count of paths beneath a per-run directory."""
        paths = [(atoms, begin, end, None) for begin, end in profile_paths(atoms, self.profiles)]
        if not paths:
            paths = list(self.variants(atoms))
        if paths and default:
            return [f"{default} reaches a profile directory under a target/ in a parameter's "
                    "default; take it from the root scripts/target-root.sh computes"], 0
        reds, safe = [], 0
        for word, begin, end, via in paths:
            red = self.verdict(word, begin, end)
            if red is None:
                safe += 1
            elif via is None:
                reds.append(f"{red[0]} {red[1]}")
            else:
                reds.append(f"{render(atoms)}, through {via}, is {red[0]}, which {red[1]}")
        return reds, safe

    def variants(self, atoms):
        """The word's paths into a profile directory once one variable is replaced by a
        value the file gives it: (variant, begin, end, how), a bounded walk that stops
        at the first replacement holding such a path."""
        frontier, seen = [(atoms, "")], {render(atoms)}
        for _ in range(VARIANT_DEPTH):
            following = []
            for word, via in frontier:
                for index, (kind, raw) in enumerate(word):
                    if kind != "exp":
                        continue
                    for label, value in self.values_of(raw):
                        variant = word[:index] + lex(value)[0] + word[index + 1:]
                        key = render(variant)
                        if key in seen or len(seen) > VARIANT_CAP:
                            continue
                        seen.add(key)
                        how = f"{via}, then {label}" if via else label
                        paths = profile_paths(variant, self.profiles)
                        for begin, end in paths:
                            yield variant, begin, end, how
                        if not paths:
                            following.append((variant, how))
            frontier = following

    def values_of(self, raw):
        """(label, value) for each value the file gives the expansion's variable. A value
        that holds a path into a profile directory itself is judged where it is written,
        so it is no source: its uses would only repeat that verdict."""
        if raw in self.sources:
            return self.sources[raw]
        out, param = [], PARAM.match(raw)
        ref = REFERENCE.match(raw)
        name = (ref.group(1) or ref.group(2)) if ref else (param.group(1) if param else None)
        for number, value in self.assigns.get(name, []):
            if value not in PLACEHOLDERS:
                out.append((f"${name} as line {number} assigns it", value))
        for number, value in self.loops.get(name, []):
            out.append((f"${name} as line {number}'s loop gives it", value))
        if param and param.group(2).lstrip(":") in "-=" and raw.endswith("}"):
            out.append((f"{raw}'s default", raw[param.end():-1]))
        self.sources[raw] = [(label, value) for label, value in out
                             if not any(profile_paths(lex(w)[0], self.profiles)
                                        for w in words(value))]
        return self.sources[raw]

    def verdict(self, atoms, begin, end):
        """None when the path at atoms[begin:end] lies beneath a per-run directory, else
        (the path, why it is red). The path starts at the word's start or after its
        last `:` before `target`."""
        start = max((i + 1 for i in range(begin) if atoms[i] == ("lit", ":")), default=0)
        prefix = LEAD.sub("", render(atoms[start:begin]), count=1)
        shown = prefix + render(atoms[begin:end])
        if not prefix.endswith("/") or DOTS.match(prefix[:-1]):
            return shown, ("reaches a profile directory under a literal target/; take it from "
                           "the root scripts/target-root.sh computes")
        if prefix == "/":
            return shown, ("reaches a profile directory under an absolute literal; take it from "
                           "the root scripts/target-root.sh computes")
        kind = self.kind_of(prefix[:-1])
        if kind == "per-run":
            return None
        if kind == "helper":
            kind = ("the root scripts/target-root.sh computes, whose builds sit at "
                    "<root>/<profile>/<name>, under no target/")
        return shown, (f"reaches a profile directory beneath {kind}; only a directory made for "
                       "this run holds its own target/")


def scan(root):
    scripts = os.path.join(root, "scripts")
    names = sorted(n for n in os.listdir(scripts) if n.endswith(".sh") and n not in SKIP) \
        if os.path.isdir(scripts) else []
    if not os.path.isfile(os.path.join(root, "justfile")) or not names:
        print(f"TARGET-ROOT SCAN SCOPE ERROR: {root} holds no justfile or no scripts/*.sh to read")
        return 2
    reds, sites, binds, safe = [], {"helper": 0, "per-run": 0}, 0, 0
    profiles = profile_directories(root)
    for rel in ["justfile"] + [f"scripts/{n}" for n in names]:
        f = File(root, rel, profiles)
        justfile = rel == "justfile"
        if justfile:
            exports = [(n, t) for n, t in f.shell if re.match(r"\s*(export\s+)?CARGO_TARGET_DIR\s*:=", t)]
            if any(t.strip() != EXPORT for _, t in exports) or len(exports) > 1:
                reds.extend(f"{rel}:{n}: CARGO_TARGET_DIR is assigned other than by the one export "
                            f"of ./scripts/target-root.sh --print" for n, t in exports)
            uses = [n for n, t in f.shell if re.search(r"\{\{[^}]*\bCARGO_TARGET_DIR\b[^}]*\}\}", t)]
            if uses and [t.strip() for _, t in exports] != [EXPORT]:
                reds.extend(f"{rel}:{n}: interpolates CARGO_TARGET_DIR with no export of "
                            "./scripts/target-root.sh --print" for n in uses)
        for number, text in f.shell + f.body:
            if RAW_SHELL.search(text) or RAW_PY.search(text):
                reds.append(f"{rel}:{number}: reads CARGO_TARGET_DIR itself; read the root through "
                            "scripts/target-root.sh")
            found, beneath = f.binaries(text)
            reds.extend(f"{rel}:{number}: {red}" for red in found)
            safe += beneath
        for number, text in f.shell:
            ws = words(text)
            for i, word in enumerate(ws):
                value = None
                if word == "--target-dir":
                    value = ws[i + 1] if i + 1 < len(ws) else ""
                elif word.startswith("--target-dir="):
                    value = word[len("--target-dir="):]
                elif re.match(r"CARGO_TARGET_DIR\+?=", word):
                    value = word.split("=", 1)[1]
                if value is None:
                    continue
                kind = f.kind_of(value) if value else "nothing"
                if kind in sites:
                    sites[kind] += 1
                else:
                    reds.append(f"{rel}:{number}: a target directory that is neither the root's "
                                f"(scripts/target-root.sh) nor made for this run: {kind}")
        for number, text in f.body:
            for match in PY_SITE.finditer(text):
                value = match.group(1).strip()
                env = PY_ENV.match(value)
                if not env:
                    reds.append(f"{rel}:{number}: \"--target-dir\", {value}: a heredoc's target "
                                "directory is read from a variable the file binds, "
                                "os.environ[\"<name>\"]")
                    continue
                binds += 1
                kind = f.var_kind(env.group(1))
                if kind in sites:
                    sites[kind] += 1
                else:
                    reds.append(f"{rel}:{number}: \"--target-dir\", {value}: a target directory "
                                f"that is neither the root's nor made for this run: {kind}")
    for red in reds:
        print(f"TARGET-ROOT SCAN: {red}")
    scope = (f"the justfile and {len(names)} of scripts/*.sh read; {sites['helper'] + sites['per-run']} "
             f"target directories ({sites['helper']} the root's, {sites['per-run']} per run; "
             f"{binds} through a heredoc's environment); {safe} paths into a profile directory "
             "under a target/, each beneath a directory made for this run; no exempt site")
    if reds:
        print(f"target-root scan FAILED: {len(reds)} red ({scope})")
        return 1
    print(f"target-root scan OK: {scope}; no script reads CARGO_TARGET_DIR but through "
          "scripts/target-root.sh, and no path, as written or through a variable's value in "
          "its file, reaches a profile directory under a target/ (a whole target component, "
          f"then at any depth a component that is {', '.join(profiles)} or that an expansion, "
          "glob or brace list can make one, whatever follows it) but beneath a directory made "
          "for this run")
    return 0


if __name__ == "__main__":
    sys.exit(scan(sys.argv[1] if len(sys.argv) > 1 else "."))
