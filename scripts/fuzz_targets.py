"""The fuzz-target census (ADR-0198 D1): the one definition of the fuzz targets.

`check-fuzz-targets.sh` and the fuzz workflows read the targets through
`census`, `check-lint-scopes.sh` through `bin_rows`; no other list of them
exists, so a new target is in every one of them from its first commit.

A manifest in the tree is one of four kinds: the root manifest; a member of
the root workspace (its `members` globs); a fuzz manifest, at
`<member>/fuzz/Cargo.toml`, with `[package.metadata] cargo-fuzz = true` and a
`[workspace]` table of its own; or a probe manifest that PROBES names with
the gate that compiles it, which the `check` recipe must run. Any other
manifest is a crate no gate compiles. The census reads the files git lists,
tracked or untracked and not ignored, so a crate counts before its first
commit and an ignored local corpus is never read.

A fuzz target is one `[[bin]]` row of a fuzz manifest, at
`fuzz_targets/<name>.rs`, and every file under `fuzz_targets/` is a target's.
Names are unique across the tree, because the workflows key corpora and
caches by name. A target's working corpus is `fuzz/corpus/<name>`; its seed
directories are `fuzz/corpora/<name>` and `fuzz/seeds/<name>`, where they
exist. A directory under `corpus/`, `corpora/` or `seeds/` exists when it
holds a listed file, and one that names no target of its workspace is red.

A target reaches production code through its crate (ADR-0198 D4). It may
include a file by `#[path]` only from a package with no library target. That
file, and any file it includes by `#[path]`, names nothing through `crate::`
or `super::`, declares no module by file without `#[path]` and calls no
`include!`, outside its test modules as `strip-test-modules.awk` finds them.
A target that includes a file defines no `macro_rules!`, carries no
`#[macro_use]` and calls no `include!`: a macro the target supplies reaches
the included file by textual scope, so the fuzz crate could give it a second
definition of what production defines. Any other shape is red rather than
followed, because rustc's rules for it would be a second implementation
here.

Python 3.11 or newer (`tomllib`), standard library only: the matrix jobs
run this module on a bare runner before any toolchain is installed.
"""
import bisect
import fnmatch
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lint_scope_table as lexer  # noqa: E402  (the gates' one Rust lexer)

# GitHub runs at most 256 jobs from one matrix. Past it the targets split
# into two matrices, by a decision that changes this module.
MATRIX_JOBS_MAX = 256
# Probe manifests: each is compiled by the gate named here, which the `check`
# recipe runs (a stale row, or a gate no recipe runs, is red).
PROBES = {
    "scripts/clock-ban-probe/Cargo.toml": "check-clock-ban.sh",
    "scripts/fuzz-check-probe/Cargo.toml": "check-fuzz-targets.sh",
    "scripts/fuzz-check-probe/stale-lock/Cargo.toml": "check-fuzz-targets.sh",
    "scripts/lint-scope-probe/Cargo.toml": "check-lint-scopes.sh",
    "scripts/waker-atomics-probe/Cargo.toml": "check-waker-atomics.sh",
}
SEED_KINDS = ("corpus", "corpora", "seeds")
# Files a target reaches through `#[path]`: its own file is depth 0, a file
# it includes depth 1, a file that file includes depth 2. Deeper is red.
PATH_DEPTH_MAX = 2
STRIPPER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "strip-test-modules.awk")

PATH_LINE = re.compile(r'^#\[path\s*=\s*"([^"\\]+)"\]\s*$')
MOD_FILE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*\s*;\s*$")
MOD_ANY = re.compile(r"\bmod\s+(?:r#)?[A-Za-z_][A-Za-z0-9_]*\s*;")
TEST_ATTR = re.compile(r"^\s*#\[cfg\((?:test|all\(test,.*\))\)\]\s*$")
ATTR_OPEN = re.compile(r"#\s*!?\s*\[")
PATH_KEY = re.compile(r"\bpath\s*=")
NAMES_ROOT = re.compile(r"\b(crate|super)\s*::")
INCLUDE = re.compile(r"\binclude!\s*[(\[{]")
MACRO_RULES = re.compile(r"\bmacro_rules\s*!")
MACRO_USE = re.compile(r"#\s*!?\s*\[\s*macro_use\b")


class CensusError(Exception):
    """A manifest the census cannot read: a scope failure, never a skip."""


@dataclass
class Target:
    workspace: str  # its fuzz manifest, relative to the root
    member: str  # the member the fuzz directory sits in
    name: str
    path: str  # its file, relative to the root
    seeds: list = field(default_factory=list)  # relative to `member`


