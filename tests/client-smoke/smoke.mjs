// node-redis smoke vs infinityd (M1-S03/S14). node-redis v4 speaks RESP2 by
// default — exercises the same surface as the python smoke plus the v4
// client machinery (scan iterator, dedicated subscriber connection).
//
// Hang discipline: node-redis v4 has no per-command timeout, so a lost or
// mis-framed reply parks an await forever (observed on shared CI runners,
// 2026-07-17). A watchdog converts any stall into a fast failure that names
// the stuck section; reconnects are capped so a dead server errors out
// instead of retrying silently.
//
// Usage: node smoke.mjs [host [port]]
import { createClient } from "redis";

const host = process.argv[2] ?? "127.0.0.1";
const port = Number(process.argv[3] ?? 6379);

let section = "startup";
const WATCHDOG_MS = 120_000;
const watchdog = setTimeout(() => {
  console.error(`client-smoke: HUNG in section "${section}" after ${WATCHDOG_MS / 1000}s`);
  process.exit(1);
}, WATCHDOG_MS);

function assert(cond, what) {
  if (!cond) throw new Error(`client-smoke: ${what}`);
}

function makeClient() {
  return createClient({
    socket: {
      host,
      port,
      connectTimeout: 10_000,
      reconnectStrategy: (retries) =>
        retries > 3 ? new Error("client-smoke: too many reconnects") : 50,
    },
  });
}

const client = makeClient();
client.on("error", (e) => {
  console.error(`client error in section "${section}":`, e);
  process.exit(1);
});

section = "connect";
await client.connect();

section = "strings+expiry";
assert((await client.ping()) === "PONG", "PING");
assert((await client.set("smoke:js", "v")) === "OK", "SET");
assert((await client.get("smoke:js")) === "v", "GET");
assert((await client.incr("smoke:js:ctr")) >= 1, "INCR");
assert((await client.expire("smoke:js", 100)) === true, "EXPIRE");
const ttl = await client.ttl("smoke:js");
assert(ttl > 0 && ttl <= 100, `TTL ${ttl}`);

section = "INFO";
const info = await client.info();
for (const field of ["redis_version", "connected_clients", "used_memory"]) {
  assert(info.includes(field), `INFO missing ${field}`);
}

section = "CLIENT";
await client.clientSetName("smoke-js");
assert((await client.clientGetName()) === "smoke-js", "CLIENT GETNAME");

section = "SCAN";
for (let i = 0; i < 50; i++) await client.set(`smoke:js:scan:${i}`, "x");
const seen = new Set();
for await (const key of client.scanIterator({ MATCH: "smoke:js:scan:*", COUNT: 13 })) {
  seen.add(key);
}
assert(seen.size === 50, `SCAN saw ${seen.size}/50`);

// Pub/sub: a dedicated subscriber connection (node-redis design). The
// subscribe confirmation is awaited BEFORE publishing — infinityd's
// guarantee is "once a client sees its confirmation, a PUBLISH from
// anywhere reaches it" (plane.rs); publishing concurrently with SUBSCRIBE
// races the registry sync and can legitimately see 0 receivers.
section = "pub/sub";
const sub = makeClient();
sub.on("error", (e) => {
  console.error(`subscriber error in section "${section}":`, e);
  process.exit(1);
});
await sub.connect();
let resolveGot;
const got = new Promise((resolve) => {
  resolveGot = resolve;
});
await sub.subscribe("smoke:js:chan", (message) => resolveGot(message));
const receivers = await client.publish("smoke:js:chan", "hello");
assert(receivers === 1, `PUBLISH receivers ${receivers}`);
assert((await got) === "hello", "pub/sub delivery");
await sub.unsubscribe("smoke:js:chan");

section = "quit";
await sub.quit();
await client.quit();

clearTimeout(watchdog);
console.log("node-redis smoke: OK");
