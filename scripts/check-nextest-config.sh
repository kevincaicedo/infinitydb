#!/usr/bin/env bash
# The tests run under cargo-nextest, one process a test (ADR-0200 D1), so a
# static lock inside a test target no longer keeps that target's tests apart.
# Each isolation rule has one owner per runner, and this gate pairs them
# (ADR-0200 D2): the lock or marker in the source for `cargo test`, an override
# in .config/nextest.toml for nextest. It also holds the install rule (D4) and
# two oracle scans (D7). Red, each with its file and line:
#
#   config   a key outside this list: the top level holds `nextest-version =
#            { required = "<x.y.z>" }`, `[test-groups]` and `[profile.default]`
#            only; a group is `{ max-threads = 1 }`; the profile holds
#            `leak-timeout = { period = "100ms", result = "fail" }` and its
#            overrides only; an override is `filter`, `priority` and either
#            `test-group` (a group's, priority 50) or `threads-required =
#            "num-test-threads"` (a run-alone one, priority 100), each value a
#            string but the priority, an integer, and every
#            run-alone override comes before every group's (nextest takes each
#            setting from the first override that matches, and queues by
#            priority, then in list order). So no filter, retry or timeout
#            setting narrows or softens a run.
#   filter   one of `binary_id(<package>)`, `binary_id(<package>::<test
#            target>)`, `binary_id(<package>::bin/<name>)` or
#            `package(<package>)`, optionally `& test(=<name>)`, naming a
#            package and a target `cargo metadata` lists.
#   lock     a `static` whose type names Mutex, RwLock or Condvar, in a .rs
#            file under crates/, bins/ or tests/, carries on the comment line
#            above it (attributes between) `nextest-group: <group>` or
#            `nextest-in-process: <reason>`. A group marker's group exists, and
#            a group override selects every test binary that holds the lock:
#            each that compiles the file (its root, or a file it includes by
#            `mod` or `#[path]`) and, for a library's file, each test binary of
#            its package and of every workspace package that links it (through
#            normal dependencies, then one normal or dev edge). Every group is
#            named by a marker, and each binary a group override selects holds a
#            lock marked with that group.
#   alone    a test target or one test that must run alone in its lane
#            carries `nextest-alone: <reason>` in its file's leading comments
#            or on the comments above its `#[test]`, and the reason gives its
#            time in seconds ("3.5 s"): each such test adds that time to every
#            run. A run-alone override selects each marked target or test, and
#            selects nothing unmarked.
#   install  a `cargo install` of cargo-nextest in .github/workflows names
#            `--locked` and `--version` with the floor of `nextest-version`.
#   oracle   no file of an integration-test target names statvfs, statfs,
#            f_bavail or f_bfree, and no library file such a target links reads
#            one in code (its comments may name them): free space is every
#            process's, so a test reads what its own process holds (D7).
#   stall    a .rs file under bins/*/tests/, crates/*/tests/ or tests/ that
#            names MEM-BUDGET names TAIL-STALL-TIMEOUT or carries
#            `nextest-stall-default: <reason>` in its leading comments: a
#            tiered namespace's stall bound is a hang bound the test states,
#            or the reason says why no write of the file can park (D7).
#   marker   every marker binds what its rule names; one that binds nothing
#            is stale.
#
# Whether an assertion's input is box-wide, or a stated bound a hang bound,
# is a property of the program that no text scan decides: review holds it.
# The source scan reads comments and string literals as blank, follows
# `mod` and `#[path]` from each target's root as rustc resolves them, and
# reads each file once. Scope failures exit 2: no config, no `cargo
# metadata`, no test binary, no .rs file, a `mod` that resolves to no file,
# no workflow. INF_CHECK_ROOT names another checkout (the self-test's
# fixtures).
set -euo pipefail
cd "${INF_CHECK_ROOT:-$(dirname "$0")/..}"

exec python3 -I - <<'PY'
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

