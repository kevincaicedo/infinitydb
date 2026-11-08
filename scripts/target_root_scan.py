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
  - a literal path under `target/` that names a built binary
    (`target/[<triple>/]{debug,release}/<name>`, also after `./` or a
    `{{…}}/` prefix): under a set CARGO_TARGET_DIR it is a stale binary or
    none.

The scan holds no list of exempt sites: none is left. Comment lines are not
read. A variable is read file-wide, not by function scope, so one name holds
one kind in a file. Exit 0 with the scope line, 1 naming each red, 2 on a
scope error (no justfile or no script).
"""

import os
import re
import sys

SKIP = {"target-root.sh", "check-scripts-selftest.sh"}
NAME = r"[A-Za-z_][A-Za-z0-9_]*"
RAW_SHELL = re.compile(r"\$\{?CARGO_TARGET_DIR\b")
RAW_PY = re.compile(r"(?:environ(?:\.get)?\s*[\[(]|getenv\s*\()\s*[\"']CARGO_TARGET_DIR[\"']")
BINARY = re.compile(r"(?:^|(?<=[\s\"'=(:;|&]))(?:\./|\{\{[^}]*\}\}/)?target/(?:[^/\s\"'$]+/)?"
                    r"(?:debug|release)/[^/\s\"']+")
HEREDOC = re.compile(r"<<(-?)\s*(['\"]?)(" + NAME + r")\2")
PY_SITE = re.compile(r"[\"']--target-dir[\"']\s*,\s*(os\.environ\[[^\]\n]*\]|[^,\]\)\n]+)")
PY_ENV = re.compile(r"os\.environ\[\s*[\"'](" + NAME + r")[\"']\s*\]$")
SOURCES = re.compile(r"(?:^|[\s;&|])(?:\.|source)\s+[\"']?[^\s\"']*target-root\.sh[\"']?(?:\s|$)")
EXPORT = "export CARGO_TARGET_DIR := `./scripts/target-root.sh --print`"
MKTEMP = re.compile(r"^\$\(\s*mktemp\s(?:.*\s)?-d(?:\s|\)).*\)$|^\$\(\s*mktemp\s+-d\)$")
KEPT = re.compile(r"^\$\(\s*target_root_kept\s+[A-Za-z0-9_-]+\s*\)$")
REF = re.compile(r"^\$\{?(" + NAME + r")\}?(?:/.*)?$")


def words(line):
    """The line's shell words, quotes kept, `$( )` and `${ }` whole."""
    out, i, n = [], 0, len(line)
    while i < n:
        while i < n and line[i] in " \t;|&<>()":
            i += 1
        start, depth = i, 0
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
            if c in ")}" and depth > 0:
                depth -= 1
                i += 1
                continue
            if depth == 0 and c in " \t;|&<>()":
                break
            i += 1
        if i > start:
            out.append(line[start:i])
    return out


def unquote(word):
    if len(word) >= 2 and word[0] == word[-1] == "'":
        return None  # single-quoted: literal text, never a directory the scan can follow
    return word.replace('"', "")


class File:
    def __init__(self, root, rel):
        self.rel, self.shell, self.body, self.assigns = rel, [], [], {}
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
            for word in words(text):
                match = re.match("(" + NAME + r")\+?=(.*)$", word, re.S)
                if match:
                    self.assigns.setdefault(match.group(1), []).append((number, match.group(2)))
            for match in re.finditer(r"\bfor\s+(" + NAME + r")\s+in\b", text):
                self.assigns.setdefault(match.group(1), []).append((number, "<a loop's word>"))
            for match in re.finditer(r"\bread\s+(?:-[A-Za-z]+\s+)*(" + NAME + r")", text):
                self.assigns.setdefault(match.group(1), []).append((number, "<read>"))
        whole = "\n".join(text for _, text in self.shell)
        self.helper = bool(SOURCES.search(whole)) and "target_root_setup" in whole
        self.memo = {}

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


def scan(root):
    scripts = os.path.join(root, "scripts")
    names = sorted(n for n in os.listdir(scripts) if n.endswith(".sh") and n not in SKIP) \
        if os.path.isdir(scripts) else []
    if not os.path.isfile(os.path.join(root, "justfile")) or not names:
        print(f"TARGET-ROOT SCAN SCOPE ERROR: {root} holds no justfile or no scripts/*.sh to read")
        return 2
    reds, sites, binds = [], {"helper": 0, "per-run": 0}, 0
    for rel in ["justfile"] + [f"scripts/{n}" for n in names]:
        f = File(root, rel)
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
            for match in BINARY.finditer(text):
                reds.append(f"{rel}:{number}: {match.group(0)} names a built binary under a literal "
                            "target/; take it from the root scripts/target-root.sh computes")
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
             f"{binds} through a heredoc's environment); no exempt site")
    if reds:
        print(f"target-root scan FAILED: {len(reds)} red ({scope})")
        return 1
    print(f"target-root scan OK: {scope}; no script reads CARGO_TARGET_DIR but through "
          "scripts/target-root.sh, and no literal target/ path names a built binary")
    return 0


if __name__ == "__main__":
    sys.exit(scan(sys.argv[1] if len(sys.argv) > 1 else "."))
