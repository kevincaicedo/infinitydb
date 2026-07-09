#!/usr/bin/env python3
"""Idle-drain curve leg: fill N same-instant PXAT keys, poll DBSIZE through
the deadline at ~100 ms resolution, report the drain curve and rate.
Usage: drain_leg.py <port> <keys> <label>"""
import subprocess, sys, time

port, keys, label = sys.argv[1], int(sys.argv[2]), sys.argv[3]
deadline = int(time.time() * 1000) + 20_000

fill = bytearray()
for i in range(keys):
    args = [b"SET", b"storm:%d" % i, b"v", b"PXAT", b"%d" % deadline]
    fill += b"*%d\r\n" % len(args)
    for a in args:
        fill += b"$%d\r\n%s\r\n" % (len(a), a)
r = subprocess.run(["redis-cli", "-p", port, "--pipe"], input=bytes(fill),
                   capture_output=True)
pipe_out = (r.stderr + r.stdout).decode().strip().splitlines()
sys.stderr.write((pipe_out[-1] if pipe_out else "(no --pipe output)") + "\n")

def dbsize():
    out = subprocess.run(["redis-cli", "-p", port, "dbsize"],
                         capture_output=True, text=True).stdout.strip()
    return int(out) if out.isdigit() else -1

assert dbsize() == keys, "fill incomplete"
while int(time.time() * 1000) < deadline - 500:
    time.sleep(0.05)

samples = []
while True:
    off = int(time.time() * 1000) - deadline
    n = dbsize()
    samples.append((off, n))
    if n == 0 or off > 30_000:
        break
    time.sleep(0.1)

print(f"== leg: {label} · keys={keys} · deadline_ms={deadline}")
for off, n in samples:
    print(f"{off:+7d} ms  dbsize={n}")
drain = [(o, n) for o, n in samples if o >= 0]
zero = next((o for o, n in drain if n == 0), None)
if zero is not None:
    print(f"drained_by: +{zero} ms after deadline")
mid = [(o, n) for o, n in drain if n > 0]
if len(mid) >= 2:
    (o1, n1), (o2, n2) = mid[0], mid[-1]
    if o2 > o1:
        print(f"observed rate over [{o1},{o2}] ms: {(n1 - n2) / ((o2 - o1) / 1000):.0f} keys/s")