@dataclass
class Census:
    errors: list = field(default_factory=list)
    scope_error: str = ""
    manifests: int = 0
    workspaces: list = field(default_factory=list)  # fuzz manifests, census order
    rows: dict = field(default_factory=dict)  # fuzz manifest -> [(name, path)]
    targets: list = field(default_factory=list)  # census order
    target_files: int = 0
    probes: list = field(default_factory=list)  # probe manifests present
    seed_dirs: list = field(default_factory=list)
    path_sites: int = 0  # `#[path]` attributes in the targets' own files
    path_files: set = field(default_factory=set)  # files those reach


def read_manifest(path):
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as err:
        raise CensusError(f"cannot read {path} ({err})") from err


def bin_rows(manifest):
    """The `[[bin]]` rows of one manifest as (name, path); a field that is
    missing or not a string is None. The row reader, and no other check."""
    rows = read_manifest(manifest).get("bin", [])
    if not isinstance(rows, list) or not all(isinstance(r, dict) for r in rows):
        raise CensusError(f"{manifest}: `bin` is not an array of tables")
    return [tuple(r.get(k) if isinstance(r.get(k), str) else None for k in ("name", "path"))
            for r in rows]


def git_listing(root):
    """Files git lists under `root`, tracked or untracked and not ignored,
    that exist in the work tree (a deleted file is not in what cargo reads)."""
    try:
        top = subprocess.run(["git", "-C", root, "rev-parse", "--show-toplevel"],
                             capture_output=True, text=True)
        listing = subprocess.run(["git", "-C", root, "ls-files", "-z", "--cached", "--others",
                                  "--exclude-standard"], capture_output=True, text=True)
    except OSError as err:
        raise CensusError(f"cannot run git ({err})") from err
    if top.returncode or os.path.realpath(top.stdout.strip()) != os.path.realpath(root):
        raise CensusError(f"{os.path.realpath(root)} is not the top of its own git work tree")
    if listing.returncode:
        raise CensusError(f"git ls-files exited {listing.returncode}")
    return {p for p in listing.stdout.split("\0") if p and os.path.isfile(os.path.join(root, p))}


def glob_match(directory, pattern):
    """Cargo's `members` glob: `*` stays inside one path segment."""
    parts, globs = directory.split("/"), pattern.strip("/").split("/")
    return len(parts) == len(globs) and all(fnmatch.fnmatchcase(p, g) for p, g in zip(parts, globs))


def check_recipe(root):
    """The `check` recipe's lines, stripped; None when the justfile has none."""
    try:
        lines = Path(root, "justfile").read_text(encoding="utf-8").split("\n")
    except OSError:
        return None
    if "check:" not in lines:
        return None
    body = []
    for line in lines[lines.index("check:") + 1:]:
        if line and not line[0].isspace():
            break
        body.append(line.strip())
    return body


def code_and_text(path):
    """The file's lines with test modules blanked (strip-test-modules.awk),
    as code only and as code with its literals; None if a test module never
    closes (a truncated scan is a scope failure)."""
    try:
        report = subprocess.run(["awk", "-v", "mode=report", "-f", STRIPPER, path],
                                capture_output=True, text=True, check=True).stdout
        out = subprocess.run(["awk", "-f", STRIPPER, path],
                             capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError) as err:
        raise CensusError(f"cannot strip the test modules of {path} ({err})") from err
    if "unterminated 1" in report.split("\n"):
        return None
    lines = out.split("\n")
    code, text = ([[" "] * len(l) for l in lines] for _ in range(2))
    for i, j, ch in lexer.code_chars(lines):
        code[i][j] = ch
    for i, j, ch in lexer.code_chars(lines, literals=True):
        text[i][j] = ch
    return ["".join(l) for l in code], ["".join(l) for l in text]


def attributes(code):
    """(line, column, body) of every attribute in code-only lines: literals
    are blanked there, so a bracket inside a string never counts."""
    joined = "\n".join(code)
    starts = [0]
    for line in code:
        starts.append(starts[-1] + len(line) + 1)
    for m in ATTR_OPEN.finditer(joined):
        depth, k = 0, m.end() - 1
        while k < len(joined):
            depth += (joined[k] == "[") - (joined[k] == "]")
            if depth == 0:
                break
            k += 1
        line = bisect.bisect_right(starts, m.start()) - 1
        yield line, m.start() - starts[line], joined[m.end():k]