NAME = "nextest-config"
CONFIG = Path(".config/nextest.toml")
ALONE_PRIORITY = 100
GROUP_PRIORITY = 50
ALL_THREADS = "num-test-threads"
LEAK = {"period": "100ms", "result": "fail"}
SECONDS = re.compile(r"(?<![\w.])[0-9]+(?:\.[0-9]+)? s\b")
LOCK_TYPES = re.compile(r"\b(?:Mutex|RwLock|Condvar)\b")
FREE_SPACE = ("statvfs", "statfs", "f_bavail", "f_bfree")
# A marker is a `//` comment line of its own that starts with its kind and a
# colon; a prose mention later in a comment is no marker. A comment that starts
# like one in any other spelling (no colon, capitals, a doc or block comment,
# beside code) is red: it would read as a marker and bind nothing.
MARKER = re.compile(r"^\s*//(?![/!])\s*nextest-(group|in-process|alone|stall-default):(.*)$")
NEAR_MARKER = re.compile(
    r"(?i)^nextest\s*[-_ ]?\s*(?:group|in\s*[-_ ]?\s*process|alone|stall\s*[-_ ]?\s*default)\b"
)
FILTER = re.compile(
    r"^(?:binary_id\((?P<binary>[A-Za-z0-9_-]+(?:::(?:bin/)?[A-Za-z0-9_-]+)?)\)"
    r"|package\((?P<package>[A-Za-z0-9_-]+)\))"
    r"(?: & test\(=(?P<test>[A-Za-z0-9_:]+)\))?$"
)
reds = []


def scope_error(message):
    print(f"{NAME} SCOPE ERROR: {message}")
    sys.exit(2)


def red(message):
    reds.append(f"{NAME}: {message}")


# ---- the config -------------------------------------------------------------
try:
    config = tomllib.loads(CONFIG.read_text(encoding="utf-8"))
except FileNotFoundError:
    scope_error(f"{CONFIG} is missing")
except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as err:
    scope_error(f"cannot read {CONFIG} ({err})")

for key in sorted(set(config) - {"nextest-version", "test-groups", "profile"}):
    red(f"{CONFIG}: top-level key `{key}` is outside the list")
floor = None
version = config.get("nextest-version")
if (isinstance(version, dict) and set(version) == {"required"}
        and isinstance(version["required"], str)
        and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version["required"])):
    floor = version["required"]
else:
    red(f'{CONFIG}: `nextest-version` is not {{ required = "<x.y.z>" }}')
groups = config.get("test-groups", {})
if not isinstance(groups, dict):
    red(f"{CONFIG}: `test-groups` is not a table")
    groups = {}
for group, value in sorted(groups.items()):
    if value != {"max-threads": 1}:
        red(f"{CONFIG}: test group `{group}` is not {{ max-threads = 1 }}")
profile = config.get("profile", {})
if not isinstance(profile, dict) or set(profile) != {"default"}:
    red(f"{CONFIG}: `profile` holds a profile other than `default`, or none")
default = profile.get("default", {}) if isinstance(profile, dict) else {}
if not isinstance(default, dict):
    default = {}
for key in sorted(set(default) - {"leak-timeout", "overrides"}):
    red(f"{CONFIG}: profile key `{key}` is outside the list")
if default.get("leak-timeout") != LEAK:
    red(f'{CONFIG}: `leak-timeout` is not {{ period = "100ms", result = "fail" }}')
overrides = default.get("overrides", [])
if not isinstance(overrides, list):
    red(f"{CONFIG}: `overrides` is not an array of tables")
    overrides = []

# ---- the workspace's test binaries ------------------------------------------
try:
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
except OSError as err:
    scope_error(f"cannot run cargo metadata ({err})")
if metadata.returncode:
    scope_error(f"cargo metadata exited {metadata.returncode}: {metadata.stderr.strip()[:300]}")
try:
    packages = json.loads(metadata.stdout)["packages"]
    root = Path.cwd().resolve()
    binaries = {}  # binary id -> (package, root file, kind)
    lib_roots = {}  # package -> the root of the library other targets link
    for package in packages:
        for target in package["targets"]:
            kinds = target["kind"]
            if set(kinds) & {"lib", "rlib", "dylib"}:
                lib_roots[package["name"]] = Path(os.path.normpath(target["src_path"])).relative_to(root)
            if "custom-build" in kinds or not target.get("test", False):
                continue
            if "bin" in kinds:
                binary = f"{package['name']}::bin/{target['name']}"
            elif "test" in kinds:
                binary = f"{package['name']}::{target['name']}"
            elif any(kind.endswith("lib") or kind == "proc-macro" for kind in kinds):
                binary = package["name"]
            else:
                binary = f"{package['name']}::{kinds[0]}/{target['name']}"
            source = Path(os.path.normpath(target["src_path"]))
            binaries[binary] = (package["name"], source.relative_to(root), kinds)
    package_names = {package["name"] for package in packages}
    # Workspace edges a test binary links through: normal and dev, not build.
    depends = {package["name"]: {(dep["name"], dep.get("kind")) for dep in package["dependencies"]
                                 if dep["name"] in package_names and dep.get("kind") != "build"}
               for package in packages}
