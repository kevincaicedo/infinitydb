"""The take of one token of a job pool, for scripts/check-stage.sh and the self-test driver.

  job_token.py take <fifo>... [--watch <fd>]

A job pool is a FIFO of one-byte tokens, named to cargo through CARGO_MAKEFLAGS
(`--jobserver-auth=fifo:<path>`); a process holding a token computes, and gives the
same byte back when it ends. A take waits until one of the FIFOs holds a byte, reads
it and prints `<index> <byte>`: the FIFO's place among the arguments, from 0, and the
byte's value, so the caller gives that byte back to that FIFO. A caller lists its own
token's FIFO first, so a token of its own goes before one of the shared pool.

With `--watch <fd>`, a descriptor the caller holds becoming readable ends the wait
first: nothing is read from it or taken, nothing is printed, exit 3. The stage watches
the line its gates write as each ends, so it waits on a token and on an ended gate at
once, and never on a timer.

The wait has no bound of its own: a token comes back when its holder ends, and the
caller holds every FIFO open for reading and writing, so a read never meets an end of
file. A FIFO that cannot be opened, or that reads empty (no writer holds it), exits 2
naming it, with nothing taken.
"""
import os
import select
import sys


def refuse(message):
    print(f"job_token: {message}", file=sys.stderr)
    sys.exit(2)


def main():
    args = sys.argv[1:]
    watch = None
    if len(args) >= 2 and args[-2] == "--watch":
        if not args[-1].isdigit():
            refuse(f"--watch '{args[-1]}' is no descriptor number")
        watch, args = int(args[-1]), args[:-2]
    if len(args) < 2 or args[0] != "take":
        refuse("usage: job_token.py take <fifo>... [--watch <fd>]")
    fds = []
    for path in args[1:]:
        try:
            fds.append(os.open(path, os.O_RDONLY | os.O_NONBLOCK))
        except OSError as err:
            refuse(f"cannot open the pool '{path}': {err.strerror}")
    waited = fds + ([watch] if watch is not None else [])
    while True:
        ready, _, _ = select.select(waited, [], [])
        for index, fd in enumerate(fds):
            if fd not in ready:
                continue
            try:
                byte = os.read(fd, 1)
            except BlockingIOError:
                continue  # another reader took it first
            if not byte:
                refuse(f"the pool '{args[1 + index]}' reads empty: no writer holds it")
            print(index, byte[0])
            return 0
        if watch is not None and watch in ready:
            return 3


if __name__ == "__main__":
    sys.exit(main())