class PathScan:
    """Reads every `#[path]` of one file (D4) and collects its reds."""

    def __init__(self, census, packages, listed):
        self.census, self.packages, self.listed = census, packages, listed

    def red(self, row, message):
        self.census.errors.append(f"{row} {message}")

    def includes(self, file, label):
        """[(line, resolved path)] of the file's `#[path]` mods; each other
        `#[path]` shape is red."""
        scanned = code_and_text(file)
        if scanned is None:
            self.red("H2", f"{label}: a test module in {file} never closes, so the scan would stop")
            return None
        code, text = scanned
        out, mod_lines = [], set()  # mod_lines: indexes of the `mod name;` a `#[path]` names
        for line, column, body in attributes(code):
            if not PATH_KEY.search(body):
                continue
            m = PATH_LINE.match(text[line])
            follow = code[line + 1] if line + 1 < len(code) else ""
            if column or not m or not MOD_FILE.match(follow):
                self.red("H2", f"{label}: {file}:{line + 1}: a `#[path]` the gate does not read "
                         "(it reads `#[path = \"…\"]` at column 0 on the line above `mod name;`)")
                continue
            mod_lines.add(line + 1)
            out.append((line + 1, os.path.normpath(os.path.join(os.path.dirname(file), m.group(1)))))
        return out, code, mod_lines

    def package(self, file):
        """The manifest of the package holding `file`, or None."""
        directory = os.path.dirname(file)
        while directory:
            if directory in self.packages:
                return directory
            directory = os.path.dirname(directory)
        return None

    def target(self, target):
        found = self.includes(target.path, target.name)
        if found is None:
            return
        edges, code, _ = found
        self.census.path_sites += len(edges)
        if edges:
            for i, line in enumerate(code):
                for rule, shape in ((MACRO_RULES, "defines a `macro_rules!`"),
                                    (MACRO_USE, "carries a `#[macro_use]`"),
                                    (INCLUDE, "calls `include!`")):
                    if rule.search(line):
                        self.red("H3", f"{target.name}: {target.path}:{i + 1} {shape} and includes "
                                 "a file by `#[path]`: a macro it supplies would reach that file")
        pending = [(target.path, line, path, 1) for line, path in edges]
        while pending:
            source, line, file, depth = pending.pop(0)
            where = f"{target.name}: {source}:{line} includes {file}"
            if file.startswith("..") or os.path.isabs(file) or file not in self.listed:
                self.red("H1", f"{where}, which is not a file of this tree")
                continue
            if depth > PATH_DEPTH_MAX:
                self.red("H2", f"{where}, a third level of `#[path]` the gate does not read")
                continue
            owner = self.package(file)
            if owner is None or self.packages[owner]:
                kind = "no package" if owner is None else f"`{owner}`, a package with a library target"
                self.red("H1", f"{where}, from {kind}: call it through the crate (ADR-0198 D4)")
            self.census.path_files.add(file)
            self.included(target, file, depth, pending)

    def included(self, target, file, depth, pending):
        found = self.includes(file, target.name)
        if found is None:
            return
        edges, code, mod_lines = found
        for i, line in enumerate(code):
            where = f"{target.name}: {file}:{i + 1}"
            if NAMES_ROOT.search(line):
                self.red("H2", f"{where} names `{NAMES_ROOT.search(line).group(1)}::`, which the "
                         "fuzz crate resolves to its own root, not the production crate's")
            if INCLUDE.search(line):
                self.red("H2", f"{where} calls `include!`")
            previous = next((code[k] for k in range(i - 1, -1, -1) if code[k].strip()), "")
            if MOD_ANY.search(line) and i not in mod_lines and not TEST_ATTR.match(previous):
                self.red("H2", f"{where} declares a module by file without `#[path]`")
        pending.extend((file, line, path, depth + 1) for line, path in edges)


def census(root="."):
    c = Census()
    try:
        _census(c, root)
    except CensusError as err:
        c.scope_error = str(err)
    return c


