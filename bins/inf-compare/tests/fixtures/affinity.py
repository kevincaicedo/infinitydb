#!/usr/bin/python3
"""Record real kernel affinity from a process, a new thread and a child."""
import os
from pathlib import Path
import subprocess
import sys
import threading


def snapshot(role, stage):
    cpus = ",".join(map(str, sorted(os.sched_getaffinity(0))))
    path = Path(os.environ["COMPARE_FIXTURE"]) / "affinity.tsv"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, f"{role}\t{stage}\t{cpus}\n".encode())
    finally:
        os.close(descriptor)


def record(role):
    snapshot(role, "main")
    thread = threading.Thread(target=snapshot, args=(role, "thread"))
    thread.start()
    thread.join()
    subprocess.run([sys.executable, __file__, role, "child"], check=True, timeout=5)


if __name__ == "__main__":
    if len(sys.argv) == 3:
        snapshot(sys.argv[1], "child")
    else:
        record(sys.argv[1])
