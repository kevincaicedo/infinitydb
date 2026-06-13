#!/usr/bin/env python3
"""redis-py smoke vs infinityd (M1-S03/S14 AC: major clients work out of the
box). Exercises connection negotiation, strings, expiry, INFO field shape,
CLIENT surface, SCAN iteration, and a pub/sub round trip.

Usage: smoke.py [host [port]]   (defaults 127.0.0.1 6379)
"""

import sys
import time

import redis

HOST = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 6379


def main() -> None:
    r = redis.Redis(host=HOST, port=PORT, decode_responses=True)
    assert r.ping() is True

    # Strings + expiry.
    assert r.set("smoke:k", "v") is True
    assert r.get("smoke:k") == "v"
    assert r.incr("smoke:ctr") == 1
    assert r.expire("smoke:k", 100) is True
    assert 0 < r.ttl("smoke:k") <= 100
    assert r.mget(["smoke:k", "smoke:missing"]) == ["v", None]
    assert r.delete("smoke:k") == 1

    # INFO: the fields redis-py and dashboards actually read.
    info = r.info()
    for field in ("redis_version", "connected_clients", "used_memory", "uptime_in_seconds"):
        assert field in info, f"INFO missing {field}"

    # CLIENT surface.
    assert r.client_setname("smoke-py") is True
    assert r.client_getname() == "smoke-py"
    assert isinstance(r.client_id(), int)

    # SCAN: full iteration sees every key written.
    for i in range(100):
        r.set(f"smoke:scan:{i}", "x")
    seen = set(r.scan_iter(match="smoke:scan:*", count=17))
    assert len(seen) == 100, f"scan saw {len(seen)}/100"

    # Pub/sub round trip (RESP2 push frames through the client machinery).
    ps = r.pubsub()
    ps.subscribe("smoke:chan")
    assert ps.get_message(timeout=5)["type"] == "subscribe"
    assert r.publish("smoke:chan", "hello") == 1
    deadline = time.time() + 5
    msg = None
    while time.time() < deadline:
        msg = ps.get_message(timeout=1)
        if msg and msg["type"] == "message":
            break
    assert msg and msg["data"] == "hello", f"pub/sub delivery failed: {msg}"
    ps.unsubscribe()
    ps.close()

    print("redis-py smoke: OK")


if __name__ == "__main__":
    main()
