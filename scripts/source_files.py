"""The source trees' walk for the Python gates: scripts/source-files.sh's `source_files`, read.

The rule of which directories a walk leaves out (one that holds Cargo's CACHEDIR.TAG, never one
by its name) lives once, in that helper; this module runs it and reads its lines, so a Python
gate reads exactly the files a shell gate reads. The helper's non-zero exit raises WalkError
with the exit and the helper's message, never a short listing.
"""

import subprocess
from pathlib import Path

HELPER = Path(__file__).resolve().parent / "source-files.sh"


class WalkError(Exception):
    """The helper exited non-zero: the walk did not read the whole tree."""


def source_files(name, *dirs):
    """The files named `name` (a find -name pattern) under `dirs`, as the helper lists them."""
    done = subprocess.run(["bash", "-c", '. "$0" && source_files "$@"', str(HELPER), name, *dirs],
                          capture_output=True, text=True, check=False)
    if done.returncode != 0:
        message = " ".join(done.stderr.split()) or "no message"
        raise WalkError(f"source_files {name} {' '.join(dirs)} exited {done.returncode}: {message}")
    return [Path(line) for line in done.stdout.splitlines()]
