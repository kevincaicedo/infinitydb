#!/usr/bin/python3
"""Controlled host engine: prove launch/reset/cleanup, not database semantics."""
import os
from pathlib import Path
import socket
import sys

if "--version" in sys.argv:
    print("infinityd fixture")
    sys.exit(0)

root = Path(os.environ["COMPARE_FIXTURE"])
if (root / "observe-affinity").exists():
    from affinity import record
    record("server-" + Path(sys.argv[0]).name)
args = sys.argv[1:]
host = "127.0.0.1"
port = args[args.index("--port") + 1]
if "--data-dir" in args:
    data = Path(args[args.index("--data-dir") + 1])
    # Exclusive creation fails if the previous replicate's data survived.
    with (data / "replicate-sentinel").open("x") as file:
        file.write(str(os.getpid()))
with (root / "launches").open("a") as file:
    file.write(f"{os.getpid()}\n")
with (root / "ports").open("a") as file:
    file.write(f"{port}\n")

with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((host, int(port)))
    listener.listen()
    while True:
        connection, _ = listener.accept()
        with connection, connection.makefile("rb") as stream:
            namespace = b"cmp"
            while line := stream.readline():
                count = int(line[1:])
                command = []
                for _ in range(count):
                    size = int(stream.readline()[1:])
                    command.append(stream.read(size))
                    assert stream.read(2) == b"\r\n"
                if command[0] == b"PING":
                    reply = b"+PONG\r\n"
                elif command[0] == b"DBSIZE":
                    reply = b":100\r\n"
                elif command[0] == b"INFO":
                    text = b"redis_version:fixture\r\n"
                    reply = b"$%d\r\n" % len(text) + text + b"\r\n"
                elif command[:2] == [b"INF.NS", b"USE"]:
                    namespace = command[2]
                    reply = b"+OK\r\n"
                elif command[:2] == [b"INF.NS", b"CREATE"] and (root / "bad-setup").exists():
                    reply = b"-ERR fixture refuses setup\r\n"
                elif command[0] == b"EXISTS":
                    reply = b":0\r\n" if namespace == b"db0" else b":1\r\n"
                else:
                    reply = b"+OK\r\n"
                connection.sendall(reply)
