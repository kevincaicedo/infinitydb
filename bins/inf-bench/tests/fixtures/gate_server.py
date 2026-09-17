#!/usr/bin/env python3
"""Synthetic gate-run fixture; never used for performance evidence."""
import asyncio
import os
import sys

if "--version" in sys.argv:
    print("gate-test fixture")
    sys.exit(0)

args = sys.argv[1:]
port = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--port=")), None)
if port is None:
    port = args[args.index("--port") + 1]

# The first Redis starts; the second attempt fails in Command::spawn.
if "--appendonly" in args and os.environ.get("INF_GATE_TEST_DISABLE_REDIS"):
    os.chmod(sys.argv[0], 0o600)

async def serve(reader, writer):
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
                info = b"cell:0\r\nloop_iter_p999_us:0\r\n"
                writer.write(b"$%d\r\n" % len(info) + info + b"\r\n")
            elif command[0] == b"GET":
                writer.write(b"$-1\r\n")
            else:
                writer.write(b"+OK\r\n")
            await writer.drain()
    finally:
        writer.close()


async def main():
    server = await asyncio.start_server(serve, "127.0.0.1", int(port), backlog=1024)
    async with server:
        await asyncio.wait_for(server.serve_forever(), timeout=30)


asyncio.run(main())
