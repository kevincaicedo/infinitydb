#!/usr/bin/env python3
"""Capture real Redis reply bytes while the server is loading an RDB.

Method: populate a throwaway Redis with enough keys that the RDB load
takes a few seconds, SAVE, restart the server on the same dir, then race
raw-RESP commands against it during load. Redis processes events
periodically while loading (processEventsWhileBlocked), so replies arrive
mid-load. Each command is sent on a FRESH connection so per-connection
state (HELLO/SELECT/SUBSCRIBE) cannot leak between probes.

Output: one artifact file with raw reply bytes per command, captured
while INFO persistence reported loading:1.
"""

import os
import socket
import subprocess
import sys
import time

DIR = sys.argv[1]
PORT = 7791
POPULATE = int(os.environ.get("POPULATE", "8000000"))

os.makedirs(DIR, exist_ok=True)


def start():
    return subprocess.Popen(
        [
            "redis-server", "--port", str(PORT), "--dir", DIR,
            "--save", "", "--appendonly", "no", "--daemonize", "no",
            "--enable-debug-command", "yes",
            "--logfile", os.path.join(DIR, "redis.log"),
        ]
    )


def connect(timeout=2.0):
    s = socket.create_connection(("127.0.0.1", PORT), timeout=timeout)
    s.settimeout(timeout)
    return s


def cmd_bytes(*argv):
    out = b"*%d\r\n" % len(argv)
    for a in argv:
        a = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def recv_reply(s):
    """Read one full RESP reply (enough framing for our matrix)."""
    data = b""
    deadline = time.time() + 2.0
    while time.time() < deadline:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        data += chunk
        if data.endswith(b"\r\n"):
            # crude but fine: all matrix replies are single frames
            break
    return data


# Phase 1: populate + SAVE + clean shutdown.
proc = start()
time.sleep(0.6)
s = connect(timeout=120)
s.sendall(cmd_bytes("DEBUG", "POPULATE", str(POPULATE)))
print("populate:", recv_reply(s))
s.sendall(cmd_bytes("SAVE"))
s.settimeout(120)
print("save:", recv_reply(s))
s.sendall(cmd_bytes("SHUTDOWN", "NOSAVE"))
s.close()
proc.wait(timeout=60)

# Phase 2: restart and race the matrix during load.
MATRIX = [
    ("GET", cmd_bytes("GET", "k")),
    ("SET", cmd_bytes("SET", "k", "v")),
    ("DEL", cmd_bytes("DEL", "k")),
    ("MGET", cmd_bytes("MGET", "a", "b")),
    ("EXISTS", cmd_bytes("EXISTS", "k")),
    ("TTL", cmd_bytes("TTL", "k")),
    ("EXPIRE", cmd_bytes("EXPIRE", "k", "10")),
    ("INCR", cmd_bytes("INCR", "n")),
    ("SCAN", cmd_bytes("SCAN", "0")),
    ("DBSIZE", cmd_bytes("DBSIZE")),
    ("KEYS", cmd_bytes("KEYS", "*")),
    ("FLUSHALL", cmd_bytes("FLUSHALL")),
    ("RANDOMKEY", cmd_bytes("RANDOMKEY")),
    ("TYPE", cmd_bytes("TYPE", "k")),
    ("PING", cmd_bytes("PING")),
    ("ECHO", cmd_bytes("ECHO", "hi")),
    ("SELECT", cmd_bytes("SELECT", "1")),
    ("HELLO", cmd_bytes("HELLO")),
    ("HELLO3", cmd_bytes("HELLO", "3")),
    ("SUBSCRIBE", cmd_bytes("SUBSCRIBE", "ch")),
    ("UNSUBSCRIBE", cmd_bytes("UNSUBSCRIBE")),
    ("PSUBSCRIBE", cmd_bytes("PSUBSCRIBE", "p*")),
    ("PUBLISH", cmd_bytes("PUBLISH", "ch", "m")),
    ("PUBSUB", cmd_bytes("PUBSUB", "CHANNELS")),
    ("INFO-persistence", cmd_bytes("INFO", "persistence")),
    ("CONFIG-GET", cmd_bytes("CONFIG", "GET", "maxmemory")),
    ("CLIENT-GETNAME", cmd_bytes("CLIENT", "GETNAME")),
    ("CLIENT-ID", cmd_bytes("CLIENT", "ID")),
    ("COMMAND-COUNT", cmd_bytes("COMMAND", "COUNT")),
    ("DEBUG-JMAP", cmd_bytes("DEBUG", "SLEEP", "0")),
    ("OBJECT", cmd_bytes("OBJECT", "ENCODING", "k")),
    ("LOLWUT", cmd_bytes("LOLWUT")),
    ("UNKNOWNCMD", cmd_bytes("NOSUCHCMD", "x")),
    ("QUIT", cmd_bytes("QUIT")),
]

proc = start()
results = {}
info_snapshot = None
t0 = time.time()
# Wait for the port to accept, then capture as fast as possible.
while time.time() - t0 < 30:
    try:
        s = connect(timeout=1.0)
    except OSError:
        time.sleep(0.005)
        continue
    # Confirm we are inside the loading window first.
    s.sendall(cmd_bytes("INFO", "persistence"))
    info = recv_reply(s)
    s.close()
    if b"loading:1" in info:
        info_snapshot = info
        break
    if b"loading:0" in info:
        print("MISSED the loading window — increase POPULATE", file=sys.stderr)
        proc.terminate()
        sys.exit(2)

if info_snapshot is None:
    print("never connected during load", file=sys.stderr)
    proc.terminate()
    sys.exit(2)

for name, raw in MATRIX:
    try:
        s = connect(timeout=2.0)
        s.sendall(raw)
        results[name] = recv_reply(s)
        s.close()
    except OSError as e:
        results[name] = b"<<connect/send failed: %s>>" % str(e).encode()

# Confirm we were still loading at the end of the sweep.
still = b"<<gone>>"
try:
    s = connect(timeout=2.0)
    s.sendall(cmd_bytes("INFO", "persistence"))
    still = recv_reply(s)
    s.close()
except OSError:
    pass

out = ["# Redis %s -LOADING capture, %s" % ("8.0.5", time.strftime("%Y-%m-%d %H:%M:%S"))]
out.append("# populate=%d, dir=%s" % (POPULATE, DIR))
out.append("# still-loading-after-sweep: %s" % (b"loading:1" in still))
out.append("")
for name, raw in MATRIX:
    out.append("%s => %r" % (name, results.get(name, b"<<missing>>")))
out.append("")
out.append("INFO persistence during load:")
out.append(repr(info_snapshot))

report = "\n".join(out)
print(report)
with open(os.path.join(DIR, "capture.txt"), "w") as f:
    f.write(report + "\n")

# Cleanup: wait for load to finish, then shut down.
try:
    s = connect(timeout=60)
    s.settimeout(60)
    while True:
        s.sendall(cmd_bytes("INFO", "persistence"))
        if b"loading:0" in recv_reply(s):
            break
        time.sleep(0.5)
    s.sendall(cmd_bytes("SHUTDOWN", "NOSAVE"))
    s.close()
except OSError:
    pass
proc.wait(timeout=60)