except (ValueError, KeyError, TypeError) as err:
    scope_error(f"invalid cargo metadata ({err})")
if not binaries:
    scope_error("cargo metadata lists no test binary")


def selects(match):
    """The binary ids a parsed filter selects."""
    if match["package"]:
        return {b for b, (package, _, _) in binaries.items() if package == match["package"]}
    return {match["binary"]} & set(binaries)


parsed = []  # (index, kind, selected ids, test or None, group or None)
for index, entry in enumerate(overrides):
    where = f"{CONFIG}: override {index + 1}"
    if not isinstance(entry, dict):
        red(f"{where} is not a table")
        continue
    keys = set(entry)
    wrong = [key for key in sorted(keys & {"filter", "test-group", "threads-required"})
             if not isinstance(entry[key], str)]
    if "priority" in entry and (type(entry["priority"]) is not int):
        wrong.append("priority")
    if wrong:
        red(f"{where}: {', '.join(wrong)} has the wrong type (a string each; priority an integer)")
        continue
    if keys == {"filter", "test-group", "priority"}:
        kind = "group"
        if entry["test-group"] not in groups:
            red(f"{where} names test group `{entry['test-group']}`, which [test-groups] lacks")
        if entry["priority"] != GROUP_PRIORITY:
            red(f"{where}: a group's override sets priority {GROUP_PRIORITY}, not {entry['priority']}")
    elif keys == {"filter", "threads-required", "priority"}:
        kind = "alone"
        if entry["threads-required"] != ALL_THREADS:
            red(f'{where}: `threads-required` is not "{ALL_THREADS}"')
        if entry["priority"] != ALONE_PRIORITY:
            red(f"{where}: a run-alone override sets priority {ALONE_PRIORITY}, not {entry['priority']}")
    else:
        red(f"{where} holds keys {sorted(keys)}: a group's is filter, test-group, priority; "
            "a run-alone one's filter, threads-required, priority")
        continue
    match = FILTER.match(entry["filter"])
    if not match:
        red(f"{where}: filter {entry['filter']!r} is in none of the forms")
        continue
    if match["package"] and match["package"] not in package_names:
        red(f"{where}: filter names package `{match['package']}`, which cargo metadata lacks")
        continue
    if match["binary"] and match["binary"] not in binaries:
        red(f"{where}: filter names test binary `{match['binary']}`, which cargo metadata lacks")
        continue
    parsed.append((index, kind, selects(match), match["test"], entry.get("test-group")))
alone_at = [index for index, kind, *_ in parsed if kind == "alone"]
group_at = [index for index, kind, *_ in parsed if kind == "group"]
if alone_at and group_at and max(alone_at) > min(group_at):
    red(f"{CONFIG}: run-alone override {max(alone_at) + 1} comes after group override "
        f"{min(group_at) + 1}; every run-alone override comes first")


# ---- the sources --------------------------------------------------------------
STRIP = re.compile(r"//|/\*|(?<![A-Za-z0-9_])[bc]?r#*\"|\"|'")
CHAR = re.compile(r"'(?:[^'\\\n]|\\(?:[nrt\\0'\"]|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}))'")


def blank(text):
    return re.sub(r"[^\n]", " ", text)


def strip(text, keep_strings=False, comments=None):
    """The text with comments and the insides of literals blanked, offsets kept;
    with `keep_strings`, comments only. Each comment's span (start, end, token)
    goes to `comments` when one is given."""
    out, at = [], 0
    while True:
        found = STRIP.search(text, at)
        if not found:
            out.append(text[at:])
            return "".join(out)
        start, token = found.start(), found.group()
        out.append(text[at:start])
        if token == "//":
            end = text.find("\n", start)
            end = len(text) if end < 0 else end
        elif token == "/*":
            depth, end = 1, start + 2
            while depth and end < len(text):
                if text.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif text.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
        elif token == "'":
            char = CHAR.match(text, start)
            if not char:  # a lifetime or a label
                out.append("'")
                at = start + 1
                continue
            end = char.end()
        elif token == '"':
            end = start + 1
            while end < len(text) and text[end] != '"':
                end += 2 if text[end] == "\\" else 1
            end = min(end + 1, len(text))
        else:  # a raw string: r"…", r#"…"#, with a b or c prefix
            close = '"' + "#" * token.count("#")
            end = text.find(close, found.end())
            end = len(text) if end < 0 else end + len(close)
        is_comment = token in ("//", "/*")
        if is_comment and comments is not None:
            comments.append((start, end, token))
        out.append(text[start:end] if keep_strings and not is_comment else blank(text[start:end]))
        at = end


