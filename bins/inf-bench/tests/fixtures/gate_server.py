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
loop_reads = [0] * cells
info_reads = [0] * cells
loop_mode = os.environ.get("INF_GATE_TEST_LOOP", "healthy")


def loop_info(cell):
    read = loop_reads[cell]
    loop_reads[cell] += 1
    buckets = [0] * 1920
    buckets[0] = 1_000_000 + min(read, 1)
    slow = loop_mode in ("diluted", "early-slow") and cell == cells - 1
    if slow:
        buckets[190] = min(2, max(0, read - 1)) * 100
        buckets[0] += max(0, read - 3) * 1_000_000
    else:
        buckets[0] += max(0, read - 1) * 1_000_000
    if loop_mode == "stale":
        buckets[0] = 1_000_000
    samples = sum(buckets)
    submits = 100 + read * 10
    if loop_mode == "rollback" and read >= 2:
        submits = 1
    info = (f"loop_histogram_schema:1\r\nloop_histogram_samples:{samples}\r\n"
            f"loop_histogram_counts:{','.join(map(str, buckets))}\r\n"
            f"loop_histogram_submits:{submits}\r\nloop_histogram_sqes:{submits * 16}\r\n"
            f"loop_histogram_iterations:{samples}\r\n")
    if loop_mode == "missing":
        return ""
    if loop_mode == "duplicate":
        return info + f"loop_histogram_samples:{samples}\r\n"
    if loop_mode == "malformed":
        return info.replace("schema:1", "schema:2")
    if loop_mode == "oversized":
        return info + "x" * 65537
    return info

# The first Redis starts; the second attempt fails in Command::spawn.
if "--appendonly" in args and os.environ.get("INF_GATE_TEST_DISABLE_REDIS"):
    os.chmod(sys.argv[0], 0o600)

async def serve(reader, writer):
    global next_cell, probe_engaged
    cell = next_cell % cells
    next_cell += 1
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
                submits = 100 + info_reads[cell] * 10
                # Feed the original scraper too: its unchecked release subtraction wraps.
                sqes = 0 if loop_mode == "rollback" and info_reads[cell] else submits * 16
                info_reads[cell] += 1
                info = (f"cell:{cell}\r\ncells:{cells}\r\nrun_id:{'0' * 40}\r\n"
                        "loop_iter_p999_us:0\r\n"
                        f"raw_submits:{submits}\r\nraw_sqes:{sqes}\r\n"
                        "memory_scope:node\r\nused_memory:4096\r\n"
                        "records_live_bytes:100\r\nindex_bytes:20\r\n"
                        "wheel_bytes:3\r\nevict_bytes:4\r\nevicted_keys:1\r\n"
                        f"acks_gated:{900 if probe_engaged else 100}\r\nfsyncs_completed:10\r\n"
                        "ckpts_completed:100\r\nmanifests_published:100\r\n"
                        "segments_truncated:100\r\n")
                if any(arg.lower() == b"loophist" for arg in command[1:]):
                    info += loop_info(cell)
                info = info.encode()
                writer.write(b"$%d\r\n" % len(info) + info + b"\r\n")
            elif command[0] in (b"GET", b"SET"):
                reply = b"$-1\r\n" if command[0] == b"GET" else b"+OK\r\n"
                refusal = os.environ.get("INF_GATE_TEST_REFUSAL")
                if refusal:
                    writer.write(f"-{refusal} fixture refused\r\n".encode())
                    await writer.drain()
                    continue
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
