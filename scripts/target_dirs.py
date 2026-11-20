"""The census of lines that name cargo's output layout (ADR-0204 D4), run by
the gates' self-test on this tree and on its plants.

A checkout builds in its own target: scripts/target-root.sh refuses every
setting that moves cargo's target directory, its build directory or its target
triple before a recipe or a script that sources it runs cargo, so every
spelling of a path into target/<profile> names the root's own build. What can
still move a build is a line that names one of those settings itself. This
census holds each such line in a table, so a new or changed one is red until
its row says why it is right.

An output-layout line holds one of these names, written in one piece:
target-dir, build-dir, --target, build.target, CARGO_TARGET_DIR,
CARGO_BUILD_TARGET, CARGO_BUILD_BUILD_DIR (every name cargo reads for its
target directory, its build directory or its target triple, as a flag, a
config key or a variable; a longer name holds a shorter one), --config (it
sets any key), CARGO_HOME (it moves the config file cargo reads last) and
.cargo/config (a line that writes or names a cargo config file). The census
is `git grep -nIzF --untracked`, one -e a name, run at the checkout's top: it
reads every file git lists, tracked or untracked and not ignored, skips binary
files, and skips Markdown, scripts/target-root.sh, the gates' self-test and
its own two files (this one and scripts/target-dirs.tsv), which hold the rule
and its plants. It reads text and parses nothing, so a name is found wherever
it sits (a heredoc, an interpolation, a JSON value, a comment), and a name
split in the text (quotes or an escape inside it, a splice, a continuation, a
concatenation, a name assembled at run time) is not: review holds those.

The table, scripts/target-dirs.tsv, holds one row per distinct line: the
file, the kind, the count of lines in that file with that text, the reason,
and the line's text without leading and trailing blanks, tab-separated, text
last. A line no row names is red; so is a row that matches more or fewer
lines than its count, none included. A cargo config file below the checkout
(a .cargo/config or .cargo/config.toml that `git ls-files -co
--exclude-standard` lists) is red unless a row of kind config names its path
with an empty text and the count 1, whatever its keys spell. A row's kind is
its claim; the census checks what the text can show, and the gates' call pins
or review hold the rest:

  export        the justfile's export, or a justfile line reading it: the
                census checks the file is the justfile
  root          the root or a path beneath it: the text holds $TARGET_ROOT,
                which the helper sets read-only
  kept          a gate's kept directory, <root>/<name>-<key>: the gate's call
                pins in the self-test
  per-run       a directory one gate run makes and removes: the call pins
  fresh         a per-run build through target_root_cargo_fresh: the text
                holds it, and the function refuses a directory an earlier
                run filled
  triple        a call that names its own triple, a path it reads naming that
                triple: review
  pass-through  a harness passing its own running target directory: review
  config        a cargo config file below the checkout, read only by a call
                from under it, or a line that writes or names one: review
  comment       a comment that states the rule: the text starts with # or //

Usage: python3 -I scripts/target_dirs.py [<checkout> [<table>]] (default: the
current directory and its scripts/target-dirs.tsv; the self-test passes a
table of its own to plant a missing row). Exit 0 with the scope line, 1
naming each red, 2 on a scope error (not a work tree's top, git failing, the
table missing or malformed, no file listed).
"""

import os
import subprocess
import sys

NAMES = ["target-dir", "build-dir", "--target", "build.target", "CARGO_TARGET_DIR",
         "CARGO_BUILD_TARGET", "CARGO_BUILD_BUILD_DIR", "--config", "CARGO_HOME",
         ".cargo/config"]
TABLE = "scripts/target-dirs.tsv"
SKIPPED = ["*.md", "scripts/target-root.sh", "scripts/check-scripts-selftest.sh",
           "scripts/target_dirs.py", TABLE]
PATHSPECS = ["."] + [f":(exclude){path}" for path in SKIPPED]
CONFIGS = [":(glob)**/.cargo/config", ":(glob)**/.cargo/config.toml"]
KINDS = ["export", "root", "kept", "per-run", "fresh", "triple", "pass-through", "config",
         "comment"]
TOKEN = "target-dirs census"


def scope_error(why):
    print(f"{TOKEN} SCOPE ERROR: {why}")
    sys.exit(2)


def git(top, *args, ok=(0,)):
    run = subprocess.run(["git", "-C", top, *args], capture_output=True)
    if run.returncode not in ok:
        scope_error(f"git {' '.join(args[:2])} exited {run.returncode}: "
                    f"{run.stderr.decode(errors='replace').strip()}")
    return run