class Source:
    """One .rs file, read once: its lines, its code with comments and literals
    blanked, its text with comments blanked, its comments, the inline module
    path at each line, and its `mod <name>;` declarations."""

    def __init__(self, path):
        self.path = path
        self.text = path.read_text(encoding="utf-8", errors="replace")
        self.raw = self.text.split("\n")
        spans = []
        self.code = strip(self.text, comments=spans).split("\n")
        self.uncommented = strip(self.text, keep_strings=True)
        self.comments = []  # (line index, comment token, the text after it, code before it)
        for start, end, token in spans:
            line = self.text.count("\n", 0, start)
            before = self.text[self.text.rfind("\n", 0, start) + 1:start]
            self.comments.append((line, token, self.text[start + 2:end], before.strip() != ""))
        self.inline = []  # the inline module path at each line's start
        self.declared = []  # (line index, inline path, name, #[path] or None)
        frames, depth = [], 0
        for number, line in enumerate(self.code):
            self.inline.append(tuple(name for name, _ in frames))
            opened = re.search(r"(?:^|[^\w])mod\s+([A-Za-z_]\w*)\s*\{", line)
            declared = re.search(r"(?:^|[^\w])mod\s+([A-Za-z_]\w*)\s*;", line)
            if declared:
                path = self.path_attribute(number)
                self.declared.append((number, self.inline[-1], declared.group(1), path))
            if opened:
                before = line[:opened.end()]
                depth += before.count("{") - before.count("}")
                frames.append((opened.group(1), depth))
                rest = line[opened.end():]
                depth += rest.count("{") - rest.count("}")
            else:
                depth += line.count("{") - line.count("}")
            while frames and frames[-1][1] > depth:
                frames.pop()

    def is_comment(self, number):
        return self.code[number].strip() == "" and self.raw[number].strip().startswith("//")

    def attribute_above(self, number):
        """The first line of the attribute that ends on `number`, or None."""
        code = self.code[number].strip()
        if code.startswith("#[") and code.count("[") == code.count("]"):
            return number
        if not code.endswith("]"):
            return None
        for up in range(number - 1, max(number - 30, -1), -1):
            if self.code[up].strip().startswith("#["):
                return up
        return None

    def path_attribute(self, number):
        """`#[path = "…"]` on the declaration's line or the attributes above it."""
        found = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]', self.raw[number])
        up = number - 1
        while not found and up >= 0:
            if self.is_comment(up):
                up -= 1
                continue
            first = self.attribute_above(up)
            if first is None:
                break
            found = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]', " ".join(self.raw[first:up + 1]))
            up = first - 1
        return found.group(1) if found else None

    def attribute_end(self, number):
        """The last line of the attribute that starts on `number`: where its
        brackets, counted from its first line, balance (an attribute spans
        lines, `#![allow(\\n…\\n)]`), or the file's last line."""
        end, balance = number, 0
        while True:
            balance += self.code[end].count("[") - self.code[end].count("]")
            if balance <= 0 or end + 1 >= len(self.code):
                return end
            end += 1

    def leading(self):
        """The line numbers of the file's leading block: comments, blank lines and
        inner attributes before its first item."""
        number, block = 0, []
        while number < len(self.raw):
            code = self.code[number].strip()
            if code == "":
                block.append(number)
                number += 1
            elif code.startswith("#!["):
                end = self.attribute_end(number)
                block.extend(range(number, end + 1))
                number = end + 1
            else:
                break
        return set(block)


sources = {}


def source(path):
    if path not in sources:
        sources[path] = Source(path)
    return sources[path]


