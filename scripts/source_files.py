"""The source trees' walks for the Python gates, each failing closed on what it cannot read.

`source_files` is scripts/source-files.sh's `source_files`, read. The rule of which directories a
walk leaves out (one that holds Cargo's CACHEDIR.TAG, never one by its name) lives once, in that
helper; this module runs it and reads its lines, so a Python gate reads exactly the files a shell
gate reads. The helper fails closed (a find that fails, a path holding a newline), and so does
this: its non-zero exit raises WalkError with the exit and the helper's message, never a short
listing.

`listing`, `tree_listing` and `subdirectories` are the forms of pathlib's `glob`, `rglob` and an
`iterdir` loop over crate directories that a gate may use: each of those reads a directory it
cannot read as an empty one (glob and rglob skip it; a probe such as `is_file()` inside a
directory it cannot search answers False), so a gate built on them reads green on a shorter
scan. These raise WalkError instead. They list what glob, rglob and iterdir list (hidden names
included, a link listed and never followed into), so a gate's file set does not move.
"""

import fnmatch
import os
import subprocess
from pathlib import Path

HELPER = Path(__file__).resolve().parent / "source-files.sh"


class WalkError(Exception):
    """The walk did not read the whole tree."""


def source_files(name, *dirs):
    """The files named `name` (a find -name pattern) under `dirs`, as the helper lists them."""
    done = subprocess.run(["bash", "-c", '. "$0" && source_files "$@"', str(HELPER), name, *dirs],
                          capture_output=True, check=False)
    if done.returncode != 0:
        message = " ".join(done.stderr.decode(errors="replace").split()) or "no message"
        raise WalkError(f"source_files {name} {' '.join(dirs)} exited {done.returncode}: {message}")
    # One path a line, split on "\n" alone as the helper writes it: the bytes are decoded as
    # the file system names them, since text mode would turn a `\r` in a name into a newline,
    # and `splitlines` would split a name at a `\r` or a form feed, which the helper lets
    # through.
    lines = os.fsdecode(done.stdout).split("\n")
    if lines[-1] == "":
        lines.pop()
    return [Path(line) for line in lines]


def unreadable(err):
    """The `onerror` hook of a gate's `os.walk`: a directory it cannot read raises WalkError."""
    raise WalkError(f"{err.filename} cannot be read ({err.strerror}), so what it holds is unknown")


def listing(directory, pattern="*"):
    """`Path(directory).glob(pattern)` for a one-component pattern, sorted: the entries whose
    names match (fnmatch, case-sensitive, as glob on POSIX)."""
    try:
        with os.scandir(directory) as found:
            names = [entry.name for entry in found]
    except OSError as err:
        unreadable(err)
    return sorted(Path(directory) / name for name in names if fnmatch.fnmatchcase(name, pattern))


def tree_listing(top, pattern):
    """`Path(top).rglob(pattern)`, sorted: every entry below `top` whose name matches, a link
    to a directory listed and not walked into."""
    found = []
    for here, dirnames, filenames in os.walk(top, onerror=unreadable):
        found += [Path(here) / name for name in dirnames + filenames
                  if fnmatch.fnmatchcase(name, pattern)]
    return sorted(found)


def subdirectories(top):
    """The directories directly under `top` (a link to one included), sorted, each one the walk
    can search: a probe inside one it cannot search reads the refusal as absence, so such an
    entry would drop out of a crate loop unseen. scripts/source-files.sh's tree_dirs keeps this
    rule for the shell gates."""
    found = []
    for entry in listing(top):
        if entry.is_dir():
            if not os.access(entry, os.X_OK):
                raise WalkError(f"{entry} cannot be searched, so what it holds is unknown")
            found.append(entry)
    return found