def _census(c, root):
    listed = git_listing(root)
    manifests = sorted(p for p in listed if os.path.basename(p) == "Cargo.toml")
    c.manifests = len(manifests)
    if "Cargo.toml" not in listed:
        raise CensusError("no root manifest (Cargo.toml)")
    parsed = {m: read_manifest(os.path.join(root, m)) for m in manifests}
    workspace = parsed["Cargo.toml"].get("workspace", {})
    globs, excluded = workspace.get("members", []), workspace.get("exclude", [])
    if not isinstance(globs, list) or not all(isinstance(g, str) for g in globs + excluded):
        raise CensusError("the root manifest's workspace members are not a list of globs")
    members = set()
    for m in manifests:
        directory = os.path.dirname(m)
        if (m != "Cargo.toml" and any(glob_match(directory, g) for g in globs)
                and not any(directory == e.strip("/") or directory.startswith(e.strip("/") + "/")
                            for e in excluded)):
            members.add(directory)
    packages = {}  # package directory -> whether it has a library target
    fuzz = []
    for m in manifests:
        directory, data = os.path.dirname(m), parsed[m]
        if "package" in data:
            packages[directory or "."] = "lib" in data or os.path.join(directory, "src/lib.rs") in listed
        metadata = data.get("package", {}).get("metadata", {})
        if isinstance(metadata, dict) and metadata.get("cargo-fuzz") is True:
            if os.path.basename(directory) != "fuzz" or os.path.dirname(directory) not in members:
                c.errors.append(f"C3 {m}: a fuzz manifest outside <member>/fuzz/Cargo.toml")
            elif "workspace" not in data:
                c.errors.append(f"C3 {m}: a fuzz manifest without a [workspace] of its own")
            fuzz.append(m)
        elif m == "Cargo.toml" or directory in members:
            continue
        elif m in PROBES:
            c.probes.append(m)
        else:
            c.errors.append(f"C2 {m}: a crate no gate compiles (not the root, a member, "
                            "a fuzz manifest or a probe the census lists)")
    recipe = check_recipe(root)
    for m, gate in sorted(PROBES.items()):
        if m not in listed:
            c.errors.append(f"C4 {m}: a probe row whose manifest is gone (a stale exclusion)")
        elif recipe is None or f"./scripts/{gate}" not in recipe:
            c.errors.append(f"C4 {m}: its gate scripts/{gate} is not run by the `check` recipe")
    c.workspaces = fuzz
    if not fuzz:
        raise CensusError("no fuzz manifest in the tree")
    seen = {}
    for m in fuzz:
        directory = os.path.dirname(m)
        rows = bin_rows(os.path.join(root, m))
        c.rows[m] = rows
        if not rows:
            c.errors.append(f"C5 {m}: a fuzz manifest with no [[bin]] row")
        for name, path in rows:
            if name is None or path != f"fuzz_targets/{name}.rs":
                c.errors.append(f"C5 {m}: [[bin]] `{name}` at `{path}`, not fuzz_targets/<name>.rs")
                continue
            if f"{directory}/{path}" not in listed:
                c.errors.append(f"C5 {m}: [[bin]] `{name}` names {path}, which is not a listed file")
            if name in seen:
                c.errors.append(f"C5 {m}: target `{name}` is also a target of {seen[name]}")
            seen.setdefault(name, m)
            c.targets.append(Target(m, os.path.dirname(directory), name, f"{directory}/{path}"))
    if not any(c.rows.values()):
        raise CensusError("no [[bin]] row in any fuzz manifest")
    fuzz_dirs = {os.path.dirname(m) for m in fuzz}
    probe_dirs = {os.path.dirname(m) for m in c.probes}
    paths = {t.path for t in c.targets}
    for p in sorted(listed):
        parts = p.split("/")
        if "fuzz_targets" not in parts:
            continue
        home = "/".join(parts[:parts.index("fuzz_targets")])
        if home in probe_dirs:
            continue  # the probe's targets are the gate's to count
        c.target_files += 1
        if home not in fuzz_dirs:
            c.errors.append(f"C5 {p}: a file under fuzz_targets/ beside no fuzz manifest")
        elif p not in paths:
            c.errors.append(f"C5 {p}: a file under fuzz_targets/ that no [[bin]] row names")
    names = {}  # fuzz directory -> its target names
    for t in c.targets:
        names.setdefault(os.path.dirname(t.workspace), set()).add(t.name)
    seed_dirs = set()
    for p in listed:
        parts = p.split("/")
        for k in range(1, len(parts) - 2):
            if parts[k - 1] == "fuzz" and parts[k] in SEED_KINDS:
                seed_dirs.add(("/".join(parts[:k]), parts[k], parts[k + 1]))
                break
    for home, kind, name in sorted(seed_dirs):
        c.seed_dirs.append(f"{home}/{kind}/{name}")
        if name not in names.get(home, set()):
            c.errors.append(f"C6 {home}/{kind}/{name}: a seed directory that names no target of "
                            f"{home}/Cargo.toml")
    for t in c.targets:
        home = os.path.dirname(t.workspace)
        t.seeds = [f"fuzz/{kind}/{t.name}" for kind in ("corpora", "seeds")
                   if (home, kind, t.name) in seed_dirs]
    if len(c.targets) > MATRIX_JOBS_MAX:
        c.errors.append(f"C7 {len(c.targets)} targets: more than one matrix holds "
                        f"({MATRIX_JOBS_MAX}, GitHub's cap)")
    scan = PathScan(c, packages, listed)
    for t in c.targets:
        if t.path in listed:
            scan.target(t)


def matrix(c):
    """One row per target, in census order (ADR-0198 D3)."""
    return {"include": [{"dir": t.member, "target": t.name, "seeds": " ".join(t.seeds)}
                        for t in c.targets]}