try:
    listed = subprocess.run(
        ["git", "ls-files", "-co", "--exclude-standard", "-z", "--", "crates", "bins", "tests"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=True,
    ).stdout.split("\0")
except (OSError, subprocess.CalledProcessError) as err:
    scope_error(f"git cannot list the sources ({err})")
rust_files = sorted(Path(name) for name in listed if name.endswith(".rs") and Path(name).is_file())
if not rust_files:
    scope_error("git lists no .rs file under crates/, bins/ or tests/")

unresolved = []  # each `mod` declaration that resolves to no file


def walk(root_file):
    """Every file a crate root compiles, by `mod` and `#[path]`, with its module
    paths. As rustc resolves them: `mod x;` in a root, a `mod.rs` or a file
    loaded by `#[path]` reads `x.rs` or `x/mod.rs` beside it, in any other file
    `<stem>/x.rs`; a `#[path]` is relative to the declaring file's directory,
    inside an inline module to that module's directory."""
    files, todo = {}, [(root_file, (), True)]
    while todo:
        path, module, owns_dir = todo.pop()
        if module in files.get(path, set()):
            continue
        if not path.is_file():
            unresolved.append(f"{path} (a crate root cargo metadata names)")
            continue
        files.setdefault(path, set()).add(module)
        for number, inline, name, attribute in source(path).declared:
            base = path.parent if owns_dir else path.parent / path.stem
            if attribute is not None:
                start = base.joinpath(*inline) if inline else path.parent
                child = Path(os.path.normpath(start / attribute))
            else:
                start = base.joinpath(*inline)
                child = start / f"{name}.rs"
                if not child.is_file():
                    child = start / name / "mod.rs"
            if not child.is_file():
                unresolved.append(f"{path}:{number + 1}: `mod {name};` resolves to no file ({child})")
                continue
            todo.append((child, module + inline + (name,), attribute is not None or child.name == "mod.rs"))
    return files


# Which test binaries compile each file, and at which module path.
compiled_by = {}  # file -> {(binary id, module path)}
for binary, (_, root_file, kinds) in sorted(binaries.items()):
    for path, modules in walk(root_file).items():
        compiled_by.setdefault(path, set()).update((binary, module) for module in modules)


def linkers(package):
    """The packages whose test binaries link `package`'s library: itself, each
    that reaches it through normal dependencies, and each that depends on one
    of those, dev dependencies included."""
    normal, grew = {package}, True
    while grew:
        grew = False
        for name, edges in depends.items():
            if name not in normal and any(dep in normal and kind is None for dep, kind in edges):
                normal.add(name)
                grew = True
    return normal | {name for name, edges in depends.items() if any(dep in normal for dep, _ in edges)}


# Which test binaries hold each file's statics: those that compile it, and for a
# library's file every test binary that links the library.
linked_by = {path: {binary for binary, _ in users} for path, users in compiled_by.items()}
for package, lib_root in sorted(lib_roots.items()):
    reach = linkers(package)
    users = {binary for binary, (owner, _, _) in binaries.items() if owner in reach}
    for path in walk(lib_root):
        linked_by.setdefault(path, set()).update(users)
if unresolved:
    scope_error("a module this gate cannot find would go unread: " + "; ".join(unresolved[:5]))

integration = {path for path, users in compiled_by.items()
               if any("test" in binaries[b][2] for b, _ in users)}
linked = {path for path, users in linked_by.items()
          if path not in integration and any("test" in binaries[b][2] for b in users)}
scanned = sorted(set(rust_files) | set(linked_by))
bound = set()  # (file, line) of each marker that binds
locks = []  # (file, line, verdict, group or reason)
holders = {}  # group -> the test binaries that hold a lock marked with it
alone_marks = []  # (file, line, test path or None)

for path in scanned:
    here = source(path)
    for number, line in enumerate(here.code):
        found = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?static\s+(?:mut\s+)?[A-Za-z_]\w*\s*:(.*)$", line)
        if not found:
            continue
        kind_text, end = found.group(1), number
        while "=" not in kind_text and ";" not in kind_text and end + 1 < len(here.code):
            end += 1
            kind_text += here.code[end]
        if not LOCK_TYPES.search(kind_text.split("=")[0]):
            continue
        above = number - 1
        while above >= 0:
            first = here.attribute_above(above)
            if first is None:
                break
            above = first - 1
        mark = MARKER.search(here.raw[above]) if above >= 0 and here.is_comment(above) else None
        if not mark or mark.group(1) not in ("group", "in-process"):
            red(f"{path}:{number + 1}: a static lock with no `nextest-group: <group>` or "
                "`nextest-in-process: <reason>` on the comment line above it")
            continue
        bound.add((path, above))
        reason = mark.group(2).strip()
        if mark.group(1) == "in-process":
            if not reason:
                red(f"{path}:{above + 1}: `nextest-in-process:` gives no reason")
            locks.append((path, number, "in-process", reason))
            continue
        if not re.fullmatch(r"[A-Za-z0-9_-]+", reason):
            red(f"{path}:{above + 1}: `nextest-group:` names no group ({reason!r})")
            continue
        locks.append((path, number, "group", reason))
        if reason not in groups:
            red(f"{path}:{above + 1}: names test group `{reason}`, which [test-groups] lacks")
        users = sorted(linked_by.get(path, set()))
        holders.setdefault(reason, set()).update(users)
        if not users:
            red(f"{path}:{number + 1}: a grouped lock in a file no test binary compiles")
        for binary in users:
            if not any(kind == "group" and group == reason and binary in chosen and test is None
                       for _, kind, chosen, test, group in parsed):
                red(f"{path}:{number + 1}: test binary `{binary}` compiles this lock, and no "
                    f"override puts it in group `{reason}`")

for path in scanned:
    here = source(path)
    lead = here.leading()
    for number, raw in enumerate(here.raw):
        if not here.is_comment(number):
            continue
        mark = MARKER.search(raw)
        if not mark or mark.group(1) != "alone":
            continue
        reason, follow = mark.group(2), number + 1
        while (follow < len(here.raw) and here.is_comment(follow)
               and here.raw[follow].strip() not in ("//", "///") and not MARKER.search(here.raw[follow])):
            reason += " " + here.raw[follow].strip().lstrip("/").strip()
            follow += 1
        if not SECONDS.search(reason):
            red(f"{path}:{number + 1}: a `nextest-alone:` reason gives no time in seconds "
                "(\"3.5 s\"): each run-alone test adds its time to every run")
        below, is_test = number + 1, False
        while below < len(here.raw):
            if here.is_comment(below):
                below += 1
                continue
            code = here.code[below].strip()
            if not code.startswith("#["):
                break
            end = here.attribute_end(below)
            is_test |= re.fullmatch(r"#\[test\]", code) is not None
            below = end + 1
        function = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)",
                            here.code[below]) if below < len(here.code) else None
        if is_test and function:
            alone_marks.append((path, number, here.inline[below] + (function.group(1),)))
        elif number in lead:
            alone_marks.append((path, number, None))
        else:
            red(f"{path}:{number + 1}: a `nextest-alone:` marker neither above a `#[test]` nor "
                "in its file's leading comments binds nothing")
            continue
        bound.add((path, number))