def read_table(path):
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError as err:
        scope_error(f"cannot read {TABLE} ({err.strerror})")
    rows, reds = {}, []
    for number, line in enumerate(text.split("\n"), 1):
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t", 4)
        if len(fields) != 5:
            scope_error(f"{TABLE}:{number}: {len(fields)} fields, not five "
                        "(file, kind, count, reason, text)")
        file, kind, count, reason, body = fields
        if not count.isdigit() or int(count) < 1:
            reds.append(f"{TABLE}:{number}: the count '{count}' is not a positive integer")
            continue
        if kind not in KINDS:
            reds.append(f"{TABLE}:{number}: the kind '{kind}' is not one of {', '.join(KINDS)}")
            continue
        if not reason.strip():
            reds.append(f"{TABLE}:{number}: the row gives no reason")
        if (file, body) in rows:
            reds.append(f"{TABLE}:{number}: a second row for {file}: {body}")
            continue
        claim = {"export": file == "justfile" or "the export's rows are the justfile's",
                 "root": "$TARGET_ROOT" in body or "a root row's text holds $TARGET_ROOT",
                 "fresh": "target_root_cargo_fresh" in body
                          or "a fresh row's text holds target_root_cargo_fresh",
                 "comment": body.startswith(("#", "//"))
                            or "a comment row's text starts with # or //",
                 "config": body != "" or int(count) == 1
                           or "a config file's row has the count 1"}.get(kind, True)
        if claim is not True:
            reds.append(f"{TABLE}:{number}: {claim}: {file}: {body}")
        if body == "" and kind != "config":
            reds.append(f"{TABLE}:{number}: an empty text names a config file, kind config")
        rows[(file, body)] = [number, kind, int(count), 0]
    return rows, reds


def main():
    top = os.path.realpath(sys.argv[1] if len(sys.argv) > 1 else ".")
    table = sys.argv[2] if len(sys.argv) > 2 else os.path.join(top, TABLE)
    if len(sys.argv) > 3:
        scope_error("usage: python3 -I scripts/target_dirs.py [<checkout> [<table>]]")
    shown = git(top, "rev-parse", "--show-toplevel", ok=(0, 128))
    named = shown.stdout.decode().strip()
    if shown.returncode != 0 or os.path.realpath(named) != top:
        scope_error(f"{top} is not the top of a git work tree "
                    f"({named or shown.stderr.decode(errors='replace').strip()})")
    listed = git(top, "ls-files", "-z", "-co", "--exclude-standard", "--", *PATHSPECS)
    files = [p for p in listed.stdout.split(b"\0") if p]
    if not files:
        scope_error("git lists no file to read")
    rows, reds = read_table(table)
    patterns = [arg for name in NAMES for arg in ("-e", name)]
    grep = git(top, "grep", "-nIzF", "--untracked", "--no-color", *patterns, "--", *PATHSPECS,
               ok=(0, 1))
    lines = 0
    for record in grep.stdout.split(b"\n"):
        if not record:
            continue
        path, number, text = record.split(b"\0", 2)
        file = path.decode("utf-8", "surrogateescape")
        body = text.decode("utf-8", "surrogateescape").strip()
        lines += 1
        row = rows.get((file, body))
        if row is None:
            reds.append(f"{file}:{number.decode()}: names cargo's output layout and no row of "
                        f"{TABLE} holds it: {body}")
        else:
            row[3] += 1
    configs = git(top, "ls-files", "-z", "-co", "--exclude-standard", "--", *CONFIGS)
    config_files = sorted(p.decode("utf-8", "surrogateescape")
                          for p in configs.stdout.split(b"\0") if p)
    for file in config_files:
        row = rows.get((file, ""))
        if row is None:
            reds.append(f"{file}: a cargo config file below the checkout, and no config row of "
                        f"{TABLE} names it: cargo reads it for a call from under it")
        else:
            row[3] += 1
    for (file, body), (number, kind, count, seen) in sorted(rows.items(), key=lambda r: r[1][0]):
        if seen != count:
            what = "a config file that git does not list" if body == "" else f"{seen} line(s)"
            reds.append(f"{TABLE}:{number}: the row matches {what}, its count is {count}: "
                        f"{file}: {body or '(the file itself)'}")
    if reds:
        for red in reds:
            print(f"{TOKEN}: {red}")
        sys.exit(1)
    kinds = {kind: 0 for kind in KINDS}
    for number, kind, count, seen in rows.values():
        kinds[kind] += 1
    print(f"{TOKEN} OK: {len(files)} files git lists read (Markdown, scripts/target-root.sh, "
          f"the self-test and the census's own two files skipped); {lines} lines naming "
          f"cargo's output layout in {len(rows) - len(config_files)} rows and "
          f"{len(config_files)} cargo config files below the checkout, each a row: "
          + ", ".join(f"{kind} {n}" for kind, n in kinds.items()))


main()
