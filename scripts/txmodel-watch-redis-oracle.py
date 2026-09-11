#!/usr/bin/env python3
"""Regenerate the WATCH-history fixture from a real Redis (ADR-0116 A5).

Every history is a seeded token sequence over two keys — `W<k>` WATCH,
`S<k>` SET, `D<k>` DEL, `F` FLUSHDB, `U` UNWATCH, `X` MULTI/SET out/EXEC —
run against a temporary redis-server over a Unix socket; the verdict is
Redis's own (`*-1` = abort). `txmodel::watch` replays the fixture and must
agree line for line, so the oracle for repeated WATCH is Redis, not the
model's reading of itself. Expire and the owner-side events (evict,
restart) have no Redis spelling and stay model-only.

Usage: scripts/txmodel-watch-redis-oracle.py [--seed N] [--count N] [--out PATH]
"""

import argparse
import random
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DEFAULT_OUT = Path(__file__).resolve().parents[1] / "bins/inf-sim/seeds/watch-redis-oracle.txt"
NAMED = [
    "W0 S0 W0 X",  # the review's F07 history: a repeated WATCH keeps the first one's dirt
    "W0 S0 W1 X",  # watching another key after the first changed
    "W0 S0 U W0 X",  # UNWATCH is the reset; the later WATCH starts clean
    "W0 U S0 X",  # nothing watched at EXEC
    "W0 D0 X",  # DEL of an absent key is not a mutation
    "W0 F X",  # FLUSHDB with the watched key absent
    "S0 W0 F X",  # FLUSHDB with the watched key present
    "W0 S1 X",  # a mutation of an unwatched key
    "W0 W1 S1 W0 X",  # the repeated WATCH does not launder the other key's dirt
]


def frame(*args):
    parts = [a.encode() for a in args]
    out = b"*%d\r\n" % len(parts)
    return out + b"".join(b"$%d\r\n%s\r\n" % (len(p), p) for p in parts)


class Conn:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(5)
        self.reader = self.sock.makefile("rb")

    def call(self, *args):
        self.sock.sendall(frame(*args))
        line = self.reader.readline()
        if line.startswith(b"*") and line not in (b"*-1\r\n", b"*0\r\n"):
            n = int(line[1:])
            for _ in range(n):
                head = self.reader.readline()
                if head.startswith(b"$") and head != b"$-1\r\n":
                    self.reader.read(int(head[1:]) + 2)
        return line

    def close(self):
        self.reader.close()
        self.sock.close()


def random_history(rng, length):
    toks = []
    for _ in range(length):
        kind = rng.randrange(9)
        k = str(rng.randrange(2))
        toks.append({0: "W" + k, 1: "W" + k, 2: "S" + k, 3: "S" + k, 4: "D" + k,
                     5: "F", 6: "U", 7: "S" + k, 8: "W" + k}[kind])
    toks.append("X")
    return " ".join(toks)


def run_history(watcher, writer, history):
    watcher.call("UNWATCH")
    writer.call("FLUSHALL")
    verdict = None
    for tok in history.split():
        if tok[0] == "W":
            assert watcher.call("WATCH", "k" + tok[1]) == b"+OK\r\n"
        elif tok[0] == "S":
            assert writer.call("SET", "k" + tok[1], "v") == b"+OK\r\n"
        elif tok[0] == "D":
            writer.call("DEL", "k" + tok[1])
        elif tok == "F":
            assert writer.call("FLUSHDB") == b"+OK\r\n"
        elif tok == "U":
            assert watcher.call("UNWATCH") == b"+OK\r\n"
        elif tok == "X":
            assert watcher.call("MULTI") == b"+OK\r\n"
            assert watcher.call("SET", "out", "1") == b"+QUEUED\r\n"
            reply = watcher.call("EXEC")
            verdict = "abort" if reply == b"*-1\r\n" else "commit"
            seen = writer.call("EXISTS", "out")
            assert seen == (b":0\r\n" if verdict == "abort" else b":1\r\n"), (history, reply, seen)
        else:
            raise ValueError(tok)
    assert verdict is not None, history
    return verdict


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=20260910)
    ap.add_argument("--count", type=int, default=400)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = ap.parse_args()
    version = subprocess.check_output(["redis-server", "--version"], text=True).strip()
    rng = random.Random(args.seed)
    histories = list(NAMED) + [random_history(rng, 4 + rng.randrange(5)) for _ in range(args.count)]
    with tempfile.TemporaryDirectory(prefix="txmodel-watch-") as d:
        sock = str(Path(d) / "redis.sock")
        cmd = ["redis-server", "--port", "0", "--unixsocket", sock, "--unixsocketperm", "700",
               "--save", "", "--appendonly", "no", "--dir", d, "--loglevel", "warning"]
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 5
            while not Path(sock).exists():
                if proc.poll() is not None or time.monotonic() > deadline:
                    sys.exit("redis-server did not start")
                time.sleep(0.01)
            watcher, writer = Conn(sock), Conn(sock)
            lines = [f"# {version}", f"# transport: Unix socket, TCP disabled; seed {args.seed}; "
                     f"{len(NAMED)} named + {args.count} random histories",
                     "# W<k> WATCH  S<k> SET  D<k> DEL  F FLUSHDB  U UNWATCH  X MULTI/SET out/EXEC"]
            aborts = 0
            for h in histories:
                v = run_history(watcher, writer, h)
                aborts += v == "abort"
                lines.append(f"{h}\t{v}")
            watcher.close()
            writer.close()
        finally:
            proc.terminate()
            proc.wait(timeout=5)
    args.out.write_text("\n".join(lines) + "\n")
    print(f"{args.out}: {len(histories)} histories, {aborts} abort, {len(histories) - aborts} commit ({version})")


if __name__ == "__main__":
    main()