expected = set()  # (binary id, test name or None)
for path, number, test in alone_marks:
    users = sorted(compiled_by.get(path, set()))
    if not users:
        red(f"{path}:{number + 1}: a `nextest-alone:` marker in a file no test binary compiles")
    for binary, module in users:
        name = "::".join(module + test) if test else None
        expected.add((binary, name))
        if not any(kind == "alone" and binary in chosen and wanted == name
                   for _, kind, chosen, wanted, _ in parsed):
            shown = f"`{binary}` test `{name}`" if name else f"test binary `{binary}`"
            red(f"{path}:{number + 1}: {shown} is marked to run alone, and no run-alone "
                "override selects it")
for index, kind, chosen, wanted, _ in parsed:
    for binary in sorted(chosen):
        if kind == "alone" and (binary, wanted) not in expected:
            shown = f"`{binary}` test `{wanted}`" if wanted else f"test binary `{binary}`"
            red(f"{CONFIG}: override {index + 1} runs {shown} alone, and no `nextest-alone:` "
                "marker names it")
for index, kind, chosen, _, group in parsed:
    for binary in sorted(chosen - holders.get(group, set())) if kind == "group" else ():
        red(f"{CONFIG}: override {index + 1} puts `{binary}` in group `{group}`, and no lock that "
            f"binary holds is marked `nextest-group: {group}`")
named = {reason for _, _, verdict, reason in locks if verdict == "group"}
for group in sorted(set(groups) - named):
    red(f"{CONFIG}: test group `{group}` is named by no lock's `nextest-group:` marker")

# ---- D7: free space and the stall bound ----------------------------------------
free_space_files = 0
for path in sorted(integration):
    free_space_files += 1
    for number, raw in enumerate(source(path).raw):
        for word in FREE_SPACE:
            if word in raw:
                red(f"{path}:{number + 1}: names `{word}` in an integration-test target: free "
                    "space is every process's, so read what this process holds")
