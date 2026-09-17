#!/usr/bin/env python3
"""Synthetic gate-run fixture; never used for performance evidence."""
import asyncio
import os
import sys
from collections import deque

if "--version" in sys.argv:
    print("gate-test fixture")
    sys.exit(0)

args = sys.argv[1:]
port = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--port=")), None)
if port is None:
    port = args[args.index("--port") + 1]
cells = int(args[args.index("--cells") + 1]) if "--cells" in args else 1
mode = os.environ.get("INF_GATE_TEST_PROBE", "basic")
pending = deque()
load_connections = set()
next_cell = 0
probe_engaged = False

# The first Redis starts; the second attempt fails in Command::spawn.
if "--appendonly" in args and os.environ.get("INF_GATE_TEST_DISABLE_REDIS"):
    os.chmod(sys.argv[0], 0o600)

async def serve(reader, writer):
    global next_cell, probe_engaged
    try:
        for _ in range(1_000_000):
            header = await reader.readline()
            if not header:
                break
            count = int(header[1:])
            assert header.startswith(b"*") and 0 < count <= 8
            command = []
            for _ in range(count):
                header = await reader.readline()
                size = int(header[1:])
                assert header.startswith(b"$") and 0 <= size <= 65536
                command.append((await reader.readexactly(size + 2))[:-2])
            if command[0] == b"INFO":
                info = (f"cell:{next_cell % cells}\r\nloop_iter_p999_us:0\r\n"
                        "memory_scope:node\r\nused_memory:4096\r\n"
                        "records_live_bytes:100\r\nindex_bytes:20\r\n"
                        "wheel_bytes:3\r\nevict_bytes:4\r\nevicted_keys:1\r\n"
                        f"acks_gated:{900 if probe_engaged else 100}\r\nfsyncs_completed:10\r\n"
                        "ckpts_completed:100\r\nmanifests_published:100\r\n"
                        "segments_truncated:100\r\n").encode()
                next_cell += 1
                writer.write(b"$%d\r\n" % len(info) + info + b"\r\n")
            elif command[0] in (b"GET", b"SET"):
                reply = b"$-1\r\n" if command[0] == b"GET" else b"+OK\r\n"
                if mode == "basic":
                    writer.write(reply)
                else:
                    load_connections.add(writer)
                    probe_engaged |= len(load_connections) >= 90
                    assert len(pending) < 20000
                    pending.append((writer, reply))
            elif command[0] == b"DBSIZE":
                writer.write(b":0\r\n")
            elif command[0] == b"PUBLISH":
                writer.write(b":1\r\n")
            elif command[0] == b"SUBSCRIBE":
                writer.write(b"*3\r\n$9\r\nsubscribe\r\n$10\r\nfan:shared\r\n:1\r\n")
                if command[1] == b"slow:chan":
                    break
            else:
                writer.write(b"+OK\r\n")
            await writer.drain()
    finally:
        load_connections.discard(writer)
        writer.close()


async def release_replies():
    while True:
        await asyncio.sleep(0.02)
        limit = len(pending) if mode == "limited" else 128
        failed = mode in ("error", "disconnect") and len(load_connections) >= 90
        for _ in range(min(limit, len(pending))):
            writer, reply = pending.popleft()
            if writer.is_closing():
                continue
            if failed and mode == "disconnect":
                writer.close()
            else:
                writer.write(b"-ERR probe refused\r\n" if failed else reply)


async def main():
    server = await asyncio.start_server(serve, "127.0.0.1", int(port), backlog=1024)
    pacer = asyncio.create_task(release_replies())
    async with server:
        try:
            await asyncio.wait_for(server.serve_forever(), timeout=180)
        finally:
            pacer.cancel()


asyncio.run(main())