# A library an integration test links reads none in code; its comments may name them.
for path in sorted(linked):
    for number, line in enumerate(source(path).uncommented.split("\n")):
        for word in FREE_SPACE:
            if word in line:
                red(f"{path}:{number + 1}: reads `{word}` in a library integration tests link: "
                    "free space is every process's, so read what this process holds")


def under_tests(path):
    parts = path.parts
    return parts[0] == "tests" or (len(parts) > 3 and parts[0] in ("bins", "crates")
                                   and parts[2] == "tests")


budget_files = stated = defaulted = 0
for path in scanned:
    here = source(path)
    lead = here.leading()
    names_budget = under_tests(path) and "MEM-BUDGET" in here.text
    has_marker = False
    for number, raw in enumerate(here.raw):
        mark = MARKER.search(raw) if here.is_comment(number) else None
        if not mark or mark.group(1) != "stall-default":
            continue
        if not names_budget or number not in lead or not mark.group(2).strip():
            red(f"{path}:{number + 1}: a `nextest-stall-default:` marker binds nothing (it "
                "belongs, with its reason, in the leading comments of a test file naming MEM-BUDGET)")
            continue
        has_marker = True
        bound.add((path, number))
    if not names_budget:
        continue
    budget_files += 1
    if "TAIL-STALL-TIMEOUT" in here.text:
        stated += 1
    elif has_marker:
        defaulted += 1
    else:
        red(f"{path}: names MEM-BUDGET, and neither names TAIL-STALL-TIMEOUT nor carries "
            "`nextest-stall-default: <reason>` in its leading comments")

for path in scanned:
    here = source(path)
    for number, raw in enumerate(here.raw):
        mark = MARKER.search(raw) if here.is_comment(number) else None
        if mark and (path, number) not in bound and mark.group(1) in ("group", "in-process"):
            red(f"{path}:{number + 1}: a `nextest-{mark.group(1)}:` marker on no static lock")
    for line, token, body, beside_code in here.comments:
        for offset, part in enumerate(body.split("\n")):
            text = part.lstrip("/!* \t")
            if not NEAR_MARKER.match(text):
                continue
            number = line + offset
            if token == "//" and not beside_code and MARKER.match(here.raw[number]):
                continue
            red(f"{path}:{number + 1}: a comment spelled like a marker (`{text[:40].rstrip()}`) "
                "is not read: a marker is a comment line of its own, `// nextest-<kind>: <reason>`")

# ---- D4: the install step ---------------------------------------------------------
workflows = sorted(Path(".github/workflows").glob("*.y*ml"))
if not workflows:
    scope_error("no workflow under .github/workflows")
installs = 0
for path in workflows:
    for number, raw in enumerate(path.read_text(encoding="utf-8").split("\n")):
        if "cargo install" not in raw or "cargo-nextest" not in raw:
            continue
        installs += 1
        words = raw.split()
        version = re.search(r"--version[ =]+(\S+)", raw)
        if "--locked" not in words or not version or version.group(1).strip("'\"") != floor:
            red(f"{path}:{number + 1}: a cargo-nextest install names `--locked` and "
                f"`--version {floor}`, the floor of {CONFIG}")

if reds:
    print("\n".join(reds))
    print(f"{NAME} FAILED: {len(reds)} violation(s); the rules are this gate's header (ADR-0200 D2, D4, D7)")
    sys.exit(1)
alone_names = [f"{b}/{t}" if t else b for b, t in sorted(expected)]
in_process = sum(1 for lock in locks if lock[2] == "in-process")
print(
    f"{NAME} OK: nextest-version {floor}; {len(groups)} groups of one thread "
    f"({', '.join(sorted(groups)) or 'none'}), {len(group_at)} group overrides at priority "
    f"{GROUP_PRIORITY}; {len(expected)} run alone at priority {ALONE_PRIORITY}, first "
    f"({', '.join(alone_names) or 'none'}); {len(locks)} static locks, {len(locks) - in_process} "
    f"grouped and {in_process} in-process; {budget_files} test files name MEM-BUDGET, {stated} "
    f"state TAIL-STALL-TIMEOUT and {defaulted} carry nextest-stall-default; no free-space read in "
    f"{free_space_files} integration-test files and {len(linked)} library files they link; "
    f"{installs} cargo-nextest installs in CI at "
    f"{floor}, each --locked; {len(scanned)} .rs files of {len(binaries)} test binaries read"
)
PY
